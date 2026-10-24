// The spiro-bench workloads, run against MariaDB (or MySQL) for comparison.
// Usually run through bench/compare.sh, which starts a private server for it.
//
//   bench_mariadb sql <socket> [n]           the `spiro-bench --sql` suite
//   bench_mariadb kv  <socket> [n] [threads] the `spiro-bench` key-value suite, on a
//                                            (k VARBINARY PRIMARY KEY, v VARBINARY) table
//
// Same rows, same random keys (identical SplitMix64 generator and seeds), same
// operation counts as spiro-bench and the other bench/ programs. Every
// statement is a server-side prepared statement, using the binary protocol.
// Tables use InnoDB, MariaDB's default engine.

#include <mysql.h>
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

static void die(MYSQL *m, const char *what) {
    fprintf(stderr, "%s: %s\n", what, mysql_error(m));
    exit(1);
}

static void stmt_die(MYSQL_STMT *s, const char *what) {
    fprintf(stderr, "%s: %s\n", what, mysql_stmt_error(s));
    exit(1);
}

static void exec(MYSQL *m, const char *sql) {
    if (mysql_query(m, sql)) die(m, sql);
    MYSQL_RES *r = mysql_store_result(m);
    if (r) mysql_free_result(r);
}

/// Connects and switches to the `bench` database, creating it if needed.
static MYSQL *connect_db(const char *socket) {
    MYSQL *m = mysql_init(NULL);
    if (!mysql_real_connect(m, NULL, "root", "", NULL, 0, socket, 0)) die(m, "connect");
    exec(m, "CREATE DATABASE IF NOT EXISTS bench");
    exec(m, "USE bench");
    return m;
}

static MYSQL_STMT *prepare(MYSQL *m, const char *sql) {
    MYSQL_STMT *s = mysql_stmt_init(m);
    if (!s || mysql_stmt_prepare(s, sql, strlen(sql))) die(m, sql);
    return s;
}

static MYSQL_BIND int64(long long *v) {
    MYSQL_BIND b;
    memset(&b, 0, sizeof b);
    b.buffer_type = MYSQL_TYPE_LONGLONG;
    b.buffer = v;
    return b;
}

static MYSQL_BIND bytes(void *buf, unsigned long capacity, unsigned long *len, enum enum_field_types type) {
    MYSQL_BIND b;
    memset(&b, 0, sizeof b);
    b.buffer_type = type;
    b.buffer = buf;
    b.buffer_length = capacity;
    b.length = len;
    return b;
}

static void bind(MYSQL_STMT *s, MYSQL_BIND *params, MYSQL_BIND *results) {
    if (params && mysql_stmt_bind_param(s, params)) stmt_die(s, "bind parameters");
    if (results && mysql_stmt_bind_result(s, results)) stmt_die(s, "bind results");
}

static void execute(MYSQL_STMT *s) {
    if (mysql_stmt_execute(s)) stmt_die(s, "execute");
}

/// Fetches every row of the last execution, returning how many there were.
static uint64_t fetch_all(MYSQL_STMT *s) {
    uint64_t rows = 0;
    int rc;
    while ((rc = mysql_stmt_fetch(s)) == 0 || rc == MYSQL_DATA_TRUNCATED) rows++;
    if (rc == 1) stmt_die(s, "fetch");
    return rows;
}

/// Runs a plain query and returns its first value, or the row count if `count_rows`.
static void query_value(MYSQL *m, const char *sql, char *out, size_t cap, int count_rows) {
    if (mysql_query(m, sql)) die(m, sql);
    MYSQL_RES *r = mysql_store_result(m);
    if (!r) die(m, sql);
    if (count_rows) {
        snprintf(out, cap, "%llu", (ull)mysql_num_rows(r));
    } else {
        MYSQL_ROW row = mysql_fetch_row(r);
        snprintf(out, cap, "%s", row && row[0] ? row[0] : "NULL");
    }
    mysql_free_result(r);
}

// ------------------------------------------------------------------ SQL suite

static void sql_suite(const char *socket, uint64_t n) {
    MYSQL *m = connect_db(socket);
    exec(m, "DROP TABLE IF EXISTS bench");
    exec(m, "CREATE TABLE bench (id BIGINT PRIMARY KEY, name VARCHAR(64) NOT NULL, score BIGINT, "
            "note VARCHAR(64), KEY bench_score (score)) ENGINE=InnoDB");
    uint64_t groups = n / 10 ? n / 10 : 1;
    long long id, score, end, out_id;
    char name[64], note[64] = "lorem ipsum dolor sit amet", updated[] = "updated", out_name[64], buf[64], note_buf[96];
    unsigned long name_len, note_len = strlen(note), updated_len = strlen(updated), out_len;
#define FILL_ROW(i)                                                           \
    do {                                                                      \
        id = (long long)(i);                                                  \
        name_len = (unsigned long)snprintf(name, sizeof name, "user%llu", (ull)(i)); \
        score = (long long)((i) % groups);                                    \
    } while (0)

    MYSQL_STMT *ins = prepare(m, "INSERT INTO bench VALUES (?, ?, ?, ?)");
    MYSQL_BIND row[4] = {int64(&id), bytes(name, sizeof name, &name_len, MYSQL_TYPE_STRING), int64(&score),
                         bytes(note, sizeof note, &note_len, MYSQL_TYPE_STRING)};
    bind(ins, row, NULL);
    double t = now();
    for (uint64_t start = 0; start < n; start += 1000) {
        exec(m, "BEGIN");
        for (uint64_t i = start; i < start + 1000 && i < n; i++) {
            FILL_ROW(i);
            execute(ins);
        }
        exec(m, "COMMIT");
    }
    report("insert, 1000 per txn", n, now() - t, 0, "");

    uint64_t single = n / 10 ? n / 10 : 1;
    t = now();
    for (uint64_t i = n; i < n + single; i++) {
        FILL_ROW(i);
        execute(ins);
    }
    report("insert, autocommit", single, now() - t, 0, "");
    uint64_t total = n + single;
    exec(m, "ANALYZE TABLE bench"); // untimed: give the optimizer statistics

    MYSQL_STMT *by_id = prepare(m, "SELECT name FROM bench WHERE id = ?");
    MYSQL_BIND id_param[1] = {int64(&id)};
    MYSQL_BIND name_result[1] = {bytes(out_name, sizeof out_name, &out_len, MYSQL_TYPE_STRING)};
    bind(by_id, id_param, name_result);
    uint64_t rng = 1, found = 0;
    t = now();
    for (uint64_t k = 0; k < n; k++) {
        id = (long long)below(&rng, total);
        execute(by_id);
        found += fetch_all(by_id) == 1 && out_len > 0;
    }
    snprintf(note_buf, sizeof note_buf, "%llu of %llu found", (ull)found, (ull)n);
    report("select by primary key", n, now() - t, 0, note_buf);

    MYSQL_STMT *by_score = prepare(m, "SELECT id FROM bench WHERE score = ?");
    MYSQL_BIND score_param[1] = {int64(&score)};
    MYSQL_BIND id_result[1] = {int64(&out_id)};
    bind(by_score, score_param, id_result);
    uint64_t lookups = n / 10 ? n / 10 : 1, rows = 0;
    rng = 2;
    t = now();
    for (uint64_t k = 0; k < lookups; k++) {
        score = (long long)below(&rng, groups);
        execute(by_score);
        rows += fetch_all(by_score);
    }
    snprintf(note_buf, sizeof note_buf, "%llu rows", (ull)rows);
    report("select by index", lookups, now() - t, 0, note_buf);

    MYSQL_STMT *range = prepare(m, "SELECT id, name FROM bench WHERE id >= ? AND id < ?");
    MYSQL_BIND range_params[2] = {int64(&id), int64(&end)};
    MYSQL_BIND range_results[2] = {int64(&out_id), bytes(out_name, sizeof out_name, &out_len, MYSQL_TYPE_STRING)};
    bind(range, range_params, range_results);
    uint64_t scans = n / 100 ? n / 100 : 1;
    rows = 0;
    rng = 3;
    t = now();
    for (uint64_t k = 0; k < scans; k++) {
        id = (long long)below(&rng, total);
        end = id + 100;
        execute(range);
        rows += fetch_all(range);
    }
    snprintf(note_buf, sizeof note_buf, "%llu rows", (ull)rows);
    report("select 100-row range", scans, now() - t, 0, note_buf);

    MYSQL_STMT *update = prepare(m, "UPDATE bench SET note = ? WHERE id = ?");
    MYSQL_BIND update_params[2] = {bytes(updated, sizeof updated, &updated_len, MYSQL_TYPE_STRING), int64(&id)};
    bind(update, update_params, NULL);
    uint64_t updates = n / 10 ? n / 10 : 1;
    rng = 4;
    t = now();
    for (uint64_t k = 0; k < updates; k++) {
        id = (long long)below(&rng, total);
        execute(update);
    }
    report("update by primary key", updates, now() - t, 0, "");

    t = now();
    query_value(m, "SELECT count(*), avg(score) FROM bench", buf, sizeof buf, 0);
    snprintf(note_buf, sizeof note_buf, "per row scanned; count = %s", buf);
    report("count + avg, full scan", total, now() - t, 0, note_buf);

    t = now();
    query_value(m, "SELECT score % 10, count(*) FROM bench GROUP BY 1", buf, sizeof buf, 1);
    snprintf(note_buf, sizeof note_buf, "per row scanned; %s groups", buf);
    report("group by, full scan", total, now() - t, 0, note_buf);
    mysql_close(m);
}

// ------------------------------------------------------------------ key-value suite

#define KEY_LEN 16
#define VALUE_SIZE 100

static unsigned char values[VALUE_SIZE + 4096];

static void key(uint64_t i, char out[KEY_LEN]) {
    for (int j = KEY_LEN - 1; j >= 0; j--) {
        out[j] = (char)('0' + i % 10);
        i /= 10;
    }
}

/// One connection's statements and the buffers they're bound to.
struct kv_conn {
    MYSQL *m;
    MYSQL_STMT *put, *get;
    char k[KEY_LEN + 1], v[VALUE_SIZE], out[256];
    unsigned long k_len, v_len, out_len;
};

static void prepare_put(struct kv_conn *c) {
    if (c->put) mysql_stmt_close(c->put);
    c->put = prepare(c->m, "INSERT INTO kv VALUES (?, ?)");
    MYSQL_BIND params[2] = {bytes(c->k, sizeof c->k, &c->k_len, MYSQL_TYPE_BLOB),
                            bytes(c->v, sizeof c->v, &c->v_len, MYSQL_TYPE_BLOB)};
    bind(c->put, params, NULL);
}

static void prepare_get(struct kv_conn *c) {
    c->get = prepare(c->m, "SELECT v FROM kv WHERE k = ?");
    MYSQL_BIND param[1] = {bytes(c->k, sizeof c->k, &c->k_len, MYSQL_TYPE_BLOB)};
    MYSQL_BIND result[1] = {bytes(c->out, sizeof c->out, &c->out_len, MYSQL_TYPE_BLOB)};
    bind(c->get, param, result);
}

static void kv_fresh(struct kv_conn *c) {
    exec(c->m, "DROP TABLE IF EXISTS kv");
    exec(c->m, "CREATE TABLE kv (k VARBINARY(32) NOT NULL PRIMARY KEY, v VARBINARY(128)) ENGINE=InnoDB");
    prepare_put(c);
}

static void put(struct kv_conn *c, uint64_t i) {
    key(i, c->k);
    c->k_len = KEY_LEN;
    memcpy(c->v, values + (i * 7919) % 4096, VALUE_SIZE);
    c->v_len = VALUE_SIZE;
    execute(c->put);
}

static int get(struct kv_conn *c, unsigned long len) {
    c->k_len = len;
    execute(c->get);
    return fetch_all(c->get) == 1 && c->out_len == VALUE_SIZE;
}

struct reader {
    const char *socket;
    uint64_t seed, ops, n, found;
};

static void *read_worker(void *arg) {
    struct reader *r = arg;
    mysql_thread_init();
    struct kv_conn c = {.m = connect_db(r->socket)};
    prepare_get(&c);
    for (uint64_t j = 0; j < r->ops; j++) {
        key(below(&r->seed, r->n), c.k);
        r->found += get(&c, KEY_LEN);
    }
    mysql_stmt_close(c.get);
    mysql_close(c.m);
    mysql_thread_end();
    return NULL;
}

static void kv_suite(const char *socket, uint64_t n, int threads) {
    uint64_t seed = 9;
    for (size_t i = 0; i < sizeof values; i++) values[i] = (unsigned char)rng_next(&seed);
    double entry = KEY_LEN + VALUE_SIZE, t;
    char note[96];
    struct kv_conn c = {.m = connect_db(socket)};

    kv_fresh(&c);
    t = now();
    for (uint64_t i = 0; i < n; i++) put(&c, i);
    report("fillseq", n, now() - t, entry, "");

    kv_fresh(&c);
    t = now();
    for (uint64_t start = 0; start < n; start += 1000) {
        exec(c.m, "BEGIN");
        for (uint64_t i = start; i < start + 1000 && i < n; i++) put(&c, i);
        exec(c.m, "COMMIT");
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
    kv_fresh(&c);
    t = now();
    for (uint64_t i = 0; i < n; i++) put(&c, order[i]);
    report("fillrandom", n, now() - t, entry, "");
    free(order);
    exec(c.m, "ANALYZE TABLE kv"); // untimed, like spiro-bench waiting for compaction

    prepare_get(&c);
    uint64_t found = 0;
    seed = 1;
    t = now();
    for (uint64_t i = 0; i < n; i++) {
        key(below(&seed, n), c.k);
        found += get(&c, KEY_LEN);
    }
    snprintf(note, sizeof note, "%llu of %llu found", (ull)found, (ull)n);
    report("readrandom", n, now() - t, 0, note);

    pthread_t tids[64];
    struct reader readers[64];
    if (threads > 64) threads = 64;
    t = now();
    for (int i = 0; i < threads; i++) {
        uint64_t ops = n / threads + ((uint64_t)i < n % threads);
        readers[i] = (struct reader){socket, 100 + (uint64_t)i, ops, n, 0};
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
        key(below(&seed, n), c.k);
        c.k[KEY_LEN] = 'x'; // between existing keys
        found += get(&c, KEY_LEN + 1);
    }
    snprintf(note, sizeof note, "%llu found", (ull)found);
    report("readmissing", n, now() - t, 0, note);

    // Prepared statements stream their rows unless told to buffer them.
    char k_out[64];
    unsigned long k_out_len;
    MYSQL_STMT *all = prepare(c.m, "SELECT k, v FROM kv");
    MYSQL_BIND all_results[2] = {bytes(k_out, sizeof k_out, &k_out_len, MYSQL_TYPE_BLOB),
                                 bytes(c.out, sizeof c.out, &c.out_len, MYSQL_TYPE_BLOB)};
    bind(all, NULL, all_results);
    t = now();
    execute(all);
    uint64_t count = fetch_all(all);
    snprintf(note, sizeof note, "%llu entries", (ull)count);
    report("readseq", n, now() - t, entry, note);

    MYSQL_STMT *scan = prepare(c.m, "SELECT k, v FROM kv WHERE k >= ? ORDER BY k LIMIT 100");
    MYSQL_BIND scan_param[1] = {bytes(c.k, sizeof c.k, &c.k_len, MYSQL_TYPE_BLOB)};
    bind(scan, scan_param, all_results);
    uint64_t scans = n / 100 ? n / 100 : 1;
    count = 0;
    seed = 3;
    t = now();
    for (uint64_t i = 0; i < scans; i++) {
        key(below(&seed, n), c.k);
        c.k_len = KEY_LEN;
        execute(scan);
        count += fetch_all(scan);
    }
    snprintf(note, sizeof note, "%llu scans of 100 keys, %llu entries", (ull)scans, (ull)count);
    report("scan100", scans, now() - t, 0, note);
    mysql_close(c.m);
}

int main(int argc, char **argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: %s sql|kv <socket> [n] [threads]\n", argv[0]);
        return 1;
    }
    if (mysql_library_init(0, NULL, NULL)) {
        fprintf(stderr, "could not initialize the MariaDB client library\n");
        return 1;
    }
    int kv = strcmp(argv[1], "kv") == 0;
    uint64_t n = argc > 3 ? strtoull(argv[3], NULL, 10) : (kv ? 1000000 : 200000);
    int threads = argc > 4 ? atoi(argv[4]) : 10;
    MYSQL *probe = connect_db(argv[2]);
    char version[64];
    snprintf(version, sizeof version, "%s", mysql_get_server_info(probe)); // e.g. "12.3.3-MariaDB"
    version[strcspn(version, "-")] = '\0';
    mysql_close(probe);
    printf("MariaDB %s, %s suite, n = %llu (innodb_flush_log_at_trx_commit=2, 64 MiB buffer pool)\n", version,
           kv ? "key-value" : "SQL", (ull)n);
    printf("------------------------------------------------------------------------------------------\n");
    if (kv) kv_suite(argv[2], n, threads);
    else sql_suite(argv[2], n);
    mysql_library_end();
    return 0;
}
