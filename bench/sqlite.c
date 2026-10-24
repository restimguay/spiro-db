// The spiro-bench workloads, run against SQLite for comparison. Usually run
// through bench/compare.sh.
//
//   bench_sqlite sql <dir> [n]           the `spiro-bench --sql` suite
//   bench_sqlite kv  <dir> [n] [threads] the `spiro-bench` key-value suite, on a
//                                        WITHOUT ROWID (k BLOB PRIMARY KEY, v BLOB) table
//
// Same rows, same random keys (identical SplitMix64 generator and seeds), same
// operation counts. SQLite runs in WAL mode with synchronous=NORMAL (commits
// survive a process crash, as SpiroDB's default does) and a 64 MiB page cache
// (SpiroDB's block cache size).

#include <pthread.h>
#include <sqlite3.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

static uint64_t rng_next(uint64_t *s) {
    *s += 0x9e3779b97f4a7c15ULL;
    uint64_t z = *s;
    z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9ULL;
    z = (z ^ (z >> 27)) * 0x94d049bb133111ebULL;
    return z ^ (z >> 31);
}
static uint64_t below(uint64_t *s, uint64_t n) { return rng_next(s) % n; }

static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

static void report(const char *name, uint64_t ops, double secs, double bytes_per_op, const char *note) {
    char tput[32] = "";
    if (bytes_per_op > 0) snprintf(tput, sizeof tput, "%.1f MB/s", ops * bytes_per_op / secs / 1e6);
    printf("%-22s %9.3f µs/op %13.0f ops/s %12s   %s\n", name, secs * 1e6 / ops, ops / secs, tput, note);
    fflush(stdout);
}

static void die(sqlite3 *db, const char *what) {
    fprintf(stderr, "%s: %s\n", what, sqlite3_errmsg(db));
    exit(1);
}
static void exec(sqlite3 *db, const char *sql) {
    char *err = NULL;
    if (sqlite3_exec(db, sql, NULL, NULL, &err) != SQLITE_OK) {
        fprintf(stderr, "%s: %s\n", sql, err);
        exit(1);
    }
}
static sqlite3_stmt *prep(sqlite3 *db, const char *sql) {
    sqlite3_stmt *s;
    if (sqlite3_prepare_v3(db, sql, -1, SQLITE_PREPARE_PERSISTENT, &s, NULL) != SQLITE_OK) die(db, sql);
    return s;
}
static void step_done(sqlite3 *db, sqlite3_stmt *s) {
    if (sqlite3_step(s) != SQLITE_DONE) die(db, "step");
    sqlite3_reset(s);
}

static sqlite3 *open_db(const char *path, int fresh) {
    if (fresh) {
        char buf[4096];
        remove(path);
        snprintf(buf, sizeof buf, "%s-wal", path);
        remove(buf);
        snprintf(buf, sizeof buf, "%s-shm", path);
        remove(buf);
    }
    sqlite3 *db;
    if (sqlite3_open(path, &db) != SQLITE_OK) die(db, "open");
    exec(db, "PRAGMA journal_mode=WAL");
    exec(db, "PRAGMA synchronous=NORMAL");
    exec(db, "PRAGMA cache_size=-65536");
    return db;
}

// ------------------------------------------------------------------ SQL suite

static void sql_suite(const char *path, uint64_t n) {
    sqlite3 *db = open_db(path, 1);
    exec(db, "CREATE TABLE bench (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score INTEGER, note TEXT)");
    exec(db, "CREATE INDEX bench_score ON bench (score)");
    uint64_t groups = n / 10 ? n / 10 : 1;
    const char *note = "lorem ipsum dolor sit amet";
    char name[32];
    sqlite3_stmt *ins = prep(db, "INSERT INTO bench VALUES (?, ?, ?, ?)");
#define BIND_ROW(i)                                                                      \
    do {                                                                                 \
        sqlite3_bind_int64(ins, 1, (sqlite3_int64)(i));                                  \
        int len = snprintf(name, sizeof name, "user%llu", (unsigned long long)(i));      \
        sqlite3_bind_text(ins, 2, name, len, SQLITE_TRANSIENT);                          \
        sqlite3_bind_int64(ins, 3, (sqlite3_int64)((i) % groups));                       \
        sqlite3_bind_text(ins, 4, note, -1, SQLITE_STATIC);                              \
    } while (0)

    double t = now();
    for (uint64_t start = 0; start < n; start += 1000) {
        exec(db, "BEGIN");
        for (uint64_t i = start; i < start + 1000 && i < n; i++) {
            BIND_ROW(i);
            step_done(db, ins);
        }
        exec(db, "COMMIT");
    }
    report("insert, 1000 per txn", n, now() - t, 0, "");

    uint64_t single = n / 10 ? n / 10 : 1;
    t = now();
    for (uint64_t i = n; i < n + single; i++) {
        BIND_ROW(i);
        step_done(db, ins);
    }
    report("insert, autocommit", single, now() - t, 0, "");
    uint64_t total = n + single;
    char note_buf[64];

    sqlite3_stmt *by_id = prep(db, "SELECT name FROM bench WHERE id = ?");
    uint64_t rng = 1, found = 0;
    t = now();
    for (uint64_t k = 0; k < n; k++) {
        sqlite3_bind_int64(by_id, 1, (sqlite3_int64)below(&rng, total));
        if (sqlite3_step(by_id) == SQLITE_ROW && sqlite3_column_text(by_id, 0)) found++;
        sqlite3_reset(by_id);
    }
    snprintf(note_buf, sizeof note_buf, "%llu of %llu found", (unsigned long long)found, (unsigned long long)n);
    report("select by primary key", n, now() - t, 0, note_buf);

    sqlite3_stmt *by_score = prep(db, "SELECT id FROM bench WHERE score = ?");
    uint64_t lookups = n / 10 ? n / 10 : 1, rows = 0;
    rng = 2;
    t = now();
    for (uint64_t k = 0; k < lookups; k++) {
        sqlite3_bind_int64(by_score, 1, (sqlite3_int64)below(&rng, groups));
        while (sqlite3_step(by_score) == SQLITE_ROW) rows += sqlite3_column_int64(by_score, 0) >= 0;
        sqlite3_reset(by_score);
    }
    snprintf(note_buf, sizeof note_buf, "%llu rows", (unsigned long long)rows);
    report("select by index", lookups, now() - t, 0, note_buf);

    sqlite3_stmt *range = prep(db, "SELECT id, name FROM bench WHERE id >= ? AND id < ?");
    uint64_t scans = n / 100 ? n / 100 : 1;
    rows = 0;
    rng = 3;
    t = now();
    for (uint64_t k = 0; k < scans; k++) {
        sqlite3_int64 start = (sqlite3_int64)below(&rng, total);
        sqlite3_bind_int64(range, 1, start);
        sqlite3_bind_int64(range, 2, start + 100);
        while (sqlite3_step(range) == SQLITE_ROW) rows += sqlite3_column_text(range, 1) != NULL;
        sqlite3_reset(range);
    }
    snprintf(note_buf, sizeof note_buf, "%llu rows", (unsigned long long)rows);
    report("select 100-row range", scans, now() - t, 0, note_buf);

    sqlite3_stmt *update = prep(db, "UPDATE bench SET note = ? WHERE id = ?");
    uint64_t updates = n / 10 ? n / 10 : 1;
    rng = 4;
    t = now();
    for (uint64_t k = 0; k < updates; k++) {
        sqlite3_bind_text(update, 1, "updated", -1, SQLITE_STATIC);
        sqlite3_bind_int64(update, 2, (sqlite3_int64)below(&rng, total));
        step_done(db, update);
    }
    report("update by primary key", updates, now() - t, 0, "");

    sqlite3_stmt *agg = prep(db, "SELECT count(*), avg(score) FROM bench");
    t = now();
    if (sqlite3_step(agg) != SQLITE_ROW) die(db, "aggregate");
    snprintf(note_buf, sizeof note_buf, "per row scanned; count = %lld", sqlite3_column_int64(agg, 0));
    sqlite3_reset(agg);
    report("count + avg, full scan", total, now() - t, 0, note_buf);

    sqlite3_stmt *grouped = prep(db, "SELECT score % 10, count(*) FROM bench GROUP BY 1");
    t = now();
    int ngroups = 0;
    while (sqlite3_step(grouped) == SQLITE_ROW) ngroups++;
    sqlite3_reset(grouped);
    snprintf(note_buf, sizeof note_buf, "per row scanned; %d groups", ngroups);
    report("group by, full scan", total, now() - t, 0, note_buf);
    sqlite3_close_v2(db);
}

// ------------------------------------------------------------------ key-value suite

#define KEY_LEN 16
#define VALUE_SIZE 100

static uint8_t values[VALUE_SIZE + 4096];

static void key(uint64_t i, char out[KEY_LEN]) {
    for (int j = KEY_LEN - 1; j >= 0; j--) {
        out[j] = (char)('0' + i % 10);
        i /= 10;
    }
}
static const uint8_t *value(uint64_t i) { return values + (i * 7919) % 4096; }

static sqlite3 *kv_fresh(const char *path) {
    sqlite3 *db = open_db(path, 1);
    exec(db, "CREATE TABLE kv (k BLOB PRIMARY KEY, v BLOB) WITHOUT ROWID");
    return db;
}

static void put(sqlite3 *db, sqlite3_stmt *ins, uint64_t i) {
    char k[KEY_LEN];
    key(i, k);
    sqlite3_bind_blob(ins, 1, k, KEY_LEN, SQLITE_TRANSIENT);
    sqlite3_bind_blob(ins, 2, value(i), VALUE_SIZE, SQLITE_STATIC);
    step_done(db, ins);
}

static int get(sqlite3_stmt *sel, const char *k, int len) {
    sqlite3_bind_blob(sel, 1, k, len, SQLITE_STATIC);
    int found = sqlite3_step(sel) == SQLITE_ROW && sqlite3_column_bytes(sel, 0) == VALUE_SIZE;
    sqlite3_reset(sel);
    return found;
}

struct reader {
    const char *path;
    uint64_t seed, ops, n, found;
};

static void *read_worker(void *arg) {
    struct reader *r = arg;
    sqlite3 *db;
    if (sqlite3_open_v2(r->path, &db, SQLITE_OPEN_READONLY | SQLITE_OPEN_NOMUTEX, NULL) != SQLITE_OK) die(db, "open");
    exec(db, "PRAGMA cache_size=-65536");
    sqlite3_stmt *sel = prep(db, "SELECT v FROM kv WHERE k = ?");
    char k[KEY_LEN];
    for (uint64_t j = 0; j < r->ops; j++) {
        key(below(&r->seed, r->n), k);
        r->found += get(sel, k, KEY_LEN);
    }
    sqlite3_finalize(sel);
    sqlite3_close_v2(db);
    return NULL;
}

static void kv_suite(const char *path, uint64_t n, int threads) {
    uint64_t seed = 9;
    for (size_t i = 0; i < sizeof values; i++) values[i] = (uint8_t)rng_next(&seed);
    double entry = KEY_LEN + VALUE_SIZE, t;
    char note[64];

    sqlite3 *db = kv_fresh(path);
    sqlite3_stmt *ins = prep(db, "INSERT INTO kv VALUES (?, ?)");
    t = now();
    for (uint64_t i = 0; i < n; i++) put(db, ins, i);
    report("fillseq", n, now() - t, entry, "");
    sqlite3_finalize(ins);
    sqlite3_close_v2(db);

    db = kv_fresh(path);
    ins = prep(db, "INSERT INTO kv VALUES (?, ?)");
    t = now();
    for (uint64_t start = 0; start < n; start += 1000) {
        exec(db, "BEGIN");
        for (uint64_t i = start; i < start + 1000 && i < n; i++) put(db, ins, i);
        exec(db, "COMMIT");
    }
    report("fillbatch", n, now() - t, entry, "batches of 1000");
    sqlite3_finalize(ins);
    sqlite3_close_v2(db);

    uint64_t *order = malloc(n * sizeof *order);
    for (uint64_t i = 0; i < n; i++) order[i] = i;
    seed = 42;
    for (uint64_t i = n - 1; i >= 1; i--) {
        uint64_t j = below(&seed, i + 1), tmp = order[i];
        order[i] = order[j];
        order[j] = tmp;
    }
    db = kv_fresh(path);
    ins = prep(db, "INSERT INTO kv VALUES (?, ?)");
    t = now();
    for (uint64_t i = 0; i < n; i++) put(db, ins, order[i]);
    report("fillrandom", n, now() - t, entry, "");
    free(order);
    sqlite3_finalize(ins);
    exec(db, "PRAGMA wal_checkpoint(TRUNCATE)");

    sqlite3_stmt *sel = prep(db, "SELECT v FROM kv WHERE k = ?");
    char k[KEY_LEN + 1];
    uint64_t found = 0;
    seed = 1;
    t = now();
    for (uint64_t i = 0; i < n; i++) {
        key(below(&seed, n), k);
        found += get(sel, k, KEY_LEN);
    }
    snprintf(note, sizeof note, "%llu of %llu found", (unsigned long long)found, (unsigned long long)n);
    report("readrandom", n, now() - t, 0, note);

    pthread_t tids[64];
    struct reader readers[64];
    if (threads > 64) threads = 64;
    t = now();
    for (int i = 0; i < threads; i++) {
        uint64_t ops = n / threads + ((uint64_t)i < n % threads);
        readers[i] = (struct reader){path, 100 + (uint64_t)i, ops, n, 0};
        pthread_create(&tids[i], NULL, read_worker, &readers[i]);
    }
    found = 0;
    for (int i = 0; i < threads; i++) {
        pthread_join(tids[i], NULL);
        found += readers[i].found;
    }
    char label[32];
    snprintf(label, sizeof label, "readrandom x%d", threads);
    snprintf(note, sizeof note, "%llu of %llu found", (unsigned long long)found, (unsigned long long)n);
    report(label, n, now() - t, 0, note);

    found = 0;
    seed = 2;
    t = now();
    for (uint64_t i = 0; i < n; i++) {
        key(below(&seed, n), k);
        k[KEY_LEN] = 'x'; // between existing keys
        found += get(sel, k, KEY_LEN + 1);
    }
    snprintf(note, sizeof note, "%llu found", (unsigned long long)found);
    report("readmissing", n, now() - t, 0, note);

    sqlite3_stmt *all = prep(db, "SELECT k, v FROM kv");
    uint64_t count = 0;
    t = now();
    while (sqlite3_step(all) == SQLITE_ROW) count += sqlite3_column_bytes(all, 1) == VALUE_SIZE;
    sqlite3_reset(all);
    snprintf(note, sizeof note, "%llu entries", (unsigned long long)count);
    report("readseq", n, now() - t, entry, note);

    sqlite3_stmt *scan = prep(db, "SELECT k, v FROM kv WHERE k >= ? ORDER BY k LIMIT 100");
    uint64_t scans = n / 100 ? n / 100 : 1;
    count = 0;
    seed = 3;
    t = now();
    for (uint64_t i = 0; i < scans; i++) {
        key(below(&seed, n), k);
        sqlite3_bind_blob(scan, 1, k, KEY_LEN, SQLITE_STATIC);
        while (sqlite3_step(scan) == SQLITE_ROW) count += sqlite3_column_bytes(scan, 1) == VALUE_SIZE;
        sqlite3_reset(scan);
    }
    snprintf(note, sizeof note, "%llu scans of 100 keys, %llu entries", (unsigned long long)scans, (unsigned long long)count);
    report("scan100", scans, now() - t, 0, note);
    sqlite3_close_v2(db);
}

int main(int argc, char **argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: %s sql|kv <dir> [n] [threads]\n", argv[0]);
        return 1;
    }
    char path[4096];
    snprintf(path, sizeof path, "%s/bench.sqlite", argv[2]);
    int kv = strcmp(argv[1], "kv") == 0;
    uint64_t n = argc > 3 ? strtoull(argv[3], NULL, 10) : (kv ? 1000000 : 200000);
    int threads = argc > 4 ? atoi(argv[4]) : 10;
    printf("SQLite %s, %s suite, n = %llu (WAL, synchronous=NORMAL, 64 MiB cache)\n", sqlite3_libversion(),
           kv ? "key-value" : "SQL", (unsigned long long)n);
    printf("------------------------------------------------------------------------------------------\n");
    if (kv) kv_suite(path, n, threads);
    else sql_suite(path, n);
    return 0;
}
