//! spiro-server: serves a SpiroDB database over TCP, as a primary or as a
//! read-only replica that follows one.
//!
//!     spiro-server <path> [--listen ADDR] [--replica-of ADDR] [--sync-replicas N]
//!     spiro-server --status ADDR      show a server's replication status
//!     spiro-server --promote ADDR     turn a replica into the primary
//!     spiro-server --follow ADDR PRIMARY   point a server at a new primary

use std::env;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::process::ExitCode;
use std::time::Duration;

use spirodb::server::{Server, ServerOptions};
use spirodb::sql::Database;

const USAGE: &str = "\
usage: spiro-server <path> [--listen ADDR] [--replica-of ADDR] [--sync-replicas N]
       spiro-server --status ADDR
       spiro-server --promote ADDR
       spiro-server --follow ADDR PRIMARY

  --listen ADDR        where to accept connections (default 127.0.0.1:7878)
  --replica-of ADDR    follow the primary at ADDR as a read-only replica
  --sync-replicas N    make each commit wait until N replicas have it (default 0)
  --status ADDR        print the replication status of the server at ADDR
  --promote ADDR       promote the replica at ADDR to primary (stop the old
                       primary first, then point the other replicas at the new one)
  --follow ADDR PRIMARY  make the server at ADDR a replica of PRIMARY (a replica
                       after a failover, or an old primary rejoining)";

/// Sends one request to a running server and prints the reply.
fn admin(addr: &str, request: &str) -> ExitCode {
    let reply = TcpStream::connect(addr).and_then(|mut stream| {
        writeln!(stream, "{request}")?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line)?;
        Ok(line)
    });
    match reply {
        Ok(line) => {
            print!("{line}");
            if line.contains("\"error\"") { ExitCode::FAILURE } else { ExitCode::SUCCESS }
        }
        Err(e) => {
            eprintln!("spiro-server: cannot reach {addr}: {e}");
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut path = None;
    let (mut listen, mut replica_of, mut sync_replicas) = ("127.0.0.1:7878".to_string(), None, 0);
    let mut i = 0;
    while i < args.len() {
        let value = args.get(i + 1).cloned();
        match (args[i].as_str(), value) {
            ("--status", Some(addr)) => return admin(&addr, r#"{"status":true}"#),
            ("--promote", Some(addr)) => return admin(&addr, r#"{"promote":true}"#),
            ("--follow", Some(addr)) if args.len() > i + 2 => {
                return admin(&addr, &format!("{{\"follow\":{:?}}}", args[i + 2]));
            }
            ("--listen", Some(addr)) => listen = addr,
            ("--replica-of", Some(addr)) => replica_of = Some(addr),
            ("--sync-replicas", Some(n)) if n.parse::<usize>().is_ok() => sync_replicas = n.parse().unwrap(),
            (arg, _) if !arg.starts_with("--") && path.is_none() => {
                path = Some(arg.to_string());
                i += 1;
                continue;
            }
            _ => {
                eprintln!("{USAGE}");
                return ExitCode::FAILURE;
            }
        }
        i += 2;
    }
    let Some(path) = path else {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    };
    let db = match Database::open(&path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("spiro-server: cannot open {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    db.set_sync_replicas(sync_replicas, Duration::from_secs(10));
    let role = match &replica_of {
        Some(primary) => format!("replica of {primary}"),
        None => "primary".to_string(),
    };
    let opts = ServerOptions { replica_of, ..ServerOptions::default() };
    let server = match Server::start(db, &listen, opts) {
        Ok(server) => server,
        Err(e) => {
            eprintln!("spiro-server: cannot listen on {listen}: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!("SpiroDB {} serving {path} on {} as {role}", env!("CARGO_PKG_VERSION"), server.addr());
    loop {
        std::thread::park();
    }
}
