//! Transactions: writes are buffered in memory, visible to the transaction's
//! own reads, and committed to the key-value store as one atomic batch.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, HashSet};
use std::ops::Bound;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::batch::WriteBatch;
use crate::coding::{Key, KeyRef, put_varint};
use crate::db::{Db, ReadView, Snapshot};
use crate::error::{Error, Result};

use super::catalog::SchemaCache;
use super::repl::{self, Replication, SEQ_KEY, encode_u64};

/// Admits one writing transaction at a time. Unlike a `MutexGuard`, its guard
/// is an owned value, so a session can hold it across statements.
#[derive(Default)]
pub struct WriterLock {
    locked: Mutex<bool>,
    cv: Condvar,
}

pub struct WriteGuard(Arc<WriterLock>);

impl WriterLock {
    pub fn acquire(self: &Arc<Self>, timeout: Duration) -> Result<WriteGuard> {
        let deadline = Instant::now() + timeout;
        let mut locked = self.locked.lock().unwrap();
        while *locked {
            let now = Instant::now();
            if now >= deadline {
                return Err(Error::Sql("database is locked by another transaction".into()));
            }
            locked = self.cv.wait_timeout(locked, deadline - now).unwrap().0;
        }
        *locked = true;
        Ok(WriteGuard(self.clone()))
    }
}

impl Drop for WriteGuard {
    fn drop(&mut self) {
        *self.0.locked.lock().unwrap() = false;
        self.0.cv.notify_one();
    }
}

/// A buffered write: the new value, or `None` to delete the key.
type Write = Option<Vec<u8>>;

/// Receives each key and value of a scan; returns `false` to stop it.
pub type Visitor<'a> = &'a mut dyn FnMut(&[u8], &[u8]) -> Result<bool>;

/// A buffered write, tagged with the statement that made it.
#[derive(Clone)]
struct Buffered {
    value: Write,
    statement: u32,
}

/// How to undo the running statement, in an explicit transaction. Its new
/// entries are recognized by their statement tag; only the entries it
/// overwrote from earlier statements need saving.
struct Undo {
    statement: u32,
    overwritten: Vec<(Key, Buffered)>,
}

pub struct Txn {
    kv: Arc<Db>,
    writes: BTreeMap<Key, Buffered>,
    /// Kept only in explicit transactions: an autocommit transaction is
    /// discarded whole if its statement fails.
    undo: Option<Undo>,
    /// Row-id counters (by sequence key), kept here and written once at commit
    /// rather than read and rewritten by every insert.
    sequences: BTreeMap<Vec<u8>, i64>,
    /// NOTIFY messages (channel, payload, statement) waiting for commit.
    notifications: Vec<(String, String, u32)>,
    /// The (channel, payload) pairs already queued: as in PostgreSQL,
    /// identical notifications in one transaction are delivered once.
    notified: HashSet<(String, String)>,
    guard: Option<WriteGuard>,
    schemas: Arc<SchemaCache>,
    repl: Arc<Replication>,
    /// Whether this transaction has changed table or index definitions, in
    /// which case it must read them from its own writes, not the cache.
    schema_changed: bool,
    /// The most threads a query may use; 0 means every core.
    scan_threads: usize,
    /// Open savepoints, oldest first.
    savepoints: Vec<Savepoint>,
    /// For a transaction that only reads: the point in time it reads at, so
    /// every read sees the same committed state, never part of a commit.
    /// A writer holds the write lock, so the newest state is its own.
    snapshot: Option<Snapshot>,
    /// The schema cache's version when the snapshot was taken: once a
    /// schema change commits, cached definitions are newer than the
    /// snapshot's, and are read from it instead.
    snapshot_schemas: u64,
}

/// The transaction's state when `SAVEPOINT name` ran, to go back to.
struct Savepoint {
    name: String,
    /// The caller's own note of its state then.
    tag: usize,
    writes: BTreeMap<Key, Buffered>,
    sequences: BTreeMap<Vec<u8>, i64>,
    notifications: Vec<(String, String, u32)>,
}

impl Txn {
    /// A transaction that may only write if it holds the write lock. One
    /// without it reads a snapshot taken now.
    pub fn new(kv: Arc<Db>, schemas: Arc<SchemaCache>, repl: Arc<Replication>, guard: Option<WriteGuard>) -> Self {
        // The version first: a schema change committed after it shows as a
        // newer version, whether or not the snapshot includes it.
        let snapshot_schemas = schemas.version();
        let snapshot = guard.is_none().then(|| kv.snapshot());
        Txn {
            kv,
            writes: BTreeMap::new(),
            undo: None,
            sequences: BTreeMap::new(),
            notifications: Vec::new(),
            notified: HashSet::new(),
            guard,
            schemas,
            repl,
            schema_changed: false,
            scan_threads: 0,
            savepoints: Vec::new(),
            snapshot,
            snapshot_schemas,
        }
    }

    /// Whether this transaction only reads: it doesn't hold the write lock.
    pub fn is_read_only(&self) -> bool {
        self.guard.is_none()
    }

    /// Remembers the transaction's state as savepoint `name`, along with
    /// `tag`, which `rollback_to` hands back.
    pub fn savepoint(&mut self, name: &str, tag: usize) {
        self.savepoints.push(Savepoint {
            name: name.to_string(),
            tag,
            writes: self.writes.clone(),
            sequences: self.sequences.clone(),
            notifications: self.notifications.clone(),
        });
    }

    fn find_savepoint(&self, name: &str) -> Result<usize> {
        self.savepoints
            .iter()
            .rposition(|s| s.name == name)
            .ok_or_else(|| Error::Sql(format!("no such savepoint: {name}")))
    }

    /// Forgets savepoint `name` and those after it, keeping every change.
    pub fn release(&mut self, name: &str) -> Result<()> {
        let i = self.find_savepoint(name)?;
        self.savepoints.truncate(i);
        Ok(())
    }

    /// Undoes every change since savepoint `name`, which stays open; those
    /// after it are forgotten. Returns the savepoint's tag.
    pub fn rollback_to(&mut self, name: &str) -> Result<usize> {
        let i = self.find_savepoint(name)?;
        self.savepoints.truncate(i + 1);
        let savepoint = &self.savepoints[i];
        self.writes = savepoint.writes.clone();
        self.sequences = savepoint.sequences.clone();
        self.notifications = savepoint.notifications.clone();
        self.notified = self.notifications.iter().map(|(c, p, _)| (c.clone(), p.clone())).collect();
        Ok(savepoint.tag)
    }

    /// Caps the threads a query in this transaction may use (0: every core).
    pub fn with_scan_threads(mut self, threads: usize) -> Self {
        self.scan_threads = threads;
        self
    }

    pub fn scan_threads(&self) -> usize {
        self.scan_threads
    }

    /// Makes each statement undoable on its own, for explicit transactions.
    pub fn with_statement_undo(mut self) -> Self {
        self.undo = Some(Undo { statement: 0, overwritten: Vec::new() });
        self
    }

    /// The counter stored under `key`, read once per transaction.
    pub fn sequence(&mut self, key: &[u8], read: impl FnOnce(Option<&[u8]>) -> Result<i64>) -> Result<&mut i64> {
        if !self.sequences.contains_key(key) {
            let stored = self.get(key)?;
            let value = read(stored.as_deref())?;
            self.sequences.insert(key.to_vec(), value);
        }
        Ok(self.sequences.get_mut(key).unwrap())
    }

    /// Queues a notification for delivery when the transaction commits.
    pub fn notify(&mut self, channel: String, payload: String) {
        if self.notified.insert((channel.clone(), payload.clone())) {
            let statement = self.undo.as_ref().map_or(0, |undo| undo.statement);
            self.notifications.push((channel, payload, statement));
        }
    }

    /// Drops a counter, as when its table is dropped.
    pub fn forget_sequence(&mut self, key: &[u8]) {
        self.sequences.remove(key);
        self.delete(key.to_vec());
    }

    /// The shared schema cache, unless this transaction has changed the schema.
    pub fn schema_cache(&self) -> Option<&SchemaCache> {
        let current = self.snapshot.is_none() || self.schemas.version() == self.snapshot_schemas;
        (!self.schema_changed && current).then_some(&*self.schemas)
    }

    /// Identifies the schema this transaction sees, for caching query plans:
    /// which database's schema cache, and its version. `None` if this
    /// transaction has changed the schema itself.
    pub fn schema_version(&self) -> Option<(usize, u64)> {
        self.schema_cache().map(|cache| (Arc::as_ptr(&self.schemas) as usize, cache.version()))
    }

    pub fn mark_schema_changed(&mut self) {
        self.schema_changed = true;
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        match self.writes.get(KeyRef::new(key)) {
            Some(buffered) => Ok(buffered.value.clone()),
            None => match &self.snapshot {
                Some(snapshot) => snapshot.view().get(key),
                None => self.kv.get(key),
            },
        }
    }

    pub fn put(&mut self, key: Vec<u8>, value: Vec<u8>) {
        self.write(key, Some(value));
    }

    pub fn delete(&mut self, key: Vec<u8>) {
        self.write(key, None);
    }

    fn write(&mut self, key: Vec<u8>, value: Write) {
        debug_assert!(self.guard.is_some(), "writing without the write lock");
        let Some(undo) = &mut self.undo else {
            self.writes.insert(Key::from(key), Buffered { value, statement: 0 });
            return;
        };
        let buffered = Buffered { value, statement: undo.statement };
        match self.writes.entry(Key::from(key)) {
            Entry::Vacant(e) => {
                e.insert(buffered);
            }
            Entry::Occupied(mut e) => {
                let previous = std::mem::replace(e.get_mut(), buffered);
                if previous.statement != undo.statement {
                    undo.overwritten.push((e.key().clone(), previous));
                }
            }
        }
    }

    /// Marks the start of a statement, which `rollback_statement` can undo.
    pub fn begin_statement(&mut self) {
        if let Some(undo) = &mut self.undo {
            undo.statement += 1;
            undo.overwritten.clear();
        }
    }

    pub fn rollback_statement(&mut self) {
        let Some(undo) = &mut self.undo else { return };
        // Rare, so a pass over every buffered write is fine.
        self.writes.retain(|_, buffered| buffered.statement != undo.statement);
        for (key, previous) in undo.overwritten.drain(..) {
            self.writes.insert(key, previous);
        }
        if self.notifications.iter().any(|n| n.2 == undo.statement) {
            self.notifications.retain(|n| n.2 != undo.statement);
            self.notified = self.notifications.iter().map(|(c, p, _)| (c.clone(), p.clone())).collect();
        }
    }

    /// Calls `f` with each key and value in `[lower, upper]`, in order,
    /// including this transaction's own uncommitted writes. Keys and values
    /// are lent, not copied. `f` returns `false` to stop early.
    /// The stored data, for reading `[lower, upper]` from several threads at
    /// once; `None` if this transaction has its own writes there, which the
    /// view wouldn't show.
    pub fn shared_view(&self, lower: &Bound<Vec<u8>>, upper: &Bound<Vec<u8>>) -> Option<ReadView> {
        if is_empty(lower, upper) || self.writes.range::<[u8], _>((borrowed(lower), borrowed(upper))).next().is_some() {
            return None;
        }
        Some(self.read_view())
    }

    /// What this transaction reads committed data from.
    fn read_view(&self) -> ReadView {
        match &self.snapshot {
            Some(snapshot) => snapshot.view().clone(),
            None => self.kv.read_view(),
        }
    }

    pub fn scan(&self, lower: Bound<Vec<u8>>, upper: Bound<Vec<u8>>, f: Visitor) -> Result<()> {
        if is_empty(&lower, &upper) {
            return Ok(());
        }
        let start = match &lower {
            Bound::Included(k) => Bound::Included(k.as_slice()),
            Bound::Excluded(k) => Bound::Excluded(k.as_slice()),
            Bound::Unbounded => Bound::Unbounded,
        };
        let mut store = match &self.snapshot {
            Some(snapshot) => snapshot.view().raw_range(start, upper.clone()),
            None => self.kv.raw_range(start, upper.clone()),
        };
        let mut overlay = self.writes.range::<[u8], _>((borrowed(&lower), borrowed(&upper))).peekable();
        if overlay.peek().is_none() {
            // Nothing of ours to merge in: let the store run on its own.
            return store.for_each(|key, value| f(key, value));
        }
        loop {
            let from_overlay = match (overlay.peek(), store.valid()) {
                (None, false) => return store.status(),
                (Some(_), false) => true,
                (None, true) => false,
                (Some((buffered, _)), true) => buffered.as_slice() <= store.key(),
            };
            if from_overlay {
                let (key, buffered) = overlay.next().unwrap();
                // A buffered write shadows the stored version of the same key.
                if store.valid() && store.key() == key.as_slice() {
                    store.next();
                }
                if let Some(value) = &buffered.value
                    && !f(key.as_slice(), value)?
                {
                    return Ok(());
                }
            } else {
                if !f(store.key(), store.value())? {
                    return Ok(());
                }
                store.next();
            }
        }
    }

    /// Whether any key in `[lower, upper]` exists.
    pub fn any_in(&self, lower: Bound<Vec<u8>>, upper: Bound<Vec<u8>>) -> Result<bool> {
        let mut found = false;
        self.scan(lower, upper, &mut |_, _| {
            found = true;
            Ok(false)
        })?;
        Ok(found)
    }

    /// Writes everything atomically. The returned value carries the
    /// transaction's notifications and still holds the write lock, so they
    /// can be delivered before the next writer commits: in commit order.
    pub fn commit(mut self) -> Result<Committed> {
        for (key, next) in std::mem::take(&mut self.sequences) {
            let mut value = Vec::new();
            put_varint(&mut value, next as u64);
            self.writes.insert(Key::from(key), Buffered { value: Some(value), statement: 0 });
        }
        let notifications: Vec<(String, String)> =
            std::mem::take(&mut self.notifications).into_iter().map(|(c, p, _)| (c, p)).collect();
        if self.writes.is_empty() && notifications.is_empty() {
            return Ok(Committed { notifications, seq: None, _guard: self.guard.take() });
        }
        debug_assert!(self.guard.is_some(), "a commit with writes holds the write lock");
        // The commit's sequence number is stored in the same atomic batch as
        // its data, so a replica applying the batch records its progress too.
        let seq = self.repl.next_seq();
        // Sized up front: a big transaction would otherwise grow it many times.
        let bytes: usize =
            self.writes.iter().map(|(k, b)| k.as_slice().len() + b.value.as_ref().map_or(0, Vec::len) + 12).sum();
        let mut batch = WriteBatch::with_capacity(bytes + 32);
        for (key, buffered) in &self.writes {
            match &buffered.value {
                Some(value) => batch.put(key.as_slice(), value),
                None => batch.delete(key.as_slice()),
            };
        }
        batch.put(SEQ_KEY, encode_u64(seq));
        self.kv.write(&batch)?;
        if self.schema_changed {
            self.schemas.invalidate();
        }
        self.repl.record(repl::Entry { seq, batch: batch.into_data(), notifications: notifications.clone() });
        Ok(Committed { notifications, seq: Some(seq), _guard: self.guard.take() })
    }
}

/// A committed transaction's notifications, and its write lock, released
/// when this is dropped.
pub struct Committed {
    pub notifications: Vec<(String, String)>,
    /// The commit's sequence number, if it changed anything.
    pub seq: Option<u64>,
    _guard: Option<WriteGuard>,
}

fn borrowed(bound: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    match bound {
        Bound::Included(k) => Bound::Included(k.as_slice()),
        Bound::Excluded(k) => Bound::Excluded(k.as_slice()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

/// Whether the range holds no keys at all. `BTreeMap::range` panics on these.
fn is_empty(lower: &Bound<Vec<u8>>, upper: &Bound<Vec<u8>>) -> bool {
    match (lower, upper) {
        (Bound::Included(l), Bound::Included(u)) => l > u,
        (Bound::Included(l) | Bound::Excluded(l), Bound::Included(u) | Bound::Excluded(u)) => l >= u,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn reads_see_buffered_writes_and_commit_is_atomic() {
        let dir = TempDir::new("txn");
        let kv = Arc::new(Db::open(dir.path()).unwrap());
        for k in ["a", "b", "c", "d"] {
            kv.put(k, "stored").unwrap();
        }
        let lock = Arc::new(WriterLock::default());
        let guard = lock.acquire(Duration::from_secs(1)).unwrap();
        let mut txn = Txn::new(kv.clone(), Arc::default(), Arc::default(), Some(guard));
        txn.put(b"b".to_vec(), b"new".to_vec());
        txn.delete(b"c".to_vec());
        txn.put(b"e".to_vec(), b"new".to_vec());
        let mut seen: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        txn.scan(Bound::Unbounded, Bound::Unbounded, &mut |k, v| {
            seen.push((k.to_vec(), v.to_vec()));
            Ok(true)
        })
        .unwrap();
        let expect = [("a", "stored"), ("b", "new"), ("d", "stored"), ("e", "new")];
        assert_eq!(seen, expect.map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec())));
        assert_eq!(kv.get("b").unwrap(), Some(b"stored".to_vec()), "nothing visible before commit");

        // A second writer must wait for the lock.
        assert!(lock.acquire(Duration::from_millis(10)).is_err());
        txn.commit().unwrap();
        assert!(lock.acquire(Duration::from_millis(10)).is_ok());
        assert_eq!(kv.get("b").unwrap(), Some(b"new".to_vec()));
        assert_eq!(kv.get("c").unwrap(), None);
    }

    #[test]
    fn statement_rollback_restores_earlier_writes() {
        let dir = TempDir::new("txn-undo");
        let kv = Arc::new(Db::open(dir.path()).unwrap());
        let lock = Arc::new(WriterLock::default());
        let guard = lock.acquire(Duration::from_secs(1)).unwrap();
        let mut txn = Txn::new(kv, Arc::default(), Arc::default(), Some(guard)).with_statement_undo();
        txn.begin_statement();
        txn.put(b"k".to_vec(), b"1".to_vec());
        txn.begin_statement();
        txn.put(b"k".to_vec(), b"2".to_vec());
        txn.put(b"j".to_vec(), b"2".to_vec());
        txn.rollback_statement();
        assert_eq!(txn.get(b"k").unwrap(), Some(b"1".to_vec()));
        assert_eq!(txn.get(b"j").unwrap(), None);
        assert!(!txn.any_in(Bound::Excluded(b"z".to_vec()), Bound::Excluded(b"a".to_vec())).unwrap());
        assert!(txn.any_in(Bound::Included(b"k".to_vec()), Bound::Included(b"k".to_vec())).unwrap());
    }
}
