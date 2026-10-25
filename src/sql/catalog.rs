//! The catalog: table and index definitions, stored in the key-value store
//! next to the data they describe.
//!
//! Every SQL key starts with a 0x00 byte, keeping SQL data apart from plain
//! key-value data:
//!
//! ```text
//! 00 'T' <table name>                                  table schema
//! 00 'I' <index name>                                  index schema
//! 00 'N'                                               next table/index id
//! 00 'S' <table id: u32>                               next row id
//! 00 'R' <table id: u32> <primary key>                 row
//! 00 'X' <index id: u32> <indexed values> <primary key>  index entry (empty value)
//! ```

use std::collections::HashMap;
use std::ops::Bound;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use crate::coding::{Reader, put_bytes, put_varint};
use crate::error::{Result, corruption};

use super::encoding::{decode_value, encode_key, encode_keys, encode_value, prefix_end};
use super::txn::Txn;
use super::value::{DataType, Value, sql_error};

/// Length of a row key's prefix (`00 'R' <table id>`); the encoded primary key follows.
pub const ROW_PREFIX_LEN: usize = 6;

fn key(kind: u8, rest: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(2 + rest.len());
    k.extend_from_slice(&[0, kind]);
    k.extend_from_slice(rest);
    k
}

fn prefix_range(prefix: Vec<u8>) -> (Bound<Vec<u8>>, Bound<Vec<u8>>) {
    let end = prefix_end(&prefix).map_or(Bound::Unbounded, Bound::Excluded);
    (Bound::Included(prefix), end)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Column {
    pub name: String,
    pub ty: DataType,
    pub not_null: bool,
    pub default: Option<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TableSchema {
    pub id: u32,
    pub name: String,
    pub columns: Vec<Column>,
    /// Column positions forming the primary key. Empty means the table is
    /// keyed by a hidden, auto-assigned row id.
    pub primary_key: Vec<usize>,
}

impl TableSchema {
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    pub fn row_prefix(&self) -> Vec<u8> {
        key(b'R', &self.id.to_be_bytes())
    }

    pub fn rows_range(&self) -> (Bound<Vec<u8>>, Bound<Vec<u8>>) {
        prefix_range(self.row_prefix())
    }

    /// The key for `row`. Tables without a primary key use `rowid`.
    pub fn row_key(&self, row: &[Value], rowid: Option<i64>) -> Vec<u8> {
        let mut k = self.row_prefix();
        if self.primary_key.is_empty() {
            encode_key(&mut k, &Value::Integer(rowid.expect("a row id for a table without a primary key")));
        } else {
            self.primary_key.iter().for_each(|&c| encode_key(&mut k, &row[c]));
        }
        k
    }

    /// The single INTEGER primary-key column, which is auto-assigned when omitted.
    pub fn auto_increment_column(&self) -> Option<usize> {
        match self.primary_key[..] {
            [c] if self.columns[c].ty == DataType::Integer => Some(c),
            _ => None,
        }
    }

    pub fn sequence_key(&self) -> [u8; 6] {
        let id = self.id.to_be_bytes();
        [0, b'S', id[0], id[1], id[2], id[3]]
    }

    /// The table as a CREATE TABLE statement.
    pub fn to_sql(&self) -> String {
        let mut defs: Vec<String> = self
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let mut def = format!("{} {}", c.name, c.ty);
                if self.primary_key == [i] {
                    def.push_str(" PRIMARY KEY");
                } else if c.not_null {
                    def.push_str(" NOT NULL");
                }
                if let Some(v) = &c.default {
                    def.push_str(&format!(" DEFAULT {}", v.literal()));
                }
                def
            })
            .collect();
        if self.primary_key.len() > 1 {
            let cols: Vec<&str> = self.primary_key.iter().map(|&c| self.columns[c].name.as_str()).collect();
            defs.push(format!("PRIMARY KEY ({})", cols.join(", ")));
        }
        format!("CREATE TABLE {} ({})", self.name, defs.join(", "))
    }

    fn encode(&self) -> Vec<u8> {
        let mut b = vec![1]; // format version
        put_varint(&mut b, self.id.into());
        put_bytes(&mut b, self.name.as_bytes());
        put_varint(&mut b, self.columns.len() as u64);
        for c in &self.columns {
            put_bytes(&mut b, c.name.as_bytes());
            b.push(type_code(c.ty));
            b.push(u8::from(c.not_null));
            match &c.default {
                None => b.push(0),
                Some(v) => {
                    b.push(1);
                    encode_value(&mut b, v);
                }
            }
        }
        put_varint(&mut b, self.primary_key.len() as u64);
        self.primary_key.iter().for_each(|&c| put_varint(&mut b, c as u64));
        b
    }

    fn decode(data: &[u8]) -> Result<TableSchema> {
        let mut r = Reader::new(data);
        if r.u8()? != 1 {
            return Err(corruption("unknown table schema version"));
        }
        let id = r.varint()? as u32;
        let name = string(&mut r)?;
        let mut columns = Vec::new();
        for _ in 0..r.varint()? {
            columns.push(Column {
                name: string(&mut r)?,
                ty: type_from_code(r.u8()?)?,
                not_null: r.u8()? != 0,
                default: if r.u8()? != 0 { Some(decode_value(&mut r)?) } else { None },
            });
        }
        let mut primary_key = Vec::new();
        for _ in 0..r.varint()? {
            primary_key.push(r.varint()? as usize);
        }
        Ok(TableSchema { id, name, columns, primary_key })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct IndexSchema {
    pub id: u32,
    pub name: String,
    pub table: String,
    pub columns: Vec<usize>,
    pub unique: bool,
}

impl IndexSchema {
    pub fn prefix(&self) -> Vec<u8> {
        key(b'X', &self.id.to_be_bytes())
    }

    /// The common prefix of every entry whose leading indexed values are `values`.
    pub fn values_prefix(&self, values: &[Value]) -> Vec<u8> {
        let mut k = self.prefix();
        encode_keys(&mut k, values);
        k
    }

    pub fn values_of(&self, row: &[Value]) -> Vec<Value> {
        self.columns.iter().map(|&c| row[c].clone()).collect()
    }

    /// The entry for a row: its indexed values, then its encoded primary key.
    pub fn entry_key(&self, row: &[Value], encoded_pk: &[u8]) -> Vec<u8> {
        let mut k = Vec::with_capacity(ROW_PREFIX_LEN + 16 * self.columns.len() + encoded_pk.len());
        k.extend_from_slice(&[0, b'X']);
        k.extend_from_slice(&self.id.to_be_bytes());
        self.columns.iter().for_each(|&c| encode_key(&mut k, &row[c]));
        k.extend_from_slice(encoded_pk);
        k
    }

    pub fn to_sql(&self, table: &TableSchema) -> String {
        let cols: Vec<&str> = self.columns.iter().map(|&c| table.columns[c].name.as_str()).collect();
        let unique = if self.unique { "UNIQUE " } else { "" };
        format!("CREATE {unique}INDEX {} ON {} ({})", self.name, self.table, cols.join(", "))
    }

    fn encode(&self) -> Vec<u8> {
        let mut b = vec![1];
        put_varint(&mut b, self.id.into());
        put_bytes(&mut b, self.name.as_bytes());
        put_bytes(&mut b, self.table.as_bytes());
        put_varint(&mut b, self.columns.len() as u64);
        self.columns.iter().for_each(|&c| put_varint(&mut b, c as u64));
        b.push(u8::from(self.unique));
        b
    }

    fn decode(data: &[u8]) -> Result<IndexSchema> {
        let mut r = Reader::new(data);
        if r.u8()? != 1 {
            return Err(corruption("unknown index schema version"));
        }
        let id = r.varint()? as u32;
        let name = string(&mut r)?;
        let table = string(&mut r)?;
        let mut columns = Vec::new();
        for _ in 0..r.varint()? {
            columns.push(r.varint()? as usize);
        }
        Ok(IndexSchema { id, name, table, columns, unique: r.u8()? != 0 })
    }
}

fn string(r: &mut Reader) -> Result<String> {
    String::from_utf8(r.len_prefixed()?.to_vec()).map_err(|_| corruption("schema name is not UTF-8"))
}

fn type_code(ty: DataType) -> u8 {
    match ty {
        DataType::Integer => 1,
        DataType::Real => 2,
        DataType::Text => 3,
        DataType::Boolean => 4,
    }
}

fn type_from_code(code: u8) -> Result<DataType> {
    Ok(match code {
        1 => DataType::Integer,
        2 => DataType::Real,
        3 => DataType::Text,
        4 => DataType::Boolean,
        _ => return Err(corruption(format!("unknown column type code {code}"))),
    })
}

/// Caches table definitions with their indexes, so statements don't re-read
/// and decode them from storage every time. Transactions that change the
/// schema bypass the cache and clear it when they commit.
#[derive(Default)]
pub struct SchemaCache {
    /// Bumped on every clear, so a lookup that raced with a schema change
    /// doesn't put the stale definition back.
    version: AtomicU64,
    tables: RwLock<HashMap<String, CachedTable>>,
}

/// Shared, so a statement takes a definition without copying it.
#[derive(Clone)]
struct CachedTable {
    schema: Option<Arc<TableSchema>>,
    indexes: Arc<[Arc<IndexSchema>]>,
}

impl SchemaCache {
    fn lookup(&self, txn: &Txn, name: &str) -> Result<CachedTable> {
        if let Some(hit) = self.tables.read().unwrap().get(name) {
            return Ok(hit.clone());
        }
        let version = self.version.load(Ordering::SeqCst);
        let entry = CachedTable { schema: load_table(txn, name)?, indexes: load_indexes(txn, name)? };
        let mut tables = self.tables.write().unwrap();
        if self.version.load(Ordering::SeqCst) == version {
            tables.insert(name.to_string(), entry.clone());
        }
        Ok(entry)
    }

    /// Changes whenever the cache is cleared, that is, after every committed schema change.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::SeqCst)
    }

    pub fn invalidate(&self) {
        let mut tables = self.tables.write().unwrap();
        self.version.fetch_add(1, Ordering::SeqCst);
        tables.clear();
    }
}

pub fn get_table(txn: &Txn, name: &str) -> Result<Option<Arc<TableSchema>>> {
    match txn.schema_cache() {
        Some(cache) => Ok(cache.lookup(txn, name)?.schema),
        None => load_table(txn, name),
    }
}

fn load_table(txn: &Txn, name: &str) -> Result<Option<Arc<TableSchema>>> {
    txn.get(&key(b'T', name.as_bytes()))?.map(|data| TableSchema::decode(&data).map(Arc::new)).transpose()
}

pub fn require_table(txn: &Txn, name: &str) -> Result<Arc<TableSchema>> {
    get_table(txn, name)?.ok_or_else(|| sql_error(format!("no such table: {name}")))
}

pub fn put_table(txn: &mut Txn, table: &TableSchema) {
    txn.mark_schema_changed();
    txn.put(key(b'T', table.name.as_bytes()), table.encode());
}

pub fn delete_table(txn: &mut Txn, name: &str) {
    txn.mark_schema_changed();
    txn.delete(key(b'T', name.as_bytes()));
}

pub fn list_tables(txn: &Txn) -> Result<Vec<TableSchema>> {
    let (lower, upper) = prefix_range(key(b'T', &[]));
    let mut tables = Vec::new();
    txn.scan(lower, upper, &mut |_, data| {
        tables.push(TableSchema::decode(data)?);
        Ok(true)
    })?;
    Ok(tables)
}

pub fn get_index(txn: &Txn, name: &str) -> Result<Option<IndexSchema>> {
    txn.get(&key(b'I', name.as_bytes()))?.map(|data| IndexSchema::decode(&data)).transpose()
}

pub fn put_index(txn: &mut Txn, index: &IndexSchema) {
    txn.mark_schema_changed();
    txn.put(key(b'I', index.name.as_bytes()), index.encode());
}

pub fn delete_index(txn: &mut Txn, name: &str) {
    txn.mark_schema_changed();
    txn.delete(key(b'I', name.as_bytes()));
}

/// Every index on `table`.
pub fn indexes_of(txn: &Txn, table: &str) -> Result<Arc<[Arc<IndexSchema>]>> {
    match txn.schema_cache() {
        Some(cache) => Ok(cache.lookup(txn, table)?.indexes),
        None => load_indexes(txn, table),
    }
}

fn load_indexes(txn: &Txn, table: &str) -> Result<Arc<[Arc<IndexSchema>]>> {
    let (lower, upper) = prefix_range(key(b'I', &[]));
    let mut indexes = Vec::new();
    txn.scan(lower, upper, &mut |_, data| {
        let index = IndexSchema::decode(data)?;
        if index.table == table {
            indexes.push(Arc::new(index));
        }
        Ok(true)
    })?;
    Ok(indexes.into())
}

/// Allocates an id for a new table or index.
pub fn next_id(txn: &mut Txn) -> Result<u32> {
    let k = key(b'N', &[]);
    let id = match txn.get(&k)? {
        Some(data) => Reader::new(&data).varint()? as u32,
        None => 1,
    };
    let mut next = Vec::new();
    put_varint(&mut next, u64::from(id) + 1);
    txn.put(k, next);
    Ok(id)
}

/// The keys of every entry in `index`.
pub fn index_range(index: &IndexSchema) -> (Bound<Vec<u8>>, Bound<Vec<u8>>) {
    prefix_range(index.prefix())
}

pub fn values_range(index: &IndexSchema, values: &[Value]) -> (Bound<Vec<u8>>, Bound<Vec<u8>>) {
    prefix_range(index.values_prefix(values))
}
