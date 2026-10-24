//! The database: ties the log, memtables and tables together, and runs the
//! background flush and compaction threads.
//!
//! Lock order, to stay deadlock-free: `wal` → `log_number` → `state` → a read
//! slot or a memtable's internal lock. Not every path takes every lock, but
//! none takes them out of this order.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::ops::{Bound, RangeBounds};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError, RwLock};
use std::thread::{self, JoinHandle};

use crate::batch::WriteBatch;
use crate::cache::BlockCache;
use crate::error::{Error, Result};
use crate::iter::{KvIter, LevelIter, MergeIter};
use crate::manifest::Manifest;
use crate::memtable::MemTable;
use crate::options::Options;
use crate::table::{Table, TableBuilder, TableMeta};
use crate::wal::{self, WalWriter};

fn table_path(dir: &Path, number: u64) -> PathBuf {
    dir.join(format!("{number:06}.sst"))
}

fn log_path(dir: &Path, number: u64) -> PathBuf {
    dir.join(format!("{number:06}.log"))
}

enum FileKind {
    Table,
    Log,
}

fn parse_file_name(name: &str) -> Option<(u64, FileKind)> {
    let (stem, ext) = name.rsplit_once('.')?;
    let number = stem.parse().ok()?;
    match ext {
        "sst" => Some((number, FileKind::Table)),
        "log" => Some((number, FileKind::Log)),
        _ => None,
    }
}

/// An immutable snapshot of the on-disk tables. Changes install a new one.
struct Version {
    /// Freshly flushed tables, newest first. Their key ranges may overlap.
    l0: Vec<Arc<Table>>,
    /// Compacted tables, sorted by key, with no overlap.
    l1: Vec<Arc<Table>>,
}

struct State {
    mem: Arc<MemTable>,
    /// The previous memtable, frozen while it is flushed to level 0.
    imm: Option<Arc<MemTable>>,
    version: Arc<Version>,
    /// Logs numbered below this are in tables recorded by a durable MANIFEST.
    durable_log: u64,
    /// The first unrecoverable error. Once set, writes are refused.
    bg_error: Option<Error>,
}

/// Everything a read needs, bundled so a reader picks it up with a single
/// refcount bump.
struct SuperVersion {
    mem: Arc<MemTable>,
    imm: Option<Arc<MemTable>>,
    version: Arc<Version>,
}

impl SuperVersion {
    fn of(state: &State) -> Arc<SuperVersion> {
        Arc::new(SuperVersion { mem: state.mem.clone(), imm: state.imm.clone(), version: state.version.clone() })
    }
}

/// Readers fetch the current super version from one of several slots, each
/// holding its own copy. Threads are spread across the slots, so concurrent
/// readers don't contend on one lock word or one refcount.
const READ_SLOTS: usize = 32;

/// Aligned to 128 bytes (Apple silicon's cache line) so slots don't share lines.
#[repr(align(128))]
struct Padded<T>(T);

fn read_slot() -> usize {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    thread_local! {
        static SLOT: usize = NEXT.fetch_add(1, Ordering::Relaxed) % READ_SLOTS;
    }
    SLOT.with(|slot| *slot)
}

#[derive(Default)]
struct Counters {
    flushes: AtomicU64,
    compactions: AtomicU64,
    write_stalls: AtomicU64,
    bytes_flushed: AtomicU64,
    bytes_compacted: AtomicU64,
}

struct Inner {
    dir: PathBuf,
    opts: Options,
    cache: Arc<BlockCache>,
    next_file: AtomicU64,
    /// Serialises writers and owns the active log.
    wal: Mutex<WalWriter>,
    /// Serialises MANIFEST rewrites, and holds the log number they record.
    log_number: Mutex<u64>,
    state: Mutex<State>,
    /// Signalled whenever `state` changes in a way a thread may be waiting for.
    cv: Condvar,
    /// What readers see. Republished, under the state lock, whenever the
    /// memtables or tables change.
    read_slots: Box<[Padded<RwLock<Arc<SuperVersion>>>]>,
    /// Set (while holding `state`) when the database is closing.
    shutdown: AtomicBool,
    counters: Counters,
    /// Holds the advisory lock that keeps other processes out.
    _lock: File,
}

/// A SpiroDB database.
///
/// All operations take `&self`; share one handle between threads with
/// `Arc<Db>`. Dropping it stops the background threads and syncs the log.
pub struct Db {
    inner: Arc<Inner>,
    workers: Vec<JoinHandle<()>>,
}

/// A snapshot of the database's shape and activity.
#[derive(Clone, Debug, Default)]
pub struct Stats {
    /// Bytes buffered in memtables, not yet flushed to tables.
    pub memtable_bytes: usize,
    pub l0_tables: usize,
    pub l1_tables: usize,
    /// Total size of all live table files.
    pub table_bytes: u64,
    pub flushes: u64,
    pub compactions: u64,
    /// Times a writer had to wait for a flush or compaction to catch up.
    pub write_stalls: u64,
    pub bytes_flushed: u64,
    pub bytes_compacted: u64,
}

impl Db {
    /// Opens the database at `path`, creating it if needed, with default options.
    pub fn open(path: impl AsRef<Path>) -> Result<Db> {
        Self::open_with(path, Options::default())
    }

    /// Opens the database at `path`, creating it if needed.
    pub fn open_with(path: impl AsRef<Path>, opts: Options) -> Result<Db> {
        opts.validate()?;
        let dir = path.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        let lock = OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("LOCK"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(Error::Locked(dir)),
            Err(TryLockError::Error(e)) => return Err(e.into()),
        }

        let cache = Arc::new(BlockCache::new(opts.block_cache_size));
        let loaded = Manifest::load(&dir)?;
        let fresh = loaded.is_none();
        let manifest = loaded.unwrap_or_default();
        let open_level = |metas: &[TableMeta]| -> Result<Vec<Arc<Table>>> {
            metas
                .iter()
                .map(|m| Ok(Arc::new(Table::open(&table_path(&dir, m.number), m.clone(), cache.clone())?)))
                .collect()
        };
        let mut l0 = open_level(&manifest.l0)?;
        let l1 = open_level(&manifest.l1)?;

        let mut next_file = manifest.next_file.max(1);
        let mut logs = Vec::new();
        let mut has_tables = false;
        for entry in fs::read_dir(&dir)? {
            if let Some((number, kind)) = entry?.file_name().to_str().and_then(parse_file_name) {
                next_file = next_file.max(number + 1);
                match kind {
                    FileKind::Log if number >= manifest.log_number => logs.push(number),
                    FileKind::Log => {}
                    FileKind::Table => has_tables = true,
                }
            }
        }
        if fresh && has_tables {
            // Carrying on would treat every table as an orphan and delete it.
            return Err(Error::Corruption("MANIFEST is missing but table files exist".into()));
        }
        logs.sort_unstable();
        let next_file = AtomicU64::new(next_file);

        // Replay the logs that never made it into a table, then write what they
        // held to a level-0 table so the database starts on a fresh, empty log.
        let recovered = MemTable::new(0);
        for &number in &logs {
            wal::replay(&log_path(&dir, number), |payload| {
                recovered.apply(&WriteBatch::decode(payload)?);
                Ok(())
            })?;
        }
        if !recovered.is_empty() {
            let number = next_file.fetch_add(1, Ordering::SeqCst);
            l0.insert(0, Arc::new(write_memtable(&dir, number, &recovered, &opts, &cache)?));
        }
        let wal_number = next_file.fetch_add(1, Ordering::SeqCst);
        let wal = WalWriter::create(&log_path(&dir, wal_number), opts.sync)?;
        let version = Version { l0, l1 };
        write_manifest(&dir, next_file.load(Ordering::SeqCst), wal_number, &version)?;

        // Delete whatever the new manifest doesn't need: replayed logs, and
        // tables left behind by an interrupted flush or compaction.
        let live: HashSet<u64> = version.l0.iter().chain(&version.l1).map(|t| t.meta.number).collect();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let obsolete = match name.to_str().and_then(parse_file_name) {
                Some((number, FileKind::Log)) => number < wal_number,
                Some((number, FileKind::Table)) => !live.contains(&number),
                None => name == "MANIFEST.tmp",
            };
            if obsolete {
                let _ = fs::remove_file(entry.path());
            }
        }

        let state = State {
            mem: Arc::new(MemTable::new(wal_number)),
            imm: None,
            version: Arc::new(version),
            durable_log: wal_number,
            bg_error: None,
        };
        let inner = Arc::new(Inner {
            dir,
            opts,
            cache,
            next_file,
            wal: Mutex::new(wal),
            log_number: Mutex::new(wal_number),
            read_slots: (0..READ_SLOTS).map(|_| Padded(RwLock::new(SuperVersion::of(&state)))).collect(),
            state: Mutex::new(state),
            cv: Condvar::new(),
            shutdown: AtomicBool::new(false),
            counters: Counters::default(),
            _lock: lock,
        });
        // Built before spawning, so that if a spawn fails, dropping `db` stops
        // the threads already running.
        let mut db = Db { inner, workers: Vec::new() };
        for (name, work) in
            [("spiro-flush", Inner::flush_loop as fn(&Inner)), ("spiro-compact", Inner::compaction_loop)]
        {
            let inner = db.inner.clone();
            db.workers.push(thread::Builder::new().name(name.into()).spawn(move || work(&inner))?);
        }
        Ok(db)
    }

    pub fn put(&self, key: impl AsRef<[u8]>, value: impl AsRef<[u8]>) -> Result<()> {
        let mut batch = WriteBatch::new();
        batch.put(key, value);
        self.write(&batch)
    }

    pub fn delete(&self, key: impl AsRef<[u8]>) -> Result<()> {
        let mut batch = WriteBatch::new();
        batch.delete(key);
        self.write(&batch)
    }

    /// Applies every operation in `batch` atomically.
    pub fn write(&self, batch: &WriteBatch) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        if batch.data().len() > u32::MAX as usize {
            return Err(Error::InvalidArgument("write batch exceeds 4 GiB".into()));
        }
        let inner = &*self.inner;
        let mut wal = inner.wal.lock().unwrap();
        let mem = inner.make_room_for_write(&mut wal, false)?;
        if let Err(e) = wal.append(batch.data(), inner.opts.sync) {
            // A partial record may now sit at the end of the log, and replay
            // stops at the first bad record. Appending after it would make
            // later writes unrecoverable, so stop accepting writes instead.
            inner.set_bg_error(e.clone());
            return Err(e);
        }
        mem.apply(batch);
        Ok(())
    }

    pub fn get(&self, key: impl AsRef<[u8]>) -> Result<Option<Vec<u8>>> {
        let key = key.as_ref();
        let sv = self.inner.super_version();
        if let Some(found) = sv.mem.get(key) {
            return Ok(found);
        }
        if let Some(imm) = &sv.imm
            && let Some(found) = imm.get(key)
        {
            return Ok(found);
        }
        let version = &sv.version;
        for table in &version.l0 {
            if table.covers(key)
                && let Some(found) = table.get(key)?
            {
                return Ok(found);
            }
        }
        let i = version.l1.partition_point(|t| t.meta.largest.as_slice() < key);
        if let Some(table) = version.l1.get(i)
            && table.covers(key)
            && let Some(found) = table.get(key)?
        {
            return Ok(found);
        }
        Ok(None)
    }

    /// Iterates over every key/value pair in key order.
    pub fn iter(&self) -> DbIterator {
        self.range::<&[u8], _>(..)
    }

    /// Iterates over the key/value pairs within `range`, in key order:
    /// `db.range("a".."m")`, `db.range(start_key..)`, and so on.
    pub fn range<K: AsRef<[u8]>, R: RangeBounds<K>>(&self, range: R) -> DbIterator {
        let start = match range.start_bound() {
            Bound::Included(start) => Bound::Included(start.as_ref()),
            Bound::Excluded(start) => Bound::Excluded(start.as_ref()),
            Bound::Unbounded => Bound::Unbounded,
        };
        let end = match range.end_bound() {
            Bound::Included(end) => Bound::Included(end.as_ref().to_vec()),
            Bound::Excluded(end) => Bound::Excluded(end.as_ref().to_vec()),
            Bound::Unbounded => Bound::Unbounded,
        };
        DbIterator { raw: self.raw_range(start, end), done: false }
    }

    /// Like `range`, but lends each key and value instead of copying them.
    pub(crate) fn raw_range(&self, start: Bound<&[u8]>, end: Bound<Vec<u8>>) -> RawIter {
        let sv = self.inner.super_version();
        let mut sources: Vec<Box<dyn KvIter>> = vec![Box::new(sv.mem.iter(end.clone()))];
        if let Some(imm) = &sv.imm {
            sources.push(Box::new(imm.iter(end.clone())));
        }
        sources.extend(sv.version.l0.iter().map(|t| Box::new(t.iter(true)) as Box<dyn KvIter>));
        sources.push(Box::new(LevelIter::new(sv.version.l1.clone(), true)));

        let mut it = MergeIter::new(sources);
        match start {
            Bound::Included(start) => it.seek(start),
            Bound::Excluded(start) => {
                it.seek(start);
                if it.valid() && it.key() == start {
                    it.next();
                }
            }
            Bound::Unbounded => it.seek(&[]),
        }
        let mut raw = RawIter { it, end, valid: false };
        raw.settle();
        raw
    }

    /// Writes the memtable out to a table now, rather than when it fills.
    /// Returns once every write made before the call is in a table on disk.
    pub fn flush(&self) -> Result<()> {
        let mut wal = self.inner.wal.lock().unwrap();
        let target = self.inner.make_room_for_write(&mut wal, true)?.wal_number;
        drop(wal);
        let mut state = self.inner.state.lock().unwrap();
        while state.durable_log < target {
            if let Some(e) = &state.bg_error {
                return Err(e.clone());
            }
            state = self.inner.cv.wait(state).unwrap();
        }
        Ok(())
    }

    pub fn stats(&self) -> Stats {
        let SuperVersion { mem, imm, version } = &*self.inner.super_version();
        let c = &self.inner.counters;
        Stats {
            memtable_bytes: mem.approximate_size() + imm.as_ref().map_or(0, |m| m.approximate_size()),
            l0_tables: version.l0.len(),
            l1_tables: version.l1.len(),
            table_bytes: version.l0.iter().chain(&version.l1).map(|t| t.meta.size).sum(),
            flushes: c.flushes.load(Ordering::Relaxed),
            compactions: c.compactions.load(Ordering::Relaxed),
            write_stalls: c.write_stalls.load(Ordering::Relaxed),
            bytes_flushed: c.bytes_flushed.load(Ordering::Relaxed),
            bytes_compacted: c.bytes_compacted.load(Ordering::Relaxed),
        }
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        {
            // Set the flag under the state lock so no worker misses the wakeup.
            let _state = self.inner.state.lock().unwrap_or_else(PoisonError::into_inner);
            self.inner.shutdown.store(true, Ordering::SeqCst);
        }
        self.inner.cv.notify_all();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
        let _ = self.inner.wal.lock().unwrap_or_else(PoisonError::into_inner).sync();
    }
}

impl Inner {
    fn super_version(&self) -> Arc<SuperVersion> {
        self.read_slots[read_slot()].0.read().unwrap().clone()
    }

    /// Hands readers the latest memtables and tables. Called with the state
    /// lock held, right after changing them, and before any write can land in
    /// a new memtable, so readers never miss a write they could have seen.
    fn publish(&self, state: &State) {
        for slot in &self.read_slots {
            *slot.0.write().unwrap() = SuperVersion::of(state);
        }
    }

    /// Makes sure the active memtable has room for a write, freezing it and
    /// switching to a fresh log when it is full (or non-empty, with `force`).
    /// Returns the memtable the write should go into. The caller holds the
    /// writer lock, and `wal` is the log it guards.
    fn make_room_for_write(&self, wal: &mut WalWriter, mut force: bool) -> Result<Arc<MemTable>> {
        let mut state = self.state.lock().unwrap();
        let mut stalled = false;
        loop {
            if let Some(e) = &state.bg_error {
                return Err(e.clone());
            }
            let full = state.mem.approximate_size() >= self.opts.memtable_size;
            if !full && !(force && !state.mem.is_empty()) {
                return Ok(state.mem.clone());
            }
            if state.imm.is_some() || state.version.l0.len() >= self.opts.l0_stop_writes_trigger {
                // The previous memtable is still being flushed, or level 0 is
                // backed up. Wait for the background threads to catch up.
                if !stalled {
                    stalled = true;
                    self.counters.write_stalls.fetch_add(1, Ordering::Relaxed);
                }
                state = self.cv.wait(state).unwrap();
                continue;
            }
            // Only writers freeze memtables and we are the writer, so `imm`
            // stays empty while the state lock is released to create the log.
            drop(state);
            let number = self.next_file.fetch_add(1, Ordering::SeqCst);
            *wal = WalWriter::create(&log_path(&self.dir, number), self.opts.sync)?;
            state = self.state.lock().unwrap();
            let frozen = std::mem::replace(&mut state.mem, Arc::new(MemTable::new(number)));
            state.imm = Some(frozen);
            self.publish(&state);
            force = false;
            self.cv.notify_all();
        }
    }

    fn set_bg_error(&self, e: Error) {
        let mut state = self.state.lock().unwrap();
        state.bg_error.get_or_insert(e);
        self.cv.notify_all();
    }

    /// Blocks until `pick` finds work, or returns `None` once the database closes.
    fn wait_for_work<T>(&self, pick: impl Fn(&State) -> Option<T>) -> Option<T> {
        let mut state = self.state.lock().unwrap();
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                return None;
            }
            if state.bg_error.is_none()
                && let Some(work) = pick(&state)
            {
                return Some(work);
            }
            state = self.cv.wait(state).unwrap();
        }
    }

    fn flush_loop(&self) {
        while let Some(imm) = self.wait_for_work(|state| state.imm.clone()) {
            if let Err(e) = self.flush_memtable(&imm) {
                self.set_bg_error(e);
            }
        }
    }

    fn compaction_loop(&self) {
        let trigger = self.opts.l0_compaction_trigger;
        while let Some(version) =
            self.wait_for_work(|state| (state.version.l0.len() >= trigger).then(|| state.version.clone()))
        {
            if let Err(e) = self.compact(&version) {
                self.set_bg_error(e);
            }
        }
    }

    /// Writes the frozen memtable to a new level-0 table.
    fn flush_memtable(&self, imm: &Arc<MemTable>) -> Result<()> {
        let table = if imm.is_empty() {
            None
        } else {
            let number = self.next_file.fetch_add(1, Ordering::SeqCst);
            Some(Arc::new(write_memtable(&self.dir, number, imm, &self.opts, &self.cache)?))
        };

        let mut log_number = self.log_number.lock().unwrap();
        let (version, live_log) = {
            let mut state = self.state.lock().unwrap();
            if let Some(table) = &table {
                let l0 = std::iter::once(table.clone()).chain(state.version.l0.iter().cloned()).collect();
                state.version = Arc::new(Version { l0, l1: state.version.l1.clone() });
            }
            state.imm = None;
            self.publish(&state);
            self.cv.notify_all();
            (state.version.clone(), state.mem.wal_number)
        };
        // The active memtable's log is now the oldest one recovery needs.
        *log_number = live_log;
        self.write_manifest(&version, live_log)?;
        drop(log_number);
        let _ = fs::remove_file(log_path(&self.dir, imm.wal_number));

        self.counters.flushes.fetch_add(1, Ordering::Relaxed);
        let bytes = table.map_or(0, |t| t.meta.size);
        self.counters.bytes_flushed.fetch_add(bytes, Ordering::Relaxed);
        let mut state = self.state.lock().unwrap();
        state.durable_log = state.durable_log.max(live_log);
        self.cv.notify_all();
        Ok(())
    }

    /// Merges every level-0 table in `version`, plus the level-1 tables they
    /// overlap, into new level-1 tables.
    fn compact(&self, version: &Version) -> Result<()> {
        let l0 = &version.l0;
        let smallest = l0.iter().map(|t| t.meta.smallest.as_slice()).min().unwrap_or_default();
        let largest = l0.iter().map(|t| t.meta.largest.as_slice()).max().unwrap_or_default();
        let overlapping: Vec<Arc<Table>> = version
            .l1
            .iter()
            .filter(|t| t.meta.largest.as_slice() >= smallest && t.meta.smallest.as_slice() <= largest)
            .cloned()
            .collect();

        // Level-0 tables are already newest first, which is what MergeIter wants.
        let mut sources: Vec<Box<dyn KvIter>> = l0.iter().map(|t| Box::new(t.iter(false)) as Box<dyn KvIter>).collect();
        sources.push(Box::new(LevelIter::new(overlapping.clone(), false)));
        let mut merged = MergeIter::new(sources);
        merged.seek(&[]);

        let mut created = Vec::new();
        let outputs = match self.write_compaction_outputs(&mut merged, &mut created) {
            Ok(Some(outputs)) => outputs,
            Ok(None) => {
                self.remove_tables(&created); // interrupted by shutdown
                return Ok(());
            }
            Err(e) => {
                self.remove_tables(&created);
                return Err(e);
            }
        };

        let inputs: HashSet<u64> = l0.iter().chain(&overlapping).map(|t| t.meta.number).collect();
        let log_number = self.log_number.lock().unwrap();
        let version = {
            let mut state = self.state.lock().unwrap();
            // Level 0 may have gained tables since the compaction began; keep those.
            let l0 = state.version.l0.iter().filter(|t| !inputs.contains(&t.meta.number)).cloned().collect();
            let mut l1: Vec<_> =
                state.version.l1.iter().filter(|t| !inputs.contains(&t.meta.number)).cloned().collect();
            l1.extend(outputs.iter().cloned());
            l1.sort_by(|a, b| a.meta.smallest.cmp(&b.meta.smallest));
            state.version = Arc::new(Version { l0, l1 });
            self.publish(&state);
            self.cv.notify_all();
            state.version.clone()
        };
        self.write_manifest(&version, *log_number)?;
        drop(log_number);
        // Readers holding an older version keep their files open, so deleting
        // is safe; on platforms that refuse, the next open cleans up.
        self.remove_tables(&inputs.into_iter().collect::<Vec<_>>());

        self.counters.compactions.fetch_add(1, Ordering::Relaxed);
        let bytes = outputs.iter().map(|t| t.meta.size).sum();
        self.counters.bytes_compacted.fetch_add(bytes, Ordering::Relaxed);
        Ok(())
    }

    /// Streams merged entries into new tables of roughly `target_file_size`.
    /// Tombstones are dropped: level 1 is the bottom of the tree, so there is
    /// nothing older left for them to hide. Returns `None` on shutdown.
    fn write_compaction_outputs(
        &self,
        merged: &mut MergeIter,
        created: &mut Vec<u64>,
    ) -> Result<Option<Vec<Arc<Table>>>> {
        let mut outputs = Vec::new();
        let mut builder: Option<TableBuilder> = None;
        let mut entries = 0u64;
        while merged.valid() {
            if let Some(value) = merged.value() {
                if builder.is_none() {
                    let number = self.next_file.fetch_add(1, Ordering::SeqCst);
                    created.push(number);
                    builder = Some(TableBuilder::create(&table_path(&self.dir, number), number, &self.opts)?);
                }
                let b = builder.as_mut().unwrap();
                b.add(merged.key(), Some(value))?;
                if b.estimated_size() >= self.opts.target_file_size {
                    outputs.push(self.finish_table(builder.take().unwrap())?);
                }
            }
            merged.next();
            entries += 1;
            if entries.is_multiple_of(4096) && self.shutdown.load(Ordering::Relaxed) {
                return Ok(None);
            }
        }
        // An input read failure ends the merge early; it must not pass for the end of the data.
        merged.status()?;
        if let Some(b) = builder {
            outputs.push(self.finish_table(b)?);
        }
        Ok(Some(outputs))
    }

    fn finish_table(&self, builder: TableBuilder) -> Result<Arc<Table>> {
        let meta = builder.finish()?;
        Ok(Arc::new(Table::open(&table_path(&self.dir, meta.number), meta, self.cache.clone())?))
    }

    fn remove_tables(&self, numbers: &[u64]) {
        for &number in numbers {
            let _ = fs::remove_file(table_path(&self.dir, number));
        }
    }

    fn write_manifest(&self, version: &Version, log_number: u64) -> Result<()> {
        write_manifest(&self.dir, self.next_file.load(Ordering::SeqCst), log_number, version)
    }
}

/// Writes a non-empty memtable to table `number` and opens it.
fn write_memtable(dir: &Path, number: u64, mem: &MemTable, opts: &Options, cache: &Arc<BlockCache>) -> Result<Table> {
    let path = table_path(dir, number);
    let mut builder = TableBuilder::create(&path, number, opts)?;
    mem.for_each(|key, value| builder.add(key, value))?;
    Table::open(&path, builder.finish()?, cache.clone())
}

fn write_manifest(dir: &Path, next_file: u64, log_number: u64, version: &Version) -> Result<()> {
    let metas = |level: &[Arc<Table>]| level.iter().map(|t| t.meta.clone()).collect();
    Manifest { next_file, log_number, l0: metas(&version.l0), l1: metas(&version.l1) }.store(dir)
}

/// Iterates over live entries in a key range, lending each key and value
/// rather than copying them. Tombstones are skipped.
pub(crate) struct RawIter {
    it: MergeIter,
    end: Bound<Vec<u8>>,
    /// Positioned on a live entry within range.
    valid: bool,
}

impl RawIter {
    /// Moves past deleted entries, and notes whether we're still in range.
    fn settle(&mut self) {
        loop {
            self.valid = self.it.valid()
                && match &self.end {
                    Bound::Included(end) => self.it.key() <= end.as_slice(),
                    Bound::Excluded(end) => self.it.key() < end.as_slice(),
                    Bound::Unbounded => true,
                };
            if !self.valid || self.it.value().is_some() {
                return;
            }
            self.it.next();
        }
    }

    pub fn valid(&self) -> bool {
        self.valid
    }

    pub fn key(&self) -> &[u8] {
        self.it.key()
    }

    pub fn value(&self) -> &[u8] {
        self.it.value().expect("deleted entries are skipped")
    }

    pub fn next(&mut self) {
        self.it.next();
        self.settle();
    }

    /// The error, if one cut the iteration short.
    pub fn status(&self) -> Result<()> {
        self.it.status()
    }
}

/// Iterates over live key/value pairs in key order.
///
/// It reads from the set of tables that existed when it was created, so
/// flushes and compactions never disturb it. Writes made while it runs may or
/// may not be seen.
pub struct DbIterator {
    raw: RawIter,
    done: bool,
}

impl Iterator for DbIterator {
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        if !self.raw.valid() {
            self.done = true;
            return self.raw.status().err().map(Err);
        }
        let item = (self.raw.key().to_vec(), self.raw.value().to_vec());
        self.raw.next();
        Some(Ok(item))
    }
}
