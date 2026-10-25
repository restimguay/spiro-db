//! Column sections: every table file also stores its SQL rows column by
//! column, for scans that read only the columns they need.
//!
//! Rows stay in the table's ordinary blocks, where point lookups, inserts
//! and updates want them. When the storage engine writes a table file (a
//! flush or a compaction), it also passes every entry to a
//! [`SectionWriter`], which lays out each SQL table's rows in that file as a
//! *chunk*: the primary keys in order, which rows are deletions, and each
//! column as its own array. Integers and reals are fixed-width, so a scan
//! copies a run of them into its batch without decoding anything.
//!
//! A scan of a key range [merges](scan) the memtables' rows with the files'
//! chunks, newest first, exactly as the storage engine merges entries:
//! where several sources hold a key, only the newest counts, and deletions
//! hide older versions. It goes in runs: while one chunk holds every key up
//! to the next source's, that stretch of its columns is handed over whole.
//!
//! A section is laid out as (all integers little-endian):
//!
//! ```text
//! section   "SCOL", chunk count: u32, per chunk: table id, rows, offset, length (u32 each), chunks
//! chunk     key kind: u8 (0 integer keys, 1 byte keys), keys,
//!           deleted: u8 (0 none, 1 one byte per row follows), column count: u32, columns
//! keys      integer: a u64 per row (the key's 8 bytes after its type tag, read big-endian)
//!           byte: offsets (rows + 1, u32), length: u32, the keys after the row prefix
//! column    kind: u8, has NULLs: u8 (then one byte per row, 1 for a value), data:
//!           NULL only: nothing; integer, real: 8 bytes per row; boolean: a byte per row;
//!           text, mixed: offsets (rows + 1, u32), length: u32, bytes (mixed: encoded values)
//! ```

use std::cmp::Ordering;
use std::ops::{Bound, Range};
use std::sync::Arc;

use crate::coding::{Reader, key_cmp};
use crate::db::ReadView;
use crate::error::{Error, Result, corruption};
use crate::iter::KvIter;
use crate::memtable::MemTable;
use crate::table::{SectionBuilder, Table};

use super::catalog::{ROW_PREFIX_LEN, TableSchema};
use super::encoding::{KEY_INTEGER, RawValue, decode_value, encode_value, visit_all};
use super::value::Value;

const MAGIC: u32 = u32::from_le_bytes(*b"SCOL");

const NULLS: u8 = 0;
const INTEGER: u8 = 1;
const REAL: u8 = 2;
const BOOLEAN: u8 = 3;
const TEXT: u8 = 4;
const MIXED: u8 = 5;

fn put32(out: &mut Vec<u8>, v: usize) {
    out.extend_from_slice(&(v as u32).to_le_bytes());
}

// ------------------------------------------------------------------ writing

/// Builds a table file's column section from its entries, in key order.
#[derive(Default)]
pub struct SectionWriter {
    bodies: Vec<u8>,
    /// Per chunk: table id, rows, offset into `bodies`, length.
    directory: Vec<[usize; 4]>,
    chunk: Option<ChunkWriter>,
    /// A row that wouldn't decode: store no section, so scans read rows.
    broken: bool,
    /// Only these columns are decoded (the rest read as NULL); `None`: all.
    needed: Option<Vec<bool>>,
}

impl SectionBuilder for SectionWriter {
    fn add(&mut self, key: &[u8], value: Option<&[u8]>) {
        // Only rows: `00 'R' <table id>`, then the primary key.
        if key.len() < ROW_PREFIX_LEN || key[..2] != [0, b'R'] || self.broken {
            return;
        }
        let table = u32::from_be_bytes(key[2..6].try_into().unwrap());
        if self.chunk.as_ref().is_some_and(|c| c.table != table) {
            self.end_chunk();
        }
        let needed = &self.needed;
        let chunk = self.chunk.get_or_insert_with(|| ChunkWriter::new(table, needed.clone()));
        if chunk.add(&key[ROW_PREFIX_LEN..], value).is_err() {
            self.broken = true;
        }
    }

    fn finish(&mut self) -> Vec<u8> {
        self.end_chunk();
        if self.directory.is_empty() || self.broken {
            return Vec::new();
        }
        let header = 8 + 16 * self.directory.len();
        let mut out = Vec::with_capacity(header + self.bodies.len());
        put32(&mut out, MAGIC as usize);
        put32(&mut out, self.directory.len());
        for &[table, rows, offset, len] in &self.directory {
            for v in [table, rows, header + offset, len] {
                put32(&mut out, v);
            }
        }
        out.extend_from_slice(&self.bodies);
        out
    }
}

impl SectionWriter {
    /// A writer that decodes only the `needed` columns, for laying out rows
    /// a scan reads from a memtable.
    pub fn only(needed: &[bool]) -> Self {
        SectionWriter { needed: Some(needed.to_vec()), ..SectionWriter::default() }
    }

    fn end_chunk(&mut self) {
        if let Some(chunk) = self.chunk.take() {
            let offset = self.bodies.len();
            let (table, rows) = (chunk.table, chunk.rows);
            chunk.write(&mut self.bodies);
            self.directory.push([table as usize, rows, offset, self.bodies.len() - offset]);
        }
    }
}

/// One SQL table's rows in one file.
struct ChunkWriter {
    table: u32,
    rows: usize,
    int_keys: Vec<u64>,
    /// Once any key isn't a single integer: every key, as offsets and bytes.
    byte_keys: Option<(Vec<usize>, Vec<u8>)>,
    deleted: Vec<bool>,
    columns: Vec<ColumnWriter>,
    needed: Option<Vec<bool>>,
}

impl ChunkWriter {
    fn new(table: u32, needed: Option<Vec<bool>>) -> Self {
        ChunkWriter {
            table,
            rows: 0,
            int_keys: Vec::new(),
            byte_keys: None,
            deleted: Vec::new(),
            columns: Vec::new(),
            needed,
        }
    }

    fn add(&mut self, key: &[u8], value: Option<&[u8]>) -> Result<()> {
        match (&mut self.byte_keys, key) {
            (None, [KEY_INTEGER, rest @ ..]) if rest.len() == 8 => {
                self.int_keys.push(u64::from_be_bytes(rest.try_into().unwrap()));
            }
            (None, _) => {
                let (mut offsets, mut bytes) = (vec![0], Vec::new());
                for k in self.int_keys.drain(..) {
                    bytes.push(KEY_INTEGER);
                    bytes.extend_from_slice(&k.to_be_bytes());
                    offsets.push(bytes.len());
                }
                bytes.extend_from_slice(key);
                offsets.push(bytes.len());
                self.byte_keys = Some((offsets, bytes));
            }
            (Some((offsets, bytes)), _) => {
                bytes.extend_from_slice(key);
                offsets.push(bytes.len());
            }
        }
        let rows = self.rows;
        self.rows += 1;
        let Some(row) = value else {
            self.deleted.push(true);
            self.columns.iter_mut().for_each(ColumnWriter::push_null);
            return Ok(());
        };
        self.deleted.push(false);
        let columns = &mut self.columns;
        let mut store = |i: usize, value| {
            while columns.len() <= i {
                columns.push(ColumnWriter::new(rows));
            }
            columns[i].push(value);
        };
        match &self.needed {
            Some(needed) => super::encoding::visit_row(row, needed, &mut store)?,
            None => visit_all(row, &mut store)?,
        }
        // Columns this row gave nothing (not needed, or written before the
        // column existed) read as NULL.
        for column in columns.iter_mut().filter(|c| c.len == rows) {
            column.push_null();
        }
        Ok(())
    }

    fn write(self, out: &mut Vec<u8>) {
        match &self.byte_keys {
            None => {
                out.push(0);
                self.int_keys.iter().for_each(|k| out.extend_from_slice(&k.to_le_bytes()));
            }
            Some((offsets, bytes)) => {
                out.push(1);
                offsets.iter().for_each(|&o| put32(out, o));
                put32(out, bytes.len());
                out.extend_from_slice(bytes);
            }
        }
        if self.deleted.contains(&true) {
            out.push(1);
            out.extend(self.deleted.iter().map(|&d| d as u8));
        } else {
            out.push(0);
        }
        put32(out, self.columns.len());
        for column in &self.columns {
            column.write(out);
        }
    }
}

/// One column's values, stored by kind; NULLs hold a placeholder.
struct ColumnWriter {
    kind: u8,
    /// Rows so far.
    len: usize,
    /// Which rows have a value; kept once the column has had one, before
    /// which every row is NULL and only `len` counts.
    valid: Vec<bool>,
    numbers: Vec<u64>,
    bools: Vec<u8>,
    offsets: Vec<usize>,
    bytes: Vec<u8>,
}

impl ColumnWriter {
    /// A column first seen after `rows` rows, which read as NULL in it.
    fn new(rows: usize) -> Self {
        ColumnWriter {
            kind: NULLS,
            len: rows,
            valid: Vec::new(),
            numbers: Vec::new(),
            bools: Vec::new(),
            offsets: vec![0],
            bytes: Vec::new(),
        }
    }

    fn push_null(&mut self) {
        self.len += 1;
        if self.kind != NULLS {
            self.valid.push(false);
            self.push_placeholder();
        }
    }

    fn push_placeholder(&mut self) {
        match self.kind {
            NULLS => {}
            INTEGER | REAL => self.numbers.push(0),
            BOOLEAN => self.bools.push(0),
            TEXT => self.offsets.push(self.bytes.len()),
            _ => {
                encode_value(&mut self.bytes, &Value::Null);
                self.offsets.push(self.bytes.len());
            }
        }
    }

    fn kind_of(value: &RawValue) -> u8 {
        match value {
            RawValue::Null => NULLS,
            RawValue::Integer(_) => INTEGER,
            RawValue::Real(_) => REAL,
            RawValue::Boolean(_) => BOOLEAN,
            RawValue::Text(_) => TEXT,
            // Kept in the row encoding.
            RawValue::Blob(_) | RawValue::Decimal(_) => MIXED,
        }
    }

    fn push(&mut self, value: RawValue) {
        let kind = Self::kind_of(&value);
        if kind == NULLS {
            return self.push_null();
        }
        if self.kind == NULLS {
            // The first value: earlier rows become NULL placeholders of its kind.
            self.kind = kind;
            self.valid = vec![false; self.len];
            for _ in 0..self.len {
                self.push_placeholder();
            }
        } else if self.kind != kind && self.kind != MIXED {
            self.make_mixed();
        }
        self.len += 1;
        self.valid.push(true);
        match (self.kind, value) {
            (INTEGER, RawValue::Integer(x)) => self.numbers.push(x as u64),
            (REAL, RawValue::Real(x)) => self.numbers.push(x.to_bits()),
            (BOOLEAN, RawValue::Boolean(b)) => self.bools.push(b as u8),
            (TEXT, RawValue::Text(t)) => {
                self.bytes.extend_from_slice(t);
                self.offsets.push(self.bytes.len());
            }
            (_, value) => {
                // Mixed: the value in the row encoding. Text that isn't
                // UTF-8 can't get here: the row was written from a string.
                encode_value(&mut self.bytes, &value.to_value().unwrap_or(Value::Null));
                self.offsets.push(self.bytes.len());
            }
        }
    }

    /// Re-encodes the values so far as the row encoding, any type allowed.
    fn make_mixed(&mut self) {
        let values: Vec<Value> = (0..self.valid.len()).map(|i| self.value(i)).collect();
        (self.kind, self.offsets, self.bytes) = (MIXED, vec![0], Vec::new());
        self.numbers.clear();
        self.bools.clear();
        for v in values {
            encode_value(&mut self.bytes, &v);
            self.offsets.push(self.bytes.len());
        }
    }

    fn value(&self, i: usize) -> Value {
        if !self.valid[i] {
            return Value::Null;
        }
        match self.kind {
            INTEGER => Value::Integer(self.numbers[i] as i64),
            REAL => Value::Real(f64::from_bits(self.numbers[i])),
            BOOLEAN => Value::Boolean(self.bools[i] != 0),
            TEXT => {
                Value::Text(String::from_utf8_lossy(&self.bytes[self.offsets[i]..self.offsets[i + 1]]).into_owned())
            }
            _ => {
                decode_value(&mut Reader::new(&self.bytes[self.offsets[i]..self.offsets[i + 1]])).unwrap_or(Value::Null)
            }
        }
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.push(self.kind);
        // A column of nothing but NULLs needs no flags to say so.
        if self.kind != NULLS && self.valid.contains(&false) {
            out.push(1);
            out.extend(self.valid.iter().map(|&v| v as u8));
        } else {
            out.push(0);
        }
        match self.kind {
            NULLS => {}
            INTEGER | REAL => self.numbers.iter().for_each(|n| out.extend_from_slice(&n.to_le_bytes())),
            BOOLEAN => out.extend_from_slice(&self.bools),
            _ => {
                self.offsets.iter().for_each(|&o| put32(out, o));
                put32(out, self.bytes.len());
                out.extend_from_slice(&self.bytes);
            }
        }
    }
}

// ------------------------------------------------------------------ reading

/// A parsed column section. The bytes stay as they are; only the positions
/// of keys and columns are worked out, once, and every one checked.
pub struct Section {
    bytes: Vec<u8>,
    chunks: Vec<ChunkInfo>,
}

struct ChunkInfo {
    table: u32,
    rows: usize,
    keys: Keys,
    deleted: Option<usize>,
    columns: Vec<ColumnInfo>,
}

enum Keys {
    /// Where the u64s start.
    Integer(usize),
    Bytes {
        offsets: usize,
        data: usize,
    },
}

struct ColumnInfo {
    kind: u8,
    valid: Option<usize>,
    data: usize,
    /// Text and mixed: where the offsets are; data then follows them.
    offsets: Option<usize>,
}

/// Reads a section front to back, refusing anything that runs past the end.
struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
    end: usize,
}

impl Cursor<'_> {
    fn take(&mut self, n: usize) -> Result<usize> {
        let at = self.pos;
        self.pos = at.checked_add(n).filter(|&p| p <= self.end).ok_or_else(bad)?;
        Ok(at)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes[self.take(1)?])
    }

    fn u32(&mut self) -> Result<usize> {
        let at = self.take(4)?;
        Ok(u32::from_le_bytes(self.bytes[at..at + 4].try_into().unwrap()) as usize)
    }

    /// Offsets (rows + 1 of them) and the bytes they point into; checks
    /// that the offsets start at 0, never go backwards and fit the bytes.
    fn offsets_and_data(&mut self, rows: usize) -> Result<(usize, usize)> {
        let offsets = self.take(rows.checked_add(1).and_then(|n| n.checked_mul(4)).ok_or_else(bad)?)?;
        let len = self.u32()?;
        let data = self.take(len)?;
        let at = |i: usize| u32::from_le_bytes(self.bytes[offsets + 4 * i..offsets + 4 * i + 4].try_into().unwrap());
        let mut last = 0;
        for i in 0..=rows {
            let o = at(i) as usize;
            if (i == 0 && o != 0) || o < last || o > len {
                return Err(bad());
            }
            last = o;
        }
        Ok((offsets, data))
    }
}

fn bad() -> Error {
    corruption("malformed column section")
}

impl Section {
    pub fn parse(bytes: Vec<u8>) -> Result<Section> {
        let mut header = Cursor { bytes: &bytes, pos: 0, end: bytes.len() };
        if header.u32()? as u32 != MAGIC {
            return Err(bad());
        }
        let count = header.u32()?;
        let mut chunks = Vec::with_capacity(count.min(1024));
        for _ in 0..count {
            let (table, rows, offset, len) = (header.u32()?, header.u32()?, header.u32()?, header.u32()?);
            let end = offset.checked_add(len).filter(|&e| e <= bytes.len()).ok_or_else(bad)?;
            let mut c = Cursor { bytes: &bytes, pos: offset, end };
            let keys = match c.u8()? {
                0 => Keys::Integer(c.take(rows.checked_mul(8).ok_or_else(bad)?)?),
                1 => {
                    let (offsets, data) = c.offsets_and_data(rows)?;
                    Keys::Bytes { offsets, data }
                }
                _ => return Err(bad()),
            };
            let deleted = match c.u8()? {
                0 => None,
                1 => Some(c.take(rows)?),
                _ => return Err(bad()),
            };
            let column_count = c.u32()?;
            let mut columns = Vec::with_capacity(column_count.min(1024));
            for _ in 0..column_count {
                let kind = c.u8()?;
                let valid = match c.u8()? {
                    0 => None,
                    1 => Some(c.take(rows)?),
                    _ => return Err(bad()),
                };
                let (data, offsets) = match kind {
                    NULLS => (0, None),
                    INTEGER | REAL => (c.take(rows.checked_mul(8).ok_or_else(bad)?)?, None),
                    BOOLEAN => (c.take(rows)?, None),
                    TEXT | MIXED => {
                        let (offsets, data) = c.offsets_and_data(rows)?;
                        (data, Some(offsets))
                    }
                    _ => return Err(bad()),
                };
                columns.push(ColumnInfo { kind, valid, data, offsets });
            }
            chunks.push(ChunkInfo { table: table as u32, rows, keys, deleted, columns });
        }
        Ok(Section { bytes, chunks })
    }

    /// The chunk holding table `id`'s rows in this file, if it has any.
    pub fn chunk(&self, id: u32) -> Option<Chunk<'_>> {
        self.chunks.iter().find(|c| c.table == id).map(|info| Chunk { bytes: &self.bytes, info })
    }

    /// The first chunk, if any: a memtable batch holds one table's rows.
    fn first_chunk(&self) -> Option<Chunk<'_>> {
        self.chunks.first().map(|info| Chunk { bytes: &self.bytes, info })
    }
}

/// One table's rows in one file.
#[derive(Clone, Copy)]
pub struct Chunk<'a> {
    bytes: &'a [u8],
    info: &'a ChunkInfo,
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

fn u32_at(bytes: &[u8], at: usize) -> usize {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize
}

impl<'a> Chunk<'a> {
    pub fn rows(&self) -> usize {
        self.info.rows
    }

    /// Row `i`'s full key, into `buf`.
    fn key_into(&self, i: usize, buf: &mut Vec<u8>) {
        buf.clear();
        buf.extend_from_slice(&[0, b'R']);
        buf.extend_from_slice(&self.info.table.to_be_bytes());
        match self.info.keys {
            Keys::Integer(at) => {
                buf.push(KEY_INTEGER);
                buf.extend_from_slice(&u64_at(self.bytes, at + 8 * i).to_be_bytes());
            }
            Keys::Bytes { offsets, data } => {
                let (start, end) = (u32_at(self.bytes, offsets + 4 * i), u32_at(self.bytes, offsets + 4 * i + 4));
                buf.extend_from_slice(&self.bytes[data + start..data + end]);
            }
        }
    }

    /// The first 7 bytes of every integer key in this chunk.
    fn int_prefix(&self) -> [u8; 7] {
        let t = self.info.table.to_be_bytes();
        [0, b'R', t[0], t[1], t[2], t[3], KEY_INTEGER]
    }

    /// Compares row `i`'s key with `key`, without building row `i`'s key
    /// when the chunk's keys are integers (one comparison of two numbers).
    fn cmp_at(&self, i: usize, key: &[u8]) -> Ordering {
        match self.info.keys {
            Keys::Integer(at) => {
                let prefix = self.int_prefix();
                let mine = u64_at(self.bytes, at + 8 * i);
                if key.len() == 15 && key[..7] == prefix {
                    return mine.cmp(&u64::from_be_bytes(key[7..15].try_into().unwrap()));
                }
                let mut full = [0u8; 15];
                full[..7].copy_from_slice(&prefix);
                full[7..].copy_from_slice(&mine.to_be_bytes());
                key_cmp(&full, key)
            }
            Keys::Bytes { offsets, data } => {
                let mut prefix = [0, b'R', 0, 0, 0, 0];
                prefix[2..].copy_from_slice(&self.info.table.to_be_bytes());
                let (start, end) = (u32_at(self.bytes, offsets + 4 * i), u32_at(self.bytes, offsets + 4 * i + 4));
                let suffix = &self.bytes[data + start..data + end];
                match key_cmp(&prefix, &key[..key.len().min(6)]) {
                    Ordering::Equal if key.len() >= 6 => key_cmp(suffix, &key[6..]),
                    Ordering::Equal => Ordering::Greater, // `key` is a prefix of ours
                    other => other,
                }
            }
        }
    }

    /// The first row in `[from, end)` whose key is at or after `key`, found
    /// by galloping from `from`: a run of a few rows costs a few comparisons.
    fn first_from(&self, from: usize, end: usize, key: &[u8]) -> usize {
        let (mut lo, mut hi, mut step) = (from, from, 1);
        while hi < end && self.cmp_at(hi, key).is_lt() {
            lo = hi + 1;
            hi += step;
            step *= 2;
        }
        let mut hi = hi.min(end);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.cmp_at(mid, key).is_lt() {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// The first row in `[from, end)` whose integer key is at or after
    /// `key`, galloping from `from`. Only for chunks with integer keys.
    fn first_int_from(&self, from: usize, end: usize, key: u64) -> usize {
        let Keys::Integer(at) = self.info.keys else { unreachable!("integer keys") };
        let k = |i: usize| u64_at(self.bytes, at + 8 * i);
        let (mut lo, mut hi, mut step) = (from, from, 1);
        while hi < end && k(hi) < key {
            lo = hi + 1;
            hi += step;
            step *= 2;
        }
        let mut hi = hi.min(end);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if k(mid) < key { lo = mid + 1 } else { hi = mid }
        }
        lo
    }

    /// Row `i`'s key as a number, if the chunk's keys are integers.
    fn int_key(&self, i: usize) -> Option<u64> {
        match self.info.keys {
            Keys::Integer(at) => Some(u64_at(self.bytes, at + 8 * i)),
            Keys::Bytes { .. } => None,
        }
    }

    /// The first row whose key is past `bound`'s start: at or after it if
    /// included, after it if excluded.
    fn first_at(&self, bound: Bound<&[u8]>, buf: &mut Vec<u8>) -> usize {
        let (key, inclusive) = match bound {
            Bound::Unbounded => return 0,
            Bound::Included(k) => (k, true),
            Bound::Excluded(k) => (k, false),
        };
        let (mut lo, mut hi) = (0, self.info.rows);
        while lo < hi {
            let mid = (lo + hi) / 2;
            self.key_into(mid, buf);
            match key_cmp(buf, key) {
                Ordering::Less => lo = mid + 1,
                Ordering::Equal if !inclusive => lo = mid + 1,
                _ => hi = mid,
            }
        }
        lo
    }

    fn deleted(&self, i: usize) -> bool {
        self.info.deleted.is_some_and(|at| self.bytes[at + i] != 0)
    }

    fn has_deletions(&self) -> bool {
        self.info.deleted.is_some()
    }

    pub fn column(&self, c: usize) -> Option<ColumnView<'a>> {
        self.info.columns.get(c).map(|info| ColumnView { bytes: self.bytes, info })
    }
}

/// One column of a chunk.
#[derive(Clone, Copy)]
pub struct ColumnView<'a> {
    bytes: &'a [u8],
    info: &'a ColumnInfo,
}

/// What a column holds, for copying it in bulk.
#[derive(PartialEq, Eq)]
pub enum Kind {
    Nulls,
    Integer,
    Real,
    Other,
}

impl<'a> ColumnView<'a> {
    pub fn kind(&self) -> Kind {
        match self.info.kind {
            NULLS => Kind::Nulls,
            INTEGER => Kind::Integer,
            REAL => Kind::Real,
            _ => Kind::Other,
        }
    }

    /// One byte per row, nonzero for a value; `None` if there are no NULLs.
    pub fn validity(&self, rows: Range<usize>) -> Option<&'a [u8]> {
        self.info.valid.map(|at| &self.bytes[at + rows.start..at + rows.end])
    }

    /// The raw 8-byte values of rows, for integer and real columns.
    pub fn words(&self, rows: Range<usize>) -> impl Iterator<Item = u64> + 'a {
        let at = self.info.data;
        self.bytes[at + 8 * rows.start..at + 8 * rows.end].as_chunks::<8>().0.iter().map(|w| u64::from_le_bytes(*w))
    }

    /// Row `i`'s value, whatever the kind.
    pub fn value(&self, i: usize) -> Result<Value> {
        if self.validity(i..i + 1).is_some_and(|v| v[0] == 0) {
            return Ok(Value::Null);
        }
        let at = self.info.data;
        Ok(match self.info.kind {
            NULLS => Value::Null,
            INTEGER => Value::Integer(u64_at(self.bytes, at + 8 * i) as i64),
            REAL => Value::Real(f64::from_bits(u64_at(self.bytes, at + 8 * i))),
            BOOLEAN => Value::Boolean(self.bytes[at + i] != 0),
            kind => {
                let offsets = self.info.offsets.expect("text and mixed columns have offsets");
                let (start, end) = (u32_at(self.bytes, offsets + 4 * i), u32_at(self.bytes, offsets + 4 * i + 4));
                let bytes = &self.bytes[at + start..at + end];
                if kind == TEXT { RawValue::Text(bytes).to_value()? } else { decode_value(&mut Reader::new(bytes))? }
            }
        })
    }
}

// ------------------------------------------------------------------ scanning

/// Receives what a [`scan`] finds: rows from memtables (and from files
/// written without a column section), and runs of a chunk's rows.
pub trait Sink {
    fn row(&mut self, data: &[u8]) -> Result<()>;
    /// Several rows at once.
    fn rows(&mut self, rows: &[&[u8]]) -> Result<()> {
        rows.iter().try_for_each(|row| self.row(row))
    }
    /// The rows in `rows`, in key order, except those in `skip` (sorted
    /// positions inside `rows`): deleted, or with a newer version elsewhere.
    fn chunk(&mut self, chunk: &Chunk, rows: Range<usize>, skip: &[usize]) -> Result<()>;
}

/// Rows a memtable source lays out per hold of the memtable's read lock.
const MEM_BATCH: usize = 4096;

/// A memtable's rows in range, laid out as a chunk a batch at a time (only
/// the needed columns decoded), so the merge treats them like any file's.
struct MemRows<'a> {
    mem: &'a Arc<MemTable>,
    /// The snapshot read at, if any.
    seq: Option<u64>,
    needed: &'a [bool],
    upper: &'a Bound<Vec<u8>>,
    /// Where the next batch starts.
    from: Bound<Vec<u8>>,
    section: Option<Section>,
    pos: usize,
    end: usize,
    done: bool,
}

impl MemRows<'_> {
    /// Lays out the next batch once the current one is used up.
    fn refill(&mut self) -> Result<()> {
        if self.pos < self.end || self.done {
            return Ok(());
        }
        let mut writer = SectionWriter::only(self.needed);
        let (mut count, mut last, mut ended) = (0, Vec::new(), true);
        let from = match &self.from {
            Bound::Included(k) => Bound::Included(k.as_slice()),
            Bound::Excluded(k) => Bound::Excluded(k.as_slice()),
            Bound::Unbounded => Bound::Unbounded,
        };
        let upper = self.upper;
        self.mem.visit_range(from, self.seq, |key, value| {
            if !before_end(key, upper) {
                return false;
            }
            writer.add(key, value);
            count += 1;
            if count == MEM_BATCH {
                last = key.to_vec();
                ended = false;
                return false;
            }
            true
        });
        self.done = ended;
        self.from = Bound::Excluded(last);
        let bytes = writer.finish();
        self.section = if bytes.is_empty() { None } else { Some(Section::parse(bytes)?) };
        self.pos = 0;
        self.end = self.chunk().map_or(0, |c| c.rows());
        Ok(())
    }

    fn chunk(&self) -> Option<Chunk<'_>> {
        self.section.as_ref().and_then(Section::first_chunk)
    }
}

/// The newest-first sources of a scan.
enum Source<'a> {
    /// Rows from a file without a column section.
    Rows(Box<dyn KvIter + 'a>),
    Cols {
        chunk: Chunk<'a>,
        pos: usize,
        end: usize,
    },
    Mem(MemRows<'a>),
}

fn before_end(key: &[u8], upper: &Bound<Vec<u8>>) -> bool {
    match upper {
        Bound::Included(end) => key_cmp(key, end).is_le(),
        Bound::Excluded(end) => key_cmp(key, end).is_lt(),
        Bound::Unbounded => true,
    }
}

/// A source's current position, seen the same way whether it is rows or a chunk.
enum At<'s> {
    Row(&'s [u8]),
    Col(Chunk<'s>, usize),
}

impl At<'_> {
    fn cmp(&self, other: &At) -> Ordering {
        match (self, other) {
            (At::Row(a), At::Row(b)) => key_cmp(a, b),
            (At::Col(chunk, i), At::Row(key)) => chunk.cmp_at(*i, key),
            (At::Row(key), At::Col(chunk, i)) => chunk.cmp_at(*i, key).reverse(),
            (At::Col(a, i), At::Col(b, j)) => match (a.int_key(*i), b.int_key(*j)) {
                // The same table, so the same key prefix: just the numbers.
                (Some(x), Some(y)) => x.cmp(&y),
                _ => {
                    let mut key = Vec::new();
                    b.key_into(*j, &mut key);
                    a.cmp_at(*i, &key)
                }
            },
        }
    }

    fn cmp_key(&self, key: &[u8]) -> Ordering {
        match self {
            At::Row(k) => key_cmp(k, key),
            At::Col(chunk, i) => chunk.cmp_at(*i, key),
        }
    }

    fn key_into(&self, buf: &mut Vec<u8>) {
        match self {
            At::Row(k) => {
                buf.clear();
                buf.extend_from_slice(k);
            }
            At::Col(chunk, i) => chunk.key_into(*i, buf),
        }
    }
}

impl Source<'_> {
    /// Gets the source ready to be looked at: a memtable lays out its next batch.
    fn prepare(&mut self) -> Result<()> {
        match self {
            Source::Mem(mem) => mem.refill(),
            _ => Ok(()),
        }
    }

    /// The current position, if the source has a key left in range. Call
    /// `prepare` first.
    fn at(&self, upper: &Bound<Vec<u8>>) -> Option<At<'_>> {
        match self {
            Source::Rows(it) => (it.valid() && before_end(it.key(), upper)).then(|| At::Row(it.key())),
            Source::Cols { chunk, pos, end } => (pos < end).then_some(At::Col(*chunk, *pos)),
            Source::Mem(mem) => (mem.pos < mem.end).then(|| At::Col(mem.chunk().unwrap(), mem.pos)),
        }
    }

    /// Hands over the current entry, unless it's a deletion.
    fn emit_one(&mut self, sink: &mut dyn Sink) -> Result<()> {
        let (chunk, pos) = match self {
            Source::Rows(it) => {
                return match it.value() {
                    Some(row) => sink.row(row),
                    None => Ok(()),
                };
            }
            Source::Cols { chunk, pos, .. } => (*chunk, *pos),
            Source::Mem(mem) => (mem.chunk().unwrap(), mem.pos),
        };
        if chunk.deleted(pos) { Ok(()) } else { sink.chunk(&chunk, pos..pos + 1, &[]) }
    }

    fn step(&mut self) {
        match self {
            Source::Rows(it) => it.next(),
            Source::Cols { pos, .. } => *pos += 1,
            Source::Mem(mem) => mem.pos += 1,
        }
    }

    /// Hands over every entry before `bound` (the next source's key), all of
    /// which are this source's to give.
    fn run(&mut self, bound: Option<&[u8]>, upper: &Bound<Vec<u8>>, sink: &mut dyn Sink) -> Result<()> {
        let (chunk, pos, end) = match self {
            Source::Rows(it) => {
                while it.valid() && before_end(it.key(), upper) && bound.is_none_or(|b| key_cmp(it.key(), b).is_lt()) {
                    if let Some(row) = it.value() {
                        sink.row(row)?;
                    }
                    it.next();
                }
                return Ok(());
            }
            Source::Cols { chunk, pos, end } => (*chunk, *pos, *end),
            Source::Mem(mem) => (mem.chunk().unwrap(), mem.pos, mem.end),
        };
        let stop = bound.map_or(end, |b| chunk.first_from(pos, end, b));
        if !chunk.has_deletions() {
            sink.chunk(&chunk, pos..stop, &[])?;
        } else {
            // Live stretches only: deletions are skipped.
            let mut start = pos;
            for i in pos..stop {
                if chunk.deleted(i) {
                    if start < i {
                        sink.chunk(&chunk, start..i, &[])?;
                    }
                    start = i + 1;
                }
            }
            if start < stop {
                sink.chunk(&chunk, start..stop, &[])?;
            }
        }
        match self {
            Source::Cols { pos, .. } => *pos = stop,
            Source::Mem(mem) => mem.pos = stop,
            Source::Rows(_) => unreachable!(),
        }
        Ok(())
    }
}

fn overlaps(table: &Table, lower: &Bound<Vec<u8>>, upper: &Bound<Vec<u8>>) -> bool {
    let (smallest, largest) = (table.meta.smallest.as_slice(), table.meta.largest.as_slice());
    let after_lower = match lower {
        Bound::Included(k) => largest >= k.as_slice(),
        Bound::Excluded(k) => largest > k.as_slice(),
        Bound::Unbounded => true,
    };
    after_lower && before_end(smallest, upper)
}

/// Hands over the rows in `rows` of `chunk` whose keys aren't in `shadow`
/// (keys a newer source has, sorted), and which aren't deletions: the whole
/// range at once, with the rows to pass over listed in `skip`.
fn emit_unshadowed(
    chunk: &Chunk,
    rows: Range<usize>,
    shadow: &[u64],
    skip: &mut Vec<usize>,
    sink: &mut dyn Sink,
) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    skip.clear();
    let key = |i: usize| chunk.int_key(i).expect("integer keys");
    let (first, last) = (key(rows.start), key(rows.end - 1));
    let mut from = rows.start;
    for &hidden in &shadow[shadow.partition_point(|&k| k < first)..] {
        if hidden > last {
            break;
        }
        let j = chunk.first_int_from(from, rows.end, hidden);
        if j < rows.end && key(j) == hidden {
            skip.push(j);
            from = j + 1;
        } else {
            from = j;
        }
    }
    if chunk.has_deletions() {
        skip.extend(rows.clone().filter(|&i| chunk.deleted(i)));
        skip.sort_unstable();
        skip.dedup();
    }
    sink.chunk(chunk, rows, skip)
}

/// Merges two sorted lists of keys, without duplicates.
fn union(a: Vec<u64>, b: Vec<u64>) -> Vec<u64> {
    if a.is_empty() {
        return b;
    }
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            Ordering::Less => {
                out.push(a[i]);
                i += 1;
            }
            Ordering::Greater => {
                out.push(b[j]);
                j += 1;
            }
            Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

/// Every live row of `table` with a key in `[lower, upper]`, from the view's
/// memtables and files, handed to `sink`: from column chunks where files
/// have them. Each key's newest version counts, as in any read.
pub fn scan(
    view: &ReadView,
    table: &TableSchema,
    needed: &[bool],
    lower: &Bound<Vec<u8>>,
    upper: &Bound<Vec<u8>>,
    sink: &mut dyn Sink,
) -> Result<()> {
    let start = match lower {
        Bound::Included(k) => Bound::Included(k.as_slice()),
        Bound::Excluded(k) => Bound::Excluded(k.as_slice()),
        Bound::Unbounded => Bound::Unbounded,
    };
    let seek = |it: &mut dyn KvIter| match start {
        Bound::Included(k) => it.seek(k),
        Bound::Excluded(k) => {
            it.seek(k);
            if it.valid() && it.key() == k {
                it.next();
            }
        }
        Bound::Unbounded => it.seek(&[]),
    };
    // Newest first: memtables, level 0, then level 1.
    let l0 = view.level0().iter().filter(|t| overlaps(t, lower, upper)).count();
    let files: Vec<&Arc<Table>> =
        view.level0().iter().chain(view.level1()).filter(|t| overlaps(t, lower, upper)).collect();
    let sections = files.iter().map(|t| t.columnar(Section::parse)).collect::<Result<Vec<_>>>()?;
    let mut buf = Vec::new();

    // Tables keyed by integers (an INTEGER PRIMARY KEY, or the hidden row
    // id) with a section in every file: rows can go in any order, so each
    // source in turn hands over whatever no newer source has a version of.
    let integer_keys = table.primary_key.is_empty() || table.auto_increment_column().is_some();
    if integer_keys && sections.iter().all(Option::is_some) {
        let prefix = {
            let t = table.id.to_be_bytes();
            [0, b'R', t[0], t[1], t[2], t[3], KEY_INTEGER]
        };
        let as_int = |key: &[u8]| {
            (key.len() == 15 && key[..7] == prefix).then(|| u64::from_be_bytes(key[7..].try_into().unwrap()))
        };
        // The upper bound as a number, when it is one of this table's keys;
        // otherwise every key of the table is below it.
        let end = match upper {
            Bound::Included(k) => as_int(k).map(|n| (n, true)),
            Bound::Excluded(k) => as_int(k).map(|n| (n, false)),
            Bound::Unbounded => None,
        };
        let below_end = |n: u64| end.is_none_or(|(e, inclusive)| n < e || (inclusive && n == e));
        let (mut shadow, mut skip): (Vec<u64>, Vec<usize>) = (Vec::new(), Vec::new());
        for mem in view.memtables() {
            // Rows go straight to the sink, a batch per hold of the
            // memtable's read lock; their keys hide older versions.
            let (mut keys, mut from, mut failed) = (Vec::new(), lower.clone(), None);
            loop {
                let (mut count, mut last, mut more) = (0, Vec::new(), false);
                let start = match &from {
                    Bound::Included(k) => Bound::Included(k.as_slice()),
                    Bound::Excluded(k) => Bound::Excluded(k.as_slice()),
                    Bound::Unbounded => Bound::Unbounded,
                };
                mem.with_range(start, view.seq(), |entries| {
                    // Live rows collected under the lock, handed over together.
                    let mut rows: Vec<&[u8]> = Vec::with_capacity(MEM_BATCH);
                    for (key, value) in entries {
                        let Some(n) = as_int(key) else {
                            // Past this table's rows; anything else would be
                            // a row key of the wrong form.
                            if before_end(key, upper) {
                                failed = Some(corruption("a row key that isn't an integer in an integer-keyed table"));
                            }
                            break;
                        };
                        if !below_end(n) {
                            break;
                        }
                        keys.push(n);
                        if let Some(row) = value
                            && (shadow.is_empty() || shadow.binary_search(&n).is_err())
                        {
                            rows.push(row);
                        }
                        count += 1;
                        if count == MEM_BATCH {
                            last = key.to_vec();
                            more = true;
                            break;
                        }
                    }
                    if failed.is_none()
                        && let Err(e) = sink.rows(&rows)
                    {
                        failed = Some(e);
                    }
                });
                if let Some(e) = failed {
                    return Err(e);
                }
                if !more {
                    break;
                }
                from = Bound::Excluded(last);
            }
            shadow = union(shadow, keys);
        }
        for (n, section) in sections.iter().enumerate() {
            let Some(chunk) = section.as_ref().and_then(|s| s.chunk(table.id)) else { continue };
            let pos = chunk.first_at(start, &mut buf);
            let end = match upper {
                Bound::Included(k) => chunk.first_at(Bound::Excluded(k), &mut buf),
                Bound::Excluded(k) => chunk.first_at(Bound::Included(k), &mut buf),
                Bound::Unbounded => chunk.rows(),
            };
            emit_unshadowed(&chunk, pos..end, &shadow, &mut skip, sink)?;
            // Level 1 comes last and its files don't overlap: only level 0's
            // keys can hide anything further down.
            if n < l0 && pos < end {
                shadow = union(shadow, (pos..end).map(|i| chunk.int_key(i).expect("integer keys")).collect());
            }
        }
        return Ok(());
    }
    let mut sources: Vec<Source> = Vec::new();
    for mem in view.memtables() {
        sources.push(Source::Mem(MemRows {
            mem,
            seq: view.seq(),
            needed,
            upper,
            from: lower.clone(),
            section: None,
            pos: 0,
            end: 0,
            done: false,
        }));
    }
    for (file, section) in files.iter().zip(&sections) {
        match section {
            // A section holds every row in its file, so no chunk means none
            // of this table's rows are there.
            Some(section) => {
                if let Some(chunk) = section.chunk(table.id) {
                    let pos = chunk.first_at(start, &mut buf);
                    let end = match upper {
                        Bound::Included(k) => chunk.first_at(Bound::Excluded(k), &mut buf),
                        Bound::Excluded(k) => chunk.first_at(Bound::Included(k), &mut buf),
                        Bound::Unbounded => chunk.rows(),
                    };
                    if pos < end {
                        sources.push(Source::Cols { chunk, pos, end });
                    }
                }
            }
            None => {
                let mut it = file.iter(true);
                seek(&mut it);
                sources.push(Source::Rows(Box::new(it)));
            }
        }
    }

    let mut bound = Vec::new();
    loop {
        for source in &mut sources {
            source.prepare()?;
        }
        // The source with the smallest key (the newest, on a tie), and the
        // one with the next smallest.
        let (mut current, mut runner_up) = (None::<usize>, None::<usize>);
        for i in 0..sources.len() {
            let Some(at) = sources[i].at(upper) else { continue };
            match current {
                None => current = Some(i),
                Some(c) if at.cmp(&sources[c].at(upper).unwrap()).is_lt() => {
                    runner_up = current;
                    current = Some(i);
                }
                Some(_) => {
                    if runner_up.is_none_or(|r| at.cmp(&sources[r].at(upper).unwrap()).is_lt()) {
                        runner_up = Some(i);
                    }
                }
            }
        }
        let Some(c) = current else { break };
        match runner_up {
            Some(r) if sources[r].at(upper).unwrap().cmp(&sources[c].at(upper).unwrap()).is_eq() => {
                // A key in several sources: the newest version counts, the
                // older ones are passed over.
                sources[c].at(upper).unwrap().key_into(&mut bound);
                sources[c].emit_one(sink)?;
                for s in &mut sources {
                    s.prepare()?;
                    if s.at(upper).is_some_and(|at| at.cmp_key(&bound).is_eq()) {
                        s.step();
                    }
                }
            }
            Some(r) => {
                sources[r].at(upper).unwrap().key_into(&mut bound);
                sources[c].run(Some(&bound), upper, sink)?;
            }
            None => sources[c].run(None, upper, sink)?,
        }
    }
    for source in &sources {
        if let Source::Rows(it) = source {
            it.status()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::encoding::encode_row;

    fn row_key(table: u32, id: i64) -> Vec<u8> {
        let mut k = vec![0, b'R'];
        k.extend_from_slice(&table.to_be_bytes());
        super::super::encoding::encode_key(&mut k, &Value::Integer(id));
        k
    }

    /// Collects what a scan hands over, as rows of values.
    #[derive(Default)]
    struct Collect {
        rows: Vec<Vec<Value>>,
        from_chunks: usize,
        width: usize,
    }

    impl Sink for Collect {
        fn row(&mut self, data: &[u8]) -> Result<()> {
            self.rows.push(super::super::encoding::decode_row(data, self.width)?);
            Ok(())
        }

        fn chunk(&mut self, chunk: &Chunk, rows: Range<usize>, skip: &[usize]) -> Result<()> {
            for i in rows.filter(|i| skip.binary_search(i).is_err()) {
                let row = (0..self.width)
                    .map(|c| chunk.column(c).map_or(Ok(Value::Null), |col| col.value(i)))
                    .collect::<Result<_>>()?;
                self.rows.push(row);
                self.from_chunks += 1;
            }
            Ok(())
        }
    }

    /// Fills table `name` (keyed by `key_type`) across several files and
    /// the memtable, with newer versions of rows in later files and in the
    /// memtable: updates and deletions.
    fn spread_out(db: &crate::sql::Database, name: &str, key_type: &str) {
        db.execute(&format!("CREATE TABLE {name} (id {key_type} PRIMARY KEY, label TEXT, score REAL)")).unwrap();
        let insert = db.prepare(&format!("INSERT INTO {name} VALUES (?, ?, ?)")).unwrap();
        let mut session = db.session();
        let id = |i: i64| if key_type == "TEXT" { Value::Text(format!("k{i:06}")) } else { Value::Integer(i) };
        for round in 0..4 {
            session.execute("BEGIN").unwrap();
            for i in 0..1500 {
                let n = round * 1500 + i;
                let score = if n % 9 == 0 { Value::Null } else { Value::Real(n as f64 / 3.0) };
                session.execute_prepared(&insert, &[id(n), Value::Text(format!("n{n}")), score]).unwrap();
            }
            session.execute("COMMIT").unwrap();
            db.kv().flush().unwrap();
        }
        db.execute(&format!("UPDATE {name} SET label = 'updated' WHERE score > 900")).unwrap();
        db.execute(&format!("DELETE FROM {name} WHERE score < 100")).unwrap();
        db.kv().flush().unwrap();
        db.execute(&format!("UPDATE {name} SET score = -1.0 WHERE label = 'updated' AND score < 1200")).unwrap();
        db.execute(&format!("DELETE FROM {name} WHERE score > 1900")).unwrap();
        let insert = db.prepare(&format!("INSERT INTO {name} VALUES (?, 'last', 2.5)")).unwrap();
        session.execute_prepared(&insert, &[id(100_000)]).unwrap();
    }

    #[test]
    fn scans_merge_chunks_and_memtables_like_any_read() {
        use crate::options::Options;
        use crate::sql::Database;
        use crate::testutil::TempDir;
        let dir = TempDir::new("columnar-scan");
        let db = Database::open_with(dir.path(), Options { memtable_size: 64 << 10, ..Options::default() }).unwrap();
        // Integer keys take the order-free path; text keys the ordered merge.
        for (name, key_type) in [("ints", "INTEGER"), ("texts", "TEXT")] {
            spread_out(&db, name, key_type);
            let schema = {
                let txn = db.read_txn();
                super::super::catalog::require_table(&txn, name).unwrap()
            };
            let sorted = |mut rows: Vec<Vec<Value>>| {
                rows.sort_by(|a, b| a[0].total_cmp(&b[0]));
                rows
            };
            let expected = db.query(&format!("SELECT * FROM {name}"), &[]).unwrap().rows;
            let view = db.kv().read_view();
            let (lower, upper) = schema.rows_range();
            let mut all = Collect { width: 3, ..Collect::default() };
            scan(&view, &schema, &[true; 3], &lower, &upper, &mut all).unwrap();
            assert_eq!(sorted(all.rows), expected, "{name}");
            assert!(
                all.from_chunks > expected.len() / 2,
                "{name}: {} of {} from chunks",
                all.from_chunks,
                expected.len()
            );

            // Any sub-range, with either kind of bound.
            let key = |row: &Vec<Value>| schema.row_key(row, None);
            for (lo, hi) in [(10, 20), (1499, 1501), (2000, 4500), (0, 1), (expected.len() - 3, expected.len() - 1)] {
                let (lo_key, hi_key) = (key(&expected[lo]), key(&expected[hi]));
                for (lower, upper, want) in [
                    (Bound::Included(lo_key.clone()), Bound::Excluded(hi_key.clone()), &expected[lo..hi]),
                    (Bound::Excluded(lo_key.clone()), Bound::Included(hi_key.clone()), &expected[lo + 1..=hi]),
                ] {
                    let mut part = Collect { width: 3, ..Collect::default() };
                    scan(&view, &schema, &[true; 3], &lower, &upper, &mut part).unwrap();
                    assert_eq!(sorted(part.rows), want, "{name} {lower:?}..{upper:?}");
                }
            }
            // Only some columns: the rest read as NULL.
            let mut some = Collect { width: 3, ..Collect::default() };
            scan(&view, &schema, &[false, false, true], &lower, &upper, &mut some).unwrap();
            let mut scores: Vec<Value> = some.rows.iter().map(|r| r[2].clone()).collect();
            let mut want: Vec<Value> = expected.iter().map(|r| r[2].clone()).collect();
            scores.sort_by(Value::total_cmp);
            want.sort_by(Value::total_cmp);
            assert_eq!(scores, want, "{name}");
        }
    }

    #[test]
    fn sections_round_trip_every_kind_of_column() {
        let mut writer = SectionWriter::default();
        writer.add(b"\x00T catalog", Some(b"ignored"));
        let rows: Vec<Vec<Value>> = (0..300)
            .map(|i| {
                vec![
                    Value::Integer(i),
                    if i % 5 == 0 { Value::Null } else { Value::Real(i as f64 / 4.0) },
                    Value::Text(format!("name {i}")),
                    // Mostly integers, then text: becomes a mixed column.
                    if i == 200 { Value::Text("odd".into()) } else { Value::Integer(i * 2) },
                    Value::Boolean(i % 2 == 0),
                ]
            })
            .collect();
        for (i, row) in rows.iter().enumerate() {
            let value = (i % 50 != 7).then(|| encode_row(row));
            writer.add(&row_key(3, i as i64), value.as_deref());
        }
        writer.add(&row_key(4, 1), Some(&encode_row(&[Value::Integer(9)])));
        let section = Section::parse(writer.finish()).unwrap();
        let chunk = section.chunk(3).unwrap();
        assert_eq!(chunk.rows(), 300);
        assert!(section.chunk(4).is_some() && section.chunk(5).is_none());
        let mut buf = Vec::new();
        for (i, row) in rows.iter().enumerate() {
            chunk.key_into(i, &mut buf);
            assert_eq!(buf, row_key(3, i as i64));
            assert_eq!(chunk.deleted(i), i % 50 == 7);
            if !chunk.deleted(i) {
                for (c, want) in row.iter().enumerate() {
                    assert_eq!(&chunk.column(c).unwrap().value(i).unwrap(), want, "row {i} column {c}");
                }
            }
        }
        assert!(chunk.column(3).unwrap().kind() == Kind::Other, "mixed");
        assert_eq!(chunk.first_at(Bound::Included(&row_key(3, 150)), &mut buf), 150);
        assert_eq!(chunk.first_at(Bound::Excluded(&row_key(3, 150)), &mut buf), 151);
        assert_eq!(chunk.first_at(Bound::Included(&row_key(3, 1000)), &mut buf), 300);

        // Damage anywhere is caught, not trusted.
        let bytes = {
            let mut w = SectionWriter::default();
            w.add(&row_key(1, 1), Some(&encode_row(&[Value::Text("x".into())])));
            w.finish()
        };
        for cut in 1..bytes.len() {
            assert!(Section::parse(bytes[..cut].to_vec()).is_err(), "cut at {cut}");
        }
    }
}
