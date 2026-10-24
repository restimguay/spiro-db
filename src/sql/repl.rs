//! Replication state: the primary numbers every commit and keeps the recent
//! ones in memory, from where they stream to read-only replicas.
//!
//! The sequence number of the last commit lives in the database itself (under
//! `SEQ_KEY`), written in the same atomic batch as the commit's data. On a
//! replica, applying a commit's batch therefore moves its position forward in
//! the same write: a crash can never leave the two out of step.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::coding::{Reader, put_bytes, put_varint};
use crate::db::Db;
use crate::error::{Result, corruption};

/// Keys starting with 0x01 are internal; SQL data lives under 0x00.
pub(crate) const SEQ_KEY: &[u8] = b"\x01seq";
/// Bumped by every promotion, so clients can tell a new primary from a stale one.
pub(crate) const EPOCH_KEY: &[u8] = b"\x01epoch";
/// On a replica installing a snapshot: the sequence number its data is
/// consistent from. Until it has applied that far, it serves no reads.
pub(crate) const SNAPSHOT_END_KEY: &[u8] = b"\x01snapshot_end";
/// Where each epoch after the first began: `(epoch, first sequence number)`
/// pairs. A replica that kept following an old primary past the point where
/// another server was promoted has commits the new primary never saw; this
/// history is how the new primary notices and sends it a snapshot instead.
pub(crate) const EPOCHS_KEY: &[u8] = b"\x01epochs";

/// How many bytes of recent commits the primary keeps for replicas to catch
/// up from. A replica further behind starts over from a snapshot.
const LOG_LIMIT: usize = 64 << 20;

/// What a database is doing in a replicated setup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Role {
    /// Accepts writes and streams them to replicas.
    Primary,
    /// Follows the primary at this address, and serves reads only.
    Replica { primary: String },
}

/// A committed transaction, as it travels to replicas.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub seq: u64,
    /// The commit's write batch, in its log encoding.
    pub batch: Vec<u8>,
    pub notifications: Vec<(String, String)>,
}

impl Entry {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.batch.len() + 16);
        put_varint(&mut out, self.seq);
        put_bytes(&mut out, &self.batch);
        put_varint(&mut out, self.notifications.len() as u64);
        for (channel, payload) in &self.notifications {
            put_bytes(&mut out, channel.as_bytes());
            put_bytes(&mut out, payload.as_bytes());
        }
        out
    }

    pub fn decode(data: &[u8]) -> Result<Entry> {
        let mut r = Reader::new(data);
        let seq = r.varint()?;
        let batch = r.len_prefixed()?.to_vec();
        let text = |b: &[u8]| String::from_utf8(b.to_vec()).map_err(|_| corruption("notification is not UTF-8"));
        let mut notifications = Vec::new();
        for _ in 0..r.varint()? {
            notifications.push((text(r.len_prefixed()?)?, text(r.len_prefixed()?)?));
        }
        Ok(Entry { seq, batch, notifications })
    }
}

/// A snapshot of the replication state, for status reports.
#[derive(Clone, Debug)]
pub struct ReplicationStatus {
    pub role: Role,
    /// The last commit (on a primary) or the last one applied (on a replica).
    pub seq: u64,
    pub epoch: u64,
    /// A replica still installing a snapshot, or replaying up to its end.
    pub catching_up: bool,
    /// On a primary: each connected replica's id and the last commit it confirmed.
    pub replicas: Vec<(u64, u64)>,
    /// On a replica: the primary's latest commit, as last heard.
    pub primary_seq: u64,
}

pub(crate) struct Replication {
    state: Mutex<State>,
    /// Signalled on new commits, applied entries, acknowledgements and role changes.
    changed: Condvar,
}

struct State {
    seq: u64,
    epoch: u64,
    epochs: Vec<(u64, u64)>,
    role: Role,
    snapshot_end: u64,
    log: VecDeque<Arc<Entry>>,
    log_bytes: usize,
    acks: HashMap<u64, u64>,
    primary_seq: u64,
    sync_replicas: usize,
    sync_timeout: Duration,
}

impl Default for Replication {
    fn default() -> Self {
        Replication {
            state: Mutex::new(State {
                seq: 0,
                epoch: 1,
                epochs: Vec::new(),
                role: Role::Primary,
                snapshot_end: 0,
                log: VecDeque::new(),
                log_bytes: 0,
                acks: HashMap::new(),
                primary_seq: 0,
                sync_replicas: 0,
                sync_timeout: Duration::from_secs(10),
            }),
            changed: Condvar::new(),
        }
    }
}

pub(crate) fn read_u64(kv: &Db, key: &[u8]) -> Result<Option<u64>> {
    kv.get(key)?.map(|data| Reader::new(&data).varint()).transpose()
}

pub(crate) fn encode_u64(n: u64) -> Vec<u8> {
    let mut out = Vec::new();
    put_varint(&mut out, n);
    out
}

pub(crate) fn encode_epochs(epochs: &[(u64, u64)]) -> Vec<u8> {
    let mut out = Vec::new();
    for &(epoch, start) in epochs {
        put_varint(&mut out, epoch);
        put_varint(&mut out, start);
    }
    out
}

pub(crate) fn decode_epochs(data: &[u8]) -> Result<Vec<(u64, u64)>> {
    let mut r = Reader::new(data);
    let mut epochs = Vec::new();
    while !r.is_empty() {
        epochs.push((r.varint()?, r.varint()?));
    }
    Ok(epochs)
}

impl Replication {
    pub fn load(kv: &Db) -> Result<Replication> {
        let repl = Replication::default();
        {
            let mut state = repl.state.lock().unwrap();
            state.seq = read_u64(kv, SEQ_KEY)?.unwrap_or(0);
            state.epoch = read_u64(kv, EPOCH_KEY)?.unwrap_or(1);
            state.snapshot_end = read_u64(kv, SNAPSHOT_END_KEY)?.unwrap_or(0);
            state.epochs = kv.get(EPOCHS_KEY)?.map(|data| decode_epochs(&data)).transpose()?.unwrap_or_default();
            state.primary_seq = state.seq;
        }
        Ok(repl)
    }

    pub fn status(&self) -> ReplicationStatus {
        let state = self.state.lock().unwrap();
        let mut replicas: Vec<(u64, u64)> = state.acks.iter().map(|(&id, &seq)| (id, seq)).collect();
        replicas.sort_unstable();
        ReplicationStatus {
            role: state.role.clone(),
            seq: state.seq,
            epoch: state.epoch,
            catching_up: state.seq < state.snapshot_end,
            replicas,
            primary_seq: state.primary_seq,
        }
    }

    pub fn seq(&self) -> u64 {
        self.state.lock().unwrap().seq
    }

    pub fn is_replica(&self) -> bool {
        matches!(self.state.lock().unwrap().role, Role::Replica { .. })
    }

    pub fn set_role(&self, role: Role) {
        self.state.lock().unwrap().role = role;
        self.changed.notify_all();
    }

    pub fn set_epoch(&self, epoch: u64) {
        self.state.lock().unwrap().epoch = epoch;
    }

    pub fn epoch(&self) -> u64 {
        self.state.lock().unwrap().epoch
    }

    pub fn set_epochs(&self, epochs: Vec<(u64, u64)>) {
        self.state.lock().unwrap().epochs = epochs;
    }

    /// The epoch history with a new epoch starting at commit `seq` added.
    pub fn epochs_with(&self, epoch: u64, seq: u64) -> Vec<(u64, u64)> {
        let mut epochs = self.state.lock().unwrap().epochs.clone();
        epochs.push((epoch, seq));
        epochs
    }

    pub fn encoded_epochs(&self) -> Vec<u8> {
        encode_epochs(&self.state.lock().unwrap().epochs)
    }

    /// Whether a replica in `epoch` that has applied up to `from` holds
    /// commits from a timeline this server doesn't share: it kept applying an
    /// older primary's commits at or past the point a newer epoch began.
    pub fn diverged(&self, epoch: u64, from: u64) -> bool {
        let state = self.state.lock().unwrap();
        state.epochs.iter().find(|&&(e, _)| e > epoch).is_some_and(|&(_, start)| from >= start)
    }

    pub fn set_sync(&self, replicas: usize, timeout: Duration) {
        let mut state = self.state.lock().unwrap();
        state.sync_replicas = replicas;
        state.sync_timeout = timeout;
    }

    /// The sequence number the next commit gets. The caller holds the write
    /// lock, so commits are numbered in the order they become durable.
    pub fn next_seq(&self) -> u64 {
        self.state.lock().unwrap().seq + 1
    }

    /// Records a commit that is now durable, for replicas to stream.
    pub fn record(&self, entry: Entry) {
        let mut state = self.state.lock().unwrap();
        state.seq = entry.seq;
        state.log_bytes += entry.batch.len();
        state.log.push_back(Arc::new(entry));
        while state.log_bytes > LOG_LIMIT
            && let Some(old) = state.log.pop_front()
        {
            state.log_bytes -= old.batch.len();
        }
        drop(state);
        self.changed.notify_all();
    }

    /// Whether a replica that has applied up to `from` must start over from a
    /// snapshot: it's ahead of us, or the commits it needs are gone.
    pub fn needs_snapshot(&self, from: u64) -> bool {
        let state = self.state.lock().unwrap();
        if from > state.seq {
            return true;
        }
        from < state.seq && state.log.front().is_none_or(|first| first.seq > from + 1)
    }

    /// The recorded commits after `after`, or `None` if some are no longer kept.
    pub fn entries_after(&self, after: u64) -> Option<Vec<Arc<Entry>>> {
        let state = self.state.lock().unwrap();
        if after >= state.seq {
            return Some(Vec::new());
        }
        let first = state.log.front()?.seq;
        if first > after + 1 {
            return None;
        }
        Some(state.log.iter().skip((after + 1 - first) as usize).cloned().collect())
    }

    /// Waits up to `timeout` for the sequence number to pass `seen`.
    pub fn wait_past(&self, seen: u64, timeout: Duration) -> u64 {
        let deadline = Instant::now() + timeout;
        let mut state = self.state.lock().unwrap();
        while state.seq <= seen {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            state = self.changed.wait_timeout(state, deadline - now).unwrap().0;
        }
        state.seq
    }

    /// On a replica: waits up to `timeout` until it has applied commit `seq`
    /// and is not mid-snapshot. Reads that need a write to be visible use this.
    pub fn wait_until_applied(&self, seq: u64, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.state.lock().unwrap();
        loop {
            if state.seq >= seq && state.seq >= state.snapshot_end {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            state = self.changed.wait_timeout(state, deadline - now).unwrap().0;
        }
    }

    /// A replica confirms it has applied everything up to `seq`.
    pub fn ack(&self, replica: u64, seq: u64) {
        self.state.lock().unwrap().acks.insert(replica, seq);
        self.changed.notify_all();
    }

    pub fn forget(&self, replica: u64) {
        self.state.lock().unwrap().acks.remove(&replica);
        self.changed.notify_all();
    }

    /// In synchronous mode, waits until enough replicas have confirmed commit
    /// `seq`. Returns `false` if they didn't within the timeout.
    pub fn wait_for_replicas(&self, seq: u64) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.sync_replicas == 0 {
            return true;
        }
        let deadline = Instant::now() + state.sync_timeout;
        loop {
            if state.acks.values().filter(|&&acked| acked >= seq).count() >= state.sync_replicas {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            state = self.changed.wait_timeout(state, deadline - now).unwrap().0;
        }
    }

    /// On a replica: notes an applied commit. It goes in the log too, so
    /// that if this replica is promoted, the others can catch up from it.
    pub fn applied(&self, entry: Entry) {
        let seq = entry.seq;
        self.record(entry);
        let mut state = self.state.lock().unwrap();
        state.primary_seq = state.primary_seq.max(seq);
    }

    pub fn set_primary_seq(&self, seq: u64) {
        self.state.lock().unwrap().primary_seq = seq;
    }

    /// On a replica: a snapshot is being installed (`end` = `u64::MAX`), or
    /// was installed at `seq` and is consistent once `end` is applied.
    pub fn snapshot(&self, seq: u64, end: u64) {
        let mut state = self.state.lock().unwrap();
        state.seq = seq;
        state.snapshot_end = end;
        // The commits before a snapshot are not ours to stream.
        state.log.clear();
        state.log_bytes = 0;
        drop(state);
        self.changed.notify_all();
    }
}

// Frames on a replication connection: [kind: u8][length: u32 LE][payload].
pub(crate) const FRAME_SNAPSHOT_BEGIN: u8 = 1;
pub(crate) const FRAME_SNAPSHOT_CHUNK: u8 = 2;
pub(crate) const FRAME_SNAPSHOT_END: u8 = 3;
pub(crate) const FRAME_ENTRY: u8 = 4;
pub(crate) const FRAME_HEARTBEAT: u8 = 5;
pub(crate) const FRAME_ACK: u8 = 6;

const MAX_FRAME: usize = 1 << 30;

pub(crate) fn write_frame(w: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    let mut header = [0u8; 5];
    header[0] = kind;
    header[1..].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    w.write_all(&header)?;
    w.write_all(payload)?;
    w.flush()
}

pub(crate) fn read_frame(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    r.read_exact(&mut header)?;
    let len = u32::from_le_bytes(header[1..].try_into().unwrap()) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "replication frame too large"));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok((header[0], payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(seq: u64, bytes: usize) -> Entry {
        Entry { seq, batch: vec![0; bytes], notifications: vec![("ch".into(), format!("n{seq}"))] }
    }

    #[test]
    fn entries_round_trip_and_the_log_serves_catch_up() {
        let e = entry(7, 10);
        assert_eq!(Entry::decode(&e.encode()).unwrap(), e);

        let repl = Replication::default();
        for seq in 1..=5 {
            repl.record(entry(seq, 10));
        }
        assert_eq!(repl.entries_after(2).unwrap().iter().map(|e| e.seq).collect::<Vec<_>>(), [3, 4, 5]);
        assert!(repl.entries_after(5).unwrap().is_empty());
        assert!(!repl.needs_snapshot(0));
        assert!(repl.needs_snapshot(9), "a replica ahead of the primary starts over");

        // Old entries fall out once the log is over its size limit.
        repl.record(entry(6, LOG_LIMIT));
        assert!(repl.entries_after(2).is_none());
        assert!(repl.needs_snapshot(2));
    }

    #[test]
    fn synchronous_commits_wait_for_acknowledgements() {
        let repl = Arc::new(Replication::default());
        repl.set_sync(1, Duration::from_millis(50));
        repl.record(entry(1, 1));
        assert!(!repl.wait_for_replicas(1), "nobody acknowledged");
        let acker = {
            let repl = repl.clone();
            std::thread::spawn(move || repl.ack(9, 1))
        };
        repl.set_sync(1, Duration::from_secs(5));
        assert!(repl.wait_for_replicas(1));
        acker.join().unwrap();
    }

    #[test]
    fn epoch_history_detects_diverged_replicas() {
        let repl = Replication::default();
        assert!(!repl.diverged(1, 100));
        // Epoch 2 began at commit 11 (the promotion); epoch 3 at commit 20.
        repl.set_epochs(vec![(2, 11), (3, 20)]);
        assert!(!repl.diverged(1, 10), "everything up to the promotion is shared");
        assert!(repl.diverged(1, 11), "commit 11 in epoch 1 is not the promotion");
        assert!(!repl.diverged(2, 19));
        assert!(repl.diverged(2, 25));
        assert!(!repl.diverged(3, 25));
        assert_eq!(decode_epochs(&encode_epochs(&[(2, 11), (3, 20)])).unwrap(), [(2, 11), (3, 20)]);
    }

    #[test]
    fn frames_round_trip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, FRAME_ENTRY, b"payload").unwrap();
        assert_eq!(read_frame(&mut buf.as_slice()).unwrap(), (FRAME_ENTRY, b"payload".to_vec()));
    }
}
