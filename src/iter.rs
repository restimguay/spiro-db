//! Iterator plumbing shared by range scans and compaction.

use std::sync::Arc;

use crate::error::Result;
use crate::table::{Table, TableIter};

/// A cursor over sorted entries, tombstones included.
pub trait KvIter: Send {
    fn valid(&self) -> bool;
    /// The current key. Only call while `valid()`.
    fn key(&self) -> &[u8];
    /// The current value, or `None` for a tombstone. Only call while `valid()`.
    fn value(&self) -> Option<&[u8]>;
    fn next(&mut self);
    /// Positions at the first entry whose key is >= `target`.
    fn seek(&mut self, target: &[u8]);
    /// The error, if one cut the iteration short.
    fn status(&self) -> Result<()>;
}

/// Iterates level 1, a sorted run of non-overlapping tables, as one sequence,
/// opening each table only when the iteration reaches it.
pub struct LevelIter {
    tables: Vec<Arc<Table>>,
    idx: usize,
    cur: Option<TableIter>,
    fill_cache: bool,
}

impl LevelIter {
    pub fn new(tables: Vec<Arc<Table>>, fill_cache: bool) -> Self {
        LevelIter { tables, idx: 0, cur: None, fill_cache }
    }

    fn open(&mut self, target: &[u8]) {
        self.cur = self.tables.get(self.idx).map(|t| {
            let mut it = t.iter(self.fill_cache);
            it.seek(target);
            it
        });
    }

    fn skip_exhausted_tables(&mut self) {
        while let Some(cur) = &self.cur {
            if cur.valid() || cur.status().is_err() {
                return;
            }
            self.idx += 1;
            self.open(&[]);
        }
    }

    fn cur(&self) -> &TableIter {
        self.cur.as_ref().expect("iterator is valid")
    }
}

impl KvIter for LevelIter {
    fn valid(&self) -> bool {
        self.cur.as_ref().is_some_and(KvIter::valid)
    }

    fn key(&self) -> &[u8] {
        self.cur().key()
    }

    fn value(&self) -> Option<&[u8]> {
        self.cur().value()
    }

    fn next(&mut self) {
        if let Some(cur) = &mut self.cur {
            cur.next();
        }
        self.skip_exhausted_tables();
    }

    fn seek(&mut self, target: &[u8]) {
        self.idx = self.tables.partition_point(|t| t.meta.largest.as_slice() < target);
        self.open(target);
        self.skip_exhausted_tables();
    }

    fn status(&self) -> Result<()> {
        self.cur.as_ref().map_or(Ok(()), KvIter::status)
    }
}

/// Merges sorted sources into one sorted stream. Sources are ordered newest
/// first; when several hold the same key, only the newest entry surfaces.
pub struct MergeIter {
    sources: Vec<Box<dyn KvIter>>,
    current: Option<usize>,
    /// The source with the smallest key after the current one. Until the
    /// current source passes it, no other source needs looking at.
    runner_up: Option<usize>,
}

impl MergeIter {
    pub fn new(sources: Vec<Box<dyn KvIter>>) -> Self {
        MergeIter { sources, current: None, runner_up: None }
    }

    fn find_current(&mut self) {
        // There are only a handful of sources, so a linear scan beats a heap.
        self.current = None;
        self.runner_up = None;
        for (i, s) in self.sources.iter().enumerate() {
            if !s.valid() {
                // A source only stops on an error by becoming invalid. Never
                // skip past one: that would silently drop its keys.
                if s.status().is_err() {
                    self.current = None;
                    self.runner_up = None;
                    return;
                }
                continue;
            }
            match self.current {
                Some(c) if s.key() >= self.sources[c].key() => {
                    if self.runner_up.is_none_or(|r| s.key() < self.sources[r].key()) {
                        self.runner_up = Some(i);
                    }
                }
                _ => {
                    self.runner_up = self.current;
                    self.current = Some(i);
                }
            }
        }
    }

    fn cur(&self) -> &dyn KvIter {
        &*self.sources[self.current.expect("iterator is valid")]
    }
}

impl KvIter for MergeIter {
    fn valid(&self) -> bool {
        self.current.is_some()
    }

    fn key(&self) -> &[u8] {
        self.cur().key()
    }

    fn value(&self) -> Option<&[u8]> {
        self.cur().value()
    }

    fn next(&mut self) {
        let Some(c) = self.current else { return };
        let shadowing = self.runner_up.is_some_and(|r| self.sources[r].key() == self.sources[c].key());
        if shadowing {
            // Newer sources can't hold the current key (it would have won the
            // tie), but older ones can: their entries are shadowed, so step
            // past them. This only happens when the runner-up ties.
            let (newer_and_current, older) = self.sources.split_at_mut(c + 1);
            let cur = &mut newer_and_current[c];
            for s in older {
                if s.valid() && s.key() == cur.key() {
                    s.next();
                }
            }
        }
        self.sources[c].next();
        // Fast path: the current source still holds the smallest key, so the
        // others (unchanged since the last full look) can be left alone.
        let cur = &self.sources[c];
        if !shadowing && cur.valid() && self.runner_up.is_none_or(|r| cur.key() < self.sources[r].key()) {
            return;
        }
        self.find_current();
    }

    fn seek(&mut self, target: &[u8]) {
        for s in &mut self.sources {
            s.seek(target);
        }
        self.find_current();
    }

    fn status(&self) -> Result<()> {
        self.sources.iter().try_for_each(|s| s.status())
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Bound;

    use super::*;
    use crate::batch::WriteBatch;
    use crate::memtable::MemTable;

    fn memtable(ops: &[(&str, Option<&str>)]) -> Box<dyn KvIter> {
        let mem = Arc::new(MemTable::new(0));
        let mut batch = WriteBatch::new();
        for &(k, v) in ops {
            match v {
                Some(v) => batch.put(k, v),
                None => batch.delete(k),
            };
        }
        mem.apply(&batch);
        Box::new(mem.iter(Bound::Unbounded))
    }

    #[test]
    fn merge_matches_a_model_across_overlapping_sources() {
        use std::collections::BTreeMap;
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut rand = |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n
        };
        // Four overlapping sources, newest first, full of shadowed keys and tombstones.
        let mut model: BTreeMap<Vec<u8>, Option<Vec<u8>>> = BTreeMap::new();
        let mut sources = Vec::new();
        for source in 0..4u64 {
            let mem = Arc::new(MemTable::new(0));
            let mut batch = WriteBatch::new();
            for _ in 0..300 {
                let key = format!("{:04}", rand(500)).into_bytes();
                if rand(5) == 0 {
                    batch.delete(&key);
                } else {
                    batch.put(&key, format!("s{source}").as_bytes());
                }
            }
            mem.apply(&batch);
            // Older sources only fill in keys no newer source has.
            let map: Vec<_> = {
                let mut it = mem.iter(Bound::Unbounded);
                it.seek(b"");
                let mut entries = Vec::new();
                while it.valid() {
                    entries.push((it.key().to_vec(), it.value().map(<[u8]>::to_vec)));
                    it.next();
                }
                entries
            };
            for (k, v) in map {
                model.entry(k).or_insert(v);
            }
            sources.push(Box::new(mem.iter(Bound::Unbounded)) as Box<dyn KvIter>);
        }
        let mut merged = MergeIter::new(sources);
        for _ in 0..50 {
            let start = format!("{:04}", rand(520)).into_bytes();
            merged.seek(&start);
            for (k, v) in model.range(start..).take(rand(200) as usize) {
                assert!(merged.valid());
                assert_eq!((merged.key(), merged.value()), (k.as_slice(), v.as_deref()));
                merged.next();
            }
        }
    }

    #[test]
    fn newest_source_wins() {
        let newer = memtable(&[("b", Some("new")), ("c", None)]);
        let older = memtable(&[("a", Some("1")), ("b", Some("old")), ("c", Some("3")), ("d", Some("4"))]);
        let mut it = MergeIter::new(vec![newer, older]);
        it.seek(b"");
        let mut out = Vec::new();
        while it.valid() {
            out.push((
                String::from_utf8(it.key().to_vec()).unwrap(),
                it.value().map(|v| String::from_utf8(v.to_vec()).unwrap()),
            ));
            it.next();
        }
        let expected = [("a", Some("1")), ("b", Some("new")), ("c", None), ("d", Some("4"))];
        assert_eq!(out, expected.map(|(k, v)| (k.to_string(), v.map(str::to_string))));
    }
}
