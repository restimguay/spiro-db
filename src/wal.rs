//! The write-ahead log. Every write batch is appended here before it touches
//! the memtable, so the memtable can be rebuilt after a crash.
//!
//! Record: [crc32c(payload): u32][payload length: u32][payload]

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;

use crate::coding::u32_at;
use crate::crc32c::crc32c;
use crate::error::Result;
use crate::manifest::sync_dir;

const HEADER_LEN: usize = 8;

pub struct WalWriter {
    file: File,
    buf: Vec<u8>,
}

impl WalWriter {
    /// Creates a new, empty log. With `durable`, the directory entry is synced
    /// too, so the file itself survives power loss.
    pub fn create(path: &Path, durable: bool) -> Result<Self> {
        let file = OpenOptions::new().write(true).create_new(true).open(path)?;
        if durable && let Some(dir) = path.parent() {
            sync_dir(dir)?;
        }
        Ok(WalWriter { file, buf: Vec::new() })
    }

    /// Appends one record with a single `write` call. The caller guarantees
    /// the payload is under 4 GiB.
    pub fn append(&mut self, payload: &[u8], sync: bool) -> Result<()> {
        self.buf.clear();
        self.buf.extend_from_slice(&crc32c(payload).to_le_bytes());
        self.buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(payload);
        self.file.write_all(&self.buf)?;
        if sync {
            self.file.sync_data()?;
        }
        if self.buf.capacity() > 1 << 20 {
            self.buf = Vec::new(); // don't hold on to one giant batch's worth of memory
        }
        Ok(())
    }

    pub fn sync(&mut self) -> Result<()> {
        Ok(self.file.sync_data()?)
    }
}

/// Feeds each intact record of the log at `path` to `apply`, in order.
///
/// Stops quietly at the first torn or corrupt record: that is the tail of a
/// write the crash interrupted, and nothing after it can be trusted.
pub fn replay(path: &Path, mut apply: impl FnMut(&[u8]) -> Result<()>) -> Result<()> {
    let data = fs::read(path)?;
    let mut pos = 0;
    while let Some(header) = data.get(pos..pos + HEADER_LEN) {
        let len = u32_at(header, 4) as usize;
        let Some(payload) = data.get(pos + HEADER_LEN..pos + HEADER_LEN + len) else { break };
        if crc32c(payload) != u32_at(header, 0) {
            break;
        }
        apply(payload)?;
        pos += HEADER_LEN + len;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn records(path: &Path) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        replay(path, |p| {
            out.push(p.to_vec());
            Ok(())
        })
        .unwrap();
        out
    }

    #[test]
    fn replays_records_and_ignores_a_torn_tail() {
        let dir = TempDir::new("wal");
        let path = dir.path().join("1.log");
        let mut wal = WalWriter::create(&path, false).unwrap();
        wal.append(b"first", false).unwrap();
        wal.append(b"", false).unwrap();
        wal.append(b"third", true).unwrap();
        drop(wal);
        assert_eq!(records(&path), vec![b"first".to_vec(), b"".to_vec(), b"third".to_vec()]);

        // A record whose header promises more bytes than were written.
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&[1, 2, 3, 4, 100, 0, 0, 0, b'x']).unwrap();
        assert_eq!(records(&path).len(), 3);
    }

    #[test]
    fn stops_at_a_corrupt_record() {
        let dir = TempDir::new("wal-corrupt");
        let path = dir.path().join("1.log");
        let mut wal = WalWriter::create(&path, false).unwrap();
        for r in [&b"one"[..], b"two", b"three"] {
            wal.append(r, false).unwrap();
        }
        drop(wal);
        let mut bytes = fs::read(&path).unwrap();
        bytes[HEADER_LEN + 3 + HEADER_LEN] ^= 0xff; // first payload byte of "two"
        fs::write(&path, bytes).unwrap();
        assert_eq!(records(&path), vec![b"one".to_vec()]);
    }
}
