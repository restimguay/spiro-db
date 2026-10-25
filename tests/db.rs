use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use spirodb::{Db, DbIterator, Error, Options, WriteBatch};

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> TempDir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let unique = format!("spirodb-it-{name}-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
        let path = std::env::temp_dir().join(unique);
        let _ = fs::remove_dir_all(&path);
        TempDir(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// SplitMix64: a tiny deterministic PRNG, so failures reproduce.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Options that force constant flushing and compaction, so small tests
/// exercise every layer of the engine. (Each flush costs a few fsyncs, which
/// are slow on some platforms, so the memtable isn't made smaller still.)
fn tiny() -> Options {
    Options {
        memtable_size: 16 << 10,
        block_size: 256,
        block_cache_size: 64 << 10,
        l0_compaction_trigger: 2,
        l0_stop_writes_trigger: 6,
        target_file_size: 32 << 10,
        ..Options::default()
    }
}

fn key(i: u64) -> Vec<u8> {
    format!("key{i:06}").into_bytes()
}

fn keys(it: DbIterator) -> Vec<Vec<u8>> {
    it.map(|item| item.unwrap().0).collect()
}

fn assert_matches_model(db: &Db, model: &BTreeMap<Vec<u8>, Vec<u8>>, keyspace: u64) {
    for i in 0..keyspace {
        assert_eq!(db.get(key(i)).unwrap().as_ref(), model.get(&key(i)), "get key {i}");
    }
    let scanned: Vec<_> = db.iter().collect::<Result<_, _>>().unwrap();
    let expected: Vec<_> = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    assert_eq!(scanned, expected, "full scan");
}

#[test]
fn put_get_delete() {
    let dir = TempDir::new("basic");
    let db = Db::open(dir.path()).unwrap();
    assert_eq!(db.get("missing").unwrap(), None);
    db.put("hello", "world").unwrap();
    assert_eq!(db.get("hello").unwrap(), Some(b"world".to_vec()));
    db.put("hello", "again").unwrap();
    assert_eq!(db.get("hello").unwrap(), Some(b"again".to_vec()));
    db.delete("hello").unwrap();
    assert_eq!(db.get("hello").unwrap(), None);
    db.put("", "empty key").unwrap();
    db.put("empty value", "").unwrap();
    assert_eq!(db.get("").unwrap(), Some(b"empty key".to_vec()));
    assert_eq!(db.get("empty value").unwrap(), Some(Vec::new()));
}

#[test]
fn write_batch_applies_every_operation() {
    let dir = TempDir::new("batch");
    let db = Db::open(dir.path()).unwrap();
    db.put("doomed", "x").unwrap();
    let mut batch = WriteBatch::new();
    batch.put("a", "1").put("b", "2").delete("doomed").put("a", "3");
    db.write(&batch).unwrap();
    assert_eq!(db.get("a").unwrap(), Some(b"3".to_vec()));
    assert_eq!(db.get("b").unwrap(), Some(b"2".to_vec()));
    assert_eq!(db.get("doomed").unwrap(), None);
}

#[test]
fn data_survives_reopen() {
    let dir = TempDir::new("reopen");
    {
        let db = Db::open(dir.path()).unwrap();
        for i in 0..1000 {
            db.put(key(i), format!("value{i}")).unwrap();
        }
        db.delete(key(500)).unwrap();
    }
    let db = Db::open(dir.path()).unwrap();
    for i in 0..1000 {
        let expected = (i != 500).then(|| format!("value{i}").into_bytes());
        assert_eq!(db.get(key(i)).unwrap(), expected);
    }
}

#[test]
fn randomized_operations_match_a_model() {
    let dir = TempDir::new("model");
    let keyspace = 2000;
    let mut model = BTreeMap::new();
    let mut rng = Rng(7);
    for round in 0..3 {
        let db = Db::open_with(dir.path(), tiny()).unwrap();
        assert_matches_model(&db, &model, keyspace);
        for _ in 0..15_000 {
            let k = key(rng.below(keyspace));
            match rng.below(10) {
                0..=5 => {
                    let v = vec![b'a' + rng.below(26) as u8; rng.below(200) as usize];
                    db.put(&k, &v).unwrap();
                    model.insert(k, v);
                }
                6..=8 => {
                    db.delete(&k).unwrap();
                    model.remove(&k);
                }
                _ => {
                    let mut batch = WriteBatch::new();
                    for _ in 0..rng.below(20) {
                        let k = key(rng.below(keyspace));
                        batch.put(&k, &k);
                        model.insert(k.clone(), k);
                    }
                    db.write(&batch).unwrap();
                }
            }
        }
        assert_matches_model(&db, &model, keyspace);

        for _ in 0..50 {
            let (a, b) = (key(rng.below(keyspace)), key(rng.below(keyspace)));
            let got = keys(db.range(a.clone()..b.clone()));
            let want: Vec<_> = if a <= b { model.range(a..b).map(|(k, _)| k.clone()).collect() } else { vec![] };
            assert_eq!(got, want, "range scan in round {round}");
        }
        let stats = db.stats();
        assert!(stats.flushes > 0, "round {round} never flushed: {stats:?}");
    }
}

#[test]
fn range_bounds() {
    let dir = TempDir::new("range");
    let db = Db::open_with(dir.path(), tiny()).unwrap();
    for i in 0..100 {
        db.put(key(i), "v").unwrap();
    }
    db.delete(key(50)).unwrap();
    let expect = |ids: &[u64]| ids.iter().map(|&i| key(i)).collect::<Vec<_>>();
    // Run once against the memtable, once against tables.
    for _ in 0..2 {
        assert_eq!(keys(db.range(key(10)..key(13))), expect(&[10, 11, 12]));
        assert_eq!(keys(db.range(key(10)..=key(13))), expect(&[10, 11, 12, 13]));
        assert_eq!(keys(db.range((Bound::Excluded(key(10)), Bound::Included(key(12))))), expect(&[11, 12]));
        assert_eq!(keys(db.range(key(48)..key(53))), expect(&[48, 49, 51, 52]));
        assert_eq!(keys(db.range(..key(3))), expect(&[0, 1, 2]));
        assert_eq!(keys(db.range(key(97)..)), expect(&[97, 98, 99]));
        assert_eq!(keys(db.range("zzz"..)), expect(&[]));
        assert_eq!(db.iter().count(), 99);
        db.flush().unwrap();
    }
}

#[test]
fn compaction_runs_and_preserves_data() {
    let dir = TempDir::new("compaction");
    let db = Db::open_with(dir.path(), tiny()).unwrap();
    for i in 0..20_000 {
        db.put(key(i % 5000), format!("v{i}")).unwrap();
    }
    for i in (0..5000).step_by(3) {
        db.delete(key(i)).unwrap();
    }
    db.flush().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while db.stats().l0_tables >= tiny().l0_compaction_trigger && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    let stats = db.stats();
    assert!(stats.compactions > 0 && stats.l1_tables > 0, "{stats:?}");
    for i in 0..5000 {
        let expected = (i % 3 != 0).then(|| format!("v{}", i + 15_000).into_bytes());
        assert_eq!(db.get(key(i)).unwrap(), expected, "key {i}");
    }
    assert_eq!(db.iter().count(), 5000 - 5000usize.div_ceil(3));
}

#[test]
fn recovers_from_a_torn_log_tail() {
    // Simulate a crash in the middle of appending a record: once with the
    // record cut short, once with its bytes garbled.
    let torn_tails: [&[u8]; 2] = [&[0xde, 0xad, 0xbe, 0xef, 100, 0, 0, 0, 1, 2, 3], &[0, 0, 0, 0, 3, 0, 0, 0, 1, 2, 3]];
    for tail in torn_tails {
        let dir = TempDir::new("torn");
        {
            let db = Db::open(dir.path()).unwrap();
            for i in 0..100 {
                db.put(key(i), "value").unwrap();
            }
        }
        let log = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "log"))
            .max()
            .unwrap();
        OpenOptions::new().append(true).open(&log).unwrap().write_all(tail).unwrap();

        let db = Db::open(dir.path()).unwrap();
        for i in 0..100 {
            assert_eq!(db.get(key(i)).unwrap(), Some(b"value".to_vec()));
        }
        db.put("after", "crash").unwrap();
        drop(db);
        let db = Db::open(dir.path()).unwrap();
        assert_eq!(db.get("after").unwrap(), Some(b"crash".to_vec()));
        assert_eq!(db.iter().count(), 101);
    }
}

#[test]
fn a_second_open_is_refused() {
    let dir = TempDir::new("lock");
    let db = Db::open(dir.path()).unwrap();
    assert!(matches!(Db::open(dir.path()), Err(Error::Locked(_))));
    drop(db);
    Db::open(dir.path()).unwrap();
}

#[test]
fn a_missing_manifest_is_refused_not_wiped() {
    let dir = TempDir::new("no-manifest");
    {
        let db = Db::open(dir.path()).unwrap();
        db.put("precious", "data").unwrap();
        db.flush().unwrap();
    }
    fs::remove_file(dir.path().join("MANIFEST")).unwrap();
    assert!(matches!(Db::open(dir.path()), Err(Error::Corruption(_))));
    let tables = fs::read_dir(dir.path())
        .unwrap()
        .filter(|e| e.as_ref().unwrap().path().extension().is_some_and(|x| x == "sst"));
    assert_eq!(tables.count(), 1, "the table must survive");
}

#[test]
fn invalid_options_are_rejected() {
    let dir = TempDir::new("options");
    let opts = Options { l0_compaction_trigger: 8, l0_stop_writes_trigger: 8, ..Options::default() };
    assert!(matches!(Db::open_with(dir.path(), opts), Err(Error::InvalidArgument(_))));
}

#[test]
fn concurrent_readers_and_writers() {
    let dir = TempDir::new("concurrent");
    let db = Arc::new(Db::open_with(dir.path(), tiny()).unwrap());
    let value_for = |k: &[u8]| k.repeat(3);
    let (writers, per_writer) = (4u64, 3000u64);
    let wkey = |t: u64, i: u64| format!("t{t}-{i:05}").into_bytes();
    let done = Arc::new(AtomicBool::new(false));

    let readers: Vec<_> = (0..4)
        .map(|r| {
            let (db, done) = (db.clone(), done.clone());
            thread::spawn(move || {
                let mut rng = Rng(r);
                while !done.load(Ordering::Relaxed) {
                    let k = wkey(rng.below(writers), rng.below(per_writer));
                    if let Some(v) = db.get(&k).unwrap() {
                        assert_eq!(v, value_for(&k));
                    }
                    let mut prev: Option<Vec<u8>> = None;
                    for item in db.range(k.clone()..).take(50) {
                        let (k, v) = item.unwrap();
                        assert_eq!(v, value_for(&k));
                        assert!(prev.as_ref().is_none_or(|p| *p < k), "scan out of order");
                        prev = Some(k);
                    }
                }
            })
        })
        .collect();
    let writer_threads: Vec<_> = (0..writers)
        .map(|t| {
            let db = db.clone();
            thread::spawn(move || {
                for i in 0..per_writer {
                    let k = wkey(t, i);
                    db.put(&k, value_for(&k)).unwrap();
                }
            })
        })
        .collect();
    for w in writer_threads {
        w.join().unwrap();
    }
    done.store(true, Ordering::Relaxed);
    for r in readers {
        r.join().unwrap();
    }
    for t in 0..writers {
        for i in 0..per_writer {
            let k = wkey(t, i);
            assert_eq!(db.get(&k).unwrap(), Some(value_for(&k)));
        }
    }
    assert_eq!(db.iter().count() as u64, writers * per_writer);
}

#[test]
fn db_is_send_and_sync() {
    fn send_sync<T: Send + Sync>() {}
    fn send<T: Send>() {}
    send_sync::<Db>();
    send::<DbIterator>();
}

#[test]
fn scan_lends_the_same_entries_as_range() {
    let dir = TempDir::new("scan");
    let db = Db::open_with(&dir.0, Options { memtable_size: 64 << 10, ..Options::default() }).unwrap();
    for i in 0..5000u32 {
        db.put(format!("key{i:05}"), format!("value{i}")).unwrap();
        if i % 7 == 0 {
            db.delete(format!("key{:05}", i / 2)).unwrap();
        }
    }
    db.flush().unwrap();
    for i in (0..5000u32).step_by(3) {
        db.put(format!("key{i:05}"), "again").unwrap();
    }
    for (lo, hi) in [("key00000", "key99999"), ("key01234", "key03000"), ("key04999", "key05000"), ("z", "zz")] {
        let copied: Vec<_> = db.range(lo..hi).map(Result::unwrap).collect();
        let mut lent = Vec::new();
        db.scan(lo..hi, |k, v| {
            lent.push((k.to_vec(), v.to_vec()));
            true
        })
        .unwrap();
        assert_eq!(lent, copied, "{lo}..{hi}");
    }
    // Stopping early.
    let mut first = Vec::new();
    db.scan::<&str, _>(.., |k, _| {
        first.push(k.to_vec());
        first.len() < 10
    })
    .unwrap();
    assert_eq!(first.len(), 10);
}
