//! SQL on top of the SpiroDB storage engine.
//!
//! Tables, rows and indexes are stored as ordinary keys in the key-value
//! store, using an order-preserving encoding, so primary keys and indexes are
//! simply ranges of sorted keys.
//!
//! ```no_run
//! use spirodb::sql::{Database, Value};
//!
//! let db = Database::open("./my-data")?;
//! db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, age INTEGER)")?;
//! db.execute("INSERT INTO users (name, age) VALUES ('Ada', 36), ('Alan', 41)")?;
//!
//! let rows = db.query("SELECT name FROM users WHERE age > ? ORDER BY name", &[Value::Integer(40)])?;
//! assert_eq!(rows.rows, vec![vec![Value::Text("Alan".into())]]);
//! println!("{}", rows.to_json()); // [{"name":"Alan"}]
//! # Ok::<(), spirodb::Error>(())
//! ```
//!
//! Statements run in autocommit mode unless a session opens a transaction
//! with `BEGIN`. Every statement is atomic: if it fails partway, none of its
//! changes remain. Transactions are serializable: `BEGIN` takes the write
//! lock (like SQLite's `BEGIN IMMEDIATE`), so writers queue, while reads
//! outside a transaction see the latest committed data.

mod ast;
mod catalog;
mod columnar;
mod datetime;
mod encoding;
mod exec;
mod expr;
mod integrity;
mod lexer;
mod notify;
mod parser;
mod plan;
pub(crate) mod repl;
mod txn;
mod value;
mod vector;
mod window;

use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::db::Db;
use crate::error::{Error, Result};
use crate::options::Options;

use ast::Statement;
use notify::{Inbox, Notifier};
pub use notify::{Notification, NotificationReceiver};
use repl::{EPOCH_KEY, EPOCHS_KEY, Entry, Replication, SEQ_KEY, SNAPSHOT_END_KEY, encode_u64};
pub use repl::{ReplicationStatus, Role};
use txn::{Committed, Txn, WriterLock};
pub use value::{DataType, Value};

/// The outcome of a statement.
#[derive(Clone, Debug, PartialEq)]
pub enum QueryResult {
    /// Rows returned by a SELECT (or EXPLAIN).
    Rows(Rows),
    /// INSERT, UPDATE or DELETE, with the number of rows affected.
    Changed { command: &'static str, rows: u64 },
    /// A statement with nothing to report, such as CREATE TABLE or COMMIT.
    Done(&'static str),
}

impl QueryResult {
    pub fn rows(&self) -> Option<&Rows> {
        match self {
            QueryResult::Rows(rows) => Some(rows),
            _ => None,
        }
    }

    pub fn rows_affected(&self) -> u64 {
        match self {
            QueryResult::Changed { rows, .. } => *rows,
            _ => 0,
        }
    }

    /// The result as JSON: an array of row objects for queries, otherwise an
    /// object describing the command, such as `{"command":"INSERT","rows":2}`.
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        match self {
            QueryResult::Rows(rows) => return rows.to_json(),
            QueryResult::Changed { command, rows } => {
                out.push_str("{\"command\":");
                value::write_json_string(&mut out, command);
                out.push_str(&format!(",\"rows\":{rows}}}"));
            }
            QueryResult::Done(command) => {
                out.push_str("{\"command\":");
                value::write_json_string(&mut out, command);
                out.push('}');
            }
        }
        out
    }
}

/// The rows a query returned.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Rows {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

impl Rows {
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The rows as a JSON array of objects keyed by column name, ready for
    /// `JSON.parse`: `[{"id":1,"name":"Ada"}, ...]`.
    pub fn to_json(&self) -> String {
        let mut out = String::from("[");
        for (i, row) in self.rows.iter().enumerate() {
            out.push_str(if i == 0 { "{" } else { ",{" });
            for (j, (column, value)) in self.columns.iter().zip(row).enumerate() {
                if j > 0 {
                    out.push(',');
                }
                value::write_json_string(&mut out, column);
                out.push(':');
                value.write_json(&mut out);
            }
            out.push('}');
        }
        out.push(']');
        out
    }
}

/// Renders rows as a table, as `psql` does.
impl fmt::Display for Rows {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cells: Vec<Vec<String>> = self.rows.iter().map(|r| r.iter().map(ToString::to_string).collect()).collect();
        let mut widths: Vec<usize> = self.columns.iter().map(|c| c.chars().count()).collect();
        for row in &cells {
            for (w, cell) in widths.iter_mut().zip(row) {
                *w = (*w).max(cell.chars().count());
            }
        }
        let header: Vec<String> = self.columns.iter().zip(&widths).map(|(c, w)| format!(" {c:<w$} ")).collect();
        writeln!(f, "{}", header.join("|"))?;
        let rule: Vec<String> = widths.iter().map(|w| "-".repeat(w + 2)).collect();
        writeln!(f, "{}", rule.join("+"))?;
        for (row, values) in cells.iter().zip(&self.rows) {
            let line: Vec<String> = row
                .iter()
                .zip(values)
                .zip(&widths)
                .map(|((cell, value), w)| match value {
                    Value::Integer(_) | Value::Real(_) => format!(" {cell:>w$} "),
                    _ => format!(" {cell:<w$} "),
                })
                .collect();
            writeln!(f, "{}", line.join("|"))?;
        }
        write!(f, "({} row{})", self.rows.len(), if self.rows.len() == 1 { "" } else { "s" })
    }
}

impl fmt::Display for QueryResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueryResult::Rows(rows) => rows.fmt(f),
            QueryResult::Changed { command, rows } => write!(f, "{command} {rows}"),
            QueryResult::Done(command) => f.write_str(command),
        }
    }
}

/// A parsed statement, ready to run many times with different parameters.
/// A query also keeps its plan, so repeated runs skip planning.
#[derive(Clone, Debug)]
pub struct Prepared {
    statement: Statement,
    params: usize,
    plans: Arc<exec::PlanCache>,
}

impl Prepared {
    fn new(statement: Statement, params: usize) -> Self {
        Prepared { statement, params, plans: Arc::default() }
    }
}

impl Prepared {
    /// How many parameters (`?` or `$N`) the statement takes.
    pub fn param_count(&self) -> usize {
        self.params
    }
}

struct Shared {
    kv: Arc<Db>,
    writer: Arc<WriterLock>,
    schemas: Arc<catalog::SchemaCache>,
    notifier: Notifier,
    repl: Arc<Replication>,
    busy_timeout: Mutex<Duration>,
    /// Most threads one query may use; 0 means every core.
    scan_threads: AtomicUsize,
}

/// A SQL database. Cheap to clone; clones share the same storage.
#[derive(Clone)]
pub struct Database {
    shared: Arc<Shared>,
}

impl Database {
    /// Opens (or creates) the database at `path` with default options.
    pub fn open(path: impl AsRef<Path>) -> Result<Database> {
        Self::open_with(path, Options::default())
    }

    pub fn open_with(path: impl AsRef<Path>, opts: Options) -> Result<Database> {
        let column_sections = opts.column_sections;
        let kv = Arc::new(Db::open_with(path, opts)?);
        if column_sections {
            // Table files also keep each table's columns, for scans to read.
            kv.set_section_builder(
                Arc::new(|| Box::new(columnar::SectionWriter::default())),
                // A failure here resurfaces when a query reads the section.
                Arc::new(|table| drop(table.columnar(columnar::Section::parse))),
            );
        }
        let repl = Arc::new(Replication::load(&kv)?);
        Ok(Database {
            shared: Arc::new(Shared {
                kv,
                writer: Arc::new(WriterLock::default()),
                schemas: Arc::default(),
                notifier: Notifier::default(),
                repl,
                busy_timeout: Mutex::new(Duration::from_secs(5)),
                scan_threads: AtomicUsize::new(0),
            }),
        })
    }

    /// The underlying key-value store, for statistics and flushing.
    pub fn kv(&self) -> &Db {
        &self.shared.kv
    }

    /// How long a writer waits for another transaction to finish before
    /// giving up with "database is locked". Defaults to 5 seconds.
    pub fn set_busy_timeout(&self, timeout: Duration) {
        *self.shared.busy_timeout.lock().unwrap() = timeout;
    }

    /// The most threads one query may use. Full scans that aggregate are
    /// split across threads; 0, the default, uses every core, and 1 keeps
    /// each query on a single core.
    pub fn set_scan_threads(&self, threads: usize) {
        self.shared.scan_threads.store(threads, Ordering::Relaxed);
    }

    /// A session, which can hold a transaction open across statements.
    pub fn session(&self) -> Session {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        Session { db: self.clone(), txn: None, id, inbox: None, pending_listens: Vec::new(), last_seq: None }
    }

    /// Runs one statement in its own transaction.
    pub fn execute(&self, sql: &str) -> Result<QueryResult> {
        self.session().execute(sql)
    }

    /// Runs one statement with parameters bound to its `?` or `$N` placeholders.
    pub fn execute_with(&self, sql: &str, params: &[Value]) -> Result<QueryResult> {
        self.session().execute_with(sql, params)
    }

    /// Runs a query and returns its rows.
    pub fn query(&self, sql: &str, params: &[Value]) -> Result<Rows> {
        self.session().query(sql, params)
    }

    pub fn prepare(&self, sql: &str) -> Result<Prepared> {
        let (statement, params) = parser::parse_statement(sql)?;
        Ok(Prepared::new(statement, params))
    }

    /// Parses every `;`-separated statement in `sql`.
    pub fn prepare_all(&self, sql: &str) -> Result<Vec<Prepared>> {
        let statements = parser::parse_statements(sql)?;
        Ok(statements.into_iter().map(|(statement, params)| Prepared::new(statement, params)).collect())
    }

    pub fn table_names(&self) -> Result<Vec<String>> {
        let txn = self.read_txn();
        Ok(catalog::list_tables(&txn)?.into_iter().map(|t| t.name).collect())
    }

    pub fn view_names(&self) -> Result<Vec<String>> {
        let txn = self.read_txn();
        Ok(catalog::list_views(&txn)?.into_iter().map(|v| v.name).collect())
    }

    /// The CREATE statements for a table and its indexes.
    pub fn schema(&self, table: &str) -> Result<Option<String>> {
        let txn = self.read_txn();
        let Some(schema) = catalog::get_table(&txn, table)? else {
            return Ok(catalog::get_view(&txn, table)?.map(|view| view.to_sql() + ";"));
        };
        let mut statements = vec![schema.to_sql()];
        statements.extend(catalog::indexes_of(&txn, table)?.iter().map(|i| i.to_sql(&schema)));
        Ok(Some(statements.join(";\n") + ";"))
    }

    fn read_txn(&self) -> Txn {
        let shared = &self.shared;
        Txn::new(shared.kv.clone(), shared.schemas.clone(), shared.repl.clone(), None)
            .with_scan_threads(shared.scan_threads.load(Ordering::Relaxed))
    }

    fn write_txn(&self) -> Result<Txn> {
        let timeout = *self.shared.busy_timeout.lock().unwrap();
        let guard = self.shared.writer.acquire(timeout)?;
        let shared = &self.shared;
        Ok(Txn::new(shared.kv.clone(), shared.schemas.clone(), shared.repl.clone(), Some(guard))
            .with_scan_threads(shared.scan_threads.load(Ordering::Relaxed)))
    }

    // ------------------------------------------------------------ replication

    /// Where this database stands in a replicated setup.
    pub fn replication_status(&self) -> ReplicationStatus {
        self.shared.repl.status()
    }

    /// Makes every commit wait until `replicas` replicas have confirmed it,
    /// for up to `timeout`. Zero (the default) means commits don't wait.
    pub fn set_sync_replicas(&self, replicas: usize, timeout: Duration) {
        self.shared.repl.set_sync(replicas, timeout);
    }

    pub(crate) fn replication(&self) -> &Replication {
        &self.shared.repl
    }

    /// Turns this database into a read-only replica of `primary`. The server
    /// streams the primary's commits into it.
    /// A primary waits for any open write transaction to finish first.
    pub(crate) fn follow(&self, primary: &str) -> Result<()> {
        let guard = self.shared.writer.acquire(*self.shared.busy_timeout.lock().unwrap())?;
        self.shared.repl.set_role(Role::Replica { primary: primary.to_string() });
        drop(guard);
        Ok(())
    }

    /// Promotes a replica to primary: it stops following and accepts writes.
    /// The promotion is recorded with a new epoch, so clients prefer this
    /// primary over the old one should that come back.
    pub fn promote(&self) -> Result<u64> {
        let repl = &self.shared.repl;
        if !repl.is_replica() {
            return Err(Error::Sql("this database is already a primary".into()));
        }
        repl.set_role(Role::Primary);
        let epoch = repl.epoch() + 1;
        let guard = self.shared.writer.acquire(*self.shared.busy_timeout.lock().unwrap())?;
        let seq = repl.next_seq();
        let epochs = repl.epochs_with(epoch, seq);
        let mut batch = crate::WriteBatch::new();
        batch
            .put(EPOCH_KEY, encode_u64(epoch))
            .put(EPOCHS_KEY, repl::encode_epochs(&epochs))
            .put(SEQ_KEY, encode_u64(seq))
            .delete(SNAPSHOT_END_KEY);
        self.shared.kv.write(&batch)?;
        repl.set_epoch(epoch);
        repl.set_epochs(epochs);
        repl.record(Entry { seq, batch: batch.into_data(), notifications: Vec::new() });
        drop(guard);
        Ok(epoch)
    }

    /// On a replica: applies a commit streamed from the primary. Its batch
    /// carries the commit's sequence number, so position and data move together.
    pub(crate) fn apply_entry(&self, entry: Entry) -> Result<()> {
        let batch = crate::WriteBatch::decode(&entry.batch)?;
        let schema_changed = batch.iter().any(|(key, _)| key.starts_with(b"\x00T") || key.starts_with(b"\x00I"));
        if let Some(epoch) = batch.iter().find(|(key, _)| *key == EPOCH_KEY).and_then(|(_, v)| v) {
            self.shared.repl.set_epoch(crate::coding::Reader::new(epoch).varint()?);
        }
        if let Some(epochs) = batch.iter().find(|(key, _)| *key == EPOCHS_KEY).and_then(|(_, v)| v) {
            self.shared.repl.set_epochs(repl::decode_epochs(epochs)?);
        }
        self.shared.kv.write(&batch)?;
        if schema_changed {
            self.shared.schemas.invalidate();
        }
        let notifications: Vec<Notification> = entry
            .notifications
            .iter()
            .map(|(channel, payload)| Notification { channel: channel.clone(), payload: payload.clone(), sender: 0 })
            .collect();
        self.shared.repl.applied(entry);
        self.shared.notifier.deliver(&notifications);
        Ok(())
    }

    /// On a replica: deletes everything before a snapshot is installed, and
    /// marks the data unusable until the snapshot is complete.
    pub(crate) fn begin_snapshot(&self) -> Result<()> {
        self.shared.repl.snapshot(0, u64::MAX);
        loop {
            let mut batch = crate::WriteBatch::new();
            for item in self.shared.kv.iter().take(10_000) {
                batch.delete(item?.0);
            }
            if batch.is_empty() {
                break;
            }
            self.shared.kv.write(&batch)?;
        }
        self.shared.kv.put(SNAPSHOT_END_KEY, encode_u64(u64::MAX))?;
        self.shared.schemas.invalidate();
        Ok(())
    }

    /// On a replica: applies one chunk of a snapshot (a batch of puts).
    pub(crate) fn apply_snapshot_chunk(&self, chunk: &[u8]) -> Result<()> {
        self.shared.kv.write(&crate::WriteBatch::decode(chunk)?)
    }

    /// On a replica: the snapshot taken at commit `start` is installed. It is
    /// fully consistent once commits up to `end` have been replayed on top.
    pub(crate) fn finish_snapshot(&self, start: u64, end: u64, epoch: u64, epochs: &[u8]) -> Result<()> {
        let mut batch = crate::WriteBatch::new();
        batch
            .put(SEQ_KEY, encode_u64(start))
            .put(SNAPSHOT_END_KEY, encode_u64(end))
            .put(EPOCH_KEY, encode_u64(epoch))
            .put(EPOCHS_KEY, epochs);
        self.shared.kv.write(&batch)?;
        self.shared.schemas.invalidate();
        self.shared.repl.set_epoch(epoch);
        self.shared.repl.set_epochs(repl::decode_epochs(epochs)?);
        self.shared.repl.snapshot(start, end);
        Ok(())
    }

    /// On a primary: streams the whole database (minus internal keys) as
    /// batches of puts, for a replica starting from scratch.
    pub(crate) fn snapshot_chunks(&self, send: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
        let mut batch = crate::WriteBatch::new();
        let mut bytes = 0;
        for item in self.shared.kv.range(b"\x00".as_slice()..b"\x01".as_slice()) {
            let (key, value) = item?;
            bytes += key.len() + value.len();
            batch.put(key, value);
            if bytes >= 1 << 20 {
                send(batch.data())?;
                batch.clear();
                bytes = 0;
            }
        }
        if !batch.is_empty() {
            send(batch.data())?;
        }
        Ok(())
    }
}

/// Where a routing client should send some SQL.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Route {
    /// Only reads, so any up-to-date replica can answer.
    pub read_only: bool,
    /// Whether a transaction is open afterwards, if the SQL opens or ends one.
    pub in_transaction: Option<bool>,
    /// Starts listening for notifications.
    pub listens: bool,
}

/// Classifies SQL for routing. SQL that doesn't parse counts as a write, so
/// the primary reports the error.
pub(crate) fn route(sql: &str) -> Route {
    let Ok(statements) = parser::parse_statements(sql) else { return Route::default() };
    let mut route = Route { read_only: !statements.is_empty(), ..Route::default() };
    for (statement, _) in &statements {
        route.read_only &= statement.is_read_only();
        match statement {
            Statement::Begin => route.in_transaction = Some(true),
            Statement::Commit | Statement::Rollback => route.in_transaction = Some(false),
            Statement::Listen(_) => route.listens = true,
            _ => {}
        }
    }
    route
}

/// A connection-like handle that runs statements, one at a time, and can keep
/// a transaction open between them. Dropping it rolls back any open transaction.
pub struct Session {
    db: Database,
    txn: Option<Txn>,
    id: u64,
    /// Where this session's notifications arrive, created by its first LISTEN.
    inbox: Option<Arc<Inbox>>,
    /// LISTEN and UNLISTEN inside a transaction take effect when it commits.
    pending_listens: Vec<Statement>,
    last_seq: Option<u64>,
}

impl Drop for Session {
    fn drop(&mut self) {
        // Like a closed PostgreSQL connection: stop listening everywhere.
        if let Some(inbox) = &self.inbox {
            inbox.unlisten(None);
        }
    }
}

impl Session {
    /// Identifies this session as the sender of its notifications.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The channels this session listens on.
    pub fn listening(&self) -> Vec<String> {
        self.inbox.as_ref().map_or_else(Vec::new, |inbox| inbox.channels())
    }

    /// Notifications received on the channels this session listens on,
    /// oldest first. Doesn't wait.
    pub fn notifications(&mut self) -> Vec<Notification> {
        self.inbox.as_ref().map_or_else(Vec::new, |inbox| inbox.drain())
    }

    /// Waits up to `timeout` for the next notification.
    pub fn wait_for_notification(&mut self, timeout: Duration) -> Option<Notification> {
        self.inbox().wait(Some(timeout))
    }

    /// A handle that receives this session's notifications on another thread,
    /// while this one keeps running statements.
    pub fn notification_receiver(&mut self) -> NotificationReceiver {
        NotificationReceiver::new(self.inbox().clone())
    }

    fn inbox(&mut self) -> &Arc<Inbox> {
        self.inbox.get_or_insert_with(|| {
            let inbox = Arc::new(Inbox::default());
            self.db.shared.notifier.register(&inbox);
            inbox
        })
    }

    fn apply_listen(&mut self, change: &Statement) {
        match change {
            Statement::Listen(channel) => self.inbox().listen(channel),
            Statement::Unlisten(channel) => {
                if let Some(inbox) = &self.inbox {
                    inbox.unlisten(channel.as_deref());
                }
            }
            _ => {}
        }
    }

    /// Delivers a commit's notifications. The commit still holds the write
    /// lock until this returns, so listeners see commits in order.
    /// The sequence number of this session's latest commit that changed
    /// anything. A client reading from a replica passes it along, so the
    /// replica waits until it has that commit: you always read your own writes.
    pub fn last_commit_seq(&self) -> Option<u64> {
        self.last_seq
    }

    /// Delivers a commit's notifications, releases its write lock, and in
    /// synchronous mode waits for replicas to confirm it.
    fn finish_commit(&mut self, committed: Committed) -> Result<()> {
        let seq = committed.seq;
        self.deliver(committed);
        let Some(seq) = seq else { return Ok(()) };
        self.last_seq = Some(seq);
        if !self.db.shared.repl.wait_for_replicas(seq) {
            return Err(Error::Sql(format!(
                "the transaction committed on the primary (commit {seq}), but not enough replicas confirmed it in time; \
                 it could be lost if the primary fails now"
            )));
        }
        Ok(())
    }

    fn deliver(&self, committed: Committed) {
        let notifications: Vec<Notification> = committed
            .notifications
            .into_iter()
            .map(|(channel, payload)| Notification { channel, payload, sender: self.id })
            .collect();
        self.db.shared.notifier.deliver(&notifications);
    }

    pub fn execute(&mut self, sql: &str) -> Result<QueryResult> {
        self.execute_with(sql, &[])
    }

    pub fn execute_with(&mut self, sql: &str, params: &[Value]) -> Result<QueryResult> {
        let prepared = self.db.prepare(sql)?;
        self.execute_prepared(&prepared, params)
    }

    pub fn execute_prepared(&mut self, prepared: &Prepared, params: &[Value]) -> Result<QueryResult> {
        if params.len() != prepared.params {
            return Err(Error::Sql(format!("expected {} parameters, got {}", prepared.params, params.len())));
        }
        self.run(prepared, params)
    }

    /// Runs a prepared query and hands each result row to `f` as it is
    /// produced, instead of collecting them all. Return `false` to stop.
    ///
    /// The row is lent, not given: it lives in a buffer reused for the next
    /// row, text included, so a query that streams needs no allocation per
    /// row. Queries that group, sort or remove duplicates must see every row
    /// first; those are collected, then handed over one by one.
    ///
    /// ```no_run
    /// # use spirodb::sql::{Database, Value};
    /// # let db = Database::open("./my-data")?;
    /// let query = db.prepare("SELECT id, name FROM users WHERE id >= ? AND id < ?")?;
    /// let mut session = db.session();
    /// let mut names = 0;
    /// session.query_each(&query, &[Value::Integer(100), Value::Integer(200)], |row| {
    ///     names += row[1].as_str().map_or(0, str::len);
    ///     true
    /// })?;
    /// # Ok::<(), spirodb::Error>(())
    /// ```
    pub fn query_each(
        &mut self,
        prepared: &Prepared,
        params: &[Value],
        mut f: impl FnMut(&[Value]) -> bool,
    ) -> Result<()> {
        if params.len() != prepared.params {
            return Err(Error::Sql(format!("expected {} parameters, got {}", prepared.params, params.len())));
        }
        if !matches!(prepared.statement, Statement::Select(_)) {
            if let QueryResult::Rows(rows) = self.run(prepared, params)? {
                rows.rows.iter().take_while(|row| f(row)).for_each(drop);
            }
            return Ok(());
        }
        let mut visit = |row: &[Value]| Ok(f(row));
        match &mut self.txn {
            Some(txn) => {
                txn.begin_statement();
                exec::stream(txn, &prepared.statement, params, &prepared.plans, &mut visit)
            }
            // Reading only, so there is nothing to commit.
            None => exec::stream(&self.db.read_txn(), &prepared.statement, params, &prepared.plans, &mut visit),
        }
    }

    /// Runs a query and returns its rows.
    pub fn query(&mut self, sql: &str, params: &[Value]) -> Result<Rows> {
        match self.execute_with(sql, params)? {
            QueryResult::Rows(rows) => Ok(rows),
            other => Err(Error::Sql(format!("expected a query, but the statement was {}", other))),
        }
    }

    /// Runs several `;`-separated statements, stopping at the first error.
    pub fn execute_script(&mut self, sql: &str) -> Result<Vec<QueryResult>> {
        self.db.prepare_all(sql)?.iter().map(|prepared| self.execute_prepared(prepared, &[])).collect()
    }

    pub fn in_transaction(&self) -> bool {
        self.txn.is_some()
    }

    fn run(&mut self, prepared: &Prepared, params: &[Value]) -> Result<QueryResult> {
        let statement = &prepared.statement;
        match statement {
            Statement::Begin => {
                if self.txn.is_some() {
                    return Err(Error::Sql("a transaction is already open".into()));
                }
                // A replica's transactions can only read, so they needn't wait for the write lock.
                let txn = if self.db.shared.repl.is_replica() { self.db.read_txn() } else { self.db.write_txn()? };
                self.txn = Some(txn.with_statement_undo());
                Ok(QueryResult::Done("BEGIN"))
            }
            Statement::Commit => {
                let txn = self.txn.take().ok_or_else(|| Error::Sql("no transaction is open".into()))?;
                let pending = std::mem::take(&mut self.pending_listens);
                let committed = txn.commit()?;
                // As in PostgreSQL, a LISTEN committed together with a NOTIFY
                // on the same channel receives it.
                pending.iter().for_each(|change| self.apply_listen(change));
                self.finish_commit(committed)?;
                Ok(QueryResult::Done("COMMIT"))
            }
            Statement::Rollback => {
                self.txn.take().ok_or_else(|| Error::Sql("no transaction is open".into()))?;
                self.pending_listens.clear();
                Ok(QueryResult::Done("ROLLBACK"))
            }
            Statement::Savepoint(name) | Statement::Release(name) | Statement::RollbackTo(name) => {
                let txn =
                    self.txn.as_mut().ok_or_else(|| Error::Sql("savepoints need a transaction (BEGIN)".into()))?;
                Ok(QueryResult::Done(match statement {
                    Statement::Savepoint(_) => {
                        txn.savepoint(name, self.pending_listens.len());
                        "SAVEPOINT"
                    }
                    Statement::Release(_) => {
                        txn.release(name)?;
                        "RELEASE"
                    }
                    _ => {
                        let listens = txn.rollback_to(name)?;
                        self.pending_listens.truncate(listens);
                        "ROLLBACK"
                    }
                }))
            }
            Statement::Listen(_) | Statement::Unlisten(_) => {
                if self.txn.is_some() {
                    self.pending_listens.push(statement.clone());
                } else {
                    self.apply_listen(statement);
                }
                Ok(QueryResult::Done(if matches!(statement, Statement::Listen(_)) { "LISTEN" } else { "UNLISTEN" }))
            }
            _ if !statement.is_read_only() && self.db.shared.repl.is_replica() => {
                Err(Error::ReadOnly("this database is a read-only replica; send writes to the primary".into()))
            }
            _ => match &mut self.txn {
                Some(txn) => {
                    txn.begin_statement();
                    let result = exec::execute(txn, statement, params, &prepared.plans);
                    if result.is_err() {
                        txn.rollback_statement();
                    }
                    result
                }
                None => {
                    let mut txn = if statement.is_read_only() { self.db.read_txn() } else { self.db.write_txn()? };
                    let result = exec::execute(&mut txn, statement, params, &prepared.plans)?;
                    let committed = txn.commit()?;
                    self.finish_commit(committed)?;
                    Ok(result)
                }
            },
        }
    }
}
