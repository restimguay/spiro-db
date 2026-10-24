//! The memtable: recent writes, kept sorted in memory until flushed to a table.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use crate::batch::WriteBatch;
use crate::error::Result;
use crate::iter::KvIter;

/// Approximate bookkeeping cost of one entry: its share of a B-tree node plus
/// two `Vec` headers.
const ENTRY_OVERHEAD: usize = 64;

/// A `None` value is a tombstone: the key was deleted.
type Map = BTreeMap<Vec<u8>, Option<Vec<u8>>>;

pub struct MemTable {
    map: RwLock<Map>,
    size: AtomicUsize,
    /// The write-ahead log holding this memtable's writes.
    pub wal_number: u64,
}

impl MemTable {
    pub fn new(wal_number: u64) -> Self {
        MemTable { map: RwLock::default(), size: AtomicUsize::new(0), wal_number }
    }

    pub fn apply(&self, batch: &WriteBatch) {
        let mut map = self.map.write().unwrap();
        let mut size = self.size.load(Ordering::Relaxed);
        for (key, value) in batch.iter() {
            let value = value.map(<[u8]>::to_vec);
            size += value.as_ref().map_or(0, Vec::len);
            if let Some(slot) = map.get_mut(key) {
                size -= slot.as_ref().map_or(0, Vec::len);
                *slot = value;
            } else {
                size += ENTRY_OVERHEAD + key.len();
                map.insert(key.to_vec(), value);
            }
        }
        self.size.store(size, Ordering::Relaxed);
    }

    /// `None` if the key isn't here; `Some(None)` if it was deleted here.
    pub fn get(&self, key: &[u8]) -> Option<Option<Vec<u8>>> {
        self.map.read().unwrap().get(key).cloned()
    }

    pub fn approximate_size(&self) -> usize {
        self.size.load(Ordering::Relaxed)
    }

    pub fn is_empty(&self) -> bool {
        self.map.read().unwrap().is_empty()
    }

    /// Visits every entry in key order, holding a read lock throughout.
    pub fn for_each(&self, mut f: impl FnMut(&[u8], Option<&[u8]>) -> Result<()>) -> Result<()> {
        for (key, value) in self.map.read().unwrap().iter() {
            f(key, value.as_deref())?;
        }
        Ok(())
    }

    /// Iterates entries with keys up to `end`.
    pub fn iter(self: &Arc<Self>, end: Bound<Vec<u8>>) -> MemIter {
        MemIter { mem: self.clone(), end, batch: Vec::new(), batch_size: FIRST_BATCH, pos: 0, exhausted: true }
    }
}

/// Entries copied by a seek. Short scans are common, so start small and
/// double with each refill, up to `MAX_BATCH`.
const FIRST_BATCH: usize = 2;
const MAX_BATCH: usize = 128;

/// Iterates a memtable without holding its lock: entries are copied out a
/// batch at a time, so writers are never blocked for long. Writes that land
/// while iterating may or may not be observed.
pub struct MemIter {
    mem: Arc<MemTable>,
    /// Entries past this are never copied out.
    end: Bound<Vec<u8>>,
    batch: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    batch_size: usize,
    pos: usize,
    exhausted: bool,
}

impl MemIter {
    fn fill(&mut self, from: Bound<&[u8]>) {
        self.batch.clear();
        self.pos = 0;
        let end = match &self.end {
            Bound::Included(e) => Bound::Included(e.as_slice()),
            Bound::Excluded(e) => Bound::Excluded(e.as_slice()),
            Bound::Unbounded => Bound::Unbounded,
        };
        // `BTreeMap::range` panics on an inverted range; that's just an empty one.
        let empty = match (from, end) {
            (Bound::Included(f), Bound::Included(e)) => f > e,
            (Bound::Included(f) | Bound::Excluded(f), Bound::Included(e) | Bound::Excluded(e)) => f >= e,
            _ => false,
        };
        if !empty {
            let map = self.mem.map.read().unwrap();
            let entries = map.range::<[u8], _>((from, end)).take(self.batch_size);
            self.batch.extend(entries.map(|(k, v)| (k.clone(), v.clone())));
        }
        self.exhausted = self.batch.len() < self.batch_size;
    }
}

impl KvIter for MemIter {
    fn valid(&self) -> bool {
        self.pos < self.batch.len()
    }

    fn key(&self) -> &[u8] {
        &self.batch[self.pos].0
    }

    fn value(&self) -> Option<&[u8]> {
        self.batch[self.pos].1.as_deref()
    }

    fn next(&mut self) {
        self.pos += 1;
        if self.pos >= self.batch.len() && !self.exhausted {
            let (last, _) = self.batch.pop().expect("a full batch is not empty");
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
        mem.apply(&batch);
        assert_eq!(mem.get(b"a"), Some(Some(b"3".to_vec())));
        assert_eq!(mem.get(b"b"), Some(None));
        assert_eq!(mem.get(b"c"), None);
    }

    #[test]
    fn iterator_crosses_batch_boundaries() {
        let mem = Arc::new(MemTable::new(1));
        let mut batch = WriteBatch::new();
        for i in 0..1000 {
            batch.put(format!("{i:04}"), "v");
        }
        mem.apply(&batch);
        let mut it = mem.iter(Bound::Unbounded);
        it.seek(b"0100");
        let mut seen = 0;
        while it.valid() {
            assert_eq!(it.key(), format!("{:04}", 100 + seen).as_bytes());
            seen += 1;
            it.next();
        }
        assert_eq!(seen, 900);

        // An end bound stops the copying, and a seek past it finds nothing.
        let mut bounded = mem.iter(Bound::Excluded(b"0105".to_vec()));
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
}
