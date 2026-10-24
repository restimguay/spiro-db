//! Sorted key/value blocks, the unit of disk reads and caching inside a table.
//!
//! Entry:   [shared: varint][unshared: varint][value tag: varint][key suffix][value]
//!          The value tag is 0 for a tombstone, otherwise the value length + 1.
//! Trailer: [restart offset: u32]... [restart count: u32]
//!
//! Each key is stored as the length it shares with the previous key plus the
//! rest. Every 16 entries a *restart point* stores the full key, so a lookup
//! binary-searches the restart points and then scans at most 16 entries.

use std::ops::Deref;
use std::sync::Arc;

use crate::coding::{get_varint, put_varint, u32_at};
use crate::error::{Result, corruption};

const RESTART_INTERVAL: usize = 16;

pub struct BlockBuilder {
    buf: Vec<u8>,
    restarts: Vec<u32>,
    counter: usize,
    last_key: Vec<u8>,
}

impl BlockBuilder {
    pub fn new() -> Self {
        BlockBuilder { buf: Vec::new(), restarts: vec![0], counter: 0, last_key: Vec::new() }
    }

    /// Adds an entry; keys must arrive in strictly increasing order.
    pub fn add(&mut self, key: &[u8], value: Option<&[u8]>) {
        let shared = if self.counter == RESTART_INTERVAL {
            self.restarts.push(self.buf.len() as u32);
            self.counter = 0;
            0
        } else {
            self.last_key.iter().zip(key).take_while(|(a, b)| a == b).count()
        };
        put_varint(&mut self.buf, shared as u64);
        put_varint(&mut self.buf, (key.len() - shared) as u64);
        put_varint(&mut self.buf, value.map_or(0, |v| v.len() as u64 + 1));
        self.buf.extend_from_slice(&key[shared..]);
        if let Some(v) = value {
            self.buf.extend_from_slice(v);
        }
        self.last_key.truncate(shared);
        self.last_key.extend_from_slice(&key[shared..]);
        self.counter += 1;
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn size_estimate(&self) -> usize {
        self.buf.len() + 4 * self.restarts.len() + 4
    }

    /// Returns the finished block and resets the builder for the next one.
    pub fn finish(&mut self) -> Vec<u8> {
        for r in &self.restarts {
            self.buf.extend_from_slice(&r.to_le_bytes());
        }
        self.buf.extend_from_slice(&(self.restarts.len() as u32).to_le_bytes());
        self.restarts.clear();
        self.restarts.push(0);
        self.counter = 0;
        self.last_key.clear();
        std::mem::take(&mut self.buf)
    }
}

pub struct Block {
    data: Vec<u8>,
    restarts_offset: usize,
    num_restarts: usize,
}

impl Block {
    pub fn new(data: Vec<u8>) -> Result<Block> {
        if data.len() < 4 {
            return Err(corruption("block too short"));
        }
        let num_restarts = u32_at(&data, data.len() - 4) as usize;
        let trailer = num_restarts
            .checked_mul(4)
            .and_then(|n| n.checked_add(4))
            .filter(|&n| num_restarts > 0 && n <= data.len())
            .ok_or_else(|| corruption("bad block restart array"))?;
        Ok(Block { restarts_offset: data.len() - trailer, num_restarts, data })
    }

    /// Bytes of memory the block occupies.
    pub fn size(&self) -> usize {
        self.data.len()
    }

    fn entries(&self) -> &[u8] {
        &self.data[..self.restarts_offset]
    }

    fn restart_point(&self, i: usize) -> usize {
        u32_at(&self.data, self.restarts_offset + 4 * i) as usize
    }
}

struct Entry {
    shared: usize,
    key_start: usize,
    key_len: usize,
    value: Option<(usize, usize)>,
    end: usize,
}

fn decode_entry(data: &[u8], mut pos: usize) -> Option<Entry> {
    if pos >= data.len() {
        return None;
    }
    let mut varint = || {
        let (v, n) = get_varint(&data[pos..])?;
        pos += n;
        usize::try_from(v).ok()
    };
    let (shared, key_len, tag) = (varint()?, varint()?, varint()?);
    let key_start = pos;
    let value_start = key_start.checked_add(key_len)?;
    let (value, end) = match tag {
        0 => (None, value_start),
        tag => (Some((value_start, tag - 1)), value_start.checked_add(tag - 1)?),
    };
    (end <= data.len()).then_some(Entry { shared, key_start, key_len, value, end })
}

/// A cursor over the entries of one block. It can own the block (`Arc`) or,
/// for short lookups, borrow it and skip the refcount traffic.
pub struct BlockIter<B: Deref<Target = Block> = Arc<Block>> {
    block: B,
    key: Vec<u8>,
    value: Option<(usize, usize)>,
    next: usize,
    valid: bool,
}

impl<B: Deref<Target = Block>> BlockIter<B> {
    pub fn new(block: B) -> Self {
        BlockIter { block, key: Vec::new(), value: None, next: 0, valid: false }
    }

    pub fn valid(&self) -> bool {
        self.valid
    }

    pub fn key(&self) -> &[u8] {
        &self.key
    }

    pub fn value(&self) -> Option<&[u8]> {
        self.value.map(|(start, len)| &self.block.data[start..start + len])
    }

    pub fn seek_to_first(&mut self) {
        self.key.clear();
        self.next = 0;
        self.advance();
    }

    /// Positions at the first entry whose key is >= `target`.
    pub fn seek(&mut self, target: &[u8]) {
        // Find the last restart point whose key is < target, then scan forward.
        let (mut lo, mut hi) = (0, self.block.num_restarts - 1);
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            match self.restart_key(mid) {
                Some(key) if key < target => lo = mid,
                _ => hi = mid - 1,
            }
        }
        self.key.clear();
        self.next = self.block.restart_point(lo);
        loop {
            self.advance();
            if !self.valid || self.key.as_slice() >= target {
                return;
            }
        }
    }

    pub fn advance(&mut self) {
        let entries = self.block.entries();
        match decode_entry(entries, self.next) {
            Some(e) if e.shared <= self.key.len() => {
                self.key.truncate(e.shared);
                self.key.extend_from_slice(&entries[e.key_start..e.key_start + e.key_len]);
                self.value = e.value;
                self.next = e.end;
                self.valid = true;
            }
            _ => self.valid = false,
        }
    }

    fn restart_key(&self, i: usize) -> Option<&[u8]> {
        let entries = self.block.entries();
        let e = decode_entry(entries, self.block.restart_point(i))?;
        (e.shared == 0).then(|| &entries[e.key_start..e.key_start + e.key_len])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(n: usize) -> Arc<Block> {
        let mut b = BlockBuilder::new();
        for i in 0..n {
            let key = format!("key{i:05}");
            if i % 7 == 0 {
                b.add(key.as_bytes(), None);
            } else {
                b.add(key.as_bytes(), Some(format!("value{i}").as_bytes()));
            }
        }
        Arc::new(Block::new(b.finish()).unwrap())
    }

    #[test]
    fn iterates_in_order_with_tombstones() {
        let mut it = BlockIter::new(build(100));
        it.seek_to_first();
        for i in 0..100 {
            assert!(it.valid());
            assert_eq!(it.key(), format!("key{i:05}").as_bytes());
            let expected = (i % 7 != 0).then(|| format!("value{i}").into_bytes());
            assert_eq!(it.value().map(<[u8]>::to_vec), expected);
            it.advance();
        }
        assert!(!it.valid());
    }

    #[test]
    fn seek_finds_first_key_at_or_after_target() {
        let mut it = BlockIter::new(build(100));
        for i in 0..100 {
            it.seek(format!("key{i:05}").as_bytes());
            assert_eq!(it.key(), format!("key{i:05}").as_bytes());
        }
        it.seek(b"key00042x");
        assert_eq!(it.key(), b"key00043");
        it.seek(b"");
        assert_eq!(it.key(), b"key00000");
        it.seek(b"zzz");
        assert!(!it.valid());
    }

    #[test]
    fn rejects_garbage() {
        assert!(Block::new(vec![1, 2]).is_err());
        assert!(Block::new(vec![0, 0, 0, 0]).is_err());
        assert!(Block::new(vec![0xff, 0xff, 0xff, 0x7f]).is_err());
    }
}
