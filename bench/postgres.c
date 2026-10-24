// The spiro-bench workloads, run against PostgreSQL for comparison. Usually
// run through bench/compare.sh, which starts a private server for it.
//
//   bench_postgres sql "<conninfo>" [n]           the `spiro-bench --sql` suite
//   bench_postgres kv  "<conninfo>" [n] [threads] the `spiro-bench` key-value suite,
//                                                 on a (k BYTEA PRIMARY KEY, v BYTEA) table
//
// Same rows, same random keys (identical SplitMix64 generator and seeds), same
// operation counts as spiro-bench and bench/sqlite.c. Every statement is
// prepared once and run with PQexecPrepared; key-value data travels in binary.

#include <libpq-fe.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

typedef unsigned long long ull;

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

static PGconn *connect_db(const char *conninfo) {
    PGconn *c = PQconnectdb(conninfo);
    if (PQstatus(c) != CONNECTION_OK) {
        fprintf(stderr, "connect: %s", PQerrorMessage(c));
        exit(1);
    }
    return c;
}

static void check(PGresult *r, const char *what) {
    ExecStatusType s = PQresultStatus(r);
    if (s != PGRES_COMMAND_OK && s != PGRES_TUPLES_OK && s != PGRES_SINGLE_TUPLE) {
        fprintf(stderr, "%s: %s", what, PQresultErrorMessage(r));
        exit(1);
    }
}

static void exec(PGconn *c, const char *sql) {
    PGresult *r = PQexec(c, sql);
    check(r, sql);
    PQclear(r);
}

static void prepare(PGconn *c, const char *name, const char *sql) {
    PGresult *r = PQprepare(c, name, sql, 0, NULL);
    check(r, sql);
    PQclear(r);
}

/// Runs a prepared statement with text parameters and text results.
static PGresult *run(PGconn *c, const char *name, int nparams, const char *const *params) {
    PGresult *r = PQexecPrepared(c, name, nparams, params, NULL, NULL, 0);
    check(r, name);
    return r;
}

// ------------------------------------------------------------------ SQL suite

static void sql_suite(const char *conninfo, uint64_t n) {
    PGconn *c = connect_db(conninfo);
    exec(c, "SET client_min_messages = warning");
    exec(c, "DROP TABLE IF EXISTS bench");
    exec(c, "CREATE TABLE bench (id BIGINT PRIMARY KEY, name TEXT NOT NULL, score BIGINT, note TEXT)");
    exec(c, "CREATE INDEX bench_score ON bench (score)");
    uint64_t groups = n / 10 ? n / 10 : 1;
    char id[24], name[32], score[24], end[24], note_buf[64];
    const char *row[4] = {id, name, score, "lorem ipsum dolor sit amet"};
#define FILL_ROW(i)                                                    \
    do {                                                               \
        snprintf(id, sizeof id, "%llu", (ull)(i));                     \
        snprintf(name, sizeof name, "user%llu", (ull)(i));             \
        snprintf(score, sizeof score, "%llu", (ull)((i) % groups));    \
    } while (0)

    prepare(c, "ins", "INSERT INTO bench VALUES ($1, $2, $3, $4)");
    double t = now();
    for (uint64_t start = 0; start < n; start += 1000) {
        exec(c, "BEGIN");
        for (uint64_t i = start; i < start + 1000 && i < n; i++) {
            FILL_ROW(i);
            PQclear(run(c, "ins", 4, row));
        }
        exec(c, "COMMIT");
    }
    report("insert, 1000 per txn", n, now() - t, 0, "");

    uint64_t single = n / 10 ? n / 10 : 1;
    t = now();
    for (uint64_t i = n; i < n + single; i++) {
        FILL_ROW(i);
        PQclear(run(c, "ins", 4, row));
    }
    report("insert, autocommit", single, now() - t, 0, "");
    uint64_t total = n + single;
    exec(c, "ANALYZE bench"); // untimed: give the planner statistics

    prepare(c, "by_id", "SELECT name FROM bench WHERE id = $1");
    const char *one[1] = {id};
    uint64_t rng = 1, found = 0;
    t = now();
    for (uint64_t k = 0; k < n; k++) {
        snprintf(id, sizeof id, "%llu", (ull)below(&rng, total));
        PGresult *r = run(c, "by_id", 1, one);
        found += PQntuples(r) == 1 && PQgetlength(r, 0, 0) > 0;
        PQclear(r);
    }
    snprintf(note_buf, sizeof note_buf, "%llu of %llu found", (ull)found, (ull)n);
    report("select by primary key", n, now() - t, 0, note_buf);

    prepare(c, "by_score", "SELECT id FROM bench WHERE score = $1");
    uint64_t lookups = n / 10 ? n / 10 : 1, rows = 0;
    rng = 2;
    t = now();
    for (uint64_t k = 0; k < lookups; k++) {
        snprintf(id, sizeof id, "%llu", (ull)below(&rng, groups));
        PGresult *r = run(c, "by_score", 1, one);
        rows += PQntuples(r);
        PQclear(r);
    }
    snprintf(note_buf, sizeof note_buf, "%llu rows", (ull)rows);
    report("select by index", lookups, now() - t, 0, note_buf);

    prepare(c, "range", "SELECT id, name FROM bench WHERE id >= $1 AND id < $2");
    const char *bounds[2] = {id, end};
    uint64_t scans = n / 100 ? n / 100 : 1;
    rows = 0;
    rng = 3;
    t = now();
    for (uint64_t k = 0; k < scans; k++) {
        uint64_t start = below(&rng, total);
        snprintf(id, sizeof id, "%llu", (ull)start);
        snprintf(end, sizeof end, "%llu", (ull)(start + 100));
        PGresult *r = run(c, "range", 2, bounds);
        rows += PQntuples(r);
        PQclear(r);
    }
    snprintf(note_buf, sizeof note_buf, "%llu rows", (ull)rows);
    report("select 100-row range", scans, now() - t, 0, note_buf);

    prepare(c, "upd", "UPDATE bench SET note = $1 WHERE id = $2");
    const char *upd[2] = {"updated", id};
    uint64_t updates = n / 10 ? n / 10 : 1;
    rng = 4;
    t = now();
    for (uint64_t k = 0; k < updates; k++) {
        snprintf(id, sizeof id, "%llu", (ull)below(&rng, total));
        PQclear(run(c, "upd", 2, upd));
    }
    report("update by primary key", updates, now() - t, 0, "");

    t = now();
    PGresult *r = PQexec(c, "SELECT count(*), avg(score) FROM bench");
    check(r, "aggregate");
    snprintf(note_buf, sizeof note_buf, "per row scanned; count = %s", PQgetvalue(r, 0, 0));
    PQclear(r);
    report("count + avg, full scan", total, now() - t, 0, note_buf);

    t = now();
    r = PQexec(c, "SELECT score % 10, count(*) FROM bench GROUP BY 1");
    check(r, "group by");
    snprintf(note_buf, sizeof note_buf, "per row scanned; %d groups", PQntuples(r));
    PQclear(r);
    report("group by, full scan", total, now() - t, 0, note_buf);
    PQfinish(c);
}

// ------------------------------------------------------------------ key-value suite

#define KEY_LEN 16
#define VALUE_SIZE 100

static uint8_t values[VALUE_SIZE + 4096];
static const int binary[2] = {1, 1};

static void key(uint64_t i, char out[KEY_LEN]) {
    for (int j = KEY_LEN - 1; j >= 0; j--) {
        out[j] = (char)('0' + i % 10);
        i /= 10;
    }
}
static const char *value(uint64_t i) { return (const char *)values + (i * 7919) % 4096; }

static void kv_fresh(PGconn *c) {
    exec(c, "DEALLOCATE ALL");
    exec(c, "DROP TABLE IF EXISTS kv");
    exec(c, "CREATE TABLE kv (k BYTEA PRIMARY KEY, v BYTEA)");
    prepare(c, "put", "INSERT INTO kv VALUES ($1, $2)");
}

static void put(PGconn *c, uint64_t i) {
    char k[KEY_LEN];
    key(i, k);
    const char *params[2] = {k, value(i)};
    const int lengths[2] = {KEY_LEN, VALUE_SIZE};
    PGresult *r = PQexecPrepared(c, "put", 2, params, lengths, binary, 0);
    check(r, "put");
    PQclear(r);
}

static int get(PGconn *c, const char *k, int len) {
    const char *params[1] = {k};
    PGresult *r = PQexecPrepared(c, "get", 1, params, &len, binary, 1);
    check(r, "get");
    int found = PQntuples(r) == 1 && PQgetlength(r, 0, 0) == VALUE_SIZE;
    PQclear(r);
    return found;
}

struct reader {
    const char *conninfo;
    uint64_t seed, ops, n, found;
};

static void *read_worker(void *arg) {
    struct reader *r = arg;
    PGconn *c = connect_db(r->conninfo);
    prepare(c, "get", "SELECT v FROM kv WHERE k = $1");
    char k[KEY_LEN];
    for (uint64_t j = 0; j < r->ops; j++) {
        key(below(&r->seed, r->n), k);
        r->found += get(c, k, KEY_LEN);
    }
    PQfinish(c);
    return NULL;
}

static void kv_suite(const char *conninfo, uint64_t n, int threads) {
    uint64_t seed = 9;
    for (size_t i = 0; i < sizeof values; i++) values[i] = (uint8_t)rng_next(&seed);
    double entry = KEY_LEN + VALUE_SIZE, t;
    char note[96];
    PGconn *c = connect_db(conninfo);
    exec(c, "SET client_min_messages = warning");

    kv_fresh(c);
    t = now();
    for (uint64_t i = 0; i < n; i++) put(c, i);
    report("fillseq", n, now() - t, entry, "");

    kv_fresh(c);
    t = now();
    for (uint64_t start = 0; start < n; start += 1000) {
        exec(c, "BEGIN");
        for (uint64_t i = start; i < start + 1000 && i < n; i++) put(c, i);
        exec(c, "COMMIT");
    }
    report("fillbatch", n, now() - t, entry, "batches of 1000");

    uint64_t *order = malloc(n * sizeof *order);
    for (uint64_t i = 0; i < n; i++) order[i] = i;
    seed = 42;
    for (uint64_t i = n - 1; i >= 1; i--) {
        uint64_t j = below(&seed, i + 1), tmp = order[i];
        order[i] = order[j];
        order[j] = tmp;
    }
    kv_fresh(c);
    t = now();
    for (uint64_t i = 0; i < n; i++) put(c, order[i]);
    report("fillrandom", n, now() - t, entry, "");
    free(order);
    exec(c, "VACUUM ANALYZE kv"); // untimed, like spiro-bench waiting for compaction

    prepare(c, "get", "SELECT v FROM kv WHERE k = $1");
    char k[KEY_LEN + 1];
    uint64_t found = 0;
    seed = 1;
    t = now();
    for (uint64_t i = 0; i < n; i++) {
        key(below(&seed, n), k);
        found += get(c, k, KEY_LEN);
    }
    snprintf(note, sizeof note, "%llu of %llu found", (ull)found, (ull)n);
    report("readrandom", n, now() - t, 0, note);

    pthread_t tids[64];
    struct reader readers[64];
    if (threads > 64) threads = 64;
    t = now();
    for (int i = 0; i < threads; i++) {
        uint64_t ops = n / threads + ((uint64_t)i < n % threads);
        readers[i] = (struct reader){conninfo, 100 + (uint64_t)i, ops, n, 0};
        pthread_create(&tids[i], NULL, read_worker, &readers[i]);
    }
    found = 0;
    for (int i = 0; i < threads; i++) {
        pthread_join(tids[i], NULL);
        found += readers[i].found;
    }
    char label[32];
    snprintf(label, sizeof label, "readrandom x%d", threads);
    snprintf(note, sizeof note, "%llu of %llu found", (ull)found, (ull)n);
    report(label, n, now() - t, 0, note);

    found = 0;
    seed = 2;
    t = now();
    for (uint64_t i = 0; i < n; i++) {
        key(below(&seed, n), k);
        k[KEY_LEN] = 'x'; // between existing keys
        found += get(c, k, KEY_LEN + 1);
    }
    snprintf(note, sizeof note, "%llu found", (ull)found);
    report("readmissing", n, now() - t, 0, note);

    // Stream the full scan row by row rather than buffering a million rows.
    prepare(c, "all", "SELECT k, v FROM kv");
    uint64_t count = 0;
    t = now();
    if (!PQsendQueryPrepared(c, "all", 0, NULL, NULL, NULL, 1) || !PQsetSingleRowMode(c)) {
        fprintf(stderr, "scan: %s", PQerrorMessage(c));
        exit(1);
    }
    for (PGresult *r; (r = PQgetResult(c)) != NULL; PQclear(r)) {
        check(r, "scan");
        if (PQresultStatus(r) == PGRES_SINGLE_TUPLE) count += PQgetlength(r, 0, 1) == VALUE_SIZE;
    }
    snprintf(note, sizeof note, "%llu entries", (ull)count);
    report("readseq", n, now() - t, entry, note);

    prepare(c, "scan", "SELECT k, v FROM kv WHERE k >= $1 ORDER BY k LIMIT 100");
    uint64_t scans = n / 100 ? n / 100 : 1;
    count = 0;
    seed = 3;
    t = now();
    for (uint64_t i = 0; i < scans; i++) {
        key(below(&seed, n), k);
        const char *params[1] = {k};
        const int len = KEY_LEN;
        PGresult *r = PQexecPrepared(c, "scan", 1, params, &len, binary, 1);
        check(r, "scan100");
        for (int row = 0; row < PQntuples(r); row++) count += PQgetlength(r, row, 1) == VALUE_SIZE;
        PQclear(r);
    }
    snprintf(note, sizeof note, "%llu scans of 100 keys, %llu entries", (ull)scans, (ull)count);
    report("scan100", scans, now() - t, 0, note);
    PQfinish(c);
}

int main(int argc, char **argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: %s sql|kv \"<conninfo>\" [n] [threads]\n", argv[0]);
        return 1;
    }
    int kv = strcmp(argv[1], "kv") == 0;
    uint64_t n = argc > 3 ? strtoull(argv[3], NULL, 10) : (kv ? 1000000 : 200000);
    int threads = argc > 4 ? atoi(argv[4]) : 10;
    PGconn *probe = connect_db(argv[2]);
    int version = PQserverVersion(probe); // e.g. 170011 for 17.11
    printf("PostgreSQL %d.%d, %s suite, n = %llu (synchronous_commit=off, 64 MiB shared_buffers)\n", version / 10000,
           version % 100, kv ? "key-value" : "SQL", (ull)n);
    PQfinish(probe);
    printf("------------------------------------------------------------------------------------------\n");
    if (kv) kv_suite(argv[2], n, threads);
    else sql_suite(argv[2], n);
    return 0;
}
