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
