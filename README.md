# SpiroDB

A fast embedded SQL database, written from scratch in Rust, with a server mode that replicates to read-only replicas and a client that routes every `SELECT` to them automatically.

SpiroDB depends only on the standard library. Everything is built in this repository: the SQL parser, planner and executor, the storage engine underneath, the checksums (hardware CRC-32C), the bloom filters, the block cache, and the on-disk formats. The storage engine is a log-structured merge tree (LSM tree), the design behind LevelDB and RocksDB. The SQL layer stores tables and indexes in it as sorted keys, the way CockroachDB and TiDB do.

```rust
use spirodb::sql::{Database, Value};

let db = Database::open("./my-data")?;
db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, age INTEGER)")?;
db.execute("INSERT INTO users (name, age) VALUES ('Ada', 36), ('Alan', 41)")?;

let rows = db.query("SELECT name FROM users WHERE age > ? ORDER BY name", &[Value::Integer(40)])?;
assert_eq!(rows.rows, vec![vec![Value::Text("Alan".into())]]);
println!("{}", rows.to_json()); // [{"name":"Alan"}]
```

## SQL

**Statements**

- `CREATE TABLE` (with `IF NOT EXISTS`) and `DROP TABLE` (with `IF EXISTS`)
- `CREATE [UNIQUE] INDEX` and `DROP INDEX`, on one or more columns
- `CREATE [OR REPLACE] VIEW name [(columns)] AS query` and `DROP VIEW`
- `ALTER TABLE ... ADD [COLUMN]`, `DROP [COLUMN]`, `RENAME [COLUMN] ... TO ...` and `RENAME TO`
- `INSERT ... VALUES` (many rows at once) and `INSERT ... SELECT`
- Upserts: `ON CONFLICT [(columns)] DO NOTHING | DO UPDATE SET ... [WHERE ...]`, plus SQLite's `INSERT OR IGNORE` and MySQL's `ON DUPLICATE KEY UPDATE`
- `UPDATE` and `DELETE`
- `RETURNING` on `INSERT`, `UPDATE` and `DELETE`
- `SELECT` with `WHERE`, `[INNER | LEFT] JOIN ... ON`, `CROSS JOIN`, `GROUP BY`, `HAVING`, `DISTINCT`, `ORDER BY` and `LIMIT`/`OFFSET`
- `UNION [ALL]`, `INTERSECT [ALL]` and `EXCEPT [ALL]`, with `INTERSECT` binding first as in standard SQL; `ORDER BY` and `LIMIT` after them apply to the whole result
- Subqueries: `(SELECT ...)` as a value, `[NOT] IN (SELECT ...)`, `[NOT] EXISTS (SELECT ...)`, and `FROM (SELECT ...) AS name [(columns)]`
- `WITH name [(columns)] AS (query), ...` and `WITH RECURSIVE`
- `BEGIN`, `COMMIT`, `ROLLBACK`, and `SAVEPOINT`, `RELEASE [SAVEPOINT]`, `ROLLBACK TO [SAVEPOINT]`
- `EXPLAIN`, which shows whether a query scans or uses a primary key or index

**Types and constraints**

- Types are `INTEGER`, `REAL`, `TEXT`, `BOOLEAN`, `DATE`, `TIME` and `TIMESTAMP`, plus the common aliases (`INT`, `BIGINT`, `VARCHAR(n)`, `DOUBLE PRECISION`, `DATETIME`, `TIMESTAMPTZ`, ...).
- Constraints are `PRIMARY KEY` (including composite keys), `NOT NULL`, `UNIQUE`, `CHECK`, `FOREIGN KEY`/`REFERENCES` and `DEFAULT`, including `DEFAULT CURRENT_TIMESTAMP` (or `CURRENT_DATE`, `CURRENT_TIME`). Table constraints may be named with `CONSTRAINT name`.
- An `INTEGER PRIMARY KEY` is assigned automatically when omitted.

**Expressions**

- Arithmetic, comparisons and `||`, with SQL's three-valued `NULL` logic
- `AND`/`OR`/`NOT`, `IS [NOT] NULL`, `[NOT] IN`, `[NOT] BETWEEN`, and `[NOT] LIKE`/`ILIKE`
- `CASE`, `CAST`
- Aggregates: `count(*)`, `count`, `sum`, `avg`, `min` and `max`, each optionally with `DISTINCT`
- Window functions: `row_number`, `rank`, `dense_rank`, `percent_rank`, `cume_dist`, `ntile`, `lag`, `lead`, `first_value`, `last_value`, `nth_value`, and the aggregates, with `OVER (PARTITION BY ... ORDER BY ... [ROWS | RANGE frame])` and named windows (`WINDOW w AS (...)`)
- Functions: `lower`, `upper`, `length`, `trim`, `substr`, `replace`, `abs`, `round`, `coalesce`, `nullif` and `typeof`
- Dates and times: see below

**Subqueries** may refer to the columns of the query around them (*correlated* subqueries), one level out:

```sql
SELECT name, (SELECT count(*) FROM orders o WHERE o.user_id = u.id) AS orders
FROM users u
WHERE EXISTS (SELECT 1 FROM orders WHERE user_id = u.id AND total > 100);
```

A subquery that reads no outer column runs once per statement; a correlated one runs once per distinct set of outer values it reads. `IN (SELECT ...)` collects the subquery's values into a hash set, and follows SQL's rules for NULL: `x NOT IN (...)` is unknown when the subquery returns a NULL. A subquery used as a value must return one column and at most one row (no rows reads as NULL).

**Common table expressions.** A `WITH` query can name those before it. `WITH RECURSIVE` runs `anchor UNION [ALL] recursive part`: the recursive part reads the rows the previous round added, until a round adds none. `UNION` drops rows already found, so walks over cyclic graphs end. Rows are produced as they are found, so a `LIMIT` stops even an endless query.

```sql
WITH RECURSIVE chain AS (
    SELECT id, name, 0 AS depth FROM staff WHERE boss IS NULL
    UNION ALL
    SELECT s.id, s.name, c.depth + 1 FROM staff s JOIN chain c ON s.boss = c.id
)
SELECT name, depth FROM chain ORDER BY depth;
```

**Window functions** see the rows left after `WHERE`, `GROUP BY` and `HAVING`, so they can rank groups too (`rank() OVER (ORDER BY sum(total) DESC)`). Without a frame, an ordered window runs from the start of the partition to the current row and the rows that tie with it, as in standard SQL; an unordered one covers the whole partition. Frames are `ROWS` with offsets (`ROWS BETWEEN 2 PRECEDING AND CURRENT ROW`) or `RANGE` with `UNBOUNDED`/`CURRENT ROW` bounds. Aggregates whose frames only grow are computed as running totals. To filter on a window value, wrap the query: `SELECT * FROM (SELECT ..., row_number() OVER (...) AS n FROM t) AS ranked WHERE n = 1`.

**Views** are stored queries that read like tables, in joins, subqueries and `WITH` queries alike. They are planned afresh each time they are used, so they see the tables' current columns and data; they can't be written to, and can't refer to themselves, even through other views. `.schema` in the shell shows them as written.

**Dates and times.** `DATE`, `TIME` and `TIMESTAMP` values are checked when stored and kept as ISO 8601 text in UTC: `2024-03-15`, `09:30:00`, `2024-03-15 09:30:00.25`. Input may use `T` instead of the space and end in `Z` or an offset such as `+02:00`, which is converted to UTC. These forms sort as text in time order, so comparisons, `ORDER BY`, `BETWEEN` and indexes need nothing special. Compare against literals in the same form (`at >= '2024-03-01'`).

- Literals: `DATE '2024-03-15'`, `TIME '09:30'`, `TIMESTAMP '2024-03-15 09:30'`, and `CAST(x AS DATE)`
- The current time: `now()`, `CURRENT_TIMESTAMP`, `CURRENT_DATE` and `CURRENT_TIME`, which read the clock once per statement
- Arithmetic: `at + INTERVAL '1 day 2 hours'`, `day - INTERVAL 3 MONTH`, `date_add(x, '2 weeks')` and `date_sub`. Adding a month to the 31st gives the last day of a shorter month, as in PostgreSQL.
- PostgreSQL's `EXTRACT(field FROM x)`, `date_part` and `date_trunc` (`year`, `quarter`, `month`, `week`, `day`, `hour`, `minute`, `second`, plus `dow`, `doy`, `epoch`, ...)
- SQLite's `date`, `time`, `datetime`, `strftime`, `julianday` and `unixepoch`, with modifiers such as `'+1 day'`, `'start of month'`, `'weekday 1'` and `'unixepoch'`
- MySQL's `datediff(a, b)`, the number of days from `b` to `a`

**Foreign keys** refer to a primary key or unique columns, by default the referenced table's primary key. A reference with a NULL in it refers to nothing. `ON DELETE` and `ON UPDATE` take `NO ACTION` (the default), `RESTRICT`, `CASCADE`, `SET NULL` or `SET DEFAULT`, and actions chain through further foreign keys. References are checked when the statement's writes are done, so rows of one statement may refer to each other, and `UPDATE staff SET id = id + 10` works on a table that refers to itself. A table can't be dropped while another refers to it. Finding the rows that refer to a deleted or changed row uses an index on the referencing columns when there is one, and reads the whole referencing table when there isn't, so index foreign key columns of large tables.

**CHECK** constraints refuse rows for which the expression is false; unknown (NULL) passes, as in standard SQL.

```sql
CREATE TABLE orders (
    id INTEGER PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users ON DELETE CASCADE,
    total REAL CHECK (total >= 0)
);
```

**Upserts.** A row whose primary key or unique value is already taken can be skipped or merged into the stored row. In `DO UPDATE`, `excluded.column` is the value the insert proposed, and a bare column name is the stored row's. MySQL's `VALUES(column)` means the same as `excluded.column`. Naming columns in `ON CONFLICT (...)` limits which constraint counts. A collision on any other constraint still fails.

```sql
INSERT INTO page_views (path, views) VALUES ('/home', 1)
ON CONFLICT (path) DO UPDATE SET views = views + excluded.views
RETURNING views;
```

`RETURNING` gives back the rows as written: auto-assigned ids and defaults included, the new versions for `UPDATE`, the removed rows for `DELETE`. An upsert returns the rows it inserted or updated, not the ones it skipped.

**Parameters.** Use `?` or `$1` placeholders with `execute_with` and `query`, or `prepare` a statement once and run it many times. Parameters are always values, never SQL, so they are safe from injection.

**Transactions.** Every statement is atomic: if it fails partway, none of its changes remain. A session can group statements with `BEGIN` ... `COMMIT`. Inside one, `SAVEPOINT name` marks a point that `ROLLBACK TO name` returns to, undoing only the changes since; the savepoint stays open, and `RELEASE name` forgets it. Uncommitted changes are visible only to their own session, and a failing statement inside a transaction is undone without aborting the rest. `BEGIN` takes the database's write lock, like SQLite's `BEGIN IMMEDIATE`. Writers therefore take turns and transactions are serializable. Reads outside a transaction never wait and see the latest committed data.

**Notifications.** `LISTEN`, `UNLISTEN` and `NOTIFY` work as in PostgreSQL:

- A notification is delivered when the sending transaction commits, never if it rolls back, and listeners receive notifications in commit order.
- Identical notifications sent in one transaction are delivered once.
- A session receives its own notifications.
- Inside a transaction, `LISTEN` takes effect when the transaction commits.
- Payloads are text, up to 8,000 bytes, and may be a parameter (`NOTIFY jobs, $1`).

```rust
let mut worker = db.session();
worker.execute("LISTEN jobs")?;
db.execute("NOTIFY jobs, 'resize image 42'")?;
for n in worker.notifications() {          // or wait_for_notification(timeout),
    println!("{}: {}", n.channel, n.payload); // or notification_receiver() for another thread
}
```

The shell prints notifications after each statement, as `psql` does. The Node client emits them as events: `db.on("notification", ({ channel, payload }) => …)`, along with `db.listen()`, `db.notify()` and `db.unlisten()`.

Embedded, notifications travel between the sessions of one process, because only one process can open a database directory. Through [`spiro-server`](#server-and-replication) they reach any client, and replicas deliver them too.

**Not supported yet:** `RIGHT`/`FULL` joins, which the parser reports with a clear error.

### How SQL maps onto the storage engine

Every row and every index entry is a key in the engine, encoded so that comparing the bytes gives the same order as comparing the values:

```
00 'T' <table name>                                   table definition
00 'R' <table id> <primary key>                       row (value: the encoded columns)
00 'X' <index id> <indexed values> <primary key>      index entry
```

A primary-key lookup is a single `get`, and a range of keys is a range scan, because the engine keeps keys sorted. A secondary-index lookup scans the index entries for a value and fetches each row by the primary key at the end of the entry. Tables without a primary key get a hidden, auto-assigned row id. The planner picks the cheapest access path for each table, as `EXPLAIN` shows:

```
spiro> EXPLAIN SELECT * FROM users u JOIN orders o ON o.user_id = u.id WHERE u.id = 1;
 SEARCH users AS u USING PRIMARY KEY (id = ?)
 JOIN SEARCH orders AS o USING INDEX orders_user (user_id = ?)

spiro> EXPLAIN SELECT id FROM users WHERE age BETWEEN 30 AND 40;
 SEARCH users USING INDEX users_age (age >= ? AND age <= ?) (index only)

spiro> EXPLAIN SELECT * FROM users WHERE id > 10 AND name LIKE 'A%';
 SEARCH users USING PRIMARY KEY (id > ?)
   FILTER
```

- **`FILTER`** appears only when rows must still be checked against conditions the key range doesn't already guarantee.
- **`(index only)`** marks reads answered entirely from the index: every index entry holds the indexed values and the primary key, so the rows themselves are never fetched.
- **Prepared statements keep their plan** and reuse it for every execution until the schema changes.

A test runs thousands of random queries and updates against an indexed table and an identical unindexed one, and requires identical results every time.

### Reading rows as they come

`execute_prepared` and `query` collect every row into owned values. For big results, `query_each` hands each row to a closure as it is produced instead. The row is lent from one buffer that is reused for the next row, text included, so a streaming query allocates nothing per row:

```rust
let range = db.prepare("SELECT id, name FROM users WHERE id >= ? AND id < ?")?;
let mut session = db.session();
session.query_each(&range, &[Value::Integer(100), Value::Integer(200)], |row| {
    println!("{} {}", row[0], row[1]);
    true // false stops early
})?;
```

Queries that group, sort or remove duplicates need every row first; those are collected, then handed over one by one.

### Aggregates: batches and every core

Aggregate queries on one table (`count`, `sum`, `avg`, `min`, `max`, with or without `GROUP BY`, `WHERE` and `HAVING`) take a faster path:

- **Columns.** Every table file also stores its SQL rows column by column (see [column sections](#column-sections)), so a full scan reads only the columns the query uses. Integers and reals are fixed-width arrays, copied into the batch a run at a time without decoding any rows.
- **Batches.** Rows are gathered 1,024 at a time, decoding only the columns the query reads straight from the stored bytes. Integer and real columns become plain arrays, with a mask for NULLs. Expressions such as `score % 10` or `price * qty > 100` then run as tight loops over the arrays, and groups are found by hashing, with a fast path for a single integer key.
- **Every core.** A full scan of a large table is split into key ranges at the boundaries of the stored blocks, plus sampled keys from the in-memory table. A pool of threads, one per core, takes turns claiming ranges, so the faster cores take more. Each thread aggregates its share, and the partial groups are merged. The threads read from one fixed set of files, so they all see the same data. `db.set_scan_threads(n)` caps the threads one query may use; 1 keeps every query on a single core.
- **Same answers.** Anything without a fast path (text functions, `CASE`, an integer overflow, a division by zero) is evaluated row by row with the ordinary evaluator, so results and error messages are the same. A test runs about 45 aggregate queries both ways over 20,000 random rows with NULLs, reals, text and booleans, single-threaded and split, and requires identical results.

Scans also got cheaper underneath. Each layer of the storage engine now loops over its own entries, instead of every entry climbing through all the layers. A block whose last key is inside the range needs no per-entry bound checks.

### Column sections

Rows stay in the table files' blocks, where point lookups, inserts and updates want them. Whenever the engine writes a table file (a flush or a compaction), it also passes every entry to the SQL layer, which lays out each SQL table's rows in that file as a *chunk*:

- the primary keys in order (plain integers for an `INTEGER PRIMARY KEY` or the hidden row id);
- one flag per row that marks deletions;
- each column as its own array: 8 bytes per integer or real, a byte per boolean, offsets and bytes for text, with a mask for NULLs.

The section is stored in the table file it describes, after the index, with its own checksum, so it is written, compacted and deleted along with that file. The background thread that writes a file also parses its section, so queries find it ready.

A full scan merges the newest data with the files, as every read in an LSM tree does: each key's newest version counts, and deletions hide older ones. The memtable's rows are laid out the same way as they are read, a few thousand at a time and decoding only the columns needed. For aggregates the order of rows doesn't matter, so tables keyed by integers skip the ordered merge. Each source in turn, newest first, hands over every row that no newer source has a version of, jumping from one newer key to the next, and the long stretches between go over whole. Other tables use an ordered merge that still moves in runs.

Sections cost disk space and background work. For the benchmark's table, mostly text, the database takes 30 MB with them and 17 MB without; insert speed is unchanged. Turn them off with `Options { column_sections: false, .. }`; full scans then read rows, as before.

## The `spiro` shell

```sh
cargo build --release
./target/release/spiro ./my-data
```

```
spiro> CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER);
CREATE TABLE
spiro> INSERT INTO users (name, age) VALUES ('Ada', 36), ('Grace', 85);
INSERT 2
spiro> SELECT * FROM users WHERE age > 40;
 id | name  | age
----+-------+-----
  2 | Grace |  85
(1 row)
```

Statements end with `;` and may span lines. The prompt changes to `spiro*>` while a transaction is open. Dot commands:

| Command | Effect |
|---|---|
| `.tables`, `.schema [table]` | List tables, or show their `CREATE` statements |
| `.mode table` / `.mode json` | Print results as tables or as JSON |
| `.timer on` / `.timer off` | Show how long each statement takes |
| `.stats`, `.flush` | Show storage engine statistics, or flush buffered data to disk |

To run SQL from a script, pass it as an argument:

```sh
./target/release/spiro ./my-data --json "SELECT * FROM users"
# [{"id":1,"name":"Ada","age":36},{"id":2,"name":"Grace","age":85}]
```

## Using SpiroDB from JavaScript

Query results come back as plain JavaScript arrays of row objects. [`examples/node/spirodb.mjs`](examples/node/spirodb.mjs) is a Node.js client with no npm dependencies. It keeps one `spiro --json` process open and exchanges one JSON line per statement:

```js
import { SpiroDB, sql } from "./examples/node/spirodb.mjs";

const db = new SpiroDB("./my-data", { binary: "./target/release/spiro" });
await db.query("CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER)");
await db.query(sql`INSERT INTO users (name, age) VALUES (${"Miles O'Brien"}, ${52})`);

const users = await db.query("SELECT * FROM users ORDER BY id");
// [ { id: 1, name: "Miles O'Brien", age: 52 } ]
console.log(users.map((u) => u.name));
await db.close();
```

The `sql` tagged template quotes interpolated values safely. Failed statements reject with the database's error message, and later queries keep working. Run the demo with `node examples/node/demo.mjs`, and the tests with `node --test examples/node/spirodb.test.mjs`.

## Server and replication

`spiro-server` serves a database over TCP, so many processes and languages can share it. Servers can replicate: a **primary** takes the writes and streams every commit to any number of read-only **replicas**.

```sh
spiro-server ./primary --listen 10.0.0.1:7878
spiro-server ./replica1 --listen 10.0.0.2:7878 --replica-of 10.0.0.1:7878
spiro-server ./replica2 --listen 10.0.0.3:7878 --replica-of 10.0.0.1:7878
```

A new replica copies a snapshot of the primary while the primary keeps taking writes, then applies each commit as it happens. A replica that restarts, or loses its connection, resumes from its last applied commit. Each replica's position is stored in the same atomic write as the data it applies, so a crash never leaves the two out of step.

### Auto-routing clients

Give the client every server's address and use it like one database. It works out which server is which and routes each statement:

- **`SELECT`** (and `EXPLAIN`) goes to a replica, round-robin across them.
- **Writes, transactions** (everything from `BEGIN` to `COMMIT`, including their reads) **and `LISTEN`** go to the primary.
- **You always read your own writes.** Each read carries the sequence number of the client's latest commit. A replica that hasn't applied it yet answers "behind", and the read goes to the primary instead. Replicas that are down are skipped.

```rust
use spirodb::client::Client;

let mut db = Client::connect(&["10.0.0.1:7878", "10.0.0.2:7878", "10.0.0.3:7878"])?;
db.execute("INSERT INTO users (name) VALUES ('Ada')")?;     // the primary
let rows = db.query("SELECT * FROM users", &[])?;            // a replica, which has Ada
```

```js
import { SpiroCluster } from "./examples/node/cluster.mjs";

const db = await SpiroCluster.connect(["10.0.0.1:7878", "10.0.0.2:7878", "10.0.0.3:7878"]);
await db.query("INSERT INTO users (name) VALUES (?)", ["Ada"]); // the primary
const rows = await db.query("SELECT * FROM users");             // a replica: [{ id: 1, name: "Ada" }]
db.on("notification", ({ channel, payload }) => console.log(channel, payload));
await db.listen("jobs");
```

The Rust client classifies statements with SpiroDB's own parser. The JavaScript client goes by the first keyword. Either way the server checks: a write that reaches a replica is refused without running, and the client resends it to the primary.

### Synchronous replication

Replication is asynchronous by default: a commit returns once it is durable on the primary, and replicas follow within a moment. To make every commit wait until replicas have it, use `--sync-replicas N` (or `db.set_sync_replicas(n, timeout)`). If replicas don't confirm in time, the commit has still happened on the primary, but it returns an error saying so.

### Failover

Promotion is manual, so a network blip can't produce two primaries:

```sh
spiro-server --promote 10.0.0.2:7878                  # stop the old primary first
spiro-server --follow 10.0.0.3:7878 10.0.0.2:7878     # point the other replicas at the new one
spiro-server --follow 10.0.0.1:7878 10.0.0.2:7878     # the old primary rejoins as a replica
spiro-server --status 10.0.0.2:7878                   # role, epoch, position, replica acknowledgements
```

Each promotion starts a new **epoch**. Clients pick the primary with the highest epoch, so an old primary that comes back still claiming to be primary is ignored. Replicas refuse to follow a primary from an older epoch. Servers keep a history of where each epoch began. When a server rejoins holding commits the new primary never received (writes the old primary took after the last replica copied from it), it is resynced from a snapshot, so it never carries a diverged history. With asynchronous replication, those commits are lost in a failover. Synchronous replication prevents that.

Clients rediscover the primary when it stops answering. Reads are retried elsewhere. A write is not retried when its connection breaks mid-request, because it may have committed, so the error is returned to you.

### The protocol

One JSON object per line, in both directions, so any language with sockets can talk to a server:

```text
{"sql": "SELECT name FROM users WHERE id = ?", "params": [1]}  ->  {"columns": ["name"], "rows": [["Ada"]]}
{"sql": "INSERT INTO users (name) VALUES ('Alan')"}           ->  {"command": "INSERT", "rows": 1, "seq": 43}
{"sql": "SELECT * FROM users", "min_seq": 43}                  ->  rows, or {"error": "...", "kind": "behind"}
{"status": true}  {"promote": true}  {"follow": "host:port"}
```

Each connection is a session: transactions span requests, and notifications arrive as extra `{"notification": {...}}` lines. Errors carry a `kind`: `syntax`, `constraint`, `sql`, `readonly` or `behind`.

## The storage engine

The engine is also usable on its own as an ordered key-value store:

```rust
use spirodb::{Db, WriteBatch};

let db = Db::open("./kv-data")?;
db.put("user:1", "Ada Lovelace")?;
let mut batch = WriteBatch::new(); // several writes, applied atomically
batch.put("user:2", "Alan Turing").delete("user:1");
db.write(&batch)?;
for item in db.range("user:".."user;") {
    let (key, value) = item?;
}
```

```
  put / delete / write(batch)                                get / range
            │                                                      │
            ▼                                                      ▼
   ┌─────────────────┐   ┌──────────────────┐        ┌──────────────────────────┐
   │ write-ahead log │──▶│ memtable (active)│◀───────│ 1. active memtable       │
   │  000007.log     │   └────────┬─────────┘        │ 2. frozen memtable       │
   └─────────────────┘            │ full: freeze     │ 3. level 0, newest first │
                                  ▼                  │ 4. level 1, binary search│
                         ┌──────────────────┐        └──────────────────────────┘
                         │ memtable (frozen)│   each table: key-range check →
                         └────────┬─────────┘   bloom filter → index → 1 block
                 flush thread     │
                                  ▼
   level 0   [sst] [sst] [sst] [sst]      overlapping, newest first
                 compaction thread│  merge, drop overwritten values + tombstones
                                  ▼
   level 1   [sst][sst][sst][sst][sst]    one sorted run, no overlap
```

- **Writes** append one record to the write-ahead log, then insert into the in-memory, sorted *memtable*. There is no random I/O on the write path.
- **The memtable** is a B+tree built for this job: nodes of 64 short keys stored inline, searched by bisection, and a few *fingers* remembering the leaves last written. Keys that keep growing (a table's row ids, its index entries) land in a remembered leaf without a search from the root. The standard library's `BTreeMap` searches each node from the left, so increasing keys scan every node on the way down.
- **New row ids skip the uniqueness check.** A table's row-id counter is above every id it has ever stored, and it is written in the same atomic batch as the rows. An `INSERT` with an id at or past the counter therefore can't collide, and doesn't probe the memtable and every table's bloom filter to make sure, as SQLite knows for its rowid tables.
- **Flushes.** When the memtable reaches `memtable_size`, it is frozen and a fresh one takes over. A background thread writes the frozen memtable out as an immutable, sorted table file in level 0.
- **Compaction.** When level 0 holds `l0_compaction_trigger` tables, a second background thread merges them with the overlapping level-1 tables. Overwritten values and deletion markers disappear in the process. If level 0 backs up to `l0_stop_writes_trigger`, writers pause until compaction catches up.
- **Scans** come in two forms: `db.range(..)`, a standard iterator that yields owned copies, and `db.scan(.., |key, value| ...)`, which lends each entry from the engine's own buffers and allocates nothing per entry.
- **Reads** consult the memtables, then level 0 newest first, then level 1 by binary search. In each table, a bloom filter rules out most absent keys without touching disk, an in-memory index locates the one block that could hold the key, and a CLOCK block cache keeps hot blocks in memory.
- **Concurrent reads.** Readers never touch the global lock. The current set of memtables and tables (the *super version*) is published into 32 cache-line-padded slots, and each thread reads its own, so readers on different cores don't contend.

### Durability and recovery

- Every byte on disk is covered by a CRC-32C checksum: log records, table blocks, the bloom filter, the index and the manifest.
- The `MANIFEST` lists the live tables and the oldest log still needed. It is rewritten whole and atomically (write a temp file, fsync, rename), so a crash leaves either the old version or the new one.
- On open, SpiroDB replays any unflushed logs and stops at a torn tail left by a crash mid-write. It writes the replayed data to a table, then deletes files the manifest no longer references. If the manifest itself is missing while tables exist, it refuses to open rather than guess.
- With `Options::sync = false` (the default), acknowledged writes survive a process crash but may be lost if the machine loses power. Set `sync = true` to fsync the log on every write.
- Table files and the manifest are always synced before anything depends on them. With `sync = false` on macOS, that sync is a write barrier (`F_BARRIERFSYNC`), which keeps writes in order without flushing the drive's cache. SQLite doesn't flush the drive's cache by default either. A power failure can then lose the newest data but never leaves a manifest naming a table that isn't there. With `sync = true` it is a full flush (`F_FULLFSYNC`). A full flush holds up every other write to the disk, so using one for each background table write slowed foreground inserts by about 20%.
- A `LOCK` file prevents two processes from opening the same database.

### On-disk formats

| File | Layout |
|---|---|
| `NNNNNN.log` | records of `[crc32c u32][len u32][write batch]` |
| `NNNNNN.sst` | `[data block + crc]… [bloom filter + crc] [index block + crc] [column section + crc] [64-byte footer]` |
| `MANIFEST` | `[magic][crc][next file, log number, per level: tables with number, size, key range]` |

The column section is optional, and files with the older 48-byte footer (no section) still open. Its layout is described in [`src/sql/columnar.rs`](src/sql/columnar.rs). Data blocks are about 4 KiB. Each key is stored as the length it shares with the previous key plus the remainder, with a full key every 16 entries (a *restart point*). A lookup binary-searches the restart points, then scans at most 16 entries.

## Performance: SpiroDB vs SQLite, PostgreSQL, MariaDB, RocksDB and DuckDB

[`bench/compare.sh`](bench/compare.sh) runs identical workloads against every engine on the same machine: the same rows, the same random keys and the same operation counts. Each round runs every engine in turn, and the tables show each engine's best of several rounds. The settings match SpiroDB's defaults as closely as each engine allows:

- **Durability:** commits survive a crash of the database process but may be lost if the machine loses power. That means SQLite in WAL mode with `synchronous=NORMAL`, PostgreSQL with `synchronous_commit=off`, MariaDB with `innodb_flush_log_at_trx_commit=2` and the binary log off, and RocksDB's log without an fsync per write.
- **Memory:** each engine gets a 64 MiB cache (SQLite's page cache, PostgreSQL's `shared_buffers`, MariaDB's InnoDB buffer pool, RocksDB's block cache).
- **One core per query:** PostgreSQL's parallel workers are off, DuckDB runs one thread, and SpiroDB runs with `--scan-threads 1`, since SQLite and MariaDB run each query on a single core.
- **Rows read as they come:** each query's rows are read as the engine produces them, as SQLite's `sqlite3_step` hands them out. SpiroDB's benchmark uses [`query_each`](#reading-rows-as-they-come) for this. With `execute_prepared`, which collects every row into owned values first, the 100-row range costs about 8 µs instead of 3.3.
- **RocksDB matches SpiroDB's storage configuration:** an 8 MiB memtable, 4 KiB blocks, 10-bit bloom filters, and no compression.
- **Prepared statements** everywhere, with the two servers reached over Unix sockets, their fastest local connection.

PostgreSQL and MariaDB run as private, throwaway servers that the script starts and deletes, so they never touch an existing installation. Engines that aren't installed are skipped. RocksDB has no SQL, so it runs only in the key-value suite, and DuckDB only in the SQL suite. The benchmark programs for the other engines are [`bench/sqlite.c`](bench/sqlite.c), [`bench/postgres.c`](bench/postgres.c), [`bench/mariadb.c`](bench/mariadb.c), [`bench/rocksdb.c`](bench/rocksdb.c) and [`bench/duckdb.c`](bench/duckdb.c).

```sh
bench/compare.sh --suite sql   # 200,000 rows, best of 2 rounds
bench/compare.sh --suite kv    # 1,000,000 keys
```

These numbers come from an Apple M5 (4 performance + 6 efficiency cores) running SQLite 3.51, PostgreSQL 17.11, MariaDB 12.3, RocksDB 11.8 and DuckDB 2.0.

### SQL

A table with an `INTEGER PRIMARY KEY`, three other columns and a secondary index, 200,000 rows, best of 3 rounds. Times are microseconds per operation, so lower is better, and the fastest is in bold:

| Workload | SpiroDB | SQLite | PostgreSQL | MariaDB | DuckDB |
|---|---:|---:|---:|---:|---:|
| `INSERT`, autocommit | **2.5** | 18.0 | 13.1 | 18.4 | 115 |
| `UPDATE` by primary key | **4.0** | 13.1 | 15.0 | 20.1 | 126 |
| `SELECT` by primary key | **0.78** | 2.1 | 11.4 | 11.8 | 101 |
| `SELECT` by index (10 rows) | **1.6** | 2.4 | 16.0 | 14.6 | 79 |
| `INSERT`, 1,000 per transaction | **0.67** | 0.84 | 11.6 | 13.7 | 43 |
| `SELECT` 100-row range | **3.4** | 4.9 | 26.3 | 25.9 | 257 |
| `count(*)` + `avg()`, full scan (per row) | **0.006** | 0.016 | 0.038 | 0.061 | 0.007 |
| `GROUP BY`, full scan (per row) | **0.007** | 0.070 | 0.057 | 0.088 | 0.008 |

SpiroDB is fastest at every workload here. This run was made on a busy machine (load average about 3.5, the same for every engine), which is why some rows read slower than in a quiet run.

The two full scans are the closest. DuckDB stores data by column and processes it in batches, which makes it very fast at full-table aggregates (and slow at single-row work). SpiroDB keeps a copy of each table's columns in its table files ([column sections](#column-sections)) and aggregates them in batches, which brings it level with DuckDB or just ahead: in other runs DuckDB's `GROUP BY` measured anywhere from 0.006 to 0.015, so on that row the order can flip from one run to the next. With every core, the default, SpiroDB's full scans measure 0.004 to 0.005 (`count` + `avg`) and 0.003 (`GROUP BY`).

PostgreSQL and MariaDB are servers: every statement crosses a socket to another process, which costs them 11 to 12 µs per statement here before any work is done. Their small-query numbers mostly measure that, as for any client-server database. Their query engines show in the full scans, where that cost is paid once.

### Key-value

The storage engine on its own, against RocksDB and against the other three used as ordered key-value stores (a table with a binary primary key and a value column). The data is 1,000,000 keys of 16 bytes with 100-byte values, best of 2 rounds, in microseconds per operation:

| Workload | SpiroDB | RocksDB | SQLite | PostgreSQL | MariaDB |
|---|---:|---:|---:|---:|---:|
| Sequential writes | **1.4** | 1.6 | 9.9 | 11.7 | 15.8 |
| Batched writes (1,000 per batch) | **0.16** | 0.34 | 0.83 | 10.1 | 11.9 |
| Random writes | **1.5** | 2.1 | 48.5 | 12.3 | 34.8 |
| Random reads | **1.1** | 1.5 | 5.0 | 14.6 | 21.6 |
| Random reads, 10 threads (per read) | **0.30** | 0.45 | 3.4 | 3.7 | 5.6 |
| Reads of missing keys | **0.16** | 0.28 | 5.3 | 11.7 | 21.5 |
| Seek + read 100 keys | **4.9** | 12.2 | 8.9 | 92 | 125 |
| Full scan (per key) | **0.037** | 0.092 | 0.075 | 0.117 | 0.293 |

Scans read each entry where it lies, as the other engines' iterators hand out pointers into their own buffers. SpiroDB's benchmark uses `Db::scan`, which lends each key and value to a closure. `Db::range` is a standard Rust iterator, so it must copy every entry it yields, which roughly doubles the cost of a scan.

RocksDB shares SpiroDB's LSM design, so it is the like-for-like comparison. Configured with the same memtable, block size, bloom filters, cache and no compression, it trails SpiroDB on every workload here. Batched writes used to be its win. The memtable's B+tree (see [the storage engine](#the-storage-engine)) and ordered rather than full syncs of background table files took SpiroDB from 0.47 to 0.15.

At this size SpiroDB's simpler two-level structure is an advantage. RocksDB's multi-level compaction exists for datasets many times larger than this one, where it keeps write and space amplification bounded and SpiroDB's two levels would fall behind (see the roadmap). The other engines are B-trees, so the usual LSM trade-offs show:

- **Writes are appends** where B-trees rewrite pages, which matters most for random writes.
- **Bloom filters answer most lookups of missing keys** without reading a block.
- **Concurrent readers never contend** on shared locks.

## Development

```sh
cargo test                                   # Rust unit, integration and doc tests
node --test examples/node/spirodb.test.mjs   # the JavaScript client (after cargo build --release)
node --test examples/node/cluster.test.mjs   # the JavaScript cluster client, against real servers
cargo clippy --all-targets
cargo run --release --bin spiro-bench -- --sql   # SpiroDB alone (drop --sql for the key-value engine)
bench/compare.sh                             # side by side with SQLite and PostgreSQL
```

- **SQL:** the tests cover every statement and clause, `NULL` semantics, constraints, transactions and schema isolation, reopening from disk, parameters, concurrent writers, and the indexed-versus-unindexed differential test.
- **Replication:** the tests run real servers on local ports: routing and read-your-writes, snapshot bootstrap of 20,000 rows with indexes, restart and catch-up, lagging replicas, synchronous commits, notifications on replicas, and a failover where the old primary rejoins with a diverged commit and is resynced.
- **Storage engine:** the tests run randomized workloads against a `BTreeMap` model with tiny memtables, so every flush, compaction and reopen path gets exercised. They also cover torn-log recovery and locking.

## Roadmap

- **SQL:** `RIGHT` and `FULL` joins, and a cost-based join order.
- **Concurrency:** snapshots and MVCC through sequence numbers, so transactions read a consistent point in time and readers never see a half-applied scan.
- **Storage:** leveled compaction across L1–L6 with 10× size ratios (today's two levels re-merge all of level 1 as data grows), and group commit so concurrent writers share one log write and one fsync.
- **Speed:** keys and values of the memtable in one contiguous arena (recent writes interleaved with older data are now the slowest part of a full scan), batched execution for joins and plain scans as well as aggregates, and block compression implemented from scratch.
- **Replication:** automatic failover by consensus (Raft) among the servers, TLS and authentication on the server, and replicas fed from other replicas to fan out further.
- **Access from other languages:** a native Node.js module and a WebAssembly build for the browser.

## License

MIT OR Apache-2.0
