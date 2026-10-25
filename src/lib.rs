//! # SpiroDB
//!
//! A fast, embedded key-value storage engine written from scratch in Rust,
//! with no dependencies outside the standard library.
//!
//! SpiroDB is a log-structured merge tree (LSM tree):
//!
//! - **Writes** append to a write-ahead log, then land in an in-memory sorted
//!   *memtable*. There is no random disk I/O on the write path.
//! - When the memtable fills, it is frozen and a background thread writes it
//!   out as an immutable, sorted *table* file in level 0.
//! - A second background thread *compacts* level-0 tables into level 1, a
//!   single sorted run of non-overlapping tables, discarding overwritten values
//!   and deletion markers along the way.
//! - **Reads** check the memtables, then level 0 newest first, then level 1 by
//!   binary search. Per-table bloom filters skip tables that cannot hold the
//!   key, and a block cache keeps hot data in memory.
//!
//! ```no_run
//! use spirodb::{Db, WriteBatch};
//!
//! let db = Db::open("./my-data")?;
//! db.put("hello", "world")?;
//! assert_eq!(db.get("hello")?.as_deref(), Some(&b"world"[..]));
//!
//! let mut batch = WriteBatch::new();
//! batch.put("a", "1").put("b", "2").delete("hello");
//! db.write(&batch)?;
//!
//! for item in db.range("a".."c") {
//!     let (key, value) = item?;
//!     println!("{} = {}", String::from_utf8_lossy(&key), String::from_utf8_lossy(&value));
//! }
//! # Ok::<(), spirodb::Error>(())
//! ```

mod batch;
mod block;
mod bloom;
mod cache;
pub mod client;
mod coding;
mod crc32c;
mod db;
mod error;
mod iter;
mod json;
mod keymap;
mod manifest;
mod memtable;
mod options;
pub mod server;
pub mod sql;
mod table;
mod wal;

#[cfg(test)]
mod testutil;

pub use batch::WriteBatch;
pub use db::{Db, DbIterator, Stats};
pub use error::{Error, Result};
pub use options::Options;
