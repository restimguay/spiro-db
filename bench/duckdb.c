// The spiro-bench SQL workloads, run against DuckDB for comparison. Usually
// run through bench/compare.sh.
//
//   bench_duckdb sql <dir> [n]
//
// Same rows, random sequences (identical SplitMix64 generator and seeds) and
// operation counts as spiro-bench. Every statement is prepared once. DuckDB
// runs each query on one thread, as the other engines do; it is built for
// analytics, so its strengths and weaknesses differ from the others'.

#include <duckdb.h>
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

static void report(const char *name, uint64_t ops, double secs, const char *note) {
    printf("%-22s %9.3f µs/op %13.0f ops/s %12s   %s\n", name, secs * 1e6 / ops, ops / secs, "", note);
    fflush(stdout);
}

static duckdb_connection con;

static void exec(const char *sql) {
    duckdb_result r;
    if (duckdb_query(con, sql, &r) == DuckDBError) {
        fprintf(stderr, "%s: %s\n", sql, duckdb_result_error(&r));
        exit(1);
    }
    duckdb_destroy_result(&r);
}

static duckdb_prepared_statement prepare(const char *sql) {
    duckdb_prepared_statement s;
    if (duckdb_prepare(con, sql, &s) == DuckDBError) {
        fprintf(stderr, "%s: %s\n", sql, duckdb_prepare_error(s));
        exit(1);
    }
    return s;
}

/// Runs a bound statement and returns how many rows it produced, reading
/// every result chunk.
static uint64_t run(duckdb_prepared_statement s) {
    duckdb_result r;
    if (duckdb_execute_prepared(s, &r) == DuckDBError) {
        fprintf(stderr, "execute: %s\n", duckdb_result_error(&r));
        exit(1);
    }
    uint64_t rows = 0;
    for (duckdb_data_chunk c; (c = duckdb_fetch_chunk(r)) != NULL; duckdb_destroy_data_chunk(&c)) {
        rows += duckdb_data_chunk_get_size(c);
    }
    duckdb_destroy_result(&r);
    return rows;
}

/// Runs a query and returns the first column of its first row as an integer.
static long long first_int(const char *sql, uint64_t *rows) {
    duckdb_result r;
    if (duckdb_query(con, sql, &r) == DuckDBError) {
        fprintf(stderr, "%s: %s\n", sql, duckdb_result_error(&r));
        exit(1);
    }
    long long value = 0;
    *rows = 0;
    for (duckdb_data_chunk c; (c = duckdb_fetch_chunk(r)) != NULL; duckdb_destroy_data_chunk(&c)) {
        if (*rows == 0) value = ((int64_t *)duckdb_vector_get_data(duckdb_data_chunk_get_vector(c, 0)))[0];
        *rows += duckdb_data_chunk_get_size(c);
    }
    duckdb_destroy_result(&r);
    return value;
}

static void sql_suite(uint64_t n) {
    exec("CREATE TABLE bench (id BIGINT PRIMARY KEY, name VARCHAR NOT NULL, score BIGINT, note VARCHAR)");
    exec("CREATE INDEX bench_score ON bench (score)");
    uint64_t groups = n / 10 ? n / 10 : 1;
    char name[32], note_buf[96];
    duckdb_prepared_statement ins = prepare("INSERT INTO bench VALUES ($1, $2, $3, $4)");
#define BIND_ROW(i)                                                         \
    do {                                                                    \
        duckdb_bind_int64(ins, 1, (int64_t)(i));                            \
        snprintf(name, sizeof name, "user%llu", (ull)(i));                  \
        duckdb_bind_varchar(ins, 2, name);                                  \
        duckdb_bind_int64(ins, 3, (int64_t)((i) % groups));                 \
        duckdb_bind_varchar(ins, 4, "lorem ipsum dolor sit amet");          \
    } while (0)

    double t = now();
    for (uint64_t start = 0; start < n; start += 1000) {
        exec("BEGIN TRANSACTION");
        for (uint64_t i = start; i < start + 1000 && i < n; i++) {
            BIND_ROW(i);
            run(ins);
        }
        exec("COMMIT");
    }
    report("insert, 1000 per txn", n, now() - t, "");

    uint64_t single = n / 10 ? n / 10 : 1;
    t = now();
    for (uint64_t i = n; i < n + single; i++) {
        BIND_ROW(i);
        run(ins);
    }
    report("insert, autocommit", single, now() - t, "");
    uint64_t total = n + single;
    exec("CHECKPOINT"); // untimed, like the other engines' compaction or ANALYZE

    duckdb_prepared_statement by_id = prepare("SELECT name FROM bench WHERE id = $1");
    uint64_t rng = 1, found = 0;
    t = now();
    for (uint64_t k = 0; k < n; k++) {
        duckdb_bind_int64(by_id, 1, (int64_t)below(&rng, total));
        found += run(by_id);
    }
    snprintf(note_buf, sizeof note_buf, "%llu of %llu found", (ull)found, (ull)n);
    report("select by primary key", n, now() - t, note_buf);

    duckdb_prepared_statement by_score = prepare("SELECT id FROM bench WHERE score = $1");
    uint64_t lookups = n / 10 ? n / 10 : 1, rows = 0;
    rng = 2;
    t = now();
    for (uint64_t k = 0; k < lookups; k++) {
        duckdb_bind_int64(by_score, 1, (int64_t)below(&rng, groups));
        rows += run(by_score);
    }
    snprintf(note_buf, sizeof note_buf, "%llu rows", (ull)rows);
    report("select by index", lookups, now() - t, note_buf);

    duckdb_prepared_statement range = prepare("SELECT id, name FROM bench WHERE id >= $1 AND id < $2");
    uint64_t scans = n / 100 ? n / 100 : 1;
    rows = 0;
    rng = 3;
    t = now();
    for (uint64_t k = 0; k < scans; k++) {
        int64_t start = (int64_t)below(&rng, total);
        duckdb_bind_int64(range, 1, start);
        duckdb_bind_int64(range, 2, start + 100);
        rows += run(range);
    }
    snprintf(note_buf, sizeof note_buf, "%llu rows", (ull)rows);
    report("select 100-row range", scans, now() - t, note_buf);

    duckdb_prepared_statement update = prepare("UPDATE bench SET note = $1 WHERE id = $2");
    uint64_t updates = n / 10 ? n / 10 : 1;
    rng = 4;
    t = now();
    for (uint64_t k = 0; k < updates; k++) {
        duckdb_bind_varchar(update, 1, "updated");
        duckdb_bind_int64(update, 2, (int64_t)below(&rng, total));
        run(update);
    }
    report("update by primary key", updates, now() - t, "");

    uint64_t result_rows;
    t = now();
    long long count = first_int("SELECT count(*), avg(score) FROM bench", &result_rows);
    snprintf(note_buf, sizeof note_buf, "per row scanned; count = %lld", count);
    report("count + avg, full scan", total, now() - t, note_buf);

    t = now();
    first_int("SELECT score % 10, count(*) FROM bench GROUP BY 1", &result_rows);
    snprintf(note_buf, sizeof note_buf, "per row scanned; %llu groups", (ull)result_rows);
    report("group by, full scan", total, now() - t, note_buf);
}

int main(int argc, char **argv) {
    if (argc < 3 || strcmp(argv[1], "sql") != 0) {
        fprintf(stderr, "usage: %s sql <dir> [n]   (only the SQL suite)\n", argv[0]);
        return 1;
    }
    char path[4096];
    snprintf(path, sizeof path, "%s/bench.duckdb", argv[2]);
    remove(path);
    uint64_t n = argc > 3 ? strtoull(argv[3], NULL, 10) : 200000;
    duckdb_database db;
    if (duckdb_open(path, &db) == DuckDBError || duckdb_connect(db, &con) == DuckDBError) {
        fprintf(stderr, "could not open %s\n", path);
        return 1;
    }
    exec("SET threads = 1");
    printf("DuckDB %s, SQL suite, n = %llu (one thread per query)\n", duckdb_library_version(), (ull)n);
    printf("------------------------------------------------------------------------------------------\n");
    sql_suite(n);
    duckdb_disconnect(&con);
    duckdb_close(&db);
    return 0;
}
