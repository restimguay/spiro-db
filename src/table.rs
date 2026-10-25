//! Tables (SSTables): immutable files of sorted key/value entries.
//!
//! [data block][crc32c] ... [data block][crc32c]
//! [bloom filter][crc32c]
//! [index block][crc32c]
//! [footer: filter offset, filter len, index offset, index len, entry count, magic — all u64]
//!
//! The index block maps each data block's last key to its (offset, length), so
//! a point lookup costs one bloom probe, one in-memory index search and at most
//! one block read.

use std::any::Any;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::ops::Bound;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use crate::block::{Block, BlockBuilder, BlockIter};
use crate::bloom::{self, BloomBuilder};
use crate::cache::BlockCache;
use crate::coding::{Reader, key_lt, put_varint, u32_at, u64_at};
use crate::crc32c::crc32c;
use crate::error::{Error, Result, corruption};
use crate::iter::{KvIter, Visit};
use crate::manifest::sync_file;
use crate::options::Options;

const MAGIC: u64 = 0x5453_534f_5249_5053; // "SPIROSST"
const FOOTER_LEN: usize = 48;
/// Footer of tables that may carry a column section: two more fields.
const MAGIC_V2: u64 = 0x3253_534f_5249_5053; // "SPIROSS2"
const FOOTER_V2_LEN: usize = 64;

/// Builds a table file's *column section* from its entries, which arrive in
/// key order. The storage engine stores the bytes; what they mean is up to
/// the layer that supplies the builder (the SQL layer lays out its rows'
/// columns there, for scans that read only the columns they need).
pub trait SectionBuilder: Send {
    fn add(&mut self, key: &[u8], value: Option<&[u8]>);
    /// The section, or nothing to store.
    fn finish(&mut self) -> Vec<u8>;
}

/// What the manifest records about a table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableMeta {
    pub number: u64,
    pub size: u64,
    pub smallest: Vec<u8>,
    pub largest: Vec<u8>,
}

pub struct TableBuilder {
    number: u64,
    file: BufWriter<File>,
    offset: u64,
    data: BlockBuilder,
    index: BlockBuilder,
    bloom: BloomBuilder,
    block_size: usize,
    entries: u64,
    smallest: Vec<u8>,
    last_key: Vec<u8>,
    /// Sync fully on finishing, rather than just in order (`Options::sync`).
    full_sync: bool,
    section: Option<Box<dyn SectionBuilder>>,
}

impl TableBuilder {
    pub fn create(path: &Path, number: u64, opts: &Options, section: Option<Box<dyn SectionBuilder>>) -> Result<Self> {
        Ok(TableBuilder {
            number,
            file: BufWriter::with_capacity(256 << 10, File::create(path)?),
            offset: 0,
            data: BlockBuilder::new(),
            index: BlockBuilder::new(),
            bloom: BloomBuilder::new(opts.bloom_bits_per_key),
            block_size: opts.block_size,
            entries: 0,
            smallest: Vec::new(),
            last_key: Vec::new(),
            full_sync: opts.sync,
            section,
        })
    }

    /// Adds an entry; keys must arrive in strictly increasing order.
    pub fn add(&mut self, key: &[u8], value: Option<&[u8]>) -> Result<()> {
        debug_assert!(self.entries == 0 || key > self.last_key.as_slice(), "keys out of order");
        if self.entries == 0 {
            self.smallest = key.to_vec();
        }
        self.bloom.add(key);
        self.data.add(key, value);
        if let Some(section) = &mut self.section {
            section.add(key, value);
        }
        self.last_key.clear();
        self.last_key.extend_from_slice(key);
        self.entries += 1;
        if self.data.size_estimate() >= self.block_size {
            self.flush_block()?;
        }
        Ok(())
    }

    pub fn estimated_size(&self) -> u64 {
        self.offset + self.data.size_estimate() as u64
    }

    fn flush_block(&mut self) -> Result<()> {
        if self.data.is_empty() {
            return Ok(());
        }
        let block = self.data.finish();
        let (offset, len) = self.write_checksummed(&block)?;
        let mut handle = Vec::with_capacity(20);
        put_varint(&mut handle, offset);
        put_varint(&mut handle, len);
        self.index.add(&self.last_key, Some(&handle));
        Ok(())
    }

    fn write_checksummed(&mut self, data: &[u8]) -> Result<(u64, u64)> {
        let offset = self.offset;
        self.file.write_all(data)?;
        self.file.write_all(&crc32c(data).to_le_bytes())?;
        self.offset += data.len() as u64 + 4;
        Ok((offset, data.len() as u64))
    }

    /// Writes the filter, index and footer, and syncs the file to disk.
    pub fn finish(mut self) -> Result<TableMeta> {
        self.flush_block()?;
        let (filter_offset, filter_len) = self.write_checksummed(&self.bloom.finish())?;
        let index = self.index.finish();
        let (index_offset, index_len) = self.write_checksummed(&index)?;
        let section = self.section.as_mut().map(|s| s.finish()).unwrap_or_default();
        let (section_offset, section_len) = if section.is_empty() { (0, 0) } else { self.write_checksummed(&section)? };
        let footer =
            [filter_offset, filter_len, index_offset, index_len, section_offset, section_len, self.entries, MAGIC_V2];
        for v in footer {
            self.file.write_all(&v.to_le_bytes())?;
        }
        self.offset += FOOTER_V2_LEN as u64;
        self.file.flush()?;
        sync_file(self.file.get_ref(), self.full_sync)?;
        Ok(TableMeta { number: self.number, size: self.offset, smallest: self.smallest, largest: self.last_key })
    }
}

/// An open table. Its index and bloom filter live in memory; data blocks are
/// read on demand through the shared block cache.
pub struct Table {
    pub meta: TableMeta,
    file: File,
    index: Arc<Block>,
    filter: Vec<u8>,
    cache: Arc<BlockCache>,
    /// Every data block's last key, read from the index when first needed.
    boundaries: OnceLock<Vec<Vec<u8>>>,
    /// Where the column section is (offset, length); length 0 if none.
    section: (u64, u64),
    /// The column section as the layer above parsed it, on first use.
    columnar: OnceLock<Arc<dyn Any + Send + Sync>>,
}

impl Table {
    pub fn open(path: &Path, meta: TableMeta, cache: Arc<BlockCache>) -> Result<Table> {
        let file = File::open(path)?;
        let size = file.metadata()?.len();
        if size != meta.size || size < FOOTER_LEN as u64 {
            return Err(corruption(format!("table {} is {size} bytes, expected {}", meta.number, meta.size)));
        }
        let mut magic = [0u8; 8];
        read_exact_at(&file, &mut magic, size - 8)?;
        let footer_len = match u64::from_le_bytes(magic) {
            MAGIC => FOOTER_LEN,
            MAGIC_V2 if size >= FOOTER_V2_LEN as u64 => FOOTER_V2_LEN,
            _ => return Err(corruption(format!("table {} has a bad footer", meta.number))),
        };
        let mut footer = vec![0u8; footer_len];
        read_exact_at(&file, &mut footer, size - footer_len as u64)?;
        let filter = read_block(&file, size, u64_at(&footer, 0), u64_at(&footer, 8))?;
        let index = Block::new(read_block(&file, size, u64_at(&footer, 16), u64_at(&footer, 24))?)?;
        let section = if footer_len == FOOTER_V2_LEN { (u64_at(&footer, 32), u64_at(&footer, 40)) } else { (0, 0) };
        Ok(Table {
            meta,
            file,
            index: Arc::new(index),
            filter,
            cache,
            boundaries: OnceLock::new(),
            section,
            columnar: OnceLock::new(),
        })
    }

    /// Whether `key` falls inside this table's key range.
    pub fn covers(&self, key: &[u8]) -> bool {
        self.meta.smallest.as_slice() <= key && key <= self.meta.largest.as_slice()
    }

    /// `None` if the key isn't here; `Some(None)` if it was deleted here.
    pub fn get(&self, key: &[u8]) -> Result<Option<Option<Vec<u8>>>> {
        if !bloom::may_contain(&self.filter, key) {
            return Ok(None);
        }
        let mut index = BlockIter::new(&*self.index);
        index.seek(key);
        if !index.valid() {
            return Ok(None);
        }
        let block = self.data_block(index.value().unwrap_or_default(), true)?;
        let mut it = BlockIter::new(&*block);
        it.seek(key);
        Ok((it.valid() && it.key() == key).then(|| it.value().map(<[u8]>::to_vec)))
    }

    /// What the layer above built from this table's column section with
    /// `parse`, or `None` if the table has no section. The section is read
    /// and parsed once, then kept while the table is open.
    pub fn columnar<T: Any + Send + Sync>(&self, parse: impl FnOnce(Vec<u8>) -> Result<T>) -> Result<Option<Arc<T>>> {
        if self.section.1 == 0 {
            return Ok(None);
        }
        if self.columnar.get().is_none() {
            let bytes = read_block(&self.file, self.meta.size, self.section.0, self.section.1)?;
            // Two threads may parse at once; the first to finish is kept.
            let _ = self.columnar.set(Arc::new(parse(bytes)?));
        }
        Ok(self.columnar.get().and_then(|parsed| parsed.clone().downcast::<T>().ok()))
    }

    /// The keys that end this table's data blocks, inside `(start, end)`:
    /// places to split a scan of the table.
    pub fn block_boundaries(&self, start: &Bound<Vec<u8>>, end: &Bound<Vec<u8>>) -> &[Vec<u8>] {
        let all = self.boundaries.get_or_init(|| {
            let mut index = BlockIter::new(&*self.index);
            let mut keys = Vec::new();
            index.seek_to_first();
            while index.valid() {
                keys.push(index.key().to_vec());
                index.advance();
            }
            keys
        });
        let from = match start {
            Bound::Included(k) | Bound::Excluded(k) => all.partition_point(|key| key <= k),
            Bound::Unbounded => 0,
        };
        let to = match end {
            Bound::Included(k) | Bound::Excluded(k) => all.partition_point(|key| key < k),
            Bound::Unbounded => all.len(),
        };
        &all[from..to.max(from)]
    }

    /// `fill_cache: false` keeps large one-off reads (compactions) from
    /// evicting the hot working set.
    pub fn iter(self: &Arc<Self>, fill_cache: bool) -> TableIter {
        TableIter { table: self.clone(), index: BlockIter::new(self.index.clone()), data: None, fill_cache, err: None }
    }

    fn data_block(&self, handle: &[u8], fill_cache: bool) -> Result<Arc<Block>> {
        let mut r = Reader::new(handle);
        let (offset, len) = (r.varint()?, r.varint()?);
        if let Some(block) = self.cache.get(self.meta.number, offset) {
            return Ok(block);
        }
        let block = Arc::new(Block::new(read_block(&self.file, self.meta.size, offset, len)?)?);
        if fill_cache {
            self.cache.insert(self.meta.number, offset, block.clone());
        }
        Ok(block)
    }
}

fn read_block(file: &File, file_size: u64, offset: u64, len: u64) -> Result<Vec<u8>> {
    if offset.checked_add(len).and_then(|end| end.checked_add(4)).is_none_or(|end| end > file_size) {
        return Err(corruption(format!("block at {offset}+{len} lies outside its {file_size}-byte table")));
    }
    let len = len as usize;
    let mut buf = vec![0u8; len + 4];
    read_exact_at(file, &mut buf, offset)?;
    let expected = u32_at(&buf, len);
    buf.truncate(len);
    if crc32c(&buf) != expected {
        return Err(corruption(format!("block checksum mismatch at offset {offset}")));
    }
    Ok(buf)
}

fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut done = 0;
        while done < buf.len() {
            match file.seek_read(&mut buf[done..], offset + done as u64)? {
                0 => return Err(io::ErrorKind::UnexpectedEof.into()),
                n => done += n,
            }
        }
        Ok(())
    }
}

/// Walks a table's entries in order: the index block picks data blocks, which
/// are loaded one at a time.
pub struct TableIter {
    table: Arc<Table>,
    index: BlockIter,
    data: Option<BlockIter>,
    fill_cache: bool,
    err: Option<Error>,
}

impl TableIter {
    fn load_block(&mut self) {
        self.data = None;
        if !self.index.valid() {
            return;
        }
        match self.table.data_block(self.index.value().unwrap_or_default(), self.fill_cache) {
            Ok(block) => self.data = Some(BlockIter::new(block)),
            Err(e) => self.err = Some(e),
        }
    }

    fn skip_exhausted_blocks(&mut self) {
        while self.err.is_none() && !self.data.as_ref().is_some_and(BlockIter::valid) {
            if !self.index.valid() {
                return;
            }
            self.index.advance();
            self.load_block();
            if let Some(data) = &mut self.data {
                data.seek_to_first();
            }
        }
    }

    fn data(&self) -> &BlockIter {
        self.data.as_ref().expect("iterator is valid")
    }
}

impl KvIter for TableIter {
    fn valid(&self) -> bool {
        self.err.is_none() && self.data.as_ref().is_some_and(BlockIter::valid)
    }

    fn key(&self) -> &[u8] {
        self.data().key()
    }

    fn value(&self) -> Option<&[u8]> {
        self.data().value()
    }

    fn next(&mut self) {
        if let Some(data) = &mut self.data {
            data.advance();
        }
        self.skip_exhausted_blocks();
    }

    fn seek(&mut self, target: &[u8]) {
        self.index.seek(target);
        self.load_block();
        if let Some(data) = &mut self.data {
            data.seek(target);
        }
        self.skip_exhausted_blocks();
    }

    fn status(&self) -> Result<()> {
        self.err.clone().map_or(Ok(()), Err)
    }

    fn run(&mut self, limit: Option<&[u8]>, f: Visit) -> bool {
        while self.valid() {
            // The index holds each block's last key: when that is below the
            // limit, the rest of the block needs no comparisons at all.
            let whole_block = limit.is_none_or(|limit| key_lt(self.index.key(), limit));
            let data = self.data.as_mut().expect("iterator is valid");
            let go = data.run(if whole_block { None } else { limit }, f);
            let at_limit = data.valid();
            self.skip_exhausted_blocks();
            if !go {
                return false;
            }
            if at_limit {
                return true;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn key(i: usize) -> Vec<u8> {
        format!("key{i:06}").into_bytes()
    }

    fn build(dir: &TempDir, n: usize) -> Arc<Table> {
        let path = dir.path().join("1.sst");
        let opts = Options { block_size: 512, ..Options::default() };
        let mut b = TableBuilder::create(&path, 1, &opts, None).unwrap();
        for i in 0..n {
            let value = (i % 5 != 0).then(|| format!("value{i}").into_bytes());
            b.add(&key(i * 2), value.as_deref()).unwrap();
        }
        let meta = b.finish().unwrap();
        assert_eq!((meta.smallest.clone(), meta.largest.clone()), (key(0), key((n - 1) * 2)));
        Arc::new(Table::open(&path, meta, Arc::new(BlockCache::new(1 << 20))).unwrap())
    }

    #[test]
    fn point_lookups() {
        let dir = TempDir::new("table-get");
        let table = build(&dir, 5000);
        for i in 0..5000 {
            let expected = (i % 5 != 0).then(|| format!("value{i}").into_bytes());
            assert_eq!(table.get(&key(i * 2)).unwrap(), Some(expected), "key {i}");
            assert_eq!(table.get(&key(i * 2 + 1)).unwrap(), None);
        }
    }

    #[test]
    fn iteration_and_seek() {
        let dir = TempDir::new("table-iter");
        let table = build(&dir, 5000);
        let mut it = table.iter(false);
        it.seek(b"");
        let mut n = 0;
        while it.valid() {
            assert_eq!(it.key(), key(n * 2));
            n += 1;
            it.next();
        }
        assert_eq!(n, 5000);
        it.seek(&key(1001));
        assert_eq!(it.key(), key(1002));
        it.seek(b"zzz");
        assert!(!it.valid());
        it.status().unwrap();
    }

    #[test]
    fn detects_corruption() {
        let dir = TempDir::new("table-corrupt");
        let table = build(&dir, 5000);
        let path = dir.path().join("1.sst");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[100] ^= 0xff;
        std::fs::write(&path, bytes).unwrap();
        let table = Arc::new(Table::open(&path, table.meta.clone(), Arc::new(BlockCache::new(0))).unwrap());
        assert!(matches!(table.get(&key(0)), Err(Error::Corruption(_))));
        let mut it = table.iter(false);
        it.seek(b"");
        assert!(!it.valid());
        assert!(it.status().is_err());
    }
}
