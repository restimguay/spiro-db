//! spiro: a SQL shell for SpiroDB.
//!
//!     spiro <path>                 interactive shell
//!     spiro <path> "<sql>"         run statements and exit
//!     spiro <path> --json "<sql>"  same, printing each result as JSON

use std::env;
use std::io::{self, BufRead, IsTerminal, Write};
use std::process::ExitCode;
use std::time::Instant;

use spirodb::Stats;
use spirodb::sql::{Database, Session, Value};

const HELP: &str = "\
Enter SQL statements ending with ';' (they may span several lines).

  .tables            list tables
  .schema [table]    show CREATE statements
  .mode table|json   print results as tables or as JSON
  .timer on|off      show how long each statement takes
  .stats             storage engine statistics
  .flush             write buffered data to disk now
  .help              show this message
  .exit              leave the shell";

const USAGE: &str = "usage: spiro <path> [--json] [\"<sql>\"]";

struct Shell {
    db: Database,
    session: Session,
    json: bool,
    timer: bool,
}

fn main() -> ExitCode {
    let mut args: Vec<String> = env::args().skip(1).collect();
    let json = args.iter().position(|a| a == "--json").map(|i| args.remove(i)).is_some();
    let Some((path, sql)) = args.split_first() else {
        eprintln!("{USAGE}\n\n{HELP}");
        return ExitCode::FAILURE;
    };
    let db = match Database::open(path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("spiro: cannot open {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut shell = Shell { session: db.session(), db, json, timer: false };

    if !sql.is_empty() {
        return if shell.run_sql(&sql.join(" ")) { ExitCode::SUCCESS } else { ExitCode::FAILURE };
    }

    // Prompts are for people; stay quiet when input is piped from a script.
    let interactive = io::stdin().is_terminal();
    if interactive {
        println!("SpiroDB {} at {path}. Type .help for help.", env!("CARGO_PKG_VERSION"));
    }
    let mut buffer = String::new();
    let mut line = String::new();
    loop {
        let prompt = match (buffer.is_empty(), shell.session.in_transaction()) {
            (false, _) => "   ...> ",
            (true, true) => "spiro*> ",
            (true, false) => "spiro> ",
        };
        if interactive {
            print!("{prompt}");
            let _ = io::stdout().flush();
        }
        line.clear();
        match io::stdin().lock().read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("error: {e}");
                break;
            }
        }
        if buffer.is_empty() && line.trim_start().starts_with('.') {
            if !shell.command(line.trim()) {
                break;
            }
            continue;
        }
        buffer.push_str(&line);
        if statement_complete(&buffer) {
            shell.run_sql(&buffer);
            buffer.clear();
        }
    }
    let mut ok = true;
    if !buffer.trim().is_empty() {
        ok = shell.run_sql(&buffer);
    }
    if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

impl Shell {
    /// Runs each statement in `sql`, printing results as they come. Returns
    /// whether every statement succeeded.
    fn run_sql(&mut self, sql: &str) -> bool {
        let statements = match self.db.prepare_all(sql) {
            Ok(statements) => statements,
            Err(e) => return self.report(&e),
        };
        for statement in &statements {
            let start = Instant::now();
            match self.session.execute_prepared(statement, &[]) {
                Ok(result) if self.json => println!("{}", result.to_json()),
                Ok(result) => println!("{result}"),
                Err(e) => return self.report(&e),
            }
            if self.timer {
                println!("time: {:.3} ms", start.elapsed().as_secs_f64() * 1e3);
            }
            self.print_notifications();
        }
        true
    }

    /// Shows notifications received on LISTENed channels, as psql does.
    fn print_notifications(&mut self) {
        for n in self.session.notifications() {
            if self.json {
                println!(
                    "{{\"notification\":{{\"channel\":{},\"payload\":{},\"sender\":{}}}}}",
                    Value::Text(n.channel).to_json(),
                    Value::Text(n.payload).to_json(),
                    n.sender
                );
            } else if n.payload.is_empty() {
                println!("Asynchronous notification \"{}\" received from session {}.", n.channel, n.sender);
            } else {
                println!(
                    "Asynchronous notification \"{}\" with payload \"{}\" received from session {}.",
                    n.channel, n.payload, n.sender
                );
            }
        }
    }

    /// Reports a failed statement. In JSON mode the error goes to stdout as
    /// `{"error": "..."}`, so a program reading one JSON line per statement
    /// stays in step. Always returns `false`.
    fn report(&self, e: &spirodb::Error) -> bool {
        if self.json {
            println!("{{\"error\":{}}}", Value::Text(e.to_string()).to_json());
        } else {
            eprintln!("error: {e}");
        }
        false
    }

    /// Runs a dot command. Returns `false` when the shell should exit.
    fn command(&mut self, line: &str) -> bool {
        let words: Vec<&str> = line.split_whitespace().collect();
        match words[..] {
            [".exit" | ".quit"] => return false,
            [".help"] => println!("{HELP}"),
            [".mode", "json"] => self.json = true,
            [".mode", "table"] => self.json = false,
            [".timer", "on"] => self.timer = true,
            [".timer", "off"] => self.timer = false,
            [".stats"] => print_stats(&self.db.kv().stats()),
            _ => self.fallible_command(line, &words),
        }
        true
    }

    fn fallible_command(&mut self, line: &str, words: &[&str]) {
        let result = match words {
            [".tables"] => self.db.table_names().map(|names| names.iter().for_each(|n| println!("{n}"))),
            [".schema"] => self.db.table_names().and_then(|names| {
                for name in names {
                    println!("{}", self.db.schema(&name)?.unwrap_or_default());
                }
                Ok(())
            }),
            [".schema", table] => self.db.schema(table).map(|s| match s {
                Some(s) => println!("{s}"),
                None => println!("no such table: {table}"),
            }),
            [".flush"] => self.db.kv().flush().map(|()| println!("OK")),
            _ => {
                println!("unknown command {line}; type .help");
                Ok(())
            }
        };
        if let Err(e) = result {
            eprintln!("error: {e}");
        }
    }
}

/// Whether `sql` ends with a `;` outside quotes and comments.
fn statement_complete(sql: &str) -> bool {
    let (mut quote, mut line_comment, mut block_comment) = (None, false, false);
    let mut last = None;
    let chars: Vec<char> = sql.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let (c, next) = (chars[i], chars.get(i + 1).copied());
        if line_comment {
            line_comment = c != '\n';
        } else if block_comment {
            if c == '*' && next == Some('/') {
                block_comment = false;
                i += 1;
            }
        } else if let Some(q) = quote {
            if c == q {
                quote = None; // a doubled quote just reopens on the next character
            }
            last = Some(c);
        } else if c == '-' && next == Some('-') {
            line_comment = true;
        } else if c == '/' && next == Some('*') {
            block_comment = true;
            i += 1;
        } else if c == '\'' || c == '"' {
            quote = Some(c);
            last = Some(c);
        } else if !c.is_whitespace() {
            last = Some(c);
        }
        i += 1;
    }
    quote.is_none() && !block_comment && last == Some(';')
}

fn print_stats(s: &Stats) {
    println!("memtable        {}", human_bytes(s.memtable_bytes as u64));
    println!("level-0 tables  {}", s.l0_tables);
    println!("level-1 tables  {}", s.l1_tables);
    println!("on disk         {}", human_bytes(s.table_bytes));
    println!("flushes         {} ({} written)", s.flushes, human_bytes(s.bytes_flushed));
    println!("compactions     {} ({} written)", s.compactions, human_bytes(s.bytes_compacted));
    println!("write stalls    {}", s.write_stalls);
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = n as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{n} B") } else { format!("{size:.1} {}", UNITS[unit]) }
}

#[cfg(test)]
mod tests {
    use super::statement_complete;

    #[test]
    fn detects_complete_statements() {
        assert!(statement_complete("SELECT 1;"));
        assert!(statement_complete("SELECT 1; -- done\n"));
        assert!(!statement_complete("SELECT 1"));
        assert!(!statement_complete("SELECT ';"));
        assert!(statement_complete("SELECT 'it''s';"));
        assert!(!statement_complete("SELECT 1 /* ; */"));
        assert!(!statement_complete("SELECT 1 -- ;"));
    }
}
