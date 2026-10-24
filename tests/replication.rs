use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use spirodb::Error;
use spirodb::client::{Client, Target};
use spirodb::server::{Server, ServerOptions};
use spirodb::sql::{Database, QueryResult, Value};

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> TempDir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let unique = format!("spirodb-repl-{name}-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
        let path = std::env::temp_dir().join(unique);
        let _ = fs::remove_dir_all(&path);
        TempDir(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Opens a database, retrying while a server that just shut down lets go of it.
fn open(dir: &TempDir) -> Database {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match Database::open(&dir.0) {
            Ok(db) => return db,
            Err(Error::Locked(_)) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            Err(e) => panic!("open {}: {e}", dir.0.display()),
        }
    }
}

fn serve(dir: &TempDir, replica_of: Option<&Server>) -> Server {
    let opts = ServerOptions { replica_of: replica_of.map(|p| p.addr().to_string()), ..ServerOptions::default() };
    Server::start(open(dir), "127.0.0.1:0", opts).unwrap()
}

fn addrs(servers: &[&Server]) -> Vec<String> {
    servers.iter().map(|s| s.addr().to_string()).collect()
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(10));
    }
}

fn count(db: &Database, table: &str) -> i64 {
    match db.query(&format!("SELECT COUNT(*) FROM {table}"), &[]) {
        Ok(rows) => match rows.rows[0][0] {
            Value::Integer(n) => n,
            ref other => panic!("count is {other:?}"),
        },
        Err(_) => -1, // the table hasn't arrived yet
    }
}

fn caught_up(replica: &Server, primary: &Server) -> bool {
    let status = replica.database().replication_status();
    !status.catching_up && status.seq == primary.database().replication_status().seq
}

#[test]
fn replicas_follow_the_primary_and_selects_are_routed_to_them() {
    let (dp, d1, d2) = (TempDir::new("p"), TempDir::new("r1"), TempDir::new("r2"));
    let primary = serve(&dp, None);
    let r1 = serve(&d1, Some(&primary));
    let r2 = serve(&d2, Some(&primary));
    let mut client = Client::connect(&addrs(&[&primary, &r1, &r2])).unwrap();
    assert_eq!(client.primary(), Some(primary.addr().to_string().as_str()));
    assert_eq!(client.replicas().len(), 2);

    client.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE)").unwrap();
    let primary_target = Some(Target::Primary(primary.addr().to_string()));
    assert_eq!(client.last_target(), primary_target.as_ref());

    let mut readers = HashSet::new();
    for i in 0..6 {
        let inserted =
            client.execute_with("INSERT INTO users (name) VALUES (?)", &[Value::Text(format!("u{i}"))]).unwrap();
        assert_eq!(inserted, QueryResult::Changed { command: "INSERT", rows: 1 });
        assert_eq!(client.last_target(), primary_target.as_ref());
        // Read-your-writes: the replica serving this has the row just inserted.
        let rows = client.query("SELECT name FROM users WHERE name = ?", &[Value::Text(format!("u{i}"))]).unwrap();
        assert_eq!(rows.rows, vec![vec![Value::Text(format!("u{i}"))]]);
        match client.last_target() {
            Some(Target::Replica(addr)) => readers.insert(addr.clone()),
            other => panic!("read went to {other:?}"),
        };
    }
    assert_eq!(readers.len(), 2, "reads are spread over both replicas");

    // Errors come back typed, from whichever server ran the statement.
    assert!(matches!(client.execute("INSERT INTO users (name) VALUES ('u0')"), Err(Error::Constraint(_))));
    assert!(matches!(client.execute("SELEC 1"), Err(Error::Syntax(_))));

    // A replica refuses writes, even from a session on its own database.
    assert!(matches!(r1.database().execute("INSERT INTO users (name) VALUES ('x')"), Err(Error::ReadOnly(_))));

    wait_for("replicas to catch up", || caught_up(&r1, &primary) && caught_up(&r2, &primary));
    assert_eq!(count(r2.database(), "users"), 6);
    let status = primary.database().replication_status();
    assert_eq!(status.replicas.len(), 2);
    wait_for("acknowledgements", || {
        primary.database().replication_status().replicas.iter().all(|&(_, acked)| acked == status.seq)
    });
}

#[test]
fn transactions_and_their_reads_stay_on_the_primary() {
    let (dp, d1) = (TempDir::new("p"), TempDir::new("r1"));
    let primary = serve(&dp, None);
    let r1 = serve(&d1, Some(&primary));
    let mut client = Client::connect(&addrs(&[&r1, &primary])).unwrap();
    client.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)").unwrap();
    client.execute("BEGIN").unwrap();
    assert!(client.in_transaction());
    client.execute("INSERT INTO t (v) VALUES (1)").unwrap();
    // Uncommitted, so only the primary's transaction can see it.
    assert_eq!(client.query("SELECT v FROM t", &[]).unwrap().len(), 1);
    assert!(matches!(client.last_target(), Some(Target::Primary(_))));
    client.execute("COMMIT").unwrap();
    assert!(!client.in_transaction());
    assert_eq!(client.query("SELECT v FROM t", &[]).unwrap().len(), 1);
    assert!(matches!(client.last_target(), Some(Target::Replica(_))));
}

#[test]
fn a_new_replica_starts_from_a_snapshot_then_streams() {
    let (dp, d1) = (TempDir::new("p"), TempDir::new("r1"));
    {
        // Written before the server starts, so these commits aren't in its
        // in-memory log: the replica can only get them from a snapshot.
        let db = open(&dp);
        db.execute("CREATE TABLE items (id INTEGER PRIMARY KEY, label TEXT)").unwrap();
        db.execute("CREATE INDEX items_label ON items (label)").unwrap();
        let mut session = db.session();
        session.execute("BEGIN").unwrap();
        for i in 0..20_000 {
            session
                .execute_with("INSERT INTO items (label) VALUES (?)", &[Value::Text(format!("item {i:05}"))])
                .unwrap();
        }
        session.execute("COMMIT").unwrap();
    }
    let primary = serve(&dp, None);
    let r1 = serve(&d1, Some(&primary));
    wait_for("the snapshot", || caught_up(&r1, &primary));
    assert_eq!(count(r1.database(), "items"), 20_000);
    let rows = r1.database().query("SELECT id FROM items WHERE label = 'item 12345'", &[]).unwrap();
    assert_eq!(rows.rows, vec![vec![Value::Integer(12346)]], "indexes arrive too");

    primary.database().execute("INSERT INTO items (label) VALUES ('after')").unwrap();
    wait_for("streaming", || count(r1.database(), "items") == 20_001);

    // A replica that restarts carries on from where it was.
    drop(r1);
    primary.database().execute("INSERT INTO items (label) VALUES ('while away')").unwrap();
    let r1 = serve(&d1, Some(&primary));
    wait_for("catch-up after restart", || count(r1.database(), "items") == 20_002);
}

#[test]
fn a_lagging_replica_sends_reads_to_the_primary() {
    let (dp, d1) = (TempDir::new("p"), TempDir::new("r1"));
    let primary = serve(&dp, None);
    primary.database().execute("CREATE TABLE t (id INTEGER PRIMARY KEY)").unwrap();
    let opts = ServerOptions { replica_of: Some(primary.addr().to_string()), read_wait: Duration::from_millis(50) };
    let r1 = Server::start(open(&d1), "127.0.0.1:0", opts).unwrap();
    wait_for("the replica", || caught_up(&r1, &primary));

    // A read asking for a commit the replica doesn't have yet is turned away.
    let mut stream = TcpStream::connect(r1.addr()).unwrap();
    writeln!(stream, r#"{{"sql": "SELECT * FROM t", "min_seq": 1000000}}"#).unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    assert!(line.contains(r#""kind":"behind""#), "{line}");
}

#[test]
fn synchronous_commits_wait_for_a_replica() {
    let (dp, d1) = (TempDir::new("p"), TempDir::new("r1"));
    let primary = serve(&dp, None);
    primary.database().execute("CREATE TABLE t (id INTEGER PRIMARY KEY)").unwrap();
    primary.database().set_sync_replicas(1, Duration::from_millis(200));
    // No replica confirms it: the commit happened, but the caller is told.
    let err = primary.database().execute("INSERT INTO t (id) VALUES (100)").unwrap_err();
    assert!(err.to_string().contains("replica"), "{err}");
    assert_eq!(count(primary.database(), "t"), 1);

    let r1 = serve(&d1, Some(&primary));
    wait_for("the replica to connect", || !primary.database().replication_status().replicas.is_empty());
    primary.database().set_sync_replicas(1, Duration::from_secs(10));
    primary.database().execute("INSERT INTO t (id) VALUES (200)").unwrap();
    // Synchronous: the replica has it as soon as the commit returns.
    assert!(r1.database().replication_status().seq >= primary.database().replication_status().seq);
    assert!(r1.database().query("SELECT id FROM t WHERE id = 200", &[]).unwrap().len() == 1);
}

#[test]
fn failover_promotes_a_replica_and_the_client_follows() {
    let (dp, d1, d2) = (TempDir::new("p"), TempDir::new("r1"), TempDir::new("r2"));
    let primary = serve(&dp, None);
    let r1 = serve(&d1, Some(&primary));
    let r2 = serve(&d2, Some(&primary));
    let nodes = addrs(&[&primary, &r1, &r2]);
    let mut client = Client::connect(&nodes).unwrap();
    client.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)").unwrap();
    client.execute("INSERT INTO t (v) VALUES ('before')").unwrap();
    wait_for("replicas", || caught_up(&r1, &primary) && caught_up(&r2, &primary));

    // Promote r1 and point r2 at it. The old primary is still up and takes
    // one more write that no replica will ever see.
    assert_eq!(r1.promote().unwrap(), 2);
    r2.follow(&r1.addr().to_string()).unwrap();
    primary.database().execute("INSERT INTO t (v) VALUES ('lost')").unwrap();
    drop(primary);

    // The client's write to the dead primary fails (it may or may not have
    // committed, so it is not retried); the next one finds the new primary.
    assert!(matches!(client.execute("INSERT INTO t (v) VALUES ('x')"), Err(Error::Io(_))));
    client.execute("INSERT INTO t (v) VALUES ('after')").unwrap();
    assert_eq!(client.last_target(), Some(&Target::Primary(r1.addr().to_string())));
    wait_for("r2 to follow the new primary", || caught_up(&r2, &r1));
    assert_eq!(count(r2.database(), "t"), 2);
    assert_eq!(r2.database().replication_status().epoch, 2);

    // The old primary comes back, still claiming to be primary. Clients
    // prefer the newer epoch.
    let old = serve(&dp, None);
    assert_eq!(old.database().replication_status().epoch, 1);
    let mut fresh = Client::connect(&[old.addr().to_string(), r1.addr().to_string(), r2.addr().to_string()]).unwrap();
    assert_eq!(fresh.primary(), Some(r1.addr().to_string().as_str()));

    // It rejoins as a replica. It has a commit the new primary never got, so
    // it starts over from a snapshot rather than keep a diverged history.
    old.follow(&r1.addr().to_string()).unwrap();
    wait_for("the old primary to resync", || caught_up(&old, &r1));
    let values = old.database().query("SELECT v FROM t ORDER BY id", &[]).unwrap().rows;
    assert_eq!(values, vec![vec![Value::Text("before".into())], vec![Value::Text("after".into())]]);
    fresh.discover().unwrap();
    assert_eq!(fresh.replicas().len(), 2);
}

#[test]
fn notifications_reach_listeners_through_the_client_and_on_replicas() {
    let (dp, d1) = (TempDir::new("p"), TempDir::new("r1"));
    let primary = serve(&dp, None);
    let r1 = serve(&d1, Some(&primary));
    let nodes = addrs(&[&primary, &r1]);
    let (mut listener, mut sender) = (Client::connect(&nodes).unwrap(), Client::connect(&nodes).unwrap());
    let mut on_replica = r1.database().session();
    on_replica.execute("LISTEN jobs").unwrap();

    listener.execute("LISTEN jobs").unwrap();
    sender.execute("NOTIFY jobs, 'build 7'").unwrap();
    let n = listener.wait_for_notification(Duration::from_secs(5)).unwrap().expect("a notification");
    assert_eq!((n.channel.as_str(), n.payload.as_str()), ("jobs", "build 7"));

    // Replicas deliver the notifications in the commits they apply.
    let n = on_replica.wait_for_notification(Duration::from_secs(5)).expect("a notification on the replica");
    assert_eq!(n.payload, "build 7");
}
