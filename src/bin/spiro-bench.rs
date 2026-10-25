//! spiro-bench: measures SpiroDB throughput.
//!
//!     cargo run --release --bin spiro-bench -- [options]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use spirodb::sql::{Database, Value};
use spirodb::{Db, Options, WriteBatch};

const USAGE: &str = "\
usage: spiro-bench [options]
  --num N          number of keys (default 1000000)
  --value-size B   value size in bytes (default 100)
  --threads T      reader threads for the parallel read test (default: all cores)
  --cache-mb M     block cache size in MiB (default 64)
  --scan-threads S threads one SQL query may use (default 0: every core)
  --sync           fsync the log on every write
  --sql            benchmark the SQL layer instead of the key-value API
  --path DIR       database directory (default: a temp dir, removed afterwards)";

const KEY_LEN: usize = 16;
const BATCH_SIZE: u64 = 1000;

struct Config {
    num: u64,
    value_size: usize,
    threads: usize,
    cache_mb: usize,
    scan_threads: usize,
    sync: bool,
    sql: bool,
    path: Option<PathBuf>,
}

fn parse_args() -> Result<Config, String> {
    let mut cfg = Config {
        num: 1_000_000,
        value_size: 100,
        threads: thread::available_parallelism().map_or(4, |n| n.get()),
        cache_mb: Options::default().block_cache_size >> 20,
        scan_threads: 0,
        sync: false,
        sql: false,
        path: None,
    };
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--num" => cfg.num = value()?.parse().map_err(|e| format!("--num: {e}"))?,
            "--value-size" => cfg.value_size = value()?.parse().map_err(|e| format!("--value-size: {e}"))?,
            "--threads" => cfg.threads = value()?.parse().map_err(|e| format!("--threads: {e}"))?,
            "--cache-mb" => cfg.cache_mb = value()?.parse().map_err(|e| format!("--cache-mb: {e}"))?,
            "--scan-threads" => cfg.scan_threads = value()?.parse().map_err(|e| format!("--scan-threads: {e}"))?,
            "--path" => cfg.path = Some(value()?.into()),
            "--sync" => cfg.sync = true,
            "--sql" => cfg.sql = true,
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown option {other}")),
        }
    }
    if cfg.num == 0 || cfg.threads == 0 {
        return Err("--num and --threads must be positive".into());
    }
    Ok(cfg)
}

fn main() -> ExitCode {
    let cfg = match parse_args() {
        Ok(cfg) => cfg,
        Err(msg) => {
            eprintln!("{msg}\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    let dir = cfg.path.clone().unwrap_or_else(|| env::temp_dir().join(format!("spiro-bench-{}", std::process::id())));
    let opts = Options { sync: cfg.sync, block_cache_size: cfg.cache_mb << 20, ..Options::default() };
    let n = cfg.num;
    let entry = KEY_LEN + cfg.value_size;
    let values = Values::new(cfg.value_size);
    let open_fresh = || {
        let _ = fs::remove_dir_all(&dir);
        Db::open_with(&dir, opts.clone()).expect("open database")
    };

    println!("SpiroDB {} benchmark", env!("CARGO_PKG_VERSION"));
    println!(
        "{} keys of {KEY_LEN} B, values of {} B ({:.1} MB raw), {} MiB block cache, log sync {}",
        group(n),
        cfg.value_size,
        (n as usize * entry) as f64 / 1e6,
        cfg.cache_mb,
        if cfg.sync { "on" } else { "off" },
    );
    println!("{:-<90}", "");

    if cfg.sql {
        sql_suite(n, &dir, opts, cfg.scan_threads);
        if cfg.path.is_none() {
            let _ = fs::remove_dir_all(&dir);
        }
        return ExitCode::SUCCESS;
    }

    {
        let db = open_fresh();
        run("fillseq", n, Some(entry), || {
            for i in 0..n {
                db.put(key(i), values.get(i)).unwrap();
            }
            String::new()
        });
    }
    {
        let db = open_fresh();
        run("fillbatch", n, Some(entry), || {
            let mut batch = WriteBatch::new();
            for start in (0..n).step_by(BATCH_SIZE as usize) {
                batch.clear();
                for i in start..(start + BATCH_SIZE).min(n) {
                    batch.put(key(i), values.get(i));
                }
                db.write(&batch).unwrap();
            }
            format!("batches of {BATCH_SIZE}")
        });
    }

    let db = open_fresh();
    let mut order: Vec<u64> = (0..n).collect();
    shuffle(&mut order, &mut Rng(42));
    run("fillrandom", n, Some(entry), || {
        for &i in &order {
            db.put(key(i), values.get(i)).unwrap();
        }
        String::new()
    });
    drop(order);
    db.flush().unwrap();
    wait_for_compactions(&db);
    let s = db.stats();
    println!(
        "{:>24} level 0: {} tables, level 1: {} tables, {:.1} MB on disk, {} flushes, {} compactions",
        "↳",
        s.l0_tables,
        s.l1_tables,
        s.table_bytes as f64 / 1e6,
        s.flushes,
        s.compactions
    );

    run("readrandom", n, None, || {
        let mut rng = Rng(1);
        let found = (0..n).filter(|_| db.get(key(rng.below(n))).unwrap().is_some()).count();
        format!("{} of {} found", group(found as u64), group(n))
    });
    let threads = cfg.threads;
    run(&format!("readrandom ×{threads}"), n, None, || {
        let found: usize = thread::scope(|s| {
            let workers: Vec<_> = (0..threads as u64)
                .map(|t| {
                    let db = &db;
                    let ops = n / threads as u64 + u64::from(t < n % threads as u64);
                    s.spawn(move || {
                        let mut rng = Rng(100 + t);
                        (0..ops).filter(|_| db.get(key(rng.below(n))).unwrap().is_some()).count()
                    })
                })
                .collect();
            workers.into_iter().map(|w| w.join().unwrap()).sum()
        });
        format!("{} of {} found", group(found as u64), group(n))
    });
    run("readmissing", n, None, || {
        let mut rng = Rng(2);
        // Keys that fall between existing ones, so only the bloom filters can rule them out.
        let found = (0..n)
            .filter(|_| {
                let mut k = key(rng.below(n)).to_vec();
                k.push(b'x');
                db.get(k).unwrap().is_some()
            })
            .count();
        format!("{found} found")
    });
    run("readseq", n, Some(entry), || {
        let mut count = 0u64;
        for item in db.iter() {
            item.unwrap();
            count += 1;
        }
        format!("{} entries", group(count))
    });
    let scans = (n / 100).max(1);
    run("scan100", scans, None, || {
        let mut rng = Rng(3);
        let total: usize = (0..scans).map(|_| db.range(key(rng.below(n))..).take(100).count()).sum();
        format!("{} scans of 100 keys, {} entries", group(scans), group(total as u64))
    });

    drop(db);
    if cfg.path.is_none() {
        let _ = fs::remove_dir_all(&dir);
    }
    ExitCode::SUCCESS
}

/// Benchmarks the SQL layer on a table with a secondary index.
fn sql_suite(n: u64, dir: &Path, opts: Options, scan_threads: usize) {
    let _ = fs::remove_dir_all(dir);
    let db = Database::open_with(dir, opts).expect("open database");
    db.set_scan_threads(scan_threads);
    db.execute("CREATE TABLE bench (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score INTEGER, note TEXT)").unwrap();
    db.execute("CREATE INDEX bench_score ON bench (score)").unwrap();
    let groups = (n / 10).max(1); // each score is shared by about 10 rows
    let row = |i: u64| {
        let (id, score) = (Value::Integer(i as i64), Value::Integer((i % groups) as i64));
        [id, Value::Text(format!("user{i}")), score, Value::Text("lorem ipsum dolor sit amet".into())]
    };
    let insert = db.prepare("INSERT INTO bench VALUES (?, ?, ?, ?)").unwrap();
    let mut session = db.session();

    run("insert, 1000 per txn", n, None, || {
        for start in (0..n).step_by(BATCH_SIZE as usize) {
            session.execute("BEGIN").unwrap();
            for i in start..(start + BATCH_SIZE).min(n) {
                session.execute_prepared(&insert, &row(i)).unwrap();
            }
            session.execute("COMMIT").unwrap();
        }
        String::new()
    });
    let single = (n / 10).max(1);
    run("insert, autocommit", single, None, || {
        for i in n..n + single {
            session.execute_prepared(&insert, &row(i)).unwrap();
        }
        String::new()
    });
    let total = n + single;

    // Queries read their rows as they are produced, with `query_each`, the
    // way the other engines' benchmarks step through theirs.
    let by_id = db.prepare("SELECT name FROM bench WHERE id = ?").unwrap();
    run("select by primary key", n, None, || {
        let mut rng = Rng(1);
        let mut found = 0;
        for _ in 0..n {
            let id = Value::Integer(rng.below(total) as i64);
            session
                .query_each(&by_id, &[id], |row| {
                    found += row[0].as_str().is_some() as u64;
                    true
                })
                .unwrap();
        }
        format!("{} of {} found", group(found), group(n))
    });
    let by_score = db.prepare("SELECT id FROM bench WHERE score = ?").unwrap();
    let lookups = (n / 10).max(1);
    run("select by index", lookups, None, || {
        let mut rng = Rng(2);
        let mut rows = 0;
        for _ in 0..lookups {
            let score = Value::Integer(rng.below(groups) as i64);
            session
                .query_each(&by_score, &[score], |row| {
                    rows += matches!(row[0], Value::Integer(id) if id >= 0) as u64;
                    true
                })
                .unwrap();
        }
        format!("{} rows", group(rows))
    });
    let range = db.prepare("SELECT id, name FROM bench WHERE id >= ? AND id < ?").unwrap();
    let scans = (n / 100).max(1);
    run("select 100-row range", scans, None, || {
        let mut rng = Rng(3);
        let mut rows = 0;
        for _ in 0..scans {
            let start = rng.below(total) as i64;
            let bounds = [Value::Integer(start), Value::Integer(start + 100)];
            session
                .query_each(&range, &bounds, |row| {
                    rows += row[1].as_str().is_some() as u64;
                    true
                })
                .unwrap();
        }
        format!("{} rows", group(rows))
    });
    let update = db.prepare("UPDATE bench SET note = ? WHERE id = ?").unwrap();
    let updates = (n / 10).max(1);
    run("update by primary key", updates, None, || {
        let mut rng = Rng(4);
        for _ in 0..updates {
            let params = [Value::Text("updated".into()), Value::Integer(rng.below(total) as i64)];
            session.execute_prepared(&update, &params).unwrap();
        }
        String::new()
    });
    run("count + avg, full scan", total, None, || {
        let rows = session.query("SELECT count(*), avg(score) FROM bench", &[]).unwrap();
        format!("per row scanned; count = {}", rows.rows[0][0])
    });
    run("group by, full scan", total, None, || {
        let rows = session.query("SELECT score % 10, count(*) FROM bench GROUP BY 1", &[]).unwrap();
        format!("per row scanned; {} groups", rows.len())
    });
}

fn run(name: &str, ops: u64, bytes_per_op: Option<usize>, f: impl FnOnce() -> String) {
    let start = Instant::now();
    let note = f();
    let secs = start.elapsed().as_secs_f64();
    let throughput = bytes_per_op.map_or(String::new(), |b| format!("{:.1} MB/s", ops as f64 * b as f64 / secs / 1e6));
    println!(
        "{name:<22} {:>9.3} µs/op {:>13} ops/s {throughput:>12}   {note}",
        secs * 1e6 / ops as f64,
        group((ops as f64 / secs) as u64),
    );
}

fn wait_for_compactions(db: &Db) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while db.stats().l0_tables >= Options::default().l0_compaction_trigger && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
}

/// Zero-padded decimal keys, so key order matches numeric order.
fn key(i: u64) -> [u8; KEY_LEN] {
    let mut k = [b'0'; KEY_LEN];
    let mut n = i;
    for b in k.iter_mut().rev() {
        *b = b'0' + (n % 10) as u8;
        n /= 10;
    }
    k
}

/// Random value bytes, served as windows into one buffer so generating them
/// costs nothing during the timed loops.
struct Values {
    buf: Vec<u8>,
    size: usize,
}

impl Values {
    fn new(size: usize) -> Self {
        let mut rng = Rng(9);
        Values { buf: (0..size + 4096).map(|_| rng.next() as u8).collect(), size }
    }

    fn get(&self, i: u64) -> &[u8] {
        let offset = (i as usize).wrapping_mul(7919) % (self.buf.len() - self.size);
        &self.buf[offset..offset + self.size]
    }
}

/// SplitMix64.
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

fn shuffle(v: &mut [u64], rng: &mut Rng) {
    for i in (1..v.len()).rev() {
        v.swap(i, rng.below(i as u64 + 1) as usize);
    }
}

fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}
