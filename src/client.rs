//! A client for a replicated SpiroDB cluster that routes every statement for
//! you: give it the addresses of all the servers and use it like one database.
//!
//! - `SELECT` (and `EXPLAIN`) go to the replicas, round-robin, spreading the
//!   read load. Each read carries the sequence number of this client's latest
//!   write, so it always sees its own writes: a replica that hasn't applied
//!   that write yet says so, and the read goes to the primary instead.
//! - Writes, transactions (everything from `BEGIN` to `COMMIT`) and `LISTEN`
//!   go to the primary.
//! - The primary is found by asking every server; after a promotion, the one
//!   with the highest epoch wins. When the primary stops answering, the client
//!   looks again. Reads are retried; a write whose connection broke is not,
//!   since it may or may not have committed, so its error is returned.
//!
//! ```no_run
//! use spirodb::client::Client;
//! use spirodb::sql::Value;
//!
//! let mut db = Client::connect(&["10.0.0.1:7878", "10.0.0.2:7878", "10.0.0.3:7878"])?;
//! db.execute("INSERT INTO users (name) VALUES ('Ada')")?; // primary
//! let rows = db.query("SELECT * FROM users WHERE name = ?", &[Value::Text("Ada".into())])?; // a replica
//! assert_eq!(rows.len(), 1); // read-your-writes, even on a replica
//! # Ok::<(), spirodb::Error>(())
//! ```

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::json::Json;
use crate::server::{json_to_value, value_to_json};
use crate::sql::{self, Notification, QueryResult, Rows};

/// Where the client sent its last statement, for tests and diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Primary(String),
    Replica(String),
}

pub struct Client {
    nodes: Vec<String>,
    primary: Option<Conn>,
    replicas: Vec<Conn>,
    next_replica: usize,
    /// The sequence number of this client's latest commit.
    last_seq: u64,
    in_transaction: bool,
    notifications: VecDeque<Notification>,
    last_target: Option<Target>,
    timeout: Duration,
}

/// One connection to one server.
struct Conn {
    addr: String,
    writer: TcpStream,
    reader: BufReader<TcpStream>,
    /// A line read partway when a read timed out.
    pending: Vec<u8>,
}

impl Conn {
    fn open(addr: &str, timeout: Duration) -> io::Result<Conn> {
        let mut last = io::Error::new(io::ErrorKind::NotFound, format!("{addr} did not resolve"));
        for resolved in addr.to_socket_addrs()? {
            match TcpStream::connect_timeout(&resolved, timeout) {
                Ok(stream) => {
                    stream.set_nodelay(true)?;
                    let reader = BufReader::new(stream.try_clone()?);
                    return Ok(Conn { addr: addr.to_string(), writer: stream, reader, pending: Vec::new() });
                }
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// Reads one JSON line; `Ok(None)` if `timeout` passes first.
    fn read_message(&mut self, timeout: Option<Duration>) -> io::Result<Option<Json>> {
        self.reader.get_ref().set_read_timeout(timeout)?;
        let result = self.reader.read_until(b'\n', &mut self.pending);
        self.reader.get_ref().set_read_timeout(None)?;
        match result {
            Ok(0) => Err(io::Error::new(io::ErrorKind::UnexpectedEof, format!("{} closed the connection", self.addr))),
            Ok(_) if self.pending.ends_with(b"\n") => {
                let line = std::mem::take(&mut self.pending);
                let text = std::str::from_utf8(&line).map_err(io::Error::other)?;
                Json::parse(text.trim()).map(Some).map_err(io::Error::other)
            }
            Ok(_) => Err(io::Error::new(io::ErrorKind::UnexpectedEof, format!("{} closed the connection", self.addr))),
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Sends a request and returns its response, setting aside any
    /// notifications that arrive first.
    fn request(&mut self, request: &Json, notifications: &mut VecDeque<Notification>) -> io::Result<Json> {
        writeln!(self.writer, "{request}")?;
        loop {
            let message = self.read_message(None)?.expect("reads without a timeout");
            match notification(&message) {
                Some(n) => notifications.push_back(n),
                None => return Ok(message),
            }
        }
    }
}

fn notification(message: &Json) -> Option<Notification> {
    let n = message.get("notification")?;
    Some(Notification {
        channel: n.get("channel")?.as_str()?.to_string(),
        payload: n.get("payload")?.as_str()?.to_string(),
        sender: n.get("sender").and_then(Json::as_u64).unwrap_or(0),
    })
}

fn kind(response: &Json) -> Option<&str> {
    response.get("error")?;
    Some(response.get("kind").and_then(Json::as_str).unwrap_or("internal"))
}

fn no_primary() -> Error {
    Error::Io(io::Error::new(io::ErrorKind::NotConnected, "no primary is reachable"))
}

impl Client {
    /// Connects to a cluster. `nodes` lists every server, primary and
    /// replicas, in any order; the client works out which is which.
    pub fn connect<S: AsRef<str>>(nodes: &[S]) -> Result<Client> {
        let mut client = Client {
            nodes: nodes.iter().map(|n| n.as_ref().to_string()).collect(),
            primary: None,
            replicas: Vec::new(),
            next_replica: 0,
            last_seq: 0,
            in_transaction: false,
            notifications: VecDeque::new(),
            last_target: None,
            timeout: Duration::from_secs(2),
        };
        client.discover()?;
        Ok(client)
    }

    /// Asks every server for its role. Of the primaries, the one with the
    /// highest epoch (the latest promotion) is used; an older one that came
    /// back after a failover is ignored.
    pub fn discover(&mut self) -> Result<()> {
        self.primary = None;
        self.replicas.clear();
        self.in_transaction = false;
        let mut best: Option<(u64, Conn)> = None;
        let status = Json::object(vec![("status", Json::Bool(true))]);
        for addr in &self.nodes {
            let Ok(mut conn) = Conn::open(addr, self.timeout) else { continue };
            let Ok(reply) = conn.request(&status, &mut self.notifications) else { continue };
            let epoch = reply.get("epoch").and_then(Json::as_u64).unwrap_or(0);
            match reply.get("role").and_then(Json::as_str) {
                Some("primary") if best.as_ref().is_none_or(|(best, _)| epoch > *best) => best = Some((epoch, conn)),
                Some("replica") => self.replicas.push(conn),
                _ => {}
            }
        }
        self.primary = best.map(|(_, conn)| conn);
        if self.primary.is_none() && self.replicas.is_empty() {
            return Err(Error::Io(io::Error::new(io::ErrorKind::NotConnected, "no server is reachable")));
        }
        Ok(())
    }

    /// The primary's address, if one is reachable.
    pub fn primary(&self) -> Option<&str> {
        self.primary.as_ref().map(|c| c.addr.as_str())
    }

    pub fn replicas(&self) -> Vec<&str> {
        self.replicas.iter().map(|c| c.addr.as_str()).collect()
    }

    /// Which server ran the last statement.
    pub fn last_target(&self) -> Option<&Target> {
        self.last_target.as_ref()
    }

    /// The sequence number of this client's latest commit; reads on replicas
    /// wait for it.
    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    pub fn in_transaction(&self) -> bool {
        self.in_transaction
    }

    pub fn execute(&mut self, sql: &str) -> Result<QueryResult> {
        self.execute_with(sql, &[])
    }

    pub fn query(&mut self, sql: &str, params: &[sql::Value]) -> Result<Rows> {
        match self.execute_with(sql, params)? {
            QueryResult::Rows(rows) => Ok(rows),
            _ => Err(Error::InvalidArgument("query: the statement returned no rows".into())),
        }
    }

    pub fn execute_with(&mut self, sql: &str, params: &[sql::Value]) -> Result<QueryResult> {
        let route = sql::route(sql);
        let mut fields = vec![("sql", Json::Str(sql.to_string()))];
        if !params.is_empty() {
            fields.push(("params", Json::Array(params.iter().map(value_to_json).collect())));
        }
        if route.read_only && !self.in_transaction {
            fields.push(("min_seq", Json::Int(self.last_seq as i64)));
            let request = Json::object(fields);
            if let Some(response) = self.read_from_replica(&request) {
                return to_result(response);
            }
            return to_result(self.on_primary(&request, true)?);
        }
        let response = self.on_primary(&Json::object(fields), false)?;
        if kind(&response).is_none() {
            if let Some(seq) = response.get("seq").and_then(Json::as_u64) {
                self.last_seq = self.last_seq.max(seq);
            }
            if let Some(open) = route.in_transaction {
                self.in_transaction = open;
            }
        }
        to_result(response)
    }

    /// Tries the replicas in turn. `None` means none could answer (all down,
    /// or none caught up to this client's writes), so ask the primary.
    fn read_from_replica(&mut self, request: &Json) -> Option<Json> {
        let mut tries = self.replicas.len();
        while tries > 0 && !self.replicas.is_empty() {
            tries -= 1;
            let i = self.next_replica % self.replicas.len();
            self.next_replica = self.next_replica.wrapping_add(1);
            match self.replicas[i].request(request, &mut self.notifications) {
                Ok(response) if kind(&response) == Some("behind") => {}
                Ok(response) => {
                    self.last_target = Some(Target::Replica(self.replicas[i].addr.clone()));
                    return Some(response);
                }
                Err(_) => {
                    self.replicas.remove(i);
                }
            }
        }
        None
    }

    fn on_primary(&mut self, request: &Json, retry_on_failure: bool) -> Result<Json> {
        for attempt in 0..2 {
            if self.primary.is_none() && !self.in_transaction {
                let _ = self.discover();
            }
            let Some(primary) = self.primary.as_mut() else { return Err(no_primary()) };
            match primary.request(request, &mut self.notifications) {
                // Our primary was demoted (or we were misled): nothing ran, so
                // find the real primary and send it there.
                Ok(response) if kind(&response) == Some("readonly") && attempt == 0 && !self.in_transaction => {
                    self.primary = None;
                }
                Ok(response) => {
                    self.last_target = Some(Target::Primary(primary.addr.clone()));
                    return Ok(response);
                }
                Err(e) => {
                    let was_in_transaction = self.in_transaction;
                    self.primary = None;
                    self.in_transaction = false;
                    if !retry_on_failure || was_in_transaction || attempt == 1 {
                        return Err(Error::Io(e));
                    }
                }
            }
        }
        Err(no_primary())
    }

    /// Notifications received so far for channels this client LISTENs on.
    pub fn notifications(&mut self) -> Vec<Notification> {
        self.notifications.drain(..).collect()
    }

    /// Waits up to `timeout` for a notification.
    pub fn wait_for_notification(&mut self, timeout: Duration) -> Result<Option<Notification>> {
        if let Some(n) = self.notifications.pop_front() {
            return Ok(Some(n));
        }
        let Some(primary) = self.primary.as_mut() else { return Err(no_primary()) };
        match primary.read_message(Some(timeout)) {
            Ok(message) => Ok(message.as_ref().and_then(notification)),
            Err(e) => {
                self.primary = None;
                Err(Error::Io(e))
            }
        }
    }
}

const COMMANDS: &[&str] = &[
    "INSERT",
    "UPDATE",
    "DELETE",
    "CREATE TABLE",
    "DROP TABLE",
    "CREATE INDEX",
    "DROP INDEX",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "LISTEN",
    "UNLISTEN",
    "NOTIFY",
    "PROMOTE",
];

fn command(name: Option<&str>) -> &'static str {
    COMMANDS.iter().find(|&&c| Some(c) == name).copied().unwrap_or("OK")
}

fn to_result(response: Json) -> Result<QueryResult> {
    if let Some(kind) = kind(&response) {
        let message = response.get("error").and_then(Json::as_str).unwrap_or("unknown error").to_string();
        return Err(match kind {
            "syntax" => Error::Syntax(message),
            "constraint" => Error::Constraint(message),
            "readonly" => Error::ReadOnly(message),
            _ => Error::Sql(message),
        });
    }
    if let Some(columns) = response.get("columns").and_then(Json::as_array) {
        let columns = columns.iter().map(|c| c.as_str().unwrap_or_default().to_string()).collect();
        let rows = response.get("rows").and_then(Json::as_array).unwrap_or(&[]);
        let rows = rows.iter().map(|r| r.as_array().unwrap_or(&[]).iter().map(json_to_value).collect()).collect();
        return Ok(QueryResult::Rows(Rows { columns, rows }));
    }
    let name = command(response.get("command").and_then(Json::as_str));
    Ok(match response.get("rows").and_then(Json::as_u64) {
        Some(rows) => QueryResult::Changed { command: name, rows },
        None => QueryResult::Done(name),
    })
}
