//! The SpiroDB server: SQL over TCP, from any language, plus streaming
//! replication between servers.
//!
//! Clients send one JSON request per line and get one JSON line back:
//!
//! ```text
//! {"sql": "SELECT name FROM users WHERE id = ?", "params": [1], "min_seq": 42}
//!   -> {"columns": ["name"], "rows": [["Ada"]]}
//! {"sql": "INSERT INTO users (name) VALUES ('Alan')"}
//!   -> {"command": "INSERT", "rows": 1, "seq": 43}
//!   -> {"error": "UNIQUE constraint failed: users.name", "kind": "constraint"}
//! {"status": true}  -> {"role": "primary", "epoch": 1, "seq": 43, ...}
//! {"promote": true} -> {"command": "PROMOTE", "epoch": 2}
//! {"follow": "10.0.0.2:7878"} -> {"command": "FOLLOW"}
//! ```
//!
//! Each connection is a session: transactions span requests, and
//! notifications for channels it LISTENs on arrive as extra lines,
//! `{"notification": {"channel": ..., "payload": ..., "sender": ...}}`.
//!
//! `min_seq` is for reading from replicas: a replica waits until it has
//! applied that commit (the `seq` of the client's latest write), or answers
//! with `"kind": "behind"` so the client can ask the primary instead.
//!
//! A replica connects to its primary and sends `{"replicate": {"from": N}}`;
//! from then on the connection carries binary frames: the primary streams
//! every commit after N (after a snapshot, if N is too old) and the replica
//! acknowledges what it has applied.

use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::json::Json;
use crate::sql::repl::{self, Entry, Role};
use crate::sql::{Database, QueryResult, Session, Value};

/// How a server runs.
#[derive(Clone, Debug)]
pub struct ServerOptions {
    /// Follow this primary (`host:port`) as a read-only replica.
    pub replica_of: Option<String>,
    /// How long a replica waits to catch up to a read's `min_seq` before
    /// telling the client to read from the primary instead.
    pub read_wait: Duration,
}

impl Default for ServerOptions {
    fn default() -> Self {
        ServerOptions { replica_of: None, read_wait: Duration::from_secs(1) }
    }
}

struct Shared {
    db: Database,
    read_wait: Duration,
    stop: AtomicBool,
    /// Every open connection, so shutdown can close them.
    connections: Mutex<Vec<TcpStream>>,
    follower: Mutex<Follower>,
}

#[derive(Default)]
struct Follower {
    running: Option<Arc<AtomicBool>>,
    socket: Option<TcpStream>,
    thread: Option<JoinHandle<()>>,
}

/// A running server. Dropping it shuts the server down.
pub struct Server {
    addr: SocketAddr,
    shared: Arc<Shared>,
    acceptor: Option<JoinHandle<()>>,
}

impl Server {
    /// Starts serving `db` on `addr` (use port 0 to pick a free port).
    pub fn start(db: Database, addr: impl ToSocketAddrs, opts: ServerOptions) -> Result<Server> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let shared = Arc::new(Shared {
            db,
            read_wait: opts.read_wait,
            stop: AtomicBool::new(false),
            connections: Mutex::new(Vec::new()),
            follower: Mutex::new(Follower::default()),
        });
        if let Some(primary) = opts.replica_of {
            start_following(&shared, primary)?;
        }
        let acceptor = {
            let shared = shared.clone();
            thread::Builder::new().name("spiro-accept".into()).spawn(move || accept(shared, listener))?
        };
        Ok(Server { addr, shared, acceptor: Some(acceptor) })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn database(&self) -> &Database {
        &self.shared.db
    }

    /// Promotes this replica to primary. Make sure the old primary is down
    /// first: two primaries would accept conflicting writes. The new epoch
    /// lets clients tell this primary apart from the old one.
    pub fn promote(&self) -> Result<u64> {
        promote(&self.shared)
    }

    /// Makes this server follow `primary`: a replica switching to a newly
    /// promoted primary, or an old primary rejoining as a replica. If it has
    /// commits the new primary never got, it starts over from a snapshot.
    pub fn follow(&self, primary: &str) -> Result<()> {
        follow(&self.shared, primary)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        stop_following(&self.shared);
        for conn in self.shared.connections.lock().unwrap().drain(..) {
            let _ = conn.shutdown(std::net::Shutdown::Both);
        }
        if let Some(acceptor) = self.acceptor.take() {
            let _ = acceptor.join();
        }
    }
}

fn accept(shared: Arc<Shared>, listener: TcpListener) {
    while !shared.stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_nodelay(true);
                if let Ok(copy) = stream.try_clone() {
                    let mut connections = shared.connections.lock().unwrap();
                    connections.retain(|c| c.peer_addr().is_ok());
                    connections.push(copy);
                }
                let shared = shared.clone();
                let _ = thread::Builder::new().name("spiro-conn".into()).spawn(move || {
                    let _ = serve_connection(&shared, stream);
                });
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(10)),
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn write_line(writer: &Mutex<TcpStream>, json: &Json) -> io::Result<()> {
    let mut line = json.to_string();
    line.push('\n');
    writer.lock().unwrap().write_all(line.as_bytes())
}

fn serve_connection(shared: &Arc<Shared>, stream: TcpStream) -> io::Result<()> {
    let writer = Arc::new(Mutex::new(stream.try_clone()?));
    let mut reader = BufReader::new(stream);
    let mut session = shared.db.session();
    let closed = Arc::new(AtomicBool::new(false));
    let mut pusher: Option<JoinHandle<()>> = None;
    let mut line = String::new();
    let result = loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break Ok(()),
            Ok(_) => {}
            Err(e) => break Err(e),
        }
        let request = match Json::parse(line.trim()) {
            Ok(request) => request,
            Err(e) => {
                write_line(&writer, &error_json(&e, "protocol"))?;
                continue;
            }
        };
        if let Some(replicate) = request.get("replicate") {
            drop(session);
            closed.store(true, Ordering::SeqCst);
            return serve_replica(shared, reader, &writer, replicate);
        }
        let response = handle_request(shared, &mut session, &request);
        write_line(&writer, &response)?;
        // The first LISTEN starts a thread that pushes notifications as they arrive.
        if pusher.is_none() && !session.listening().is_empty() {
            let (receiver, writer, closed) = (session.notification_receiver(), writer.clone(), closed.clone());
            pusher = Some(thread::spawn(move || {
                while !closed.load(Ordering::SeqCst) {
                    if let Some(n) = receiver.recv_timeout(Duration::from_millis(200)) {
                        let json = Json::object(vec![(
                            "notification",
                            Json::object(vec![
                                ("channel", Json::Str(n.channel)),
                                ("payload", Json::Str(n.payload)),
                                ("sender", Json::Int(n.sender as i64)),
                            ]),
                        )]);
                        if write_line(&writer, &json).is_err() {
                            break;
                        }
                    }
                }
            }));
        }
    };
    closed.store(true, Ordering::SeqCst);
    result
}

fn error_json(message: &dyn std::fmt::Display, kind: &str) -> Json {
    Json::object(vec![("error", Json::Str(message.to_string())), ("kind", Json::Str(kind.into()))])
}

fn error_kind(e: &Error) -> &'static str {
    match e {
        Error::Syntax(_) => "syntax",
        Error::Constraint(_) => "constraint",
        Error::Sql(_) => "sql",
        Error::ReadOnly(_) => "readonly",
        _ => "internal",
    }
}

pub(crate) fn value_to_json(v: &Value) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Integer(i) => Json::Int(*i),
        Value::Real(r) => Json::Float(*r),
        Value::Text(s) => Json::Str(s.clone()),
        Value::Boolean(b) => Json::Bool(*b),
        // As text, so no digit is lost to a float, as PostgreSQL drivers do.
        Value::Decimal(d) => Json::Str(d.to_string()),
        Value::Blob(_) => Json::Str(v.to_string()),
    }
}

pub(crate) fn json_to_value(j: &Json) -> Value {
    match j {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Boolean(*b),
        Json::Int(i) => Value::Integer(*i),
        Json::Float(f) => Value::Real(*f),
        Json::Str(s) => Value::Text(s.clone()),
        other => Value::Text(other.to_string()),
    }
}

fn status_json(db: &Database) -> Json {
    let status = db.replication_status();
    let mut fields = vec![
        ("role", Json::Str(if matches!(status.role, Role::Primary) { "primary" } else { "replica" }.into())),
        ("epoch", Json::Int(status.epoch as i64)),
        ("seq", Json::Int(status.seq as i64)),
        ("catching_up", Json::Bool(status.catching_up)),
    ];
    match &status.role {
        Role::Primary => fields.push((
            "replicas",
            Json::Array(
                status
                    .replicas
                    .iter()
                    .map(|&(id, acked)| {
                        Json::object(vec![("id", Json::Int(id as i64)), ("acked", Json::Int(acked as i64))])
                    })
                    .collect(),
            ),
        )),
        Role::Replica { primary } => {
            fields.push(("primary", Json::Str(primary.clone())));
            fields.push(("primary_seq", Json::Int(status.primary_seq as i64)));
        }
    }
    Json::object(fields)
}

fn handle_request(shared: &Arc<Shared>, session: &mut Session, request: &Json) -> Json {
    let db = &shared.db;
    if request.get("status").is_some() {
        return status_json(db);
    }
    if request.get("promote").is_some() {
        return match promote(shared) {
            Ok(epoch) => {
                Json::object(vec![("command", Json::Str("PROMOTE".into())), ("epoch", Json::Int(epoch as i64))])
            }
            Err(e) => error_json(&e, error_kind(&e)),
        };
    }
    if let Some(primary) = request.get("follow") {
        let Some(primary) = primary.as_str() else { return error_json(&"\"follow\" takes an address", "protocol") };
        return match follow(shared, primary) {
            Ok(()) => Json::object(vec![("command", Json::Str("FOLLOW".into()))]),
            Err(e) => error_json(&e, error_kind(&e)),
        };
    }
    let Some(sql) = request.get("sql").and_then(Json::as_str) else {
        return error_json(&"expected a request with \"sql\", \"status\", \"promote\" or \"follow\"", "protocol");
    };
    let params: Vec<Value> =
        request.get("params").and_then(Json::as_array).unwrap_or(&[]).iter().map(json_to_value).collect();
    if db.replication().is_replica() {
        let min_seq = request.get("min_seq").and_then(Json::as_u64).unwrap_or(0);
        if !db.replication().wait_until_applied(min_seq, shared.read_wait) {
            return error_json(&"this replica has not caught up yet; read from the primary", "behind");
        }
    }
    let before = session.last_commit_seq();
    let result = session.execute_with(sql, &params);
    let seq = session.last_commit_seq().filter(|&seq| Some(seq) != before);
    let mut response = match result {
        Ok(QueryResult::Rows(rows)) => Json::object(vec![
            ("columns", Json::Array(rows.columns.into_iter().map(Json::Str).collect())),
            (
                "rows",
                Json::Array(rows.rows.iter().map(|r| Json::Array(r.iter().map(value_to_json).collect())).collect()),
            ),
        ]),
        Ok(QueryResult::Changed { command, rows }) => {
            Json::object(vec![("command", Json::Str(command.into())), ("rows", Json::Int(rows as i64))])
        }
        Ok(QueryResult::Done(command)) => Json::object(vec![("command", Json::Str(command.into()))]),
        Err(e) => error_json(&e, error_kind(&e)),
    };
    if let (Some(seq), Json::Object(fields)) = (seq, &mut response) {
        fields.push(("seq".into(), Json::Int(seq as i64)));
    }
    response
}

fn promote(shared: &Shared) -> Result<u64> {
    if !shared.db.replication().is_replica() {
        return Err(Error::Sql("this server is already a primary".into()));
    }
    // Stop applying the old primary's stream before taking writes.
    stop_following(shared);
    shared.db.promote()
}

fn follow(shared: &Arc<Shared>, primary: &str) -> Result<()> {
    stop_following(shared);
    start_following(shared, primary.to_string())
}

// ------------------------------------------------------------ primary side

static NEXT_REPLICA: AtomicU64 = AtomicU64::new(1);

/// Streams commits to a replica: a snapshot first if it's too far behind,
/// then each commit as it happens, with heartbeats in between.
fn serve_replica(
    shared: &Arc<Shared>,
    mut reader: BufReader<TcpStream>,
    writer: &Mutex<TcpStream>,
    request: &Json,
) -> io::Result<()> {
    let db = &shared.db;
    let repl = db.replication();
    if repl.is_replica() {
        return write_line(writer, &error_json(&"this server is itself a replica", "sql"));
    }
    let from = request.get("from").and_then(Json::as_u64).unwrap_or(0);
    let epoch = request.get("epoch").and_then(Json::as_u64).unwrap_or(1);
    if epoch > repl.epoch() {
        // The replica has seen a promotion this server hasn't: we are a stale primary.
        let message = format!("this primary is stale: epoch {} is behind the replica's {epoch}", repl.epoch());
        return write_line(writer, &error_json(&message, "stale"));
    }
    write_line(writer, &Json::object(vec![("ok", Json::Bool(true)), ("epoch", Json::Int(repl.epoch() as i64))]))?;

    let id = NEXT_REPLICA.fetch_add(1, Ordering::Relaxed);
    let closed = Arc::new(AtomicBool::new(false));
    let acks = {
        let (db, closed) = (db.clone(), closed.clone());
        thread::spawn(move || {
            while let Ok((kind, payload)) = repl::read_frame(&mut reader) {
                if kind == repl::FRAME_ACK && payload.len() == 8 {
                    db.replication().ack(id, u64::from_le_bytes(payload.try_into().unwrap()));
                }
            }
            closed.store(true, Ordering::SeqCst);
            db.replication().forget(id);
        })
    };
    let mut out = BufWriter::new(writer.lock().unwrap().try_clone()?);
    let result = (|| -> io::Result<()> {
        let mut sent = from;
        if repl.needs_snapshot(from) || repl.diverged(epoch, from) {
            let start = repl.seq();
            repl::write_frame(&mut out, repl::FRAME_SNAPSHOT_BEGIN, &start.to_le_bytes())?;
            db.snapshot_chunks(&mut |chunk| Ok(repl::write_frame(&mut out, repl::FRAME_SNAPSHOT_CHUNK, chunk)?))
                .map_err(io::Error::other)?;
            let mut end = repl.seq().to_le_bytes().to_vec();
            end.extend_from_slice(&repl.epoch().to_le_bytes());
            end.extend_from_slice(&repl.encoded_epochs());
            repl::write_frame(&mut out, repl::FRAME_SNAPSHOT_END, &end)?;
            sent = start;
        }
        while !closed.load(Ordering::SeqCst) && !shared.stop.load(Ordering::SeqCst) {
            // A gap means commits the replica needs were dropped from the log;
            // closing makes it reconnect and start over from a snapshot.
            let Some(entries) = repl.entries_after(sent) else { return Ok(()) };
            if entries.is_empty() {
                if repl.wait_past(sent, Duration::from_millis(500)) == sent {
                    let mut beat = sent.to_le_bytes().to_vec();
                    beat.extend_from_slice(&repl.epoch().to_le_bytes());
                    repl::write_frame(&mut out, repl::FRAME_HEARTBEAT, &beat)?;
                }
                continue;
            }
            for entry in entries {
                repl::write_frame(&mut out, repl::FRAME_ENTRY, &entry.encode())?;
                sent = entry.seq;
            }
        }
        Ok(())
    })();
    let _ = out.get_ref().shutdown(std::net::Shutdown::Both);
    let _ = acks.join();
    result
}

// ------------------------------------------------------------ replica side

fn start_following(shared: &Arc<Shared>, primary: String) -> Result<()> {
    shared.db.follow(&primary)?;
    let running = Arc::new(AtomicBool::new(true));
    let thread = {
        let (shared, running) = (shared.clone(), running.clone());
        thread::Builder::new().name("spiro-follow".into()).spawn(move || {
            while running.load(Ordering::SeqCst) && !shared.stop.load(Ordering::SeqCst) {
                if follow_once(&shared, &primary, &running).is_err() && running.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(200)); // primary unreachable: retry
                }
            }
        })?
    };
    let mut follower = shared.follower.lock().unwrap();
    follower.running = Some(running);
    follower.thread = Some(thread);
    Ok(())
}

fn stop_following(shared: &Shared) {
    let mut follower = shared.follower.lock().unwrap();
    if let Some(running) = follower.running.take() {
        running.store(false, Ordering::SeqCst);
    }
    if let Some(socket) = follower.socket.take() {
        let _ = socket.shutdown(std::net::Shutdown::Both);
    }
    let thread = follower.thread.take();
    drop(follower);
    if let Some(thread) = thread {
        let _ = thread.join();
    }
}

fn read_u64(payload: &[u8], at: usize) -> io::Result<u64> {
    payload
        .get(at..at + 8)
        .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "short replication frame"))
}

/// One connection to the primary: installs a snapshot if one comes, then
/// applies commits until the connection drops or we stop following.
fn follow_once(shared: &Shared, primary: &str, running: &AtomicBool) -> io::Result<()> {
    let db = &shared.db;
    let stream = TcpStream::connect(primary)?;
    stream.set_nodelay(true)?;
    shared.follower.lock().unwrap().socket = Some(stream.try_clone()?);
    if !running.load(Ordering::SeqCst) {
        return Ok(());
    }
    let mut writer = stream.try_clone()?;
    let (from, epoch) = (db.replication().seq(), db.replication().epoch());
    let request = Json::object(vec![(
        "replicate",
        Json::object(vec![("from", Json::Int(from as i64)), ("epoch", Json::Int(epoch as i64))]),
    )]);
    writeln!(writer, "{request}")?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let reply = Json::parse(line.trim()).map_err(io::Error::other)?;
    if let Some(error) = reply.get("error") {
        return Err(io::Error::other(error.to_string()));
    }
    let to_io = |e: Error| io::Error::other(e.to_string());
    let ack = |writer: &mut TcpStream, seq: u64| repl::write_frame(writer, repl::FRAME_ACK, &seq.to_le_bytes());
    let mut snapshot_start = 0;
    while running.load(Ordering::SeqCst) {
        let (kind, payload) = repl::read_frame(&mut reader)?;
        match kind {
            repl::FRAME_SNAPSHOT_BEGIN => {
                snapshot_start = read_u64(&payload, 0)?;
                db.begin_snapshot().map_err(to_io)?;
            }
            repl::FRAME_SNAPSHOT_CHUNK => db.apply_snapshot_chunk(&payload).map_err(to_io)?,
            repl::FRAME_SNAPSHOT_END => {
                let (end, epoch) = (read_u64(&payload, 0)?, read_u64(&payload, 8)?);
                db.finish_snapshot(snapshot_start, end, epoch, &payload[16..]).map_err(to_io)?;
                ack(&mut writer, snapshot_start)?;
            }
            repl::FRAME_ENTRY => {
                let entry = Entry::decode(&payload).map_err(to_io)?;
                let expected = db.replication().seq() + 1;
                if entry.seq != expected {
                    return Err(io::Error::other(format!("expected commit {expected}, got {}", entry.seq)));
                }
                let seq = entry.seq;
                db.apply_entry(entry).map_err(to_io)?;
                ack(&mut writer, seq)?;
            }
            repl::FRAME_HEARTBEAT => {
                db.replication().set_primary_seq(read_u64(&payload, 0)?);
                ack(&mut writer, db.replication().seq())?;
            }
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "unknown replication frame")),
        }
    }
    Ok(())
}
