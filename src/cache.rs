//! A sharded block cache with CLOCK (second-chance) eviction.
//!
//! Blocks read since they were last considered for eviction get another lap
//! around the clock, so a hot working set survives a one-off scan of cold data.
//! Lookups lock only one of 64 shards.

use std::collections::{HashMap, VecDeque};
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::{Arc, Mutex};

use crate::block::Block;

const SHARDS: usize = 64;

/// (table number, block offset)
type Key = (u64, u64);

pub struct BlockCache {
    shards: Vec<Mutex<Shard>>,
    shard_capacity: usize,
}

#[derive(Default)]
struct Shard {
    map: HashMap<Key, Slot, BuildHasherDefault<KeyHasher>>,
    clock: VecDeque<Key>,
    used: usize,
}

/// Block keys are two integers, so a multiply-and-mix hash serves far
/// faster than the default SipHash, which defends against attacker-chosen keys.
#[derive(Default)]
struct KeyHasher(u64);

impl Hasher for KeyHasher {
    fn write(&mut self, bytes: &[u8]) {
        bytes.iter().for_each(|&b| self.write_u64(b.into()));
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0 ^ n).wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(31);
    }

    fn finish(&self) -> u64 {
        // SplitMix64's finalizer, so both high and low bits are well mixed.
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

struct Slot {
    block: Arc<Block>,
    referenced: bool,
}

impl BlockCache {
    pub fn new(capacity: usize) -> Self {
        BlockCache { shards: (0..SHARDS).map(|_| Mutex::default()).collect(), shard_capacity: capacity / SHARDS }
    }

    fn shard(&self, key: Key) -> &Mutex<Shard> {
        let h = (key.0 ^ key.1.rotate_left(32)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        &self.shards[(h >> (64 - SHARDS.trailing_zeros())) as usize]
    }

    pub fn get(&self, table: u64, offset: u64) -> Option<Arc<Block>> {
        let key = (table, offset);
        let mut shard = self.shard(key).lock().unwrap();
        let slot = shard.map.get_mut(&key)?;
        if !slot.referenced {
            slot.referenced = true;
        }
        Some(slot.block.clone())
    }

    pub fn insert(&self, table: u64, offset: u64, block: Arc<Block>) {
        if self.shard_capacity == 0 {
            return;
        }
        let key = (table, offset);
        let mut guard = self.shard(key).lock().unwrap();
        let shard = &mut *guard;
        if shard.map.contains_key(&key) {
            return;
        }
        shard.used += block.size();
        shard.map.insert(key, Slot { block, referenced: false });
        shard.clock.push_back(key);
        while shard.used > self.shard_capacity
            && let Some(victim) = shard.clock.pop_front()
        {
            let slot = shard.map.get_mut(&victim).expect("clock and map agree");
            if slot.referenced {
                slot.referenced = false;
                shard.clock.push_back(victim);
            } else {
                shard.used -= slot.block.size();
                shard.map.remove(&victim);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::BlockBuilder;

    fn block(len: usize) -> Arc<Block> {
        let mut b = BlockBuilder::new();
        b.add(b"k", Some(&vec![0u8; len]));
        Arc::new(Block::new(b.finish()).unwrap())
    }

    #[test]
    fn evicts_unreferenced_blocks_first() {
        // One shard's worth of capacity: room for about three 1 KiB blocks.
        let cache = BlockCache::new(SHARDS * 3200);
        let same_shard: Vec<u64> = (0..)
            .map(|i| i * 4096)
            .filter(|&off| std::ptr::eq(cache.shard((1, off)), cache.shard((1, 0))))
            .take(4)
            .collect();
        for &off in &same_shard[..3] {
            cache.insert(1, off, block(1000));
        }
        assert!(cache.get(1, same_shard[0]).is_some()); // mark the first block hot
        cache.insert(1, same_shard[3], block(1000));
        assert!(cache.get(1, same_shard[0]).is_some(), "hot block survives");
        assert!(cache.get(1, same_shard[1]).is_none(), "cold block is evicted");
        assert!(cache.get(1, same_shard[3]).is_some());
    }

    #[test]
    fn zero_capacity_caches_nothing() {
        let cache = BlockCache::new(0);
        cache.insert(1, 0, block(10));
        assert!(cache.get(1, 0).is_none());
    }
}
