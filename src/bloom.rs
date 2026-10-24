//! Bloom filters let a table answer "definitely not here" without touching disk.
//!
//! Layout: [bit array][probe count k: u8]. All k probes are derived from one
//! 64-bit hash by double hashing (Kirsch–Mitzenmacher), so each key is hashed once.

/// Collects key hashes while a table is written, then builds its filter.
pub struct BloomBuilder {
    bits_per_key: usize,
    hashes: Vec<u64>,
}

impl BloomBuilder {
    pub fn new(bits_per_key: usize) -> Self {
        BloomBuilder { bits_per_key, hashes: Vec::new() }
    }

    pub fn add(&mut self, key: &[u8]) {
        self.hashes.push(hash64(key));
    }

    pub fn finish(&self) -> Vec<u8> {
        if self.bits_per_key == 0 {
            return Vec::new();
        }
        // k = bits_per_key * ln 2 minimises the false-positive rate.
        let k = (self.bits_per_key as f64 * std::f64::consts::LN_2).round().clamp(1.0, 30.0) as u8;
        let bytes = (self.hashes.len() * self.bits_per_key).max(64).div_ceil(8);
        let nbits = bytes as u64 * 8;
        let mut filter = vec![0u8; bytes + 1];
        for &h in &self.hashes {
            let delta = h.rotate_right(33);
            let mut x = h;
            for _ in 0..k {
                let bit = x % nbits;
                filter[(bit / 8) as usize] |= 1 << (bit % 8);
                x = x.wrapping_add(delta);
            }
        }
        filter[bytes] = k;
        filter
    }
}

/// False means `key` was certainly never added; true means it probably was.
pub fn may_contain(filter: &[u8], key: &[u8]) -> bool {
    let Some((&k, bits)) = filter.split_last() else { return true };
    if bits.is_empty() {
        return true;
    }
    let nbits = bits.len() as u64 * 8;
    let h = hash64(key);
    let delta = h.rotate_right(33);
    let mut x = h;
    for _ in 0..k {
        let bit = x % nbits;
        if bits[(bit / 8) as usize] & (1 << (bit % 8)) == 0 {
            return false;
        }
        x = x.wrapping_add(delta);
    }
    true
}

/// MurmurHash64A: fast, with good avalanche behaviour for short keys.
fn hash64(data: &[u8]) -> u64 {
    const M: u64 = 0xc6a4_a793_5bd1_e995;
    const R: u32 = 47;
    const SEED: u64 = 0x5350_4952_4f44_4221;

    let mut h = SEED ^ (data.len() as u64).wrapping_mul(M);
    let (chunks, rest) = data.as_chunks::<8>();
    for &c in chunks {
        let mut k = u64::from_le_bytes(c).wrapping_mul(M);
        k ^= k >> R;
        h = (h ^ k.wrapping_mul(M)).wrapping_mul(M);
    }
    if !rest.is_empty() {
        let mut tail = [0u8; 8];
        tail[..rest.len()].copy_from_slice(rest);
        h = (h ^ u64::from_le_bytes(tail)).wrapping_mul(M);
    }
    h ^= h >> R;
    h = h.wrapping_mul(M);
    h ^ (h >> R)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(keys: impl Iterator<Item = Vec<u8>>, bits_per_key: usize) -> Vec<u8> {
        let mut b = BloomBuilder::new(bits_per_key);
        keys.for_each(|k| b.add(&k));
        b.finish()
    }

    #[test]
    fn no_false_negatives_and_few_false_positives() {
        let filter = build((0..10_000).map(|i| format!("key{i}").into_bytes()), 10);
        assert!((0..10_000).all(|i| may_contain(&filter, format!("key{i}").as_bytes())));
        let false_positives = (0..10_000).filter(|i| may_contain(&filter, format!("other{i}").as_bytes())).count();
        assert!(false_positives < 200, "{false_positives} false positives in 10000");
    }

    #[test]
    fn empty_and_disabled_filters_admit_everything() {
        assert!(may_contain(&[], b"anything"));
        assert!(may_contain(&build(std::iter::empty(), 0), b"anything"));
    }
}
