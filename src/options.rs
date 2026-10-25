use crate::error::{Error, Result};

/// Tuning knobs for a database. The defaults suit most workloads.
#[derive(Clone, Debug)]
pub struct Options {
    /// Bytes of writes buffered in memory before they are flushed to a table.
    /// Larger values mean fewer, bigger flushes and faster bulk loads.
    pub memtable_size: usize,
    /// Target size of a data block, the unit of disk reads and caching.
    pub block_size: usize,
    /// Bloom filter bits per key. 10 gives roughly a 1% false-positive rate;
    /// 0 disables the filters.
    pub bloom_bits_per_key: usize,
    /// Bytes of recently read blocks kept in memory.
    pub block_cache_size: usize,
    /// fsync the log after every write. This survives power loss, at a large
    /// cost in write latency. Without it, acknowledged writes survive a process
    /// crash but may be lost if the machine itself goes down.
    pub sync: bool,
    /// Number of level-0 tables that triggers a compaction.
    pub l0_compaction_trigger: usize,
    /// Number of level-0 tables at which writes pause until compaction catches up.
    pub l0_stop_writes_trigger: usize,
    /// Size at which a compaction starts a new output table.
    pub target_file_size: u64,
    /// For SQL databases: also store each table's rows column by column in
    /// every table file, so aggregates over whole tables read only the
    /// columns they need. Costs disk space (up to about as much again for
    /// the rows) and background work when tables are written.
    pub column_sections: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            memtable_size: 8 << 20,
            block_size: 4 << 10,
            bloom_bits_per_key: 10,
            block_cache_size: 64 << 20,
            sync: false,
            l0_compaction_trigger: 4,
            l0_stop_writes_trigger: 12,
            target_file_size: 16 << 20,
            column_sections: true,
        }
    }
}

impl Options {
    pub(crate) fn validate(&self) -> Result<()> {
        let invalid = |msg: &str| -> Result<()> { Err(Error::InvalidArgument(msg.to_string())) };
        if self.memtable_size == 0 {
            return invalid("memtable_size must be positive");
        }
        if self.block_size == 0 {
            return invalid("block_size must be positive");
        }
        if self.l0_compaction_trigger == 0 {
            return invalid("l0_compaction_trigger must be positive");
        }
        if self.l0_stop_writes_trigger <= self.l0_compaction_trigger {
            return invalid("l0_stop_writes_trigger must exceed l0_compaction_trigger");
        }
        Ok(())
    }
}
