//! The memtable: recent writes, kept sorted in memory until flushed to a table.
//!
//! Each write batch has a sequence number, and every entry remembers the
//! batch that wrote it. A reader with a snapshot (the sequence number of the
//! last batch it may see) skips later writes, so a scan never sees part of a
//! batch. When a write replaces a value a snapshot may still need, the old
//! version is kept in `history` until the memtable is dropped.

use std::collections::HashMap;
use std::ops::Bound;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::batch::WriteBatch;
use crate::error::Result;
use crate::iter::KvIter;
use crate::keymap::KeyMap;

/// Approximate bookkeeping cost of one entry: its share of a B-tree node plus
/// two `Vec` headers.
const ENTRY_OVERHEAD: usize = 64;

/// About one in this many keys (a power of two) is kept as a sample of the
/// memtable's keys.
const SAMPLE_EVERY: u64 = 128;

/// Whether `key` is one of the sampled keys. The choice goes by a hash of
/// the key, not by the order of writes, which can be periodic (a row and an
/// index entry per insert) and would then sample only one kind of key.
fn sampled(key: &[u8]) -> bool {
    let mut h = key.len() as u64;
    for chunk in key.chunks(8) {
        let mut word = [0u8; 8];
        word[..chunk.len()].copy_from_slice(chunk);
        h = (h ^ u64::from_le_bytes(word)).wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(29);
    }
    h.wrapping_mul(0xbf58_476d_1ce4_e5b9) >> 32 & (SAMPLE_EVERY - 1) == 0
}

/// A replaced version: the number of the batch that wrote it, and the value
/// (`None` for a deletion).
type Version = (u64, Option<Vec<u8>>);

/// A `None` value is a tombstone: the key was deleted.
#[derive(Default)]
struct Map {
    /// The newest version of each key.
    keys: KeyMap,
    /// Versions replaced while a snapshot might need them, oldest first,
    /// each with the number of the batch that wrote it.
    history: HashMap<Vec<u8>, Vec<Version>>,
}

impl Map {
    /// The version of an entry a reader at snapshot `seq` sees: the newest
    /// written at or before it. `None` if every version here is newer.
    fn as_of<'a>(
        &'a self,
        key: &[u8],
        value: Option<&'a [u8]>,
        written: u64,
        seq: Option<u64>,
    ) -> Option<Option<&'a [u8]>> {
        match seq {
            Some(seq) if written > seq => {
                let older = self.history.get(key)?;
                older.iter().rev().find(|(w, _)| *w <= seq).map(|(_, v)| v.as_deref())
            }
            _ => Some(value),
        }
    }

    fn range_from(&self, from: Bound<&[u8]>, seq: Option<u64>) -> AsOf<'_> {
        AsOf { map: self, inner: self.keys.range_from(from), seq }
    }
}

/// The entries of a memtable from some key on, as a reader at a snapshot
/// sees them (the newest of each, without one).
pub struct AsOf<'a> {
    map: &'a Map,
    inner: crate::keymap::Iter<'a>,
    seq: Option<u64>,
}

impl<'a> Iterator for AsOf<'a> {
    type Item = (&'a [u8], Option<&'a [u8]>);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let (key, value, written) = self.inner.next()?;
            if let Some(value) = self.map.as_of(key, value, written, self.seq) {
                return Some((key, value));
            }
        }
    }
}

pub struct MemTable {
    map: RwLock<Map>,
    size: AtomicUsize,
    /// About one in `SAMPLE_EVERY` keys, in the order written. They show how
    /// the data spreads over the key space, for splitting a scan into even
    /// parts.
    samples: Mutex<Vec<Vec<u8>>>,
    /// The write-ahead log holding this memtable's writes.
    pub wal_number: u64,
}

impl MemTable {
    pub fn new(wal_number: u64) -> Self {
        MemTable { map: RwLock::default(), size: AtomicUsize::new(0), samples: Mutex::default(), wal_number }
    }

    /// Applies `batch` as write number `seq`. A value it replaces is kept
    /// if a snapshot at or after the replaced write's number, up to
    /// `newest_snapshot`, may still read it.
    pub fn apply(&self, batch: &WriteBatch, seq: u64, newest_snapshot: u64) {
        let mut guard = self.map.write().unwrap();
        let map = &mut *guard;
        let mut size = self.size.load(Ordering::Relaxed);
        for (key, value) in batch.iter() {
            size += value.map_or(0, <[u8]>::len);
            match map.keys.insert(key, value, seq, |written| written <= newest_snapshot && written < seq) {
                Some(old) => match old.kept {
                    // Kept for a snapshot: its bytes stay counted.
                    Some(kept) => {
                        size += ENTRY_OVERHEAD;
                        map.history.entry(key.to_vec()).or_default().push((old.seq, kept));
                    }
                    None => size -= old.len.unwrap_or(0),
                },
                None => {
                    size += ENTRY_OVERHEAD + key.len();
                    if sampled(key) {
                        self.samples.lock().unwrap().push(key.to_vec());
                    }
                }
            }
        }
        self.size.store(size, Ordering::Relaxed);
    }

    /// `None` if the key isn't here; `Some(None)` if it was deleted here.
    /// As a reader at snapshot `seq` sees the memtable (`None`: now).
    pub fn get_at(&self, key: &[u8], seq: Option<u64>) -> Option<Option<Vec<u8>>> {
        let map = self.map.read().unwrap();
        let (value, written) = map.keys.get(key)?;
        map.as_of(key, value, written, seq).map(|v| v.map(<[u8]>::to_vec))
    }

    pub fn approximate_size(&self) -> usize {
        self.size.load(Ordering::Relaxed)
    }

    pub fn is_empty(&self) -> bool {
        self.map.read().unwrap().keys.is_empty()
    }

    /// Visits every entry's newest version in key order, holding a read
    /// lock throughout.
    pub fn for_each(&self, mut f: impl FnMut(&[u8], Option<&[u8]>) -> Result<()>) -> Result<()> {
        for (key, value) in self.map.read().unwrap().range_from(Bound::Unbounded, None) {
            f(key, value)?;
        }
        Ok(())
    }

    /// Calls `f` with entries from `from` on, in key order, as of snapshot
    /// `seq`, under the read lock, until it returns `false`. Keep the visits
    /// short: writers wait.
    pub fn visit_range(&self, from: Bound<&[u8]>, seq: Option<u64>, mut f: impl FnMut(&[u8], Option<&[u8]>) -> bool) {
        let map = self.map.read().unwrap();
        for (key, value) in map.range_from(from, seq) {
            if !f(key, value) {
                break;
            }
        }
    }

    /// Runs `f` on the entries from `from` on, in key order, as of snapshot
    /// `seq`, holding the read lock meanwhile. Keep it short: writers wait.
    pub fn with_range<R>(&self, from: Bound<&[u8]>, seq: Option<u64>, f: impl FnOnce(AsOf<'_>) -> R) -> R {
        let map = self.map.read().unwrap();
        f(map.range_from(from, seq))
    }

    /// The sampled keys inside `(start, end)`, sorted.
    pub fn sample_keys(&self, start: &Bound<Vec<u8>>, end: &Bound<Vec<u8>>) -> Vec<Vec<u8>> {
        let after_start = |k: &[u8]| match start {
            Bound::Included(s) | Bound::Excluded(s) => k > s.as_slice(),
            Bound::Unbounded => true,
        };
        let before_end = |k: &[u8]| match end {
            Bound::Included(e) | Bound::Excluded(e) => k < e.as_slice(),
            Bound::Unbounded => true,
        };
        let mut keys: Vec<Vec<u8>> =
            self.samples.lock().unwrap().iter().filter(|k| after_start(k) && before_end(k)).cloned().collect();
        keys.sort_unstable();
        keys
    }

    /// Iterates entries with keys up to `end`, as of snapshot `seq`.
    pub fn iter(self: &Arc<Self>, end: Bound<Vec<u8>>, seq: Option<u64>) -> MemIter {
        MemIter {
            mem: self.clone(),
            seq,
            end,
            bytes: Vec::new(),
            entries: Vec::new(),
            batch_size: FIRST_BATCH,
            pos: 0,
            exhausted: true,
        }
    }
}

/// Entries copied by a seek. Short scans are common, so start small and
/// double with each refill, up to `MAX_BATCH`.
const FIRST_BATCH: usize = 2;
const MAX_BATCH: usize = 512;

/// Where one copied entry lies in `MemIter::bytes`.
#[derive(Clone, Copy)]
struct Copied {
    key: (u32, u32),
    /// `None` for a tombstone.
    value: Option<(u32, u32)>,
}

/// Iterates a memtable without holding its lock: entries are copied out a
/// batch at a time, so writers are never blocked for long. Writes that land
/// while iterating may or may not be observed.
pub struct MemIter {
    mem: Arc<MemTable>,
    /// The snapshot read at; `None` for the newest of everything.
    seq: Option<u64>,
    /// Entries past this are never copied out.
    end: Bound<Vec<u8>>,
    /// The copied keys and values, back to back. One buffer, reused for
    /// every batch, so a scan doesn't allocate per entry.
    bytes: Vec<u8>,
    entries: Vec<Copied>,
    batch_size: usize,
    pos: usize,
    exhausted: bool,
}

impl MemIter {
    fn fill(&mut self, from: Bound<&[u8]>) {
        self.bytes.clear();
        self.entries.clear();
        self.pos = 0;
        let end = match &self.end {
            Bound::Included(e) => Bound::Included(e.as_slice()),
            Bound::Excluded(e) => Bound::Excluded(e.as_slice()),
            Bound::Unbounded => Bound::Unbounded,
        };
        let before_end = |key: &[u8]| match end {
            Bound::Included(e) => key <= e,
            Bound::Excluded(e) => key < e,
            Bound::Unbounded => true,
        };
        {
            let map = self.mem.map.read().unwrap();
            for (key, value) in map.range_from(from, self.seq).take_while(|(k, _)| before_end(k)).take(self.batch_size)
            {
                let mut copy = |data: &[u8]| {
                    let start = self.bytes.len() as u32;
                    self.bytes.extend_from_slice(data);
                    (start, data.len() as u32)
                };
                let key = copy(key);
                let value = value.map(&mut copy);
                self.entries.push(Copied { key, value });
            }
        }
        self.exhausted = self.entries.len() < self.batch_size;
    }

    fn slice(&self, (start, len): (u32, u32)) -> &[u8] {
        &self.bytes[start as usize..(start + len) as usize]
    }
}

impl KvIter for MemIter {
    fn valid(&self) -> bool {
        self.pos < self.entries.len()
    }

    fn key(&self) -> &[u8] {
        self.slice(self.entries[self.pos].key)
    }

    fn value(&self) -> Option<&[u8]> {
        self.entries[self.pos].value.map(|v| self.slice(v))
    }

    fn next(&mut self) {
        self.pos += 1;
        if self.pos >= self.entries.len() && !self.exhausted {
            let last = self.entries.last().expect("a full batch is not empty").key;
            let last = self.slice(last).to_vec();
            self.batch_size = (self.batch_size * 2).min(MAX_BATCH);
            self.fill(Bound::Excluded(&last));
        }
    }

    fn seek(&mut self, target: &[u8]) {
        self.batch_size = FIRST_BATCH;
        self.fill(Bound::Included(target));
    }

    fn status(&self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_write_wins_and_deletes_leave_tombstones() {
        let mem = MemTable::new(1);
        let mut batch = WriteBatch::new();
        batch.put("a", "1").put("b", "2").put("a", "3").delete("b");
        mem.apply(&batch, 1, 0);
        assert_eq!(mem.get_at(b"a", None), Some(Some(b"3".to_vec())));
        assert_eq!(mem.get_at(b"b", None), Some(None));
        assert_eq!(mem.get_at(b"c", None), None);
    }

    #[test]
    fn iterator_crosses_batch_boundaries() {
        let mem = Arc::new(MemTable::new(1));
        let mut batch = WriteBatch::new();
        for i in 0..1000 {
            batch.put(format!("{i:04}"), "v");
        }
        mem.apply(&batch, 1, 0);
        let mut it = mem.iter(Bound::Unbounded, None);
        it.seek(b"0100");
        let mut seen = 0;
        while it.valid() {
            assert_eq!(it.key(), format!("{:04}", 100 + seen).as_bytes());
            seen += 1;
            it.next();
        }
        assert_eq!(seen, 900);

        // An end bound stops the copying, and a seek past it finds nothing.
        let mut bounded = mem.iter(Bound::Excluded(b"0105".to_vec()), None);
        bounded.seek(b"0100");
        let mut keys = Vec::new();
        while bounded.valid() {
            keys.push(bounded.key().to_vec());
            bounded.next();
        }
        assert_eq!(keys.len(), 5);
        bounded.seek(b"0200");
        assert!(!bounded.valid());
    }

    #[test]
    fn snapshots_see_the_versions_of_their_time() {
        let mem = Arc::new(MemTable::new(1));
        let write = |seq: u64, newest_snapshot: u64, ops: &[(&str, Option<&str>)]| {
            let mut batch = WriteBatch::new();
            for (k, v) in ops {
                match v {
                    Some(v) => batch.put(k, v),
                    None => batch.delete(k),
                };
            }
            mem.apply(&batch, seq, newest_snapshot);
        };
        write(1, 0, &[("a", Some("a1")), ("b", Some("b1"))]);
        // A snapshot at 1 is open while these happen.
        write(2, 1, &[("a", Some("a2")), ("c", Some("c2"))]);
        write(3, 1, &[("a", Some("a3")), ("b", None)]);
        let at = |seq: Option<u64>, key: &str| {
            mem.get_at(key.as_bytes(), seq).map(|v| v.map(|v| String::from_utf8(v).unwrap()))
        };
        assert_eq!(at(Some(1), "a"), Some(Some("a1".into())));
        assert_eq!(at(Some(1), "b"), Some(Some("b1".into())));
        assert_eq!(at(Some(1), "c"), None); // not written yet: look further down
        assert_eq!(at(None, "a"), Some(Some("a3".into())));
        assert_eq!(at(None, "b"), Some(None));
        let scan = |seq| {
            let mut seen = Vec::new();
            mem.visit_range(Bound::Unbounded, seq, |k, v| {
                seen.push((String::from_utf8(k.to_vec()).unwrap(), v.map(|v| String::from_utf8(v.to_vec()).unwrap())));
                true
            });
            seen
        };
        assert_eq!(scan(Some(1)), vec![("a".into(), Some("a1".into())), ("b".into(), Some("b1".into()))]);
        assert_eq!(
            scan(None),
            vec![("a".into(), Some("a3".into())), ("b".into(), None), ("c".into(), Some("c2".into()))]
        );
        let mut it = mem.iter(Bound::Unbounded, Some(1));
        it.seek(b"");
        let mut keys = Vec::new();
        while it.valid() {
            keys.push(it.key().to_vec());
            it.next();
        }
        assert_eq!(keys, vec![b"a".to_vec(), b"b".to_vec()]);
    }
}
