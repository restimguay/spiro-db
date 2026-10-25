//! Byte encodings: little-endian fixed-width integers and LEB128 varints.

use crate::error::{Result, corruption};

pub fn put_varint(buf: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        buf.push(v as u8 | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

/// Appends `bytes` preceded by its length as a varint.
pub fn put_bytes(buf: &mut Vec<u8>, bytes: &[u8]) {
    put_varint(buf, bytes.len() as u64);
    buf.extend_from_slice(bytes);
}

/// Decodes a varint from the front of `buf`, returning it and its encoded length.
/// Compares byte strings eight bytes at a time. Keys are short, so this
/// beats a call to `memcmp` in the loops that compare every entry.
#[inline]
pub fn key_cmp(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    let common = a.len().min(b.len());
    let mut i = 0;
    while i + 8 <= common {
        let x = u64::from_be_bytes(a[i..i + 8].try_into().unwrap());
        let y = u64::from_be_bytes(b[i..i + 8].try_into().unwrap());
        if x != y {
            return x.cmp(&y);
        }
        i += 8;
    }
    if i < common {
        if common >= 8 {
            // The last eight bytes in common. Those before `i` are already
            // known equal, so the first difference still decides.
            let x = u64::from_be_bytes(a[common - 8..common].try_into().unwrap());
            let y = u64::from_be_bytes(b[common - 8..common].try_into().unwrap());
            if x != y {
                return x.cmp(&y);
            }
        } else if let Some(j) = (i..common).find(|&j| a[j] != b[j]) {
            return a[j].cmp(&b[j]);
        }
    }
    a.len().cmp(&b.len())
}

/// `a < b`, compared as [`key_cmp`] does.
#[inline]
pub fn key_lt(a: &[u8], b: &[u8]) -> bool {
    key_cmp(a, b).is_lt()
}

/// Keys this long or shorter are stored inside a [`Key`], not on the heap.
const INLINE: usize = 30;

/// A key in an in-memory sorted map, ordered like the bytes but compared
/// with [`key_cmp`]. Look-ups can still use plain `&[u8]`.
///
/// Short keys (most of them: a row key is about 15 bytes) are stored in
/// place. A tree search then compares bytes sitting in the tree's own nodes
/// instead of following a pointer per comparison, and storing a key costs
/// no allocation.
#[derive(Clone)]
pub struct Key(Repr);

#[derive(Clone)]
enum Repr {
    Inline { len: u8, bytes: [u8; INLINE] },
    Heap(Box<[u8]>),
}

impl Key {
    pub fn new(key: &[u8]) -> Key {
        if key.len() <= INLINE {
            let mut bytes = [0; INLINE];
            bytes[..key.len()].copy_from_slice(key);
            Key(Repr::Inline { len: key.len() as u8, bytes })
        } else {
            Key(Repr::Heap(key.into()))
        }
    }

    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        match &self.0 {
            Repr::Inline { len, bytes } => &bytes[..*len as usize],
            Repr::Heap(bytes) => bytes,
        }
    }
}

impl From<Vec<u8>> for Key {
    fn from(key: Vec<u8>) -> Key {
        if key.len() <= INLINE { Key::new(&key) } else { Key(Repr::Heap(key.into_boxed_slice())) }
    }
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_slice().fmt(f)
    }
}

impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl Eq for Key {}

impl Ord for Key {
    #[inline]
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        key_cmp(self.as_slice(), other.as_slice())
    }
}

impl PartialOrd for Key {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::borrow::Borrow<[u8]> for Key {
    fn borrow(&self) -> &[u8] {
        self.as_slice()
    }
}

/// A borrowed [`Key`], so look-ups compare the way the map's keys do rather
/// than through `memcmp`.
#[repr(transparent)]
#[derive(PartialEq, Eq)]
pub struct KeyRef([u8]);

impl KeyRef {
    pub fn new(bytes: &[u8]) -> &KeyRef {
        // SAFETY: `KeyRef` is a `repr(transparent)` wrapper around `[u8]`,
        // so the two reference types have the same layout and metadata.
        unsafe { &*(bytes as *const [u8] as *const KeyRef) }
    }
}

impl Ord for KeyRef {
    #[inline]
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        key_cmp(&self.0, &other.0)
    }
}

impl PartialOrd for KeyRef {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::borrow::Borrow<KeyRef> for Key {
    fn borrow(&self) -> &KeyRef {
        KeyRef::new(self.as_slice())
    }
}

/// Reads a varint, with the short lengths (most values) unrolled.
#[inline(always)]
pub fn fast_varint(data: &[u8], pos: &mut usize) -> Option<u64> {
    let p = *pos;
    let b0 = u64::from(*data.get(p)?);
    if b0 < 0x80 {
        *pos = p + 1;
        return Some(b0);
    }
    let b1 = u64::from(*data.get(p + 1)?);
    if b1 < 0x80 {
        *pos = p + 2;
        return Some((b0 & 0x7f) | b1 << 7);
    }
    let b2 = u64::from(*data.get(p + 2)?);
    if b2 < 0x80 {
        *pos = p + 3;
        return Some((b0 & 0x7f) | (b1 & 0x7f) << 7 | b2 << 14);
    }
    let (v, n) = get_varint(&data[p..])?;
    *pos = p + n;
    Some(v)
}

pub fn get_varint(buf: &[u8]) -> Option<(u64, usize)> {
    let mut v = 0u64;
    for (i, &b) in buf.iter().take(10).enumerate() {
        v |= u64::from(b & 0x7f) << (7 * i);
        if b < 0x80 {
            return Some((v, i + 1));
        }
    }
    None
}

pub fn u32_at(buf: &[u8], pos: usize) -> u32 {
    u32::from_le_bytes(buf[pos..pos + 4].try_into().unwrap())
}

pub fn u64_at(buf: &[u8], pos: usize) -> u64 {
    u64::from_le_bytes(buf[pos..pos + 8].try_into().unwrap())
}

/// A cursor over encoded bytes that reports truncation as corruption.
pub struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf }
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }

    pub fn varint(&mut self) -> Result<u64> {
        let (v, n) = get_varint(self.buf).ok_or_else(|| corruption("truncated varint"))?;
        self.buf = &self.buf[n..];
        Ok(v)
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.buf.len() {
            return Err(corruption("truncated data"));
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }

    /// Reads bytes written by [`put_bytes`].
    pub fn len_prefixed(&mut self) -> Result<&'a [u8]> {
        let n = self.varint()?;
        self.bytes(usize::try_from(n).map_err(|_| corruption("length overflows usize"))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_round_trip() {
        for v in [0, 1, 127, 128, 300, 16_383, 16_384, u32::MAX as u64, u64::MAX] {
            let mut buf = Vec::new();
            put_varint(&mut buf, v);
            assert_eq!(get_varint(&buf), Some((v, buf.len())));
        }
    }

    #[test]
    fn truncated_varint_is_rejected() {
        assert_eq!(get_varint(&[0x80, 0x80]), None);
        assert!(Reader::new(&[0xff]).varint().is_err());
    }

    #[test]
    fn reader_reads_length_prefixed_bytes() {
        let mut buf = Vec::new();
        put_bytes(&mut buf, b"spiro");
        put_bytes(&mut buf, b"");
        let mut r = Reader::new(&buf);
        assert_eq!(r.len_prefixed().unwrap(), b"spiro");
        assert_eq!(r.len_prefixed().unwrap(), b"");
        assert!(r.is_empty());
        assert!(r.len_prefixed().is_err());
    }
}

#[cfg(test)]
mod key_order_tests {
    use super::*;

    #[test]
    fn key_cmp_matches_byte_order() {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut rand = |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n
        };
        for _ in 0..200_000 {
            // Few distinct byte values and shared prefixes, so ties and
            // prefix relationships come up often.
            let prefix: Vec<u8> = (0..rand(20)).map(|_| rand(3) as u8 * 127).collect();
            let mut a = prefix.clone();
            let mut b = prefix;
            a.extend((0..rand(12)).map(|_| rand(3) as u8 * 127));
            b.extend((0..rand(12)).map(|_| rand(3) as u8 * 127));
            assert_eq!(key_cmp(&a, &b), a.cmp(&b), "{a:?} {b:?}");
            assert_eq!(Key::new(&a).cmp(&Key::from(b.clone())), a.cmp(&b));
            assert_eq!(Key::new(&a).as_slice(), a.as_slice());
            assert_eq!(KeyRef::new(&a).cmp(KeyRef::new(&b)), a.cmp(&b));
        }
    }
}
