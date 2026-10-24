use crate::coding::{Reader, put_bytes};
use crate::error::{Result, corruption};

const KIND_DELETE: u8 = 0;
const KIND_PUT: u8 = 1;

/// A group of puts and deletes applied atomically: after a crash, either all
/// of them are visible or none are.
///
/// The batch is kept in its log encoding, so committing it is a single append.
/// Each entry is `[kind: u8][key: len-prefixed]` followed, for puts, by
/// `[value: len-prefixed]`.
#[derive(Clone, Debug, Default)]
pub struct WriteBatch {
    rep: Vec<u8>,
    count: usize,
}

impl WriteBatch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put(&mut self, key: impl AsRef<[u8]>, value: impl AsRef<[u8]>) -> &mut Self {
        self.rep.push(KIND_PUT);
        put_bytes(&mut self.rep, key.as_ref());
        put_bytes(&mut self.rep, value.as_ref());
        self.count += 1;
        self
    }

    pub fn delete(&mut self, key: impl AsRef<[u8]>) -> &mut Self {
        self.rep.push(KIND_DELETE);
        put_bytes(&mut self.rep, key.as_ref());
        self.count += 1;
        self
    }

    /// Number of operations in the batch.
    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn clear(&mut self) {
        self.rep.clear();
        self.count = 0;
    }

    pub(crate) fn data(&self) -> &[u8] {
        &self.rep
    }

    pub(crate) fn into_data(self) -> Vec<u8> {
        self.rep
    }

    /// Rebuilds a batch from its log encoding, validating every entry.
    pub(crate) fn decode(data: &[u8]) -> Result<WriteBatch> {
        let mut r = Reader::new(data);
        let mut count = 0;
        while !r.is_empty() {
            match r.u8()? {
                KIND_PUT => {
                    r.len_prefixed()?;
                    r.len_prefixed()?;
                }
                KIND_DELETE => {
                    r.len_prefixed()?;
                }
                kind => return Err(corruption(format!("unknown write batch entry kind {kind}"))),
            }
            count += 1;
        }
        Ok(WriteBatch { rep: data.to_vec(), count })
    }

    /// Yields `(key, Some(value))` for puts and `(key, None)` for deletes.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&[u8], Option<&[u8]>)> + '_ {
        let mut r = Reader::new(&self.rep);
        // The encoding is valid by construction (or checked by `decode`), so
        // the `ok()?`s never actually stop the iteration early.
        std::iter::from_fn(move || {
            if r.is_empty() {
                return None;
            }
            let kind = r.u8().ok()?;
            let key = r.len_prefixed().ok()?;
            let value = if kind == KIND_PUT { Some(r.len_prefixed().ok()?) } else { None };
            Some((key, value))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_its_encoding() {
        let mut batch = WriteBatch::new();
        batch.put("a", "1").delete("b").put("", "");
        let decoded = WriteBatch::decode(batch.data()).unwrap();
        assert_eq!(decoded.len(), 3);
        let ops: Vec<_> = decoded.iter().collect();
        assert_eq!(ops, vec![(&b"a"[..], Some(&b"1"[..])), (&b"b"[..], None), (&b""[..], Some(&b""[..]))]);
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(WriteBatch::decode(&[7]).is_err());
        assert!(WriteBatch::decode(&[KIND_PUT, 5, b'a']).is_err());
    }
}
