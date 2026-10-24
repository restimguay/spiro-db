// The spiro-bench key-value workloads, run against RocksDB for comparison.
// Usually run through bench/compare.sh.
//
//   bench_rocksdb kv <dir> [n] [threads]
//
// Same keys, values, random sequences (identical SplitMix64 generator and
// seeds) and operation counts as spiro-bench. RocksDB is configured like
// SpiroDB's defaults: write-ahead log without an fsync per write, 8 MiB
// memtable, 4 KiB blocks, 10-bit bloom filters, 64 MiB block cache, and no
// compression (SpiroDB doesn't compress, and these values are random bytes).

#include <pthread.h>
#include <rocksdb/c.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#ifndef ROCKSDB_VERSION
#define ROCKSDB_VERSION "?"
#endif

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

static void check(char *err, const char *what) {
    if (err) {
        fprintf(stderr, "%s: %s\n", what, err);
        exit(1);
    }
}

#define KEY_LEN 16
#define VALUE_SIZE 100

static char values[VALUE_SIZE + 4096];

static void key(uint64_t i, char out[KEY_LEN]) {
    for (int j = KEY_LEN - 1; j >= 0; j--) {
        out[j] = (char)('0' + i % 10);
        i /= 10;
    }
}
static const char *value(uint64_t i) { return values + (i * 7919) % 4096; }

static rocksdb_options_t *make_options(void) {
    rocksdb_options_t *opts = rocksdb_options_create();
    rocksdb_options_set_create_if_missing(opts, 1);
    rocksdb_options_set_write_buffer_size(opts, 8 << 20);
    rocksdb_options_set_compression(opts, rocksdb_no_compression);
    rocksdb_block_based_table_options_t *table = rocksdb_block_based_options_create();
    rocksdb_block_based_options_set_block_size(table, 4096);
    rocksdb_block_based_options_set_block_cache(table, rocksdb_cache_create_lru(64 << 20));
    rocksdb_block_based_options_set_filter_policy(table, rocksdb_filterpolicy_create_bloom_full(10));
    rocksdb_options_set_block_based_table_factory(opts, table);
    return opts;
}

static rocksdb_t *open_fresh(rocksdb_options_t *opts, const char *path) {
    char *err = NULL;
    rocksdb_destroy_db(opts, path, &err);
    free(err);
    err = NULL;
    rocksdb_t *db = rocksdb_open(opts, path, &err);
    check(err, "open");
    return db;
}

static uint64_t property(rocksdb_t *db, const char *name) {
    char *v = rocksdb_property_value(db, name);
    uint64_t n = v ? strtoull(v, NULL, 10) : 0;
    free(v);
    return n;
}

/// Flushes the memtable and waits for compaction to catch up, as spiro-bench
/// does before its read benchmarks.
static void settle(rocksdb_t *db) {
    char *err = NULL;
    rocksdb_flushoptions_t *fo = rocksdb_flushoptions_create();
    rocksdb_flushoptions_set_wait(fo, 1);
    rocksdb_flush(db, fo, &err);
    check(err, "flush");
    rocksdb_flushoptions_destroy(fo);
    double deadline = now() + 120;
    while ((property(db, "rocksdb.num-files-at-level0") >= 4 || property(db, "rocksdb.num-running-compactions") > 0) &&
           now() < deadline) {
        struct timespec pause = {0, 5 * 1000 * 1000};
        nanosleep(&pause, NULL);
    }
}

static int get(rocksdb_t *db, rocksdb_readoptions_t *ro, const char *k, size_t len) {
    char *err = NULL;
    size_t vlen = 0;
    char *v = rocksdb_get(db, ro, k, len, &vlen, &err);
    check(err, "get");
    int found = v && vlen == VALUE_SIZE;
    rocksdb_free(v);
    return found;
}

struct reader {
    rocksdb_t *db;
    uint64_t seed, ops, n, found;
};

static void *read_worker(void *arg) {
    struct reader *r = arg;
    rocksdb_readoptions_t *ro = rocksdb_readoptions_create();
    char k[KEY_LEN];
    for (uint64_t j = 0; j < r->ops; j++) {
        key(below(&r->seed, r->n), k);
        r->found += get(r->db, ro, k, KEY_LEN);
    }
    rocksdb_readoptions_destroy(ro);
    return NULL;
}

static void kv_suite(const char *path, uint64_t n, int threads) {
    uint64_t seed = 9;
    for (size_t i = 0; i < sizeof values; i++) values[i] = (char)rng_next(&seed);
    double entry = KEY_LEN + VALUE_SIZE, t;
    char note[96], k[KEY_LEN + 1], *err = NULL;
    rocksdb_options_t *opts = make_options();
    rocksdb_writeoptions_t *wo = rocksdb_writeoptions_create(); // log on, no fsync per write
    rocksdb_readoptions_t *ro = rocksdb_readoptions_create();

    rocksdb_t *db = open_fresh(opts, path);
    t = now();
    for (uint64_t i = 0; i < n; i++) {
        key(i, k);
        rocksdb_put(db, wo, k, KEY_LEN, value(i), VALUE_SIZE, &err);
        check(err, "put");
    }
    report("fillseq", n, now() - t, entry, "");
    rocksdb_close(db);

    db = open_fresh(opts, path);
    rocksdb_writebatch_t *batch = rocksdb_writebatch_create();
    t = now();
    for (uint64_t start = 0; start < n; start += 1000) {
        rocksdb_writebatch_clear(batch);
        for (uint64_t i = start; i < start + 1000 && i < n; i++) {
            key(i, k);
            rocksdb_writebatch_put(batch, k, KEY_LEN, value(i), VALUE_SIZE);
        }
        rocksdb_write(db, wo, batch, &err);
        check(err, "write batch");
    }
    report("fillbatch", n, now() - t, entry, "batches of 1000");
    rocksdb_writebatch_destroy(batch);
    rocksdb_close(db);

    uint64_t *order = malloc(n * sizeof *order);
    for (uint64_t i = 0; i < n; i++) order[i] = i;
    seed = 42;
    for (uint64_t i = n - 1; i >= 1; i--) {
        uint64_t j = below(&seed, i + 1), tmp = order[i];
        order[i] = order[j];
        order[j] = tmp;
    }
    db = open_fresh(opts, path);
    t = now();
    for (uint64_t i = 0; i < n; i++) {
        key(order[i], k);
        rocksdb_put(db, wo, k, KEY_LEN, value(order[i]), VALUE_SIZE, &err);
        check(err, "put");
    }
    report("fillrandom", n, now() - t, entry, "");
    free(order);
    settle(db);

    uint64_t found = 0;
    seed = 1;
    t = now();
    for (uint64_t i = 0; i < n; i++) {
        key(below(&seed, n), k);
        found += get(db, ro, k, KEY_LEN);
    }
    snprintf(note, sizeof note, "%llu of %llu found", (ull)found, (ull)n);
    report("readrandom", n, now() - t, 0, note);

    pthread_t tids[64];
    struct reader readers[64];
    if (threads > 64) threads = 64;
    t = now();
    for (int i = 0; i < threads; i++) {
        uint64_t ops = n / threads + ((uint64_t)i < n % threads);
        readers[i] = (struct reader){db, 100 + (uint64_t)i, ops, n, 0};
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
        found += get(db, ro, k, KEY_LEN + 1);
    }
    snprintf(note, sizeof note, "%llu found", (ull)found);
    report("readmissing", n, now() - t, 0, note);

    uint64_t count = 0;
    t = now();
    rocksdb_iterator_t *it = rocksdb_create_iterator(db, ro);
    for (rocksdb_iter_seek_to_first(it); rocksdb_iter_valid(it); rocksdb_iter_next(it)) {
        size_t vlen;
        rocksdb_iter_value(it, &vlen);
        count += vlen == VALUE_SIZE;
    }
    rocksdb_iter_get_error(it, &err);
    check(err, "scan");
    rocksdb_iter_destroy(it);
    snprintf(note, sizeof note, "%llu entries", (ull)count);
    report("readseq", n, now() - t, entry, note);

    uint64_t scans = n / 100 ? n / 100 : 1;
    count = 0;
    seed = 3;
    t = now();
    for (uint64_t i = 0; i < scans; i++) {
        key(below(&seed, n), k);
        it = rocksdb_create_iterator(db, ro);
        rocksdb_iter_seek(it, k, KEY_LEN);
        for (int j = 0; j < 100 && rocksdb_iter_valid(it); j++, rocksdb_iter_next(it)) {
            size_t vlen;
            rocksdb_iter_value(it, &vlen);
            count += vlen == VALUE_SIZE;
        }
        rocksdb_iter_destroy(it);
    }
    snprintf(note, sizeof note, "%llu scans of 100 keys, %llu entries", (ull)scans, (ull)count);
    report("scan100", scans, now() - t, 0, note);
    rocksdb_close(db);
}

int main(int argc, char **argv) {
    if (argc < 3 || strcmp(argv[1], "kv") != 0) {
        fprintf(stderr, "usage: %s kv <dir> [n] [threads]   (RocksDB has no SQL, so only the kv suite)\n", argv[0]);
        return 1;
    }
    char path[4096];
    snprintf(path, sizeof path, "%s/rocksdb", argv[2]);
    uint64_t n = argc > 3 ? strtoull(argv[3], NULL, 10) : 1000000;
    int threads = argc > 4 ? atoi(argv[4]) : 10;
    printf("RocksDB %s, key-value suite, n = %llu (no compression, 10-bit bloom, 64 MiB block cache, 8 MiB memtable)\n",
           ROCKSDB_VERSION, (ull)n);
    printf("------------------------------------------------------------------------------------------\n");
    kv_suite(path, n, threads);
    return 0;
}
