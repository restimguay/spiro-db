//! The database: ties the log, memtables and tables together, and runs the
//! background flush and compaction threads.
//!
//! Lock order, to stay deadlock-free: `wal` → `log_number` → `state` → a read
//! slot or a memtable's internal lock. Not every path takes every lock, but
//! none takes them out of this order.

use std::borrow::Cow;
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::ops::{Bound, RangeBounds};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError, RwLock};
use std::thread::{self, JoinHandle};

use crate::batch::WriteBatch;
use crate::cache::BlockCache;
use crate::error::{Error, Result};
use crate::iter::{KvIter, LevelIter, MergeIter};
use crate::manifest::Manifest;
use crate::memtable::MemTable;
use crate::options::Options;
use crate::table::{SectionBuilder, Table, TableBuilder, TableMeta};
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

/// A fixed set of memtables and tables to read from. Without a snapshot,
/// writes made after it was taken may or may not show, as with any
/// iterator; with one, exactly those up to the snapshot do.
#[derive(Clone)]
pub(crate) struct ReadView {
    sv: Arc<SuperVersion>,
    /// The last write batch this view sees, or `None` for whatever is newest.
    seq: Option<u64>,
}

impl ReadView {
    /// The snapshot this view reads at, if any.
    pub(crate) fn seq(&self) -> Option<u64> {
        self.seq
    }

    pub(crate) fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let sv = &self.sv;
        for mem in std::iter::once(&sv.mem).chain(&sv.imm) {
            if let Some(found) = mem.get_at(key, self.seq) {
                return Ok(found);
            }
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

    pub(crate) fn raw_range(&self, start: Bound<&[u8]>, end: Bound<Vec<u8>>) -> RawIter {
        let sv = &self.sv;
        let mut sources: Vec<Box<dyn KvIter>> = vec![Box::new(sv.mem.iter(end.clone(), self.seq))];
        if let Some(imm) = &sv.imm {
            sources.push(Box::new(imm.iter(end.clone(), self.seq)));
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

    /// The memtables, newest first.
    pub(crate) fn memtables(&self) -> impl Iterator<Item = &Arc<MemTable>> {
        std::iter::once(&self.sv.mem).chain(&self.sv.imm)
    }

    /// Level 0's tables, newest first; their key ranges may overlap.
    pub(crate) fn level0(&self) -> &[Arc<Table>] {
        &self.sv.version.l0
    }

    /// Level 1's tables, sorted by key, without overlap.
    pub(crate) fn level1(&self) -> &[Arc<Table>] {
        &self.sv.version.l1
    }

    /// Keys inside `(start, end)` at which to split it into parts of roughly
    /// equal size: the boundaries of the tables' data blocks (about 4 KiB
    /// each), sorted. Data still in memtables isn't counted.
    ///
    /// Memtables contribute sampled keys, one per `SAMPLE_EVERY` entries,
    /// about as many as a table has blocks for the same amount of data.
    pub(crate) fn split_points(&self, start: &Bound<Vec<u8>>, end: &Bound<Vec<u8>>) -> Vec<Cow<'_, [u8]>> {
        let sv = &self.sv;
        // Level 1's tables are sorted and don't overlap, so its boundaries
        // come in order; each other source's list is sorted too.
        let mut points: Vec<Cow<[u8]>> = sv
            .version
            .l1
            .iter()
            .flat_map(|t| t.block_boundaries(start, end))
            .map(|k| Cow::Borrowed(k.as_slice()))
            .collect();
        for table in &sv.version.l0 {
            let keys = table.block_boundaries(start, end).iter().map(|k| Cow::Borrowed(k.as_slice())).collect();
            points = merge_sorted(points, keys);
        }
        for mem in std::iter::once(&sv.mem).chain(&sv.imm) {
            points = merge_sorted(points, mem.sample_keys(start, end).into_iter().map(Cow::Owned).collect());
        }
        points
    }
}

/// A consistent point-in-time view; see [`Db::snapshot`].
pub(crate) struct Snapshot {
    view: ReadView,
    inner: Arc<Inner>,
}

impl Snapshot {
    pub(crate) fn view(&self) -> &ReadView {
        &self.view
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        let seq = self.view.seq.expect("a snapshot has a sequence number");
        let mut snapshots = self.inner.snapshots.lock().unwrap();
        if let Some(at) = snapshots.iter().rposition(|&s| s == seq) {
            snapshots.remove(at);
        }
        self.inner.newest_snapshot.store(snapshots.last().copied().unwrap_or(0), Ordering::Relaxed);
    }
}

/// Merges two sorted lists into one, without duplicates.
fn merge_sorted<'a>(a: Vec<Cow<'a, [u8]>>, b: Vec<Cow<'a, [u8]>>) -> Vec<Cow<'a, [u8]>> {
    if b.is_empty() {
        return a;
    }
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut a, mut b) = (a.into_iter().peekable(), b.into_iter().peekable());
    loop {
        let next = match (a.peek(), b.peek()) {
            (Some(x), Some(y)) if x <= y => a.next(),
            (Some(_), Some(_)) | (None, Some(_)) => b.next(),
            (Some(_), None) => a.next(),
            (None, None) => return out,
        };
        let next = next.unwrap();
        if out.last() != Some(&next) {
            out.push(next);
        }
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

/// Makes a builder for each new table file's column section.
pub(crate) type SectionFactory = Arc<dyn Fn() -> Box<dyn SectionBuilder> + Send + Sync>;
/// Prepares a newly written table's column section for reading, on the
/// background thread that wrote it, so no query has to.
pub(crate) type SectionWarmer = Arc<dyn Fn(&Table) + Send + Sync>;

struct Inner {
    dir: PathBuf,
    opts: Options,
    cache: Arc<BlockCache>,
    /// Set by the layer above to add column sections to new table files.
    sections: OnceLock<(SectionFactory, SectionWarmer)>,
    next_file: AtomicU64,
    /// Serialises writers and owns the active log.
    wal: Mutex<WalWriter>,
    /// The number of the last write batch applied; readers taking a snapshot
    /// see everything up to it.
    last_seq: AtomicU64,
    /// Open snapshots, by sequence number, with how many share each. Held
    /// by writers while they apply a batch, so a snapshot can't start
    /// between their reading `newest_snapshot` and their write.
    snapshots: Mutex<Vec<u64>>,
    /// The newest open snapshot (0 if none): replaced values a snapshot up
    /// to it may read are kept.
    newest_snapshot: AtomicU64,
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
                recovered.apply(&WriteBatch::decode(payload)?, 1, 0);
                Ok(())
            })?;
        }
        if !recovered.is_empty() {
            let number = next_file.fetch_add(1, Ordering::SeqCst);
            l0.insert(0, Arc::new(write_memtable(&dir, number, &recovered, &opts, &cache, None)?));
        }
        let wal_number = next_file.fetch_add(1, Ordering::SeqCst);
        let wal = WalWriter::create(&log_path(&dir, wal_number), opts.sync)?;
        let version = Version { l0, l1 };
        write_manifest(&dir, next_file.load(Ordering::SeqCst), wal_number, &version, opts.sync)?;

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
            sections: OnceLock::new(),
            next_file,
            wal: Mutex::new(wal),
            last_seq: AtomicU64::new(0),
            snapshots: Mutex::default(),
            newest_snapshot: AtomicU64::new(0),
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
        // Applied and published under the snapshot lock: a snapshot starts
        // either before the batch (and its older values are kept for it) or
        // after (and it sees the batch whole).
        let snapshots = inner.snapshots.lock().unwrap();
        let seq = inner.last_seq.load(Ordering::Relaxed) + 1;
        mem.apply(batch, seq, inner.newest_snapshot.load(Ordering::Relaxed));
        inner.last_seq.store(seq, Ordering::Release);
        drop(snapshots);
        Ok(())
    }

    /// A consistent view of the database as it is now: later writes don't
    /// show in it, however long it is kept. Dropping it lets the engine
    /// forget the old versions it needed.
    pub(crate) fn snapshot(&self) -> Snapshot {
        let sv = self.inner.super_version();
        let mut snapshots = self.inner.snapshots.lock().unwrap();
        let seq = self.inner.last_seq.load(Ordering::Acquire);
        // Kept sorted; a snapshot is nearly always the newest, so this is a push.
        let at = snapshots.partition_point(|&s| s <= seq);
        snapshots.insert(at, seq);
        self.inner.newest_snapshot.store(*snapshots.last().unwrap(), Ordering::Relaxed);
        drop(snapshots);
        Snapshot { view: ReadView { sv, seq: Some(seq) }, inner: self.inner.clone() }
    }

    pub fn get(&self, key: impl AsRef<[u8]>) -> Result<Option<Vec<u8>>> {
        ReadView { sv: self.inner.super_version(), seq: None }.get(key.as_ref())
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

    /// Calls `f` with each key and value in `range`, in key order, until it
    /// returns `false`. Unlike [`range`](Db::range), which copies every entry
    /// it yields, this lends them from the engine's own buffers, so a scan
    /// allocates nothing per entry.
    ///
    /// ```no_run
    /// # let db = spirodb::Db::open("./my-data")?;
    /// let mut bytes = 0;
    /// db.scan("a".."m", |key, value| {
    ///     bytes += key.len() + value.len();
    ///     true
    /// })?;
    /// # Ok::<(), spirodb::Error>(())
    /// ```
    pub fn scan<K: AsRef<[u8]>, R: RangeBounds<K>>(
        &self,
        range: R,
        mut f: impl FnMut(&[u8], &[u8]) -> bool,
    ) -> Result<()> {
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
        self.raw_range(start, end).for_each(|key, value| Ok(f(key, value)))
    }

    /// Like `range`, but lends each key and value instead of copying them.
    pub(crate) fn raw_range(&self, start: Bound<&[u8]>, end: Bound<Vec<u8>>) -> RawIter {
        self.read_view().raw_range(start, end)
    }

    /// Has every table file written from now on carry a column section,
    /// built by what `make` returns. Set once, by the SQL layer.
    pub(crate) fn set_section_builder(&self, make: SectionFactory, warm: SectionWarmer) {
        let _ = self.inner.sections.set((make, warm));
    }

    /// The current memtables and tables, held fixed so several threads can
    /// each read part of a range from the same set.
    pub(crate) fn read_view(&self) -> ReadView {
        ReadView { sv: self.inner.super_version(), seq: None }
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
    fn section_builder(&self) -> Option<Box<dyn SectionBuilder>> {
        self.sections.get().map(|(make, _)| make())
    }

    fn warm(&self, table: &Table) {
        if let Some((_, warm)) = self.sections.get() {
            warm(table);
        }
    }

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
            let table = write_memtable(&self.dir, number, imm, &self.opts, &self.cache, self.section_builder())?;
            self.warm(&table);
            Some(Arc::new(table))
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
                    let path = table_path(&self.dir, number);
                    builder = Some(TableBuilder::create(&path, number, &self.opts, self.section_builder())?);
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
        let table = Table::open(&table_path(&self.dir, meta.number), meta, self.cache.clone())?;
        self.warm(&table);
        Ok(Arc::new(table))
    }

    fn remove_tables(&self, numbers: &[u64]) {
        for &number in numbers {
            let _ = fs::remove_file(table_path(&self.dir, number));
        }
    }

    fn write_manifest(&self, version: &Version, log_number: u64) -> Result<()> {
        write_manifest(&self.dir, self.next_file.load(Ordering::SeqCst), log_number, version, self.opts.sync)
    }
}

/// Writes a non-empty memtable to table `number` and opens it.
fn write_memtable(
    dir: &Path,
    number: u64,
    mem: &MemTable,
    opts: &Options,
    cache: &Arc<BlockCache>,
    section: Option<Box<dyn SectionBuilder>>,
) -> Result<Table> {
    let path = table_path(dir, number);
    let mut builder = TableBuilder::create(&path, number, opts, section)?;
    mem.for_each(|key, value| builder.add(key, value))?;
    Table::open(&path, builder.finish()?, cache.clone())
}

fn write_manifest(dir: &Path, next_file: u64, log_number: u64, version: &Version, full: bool) -> Result<()> {
    let metas = |level: &[Arc<Table>]| level.iter().map(|t| t.meta.clone()).collect();
    Manifest { next_file, log_number, l0: metas(&version.l0), l1: metas(&version.l1) }.store(dir, full)
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

    /// Calls `f` with each live entry from here to the end of the range,
    /// until it returns `false` or an error. Much faster than stepping with
    /// `next`, as each layer of the merge loops over its own entries.
    pub fn for_each(&mut self, mut f: impl FnMut(&[u8], &[u8]) -> Result<bool>) -> Result<()> {
        if !self.valid {
            return self.status();
        }
        // The same range, as an exclusive limit: the smallest key above an
        // inclusive end is that key with a zero byte appended.
        let limit = match &self.end {
            Bound::Included(end) => Some([end.as_slice(), &[0]].concat()),
            Bound::Excluded(end) => Some(end.clone()),
            Bound::Unbounded => None,
        };
        let mut result = Ok(());
        self.it.run(limit.as_deref(), &mut |key, value| match value {
            None => true, // deleted
            Some(value) => f(key, value).unwrap_or_else(|e| {
                result = Err(e);
                false
            }),
        });
        self.settle();
        result?;
        self.status()
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn snapshots_hold_their_point_in_time() {
        let dir = TempDir::new("snapshots");
        let db = Db::open(dir.path()).unwrap();
        for i in 0..100 {
            db.put(format!("k{i:03}"), "old").unwrap();
        }
        let snapshot = db.snapshot();
        let mut batch = WriteBatch::new();
        for i in 0..100 {
            if i % 2 == 0 {
                batch.put(format!("k{i:03}"), "new");
            } else {
                batch.delete(format!("k{i:03}"));
            }
        }
        batch.put("k999", "added");
        db.write(&batch).unwrap();
        let count = |view: &ReadView| {
            let (mut n, mut old) = (0, 0);
            view.raw_range(Bound::Unbounded, Bound::Unbounded)
                .for_each(|_, v| {
                    n += 1;
                    old += usize::from(v == b"old");
                    Ok(true)
                })
                .unwrap();
            (n, old)
        };
        assert_eq!(count(snapshot.view()), (100, 100));
        assert_eq!(count(&db.read_view()), (51, 0));
        // A flush moves the newest versions to a table; the snapshot still
        // reads its own memtable, and the old values in it.
        db.flush().unwrap();
        assert_eq!(count(snapshot.view()), (100, 100));
        assert_eq!(snapshot.view().get(b"k001").unwrap(), Some(b"old".to_vec()));
        assert_eq!(snapshot.view().get(b"k999").unwrap(), None);
        assert_eq!(db.get(b"k999").unwrap(), Some(b"added".to_vec()));
        // A snapshot taken after sees the flushed state.
        let later = db.snapshot();
        assert_eq!(count(later.view()), (51, 0));
        drop(snapshot);
        assert_eq!(db.inner.newest_snapshot.load(Ordering::Relaxed), later.view().seq().unwrap());
        drop(later);
        assert_eq!(db.inner.newest_snapshot.load(Ordering::Relaxed), 0);
    }

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
    }

    fn key(i: u64) -> Vec<u8> {
        format!("k{i:06}").into_bytes()
    }

    /// Fills a database whose data is spread over the memtable, level-0
    /// tables and level 1, with overwrites and deletions everywhere, and a
    /// model of what it should hold.
    fn spread_out(dir: &TempDir) -> (Db, BTreeMap<Vec<u8>, Vec<u8>>) {
        let opts = Options {
            memtable_size: 32 << 10,
            block_size: 256,
            l0_compaction_trigger: 3,
            target_file_size: 64 << 10,
            ..Options::default()
        };
        let db = Db::open_with(dir.path(), opts).unwrap();
        let mut model = BTreeMap::new();
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for round in 0..6 {
            let mut batch = WriteBatch::new();
            for _ in 0..2000 {
                let k = key(rng.below(5000));
                if rng.below(6) == 0 {
                    batch.delete(&k);
                    model.remove(&k);
                } else {
                    let v = format!("v{round}-{}", rng.below(1000)).into_bytes();
                    batch.put(&k, &v);
                    model.insert(k, v);
                }
            }
            db.write(&batch).unwrap();
            if round < 5 {
                db.flush().unwrap();
            }
        }
        let stats = db.stats();
        assert!(stats.memtable_bytes > 0 && stats.l1_tables > 0, "{stats:?}");
        (db, model)
    }

    #[test]
    fn for_each_matches_stepping_and_the_model() {
        let dir = TempDir::new("for-each");
        let (db, model) = spread_out(&dir);
        let mut rng = Rng(42);
        for _ in 0..200 {
            let (a, b) = (rng.below(5200), rng.below(5200));
            let (lo, hi) = (key(a.min(b)), key(a.max(b)));
            let end = if rng.below(2) == 0 { Bound::Included(hi.clone()) } else { Bound::Excluded(hi.clone()) };
            let expected: Vec<_> = model
                .range::<Vec<u8>, _>((Bound::Included(lo.clone()), end.clone()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();

            // Stepping one entry at a time.
            let mut stepped = Vec::new();
            let mut it = db.raw_range(Bound::Included(&lo), end.clone());
            while it.valid() {
                stepped.push((it.key().to_vec(), it.value().to_vec()));
                it.next();
            }
            assert_eq!(stepped, expected);

            // Runs, stopping at a random point and carrying on from where the
            // run left the iterator.
            let stop_after = rng.below(expected.len() as u64 + 1) as usize;
            let mut ran = Vec::new();
            let mut it = db.raw_range(Bound::Included(&lo), end);
            it.for_each(|k, v| {
                ran.push((k.to_vec(), v.to_vec()));
                Ok(ran.len() < stop_after)
            })
            .unwrap();
            while it.valid() {
                ran.push((it.key().to_vec(), it.value().to_vec()));
                it.next();
            }
            assert_eq!(ran, expected);
        }
    }

    #[test]
    fn split_points_are_sorted_and_inside_the_range() {
        let dir = TempDir::new("split-points");
        let (db, _) = spread_out(&dir);
        let (lo, hi) = (Bound::Excluded(key(1000)), Bound::Excluded(key(4000)));
        let view = db.read_view();
        let points = view.split_points(&lo, &hi);
        assert!(points.len() > 10, "{}", points.len());
        assert!(points.windows(2).all(|w| w[0] < w[1]));
        assert!(points.iter().all(|p| p.as_ref() > key(1000).as_slice() && p.as_ref() < key(4000).as_slice()));
    }
}
