//! The MANIFEST lists the live tables and the oldest log still needed for
//! recovery.
//!
//! It is tiny, so rather than appending edits it is rewritten whole: write a
//! temp file, fsync, rename it over the old one. The rename is atomic, so a
//! crash leaves either the old manifest or the new one, never a mix.
//!
//! Layout: [magic: u64][crc32c(body): u32][body]
//! Body:   next file number, log number, then for level 0 and level 1: a table
//!         count and, per table, its number, size, smallest and largest key.

use std::fs::{self, File};
use std::io::{self, ErrorKind, Write};
use std::path::Path;

use crate::coding::{Reader, put_bytes, put_varint, u32_at, u64_at};
use crate::crc32c::crc32c;
use crate::error::{Result, corruption};
use crate::table::TableMeta;

const MAGIC: u64 = 0x464e_4d4f_5249_5053; // "SPIROMNF"
const HEADER_LEN: usize = 12;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Manifest {
    pub next_file: u64,
    /// Logs numbered below this have been flushed to tables and can be ignored.
    pub log_number: u64,
    pub l0: Vec<TableMeta>,
    pub l1: Vec<TableMeta>,
}

impl Manifest {
    pub fn load(dir: &Path) -> Result<Option<Manifest>> {
        let data = match fs::read(dir.join("MANIFEST")) {
            Ok(data) => data,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if data.len() < HEADER_LEN || u64_at(&data, 0) != MAGIC {
            return Err(corruption("MANIFEST has a bad header"));
        }
        let body = &data[HEADER_LEN..];
        if crc32c(body) != u32_at(&data, 8) {
            return Err(corruption("MANIFEST checksum mismatch"));
        }
        let mut r = Reader::new(body);
        Ok(Some(Manifest {
            next_file: r.varint()?,
            log_number: r.varint()?,
            l0: read_level(&mut r)?,
            l1: read_level(&mut r)?,
        }))
    }

    pub fn store(&self, dir: &Path) -> Result<()> {
        let mut body = Vec::new();
        put_varint(&mut body, self.next_file);
        put_varint(&mut body, self.log_number);
        for level in [&self.l0, &self.l1] {
            put_varint(&mut body, level.len() as u64);
            for t in level {
                put_varint(&mut body, t.number);
                put_varint(&mut body, t.size);
                put_bytes(&mut body, &t.smallest);
                put_bytes(&mut body, &t.largest);
            }
        }
        let tmp = dir.join("MANIFEST.tmp");
        let mut file = File::create(&tmp)?;
        file.write_all(&MAGIC.to_le_bytes())?;
        file.write_all(&crc32c(&body).to_le_bytes())?;
        file.write_all(&body)?;
        file.sync_all()?;
        fs::rename(&tmp, dir.join("MANIFEST"))?;
        sync_dir(dir)?;
        Ok(())
    }
}

fn read_level(r: &mut Reader) -> Result<Vec<TableMeta>> {
    let count = r.varint()?;
    let mut tables = Vec::new();
    for _ in 0..count {
        tables.push(TableMeta {
            number: r.varint()?,
            size: r.varint()?,
            smallest: r.len_prefixed()?.to_vec(),
            largest: r.len_prefixed()?.to_vec(),
        });
    }
    Ok(tables)
}

/// Makes renames and newly created files in `dir` durable.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn round_trip_and_corruption() {
        let dir = TempDir::new("manifest");
        assert_eq!(Manifest::load(dir.path()).unwrap(), None);
        let table =
            |n: u64| TableMeta { number: n, size: n * 100, smallest: vec![b'a'; n as usize], largest: vec![b'z'] };
        let m = Manifest { next_file: 9, log_number: 7, l0: vec![table(5), table(4)], l1: vec![table(2)] };
        m.store(dir.path()).unwrap();
        assert_eq!(Manifest::load(dir.path()).unwrap(), Some(m));

        let path = dir.path().join("MANIFEST");
        let mut bytes = fs::read(&path).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        fs::write(&path, bytes).unwrap();
        assert!(Manifest::load(dir.path()).is_err());
    }
}
