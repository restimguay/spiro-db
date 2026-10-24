#!/usr/bin/env bash
# Compares SpiroDB with SQLite, PostgreSQL, MariaDB and, per suite, RocksDB
# (key-value) or DuckDB (SQL) on identical workloads, on this machine, and
# prints a side-by-side table.
#
#   bench/compare.sh [--suite sql|kv] [--num N] [--rounds R] [--threads T]
#
# Each round runs every engine once, in turn, so background load affects them
# alike; the table shows each engine's best round. Settings match SpiroDB's
# defaults as closely as each engine allows:
#   SQLite      WAL mode, synchronous=NORMAL, 64 MiB page cache
#   PostgreSQL  synchronous_commit=off, 64 MiB shared_buffers, no parallel workers
#   MariaDB     InnoDB, innodb_flush_log_at_trx_commit=2, 64 MiB buffer pool,
#               binary log off
#   RocksDB     log without an fsync per write, 8 MiB memtable, 10-bit bloom
#               filters, 64 MiB block cache, no compression
#   DuckDB      one thread per query
# In all of them, commits survive a crash of the database process but may be
# lost if the machine loses power, and each query runs on a single core.
# PostgreSQL and MariaDB run as private servers started for this run (fresh
# data directories, Unix sockets only, deleted afterwards), so existing
# installations are never touched. Engines that aren't installed are skipped.
#
# Needs a C compiler and SQLite headers; PostgreSQL needs initdb, pg_ctl and
# libpq, MariaDB needs mariadbd, mariadb-install-db and mariadb_config. Data
# goes under $SPIRO_BENCH_WORK (default: $TMPDIR).

set -euo pipefail

suite=sql rounds=2 num="" threads=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --suite) suite=$2; shift 2 ;;
    --num) num=$2; shift 2 ;;
    --rounds) rounds=$2; shift 2 ;;
    --threads) threads=$2; shift 2 ;;
    -h | --help) awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 1 ;;
  esac
done
case "$suite" in
  sql) num=${num:-200000} ;;
  kv) num=${num:-1000000} ;;
  *) echo "--suite must be sql or kv" >&2; exit 1 ;;
esac
threads=${threads:-$(getconf _NPROCESSORS_ONLN)}

root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d "${SPIRO_BENCH_WORK:-${TMPDIR:-/tmp}}/spiro-compare.XXXXXX")
# Unix socket paths are limited to about 100 bytes, so sockets get a short directory of their own.
sockets=$(mktemp -d /tmp/spiro-sockets.XXXXXX)
pgdata="$work/pgdata"
mariadb_pid=""
cleanup() {
  pg_ctl -D "$pgdata" -m immediate stop >/dev/null 2>&1 || true
  if [[ -n $mariadb_pid ]]; then
    kill "$mariadb_pid" 2>/dev/null || true
    wait "$mariadb_pid" 2>/dev/null || true
  fi
  rm -rf "$work" "$sockets"
}
trap cleanup EXIT

say() { echo "$@" >&2; }
have() { command -v "$1" >/dev/null 2>&1; }

engines=(spirodb sqlite)
say "building..."
cargo build --release --quiet --manifest-path "$root/Cargo.toml" --bin spiro-bench
cc -O2 -o "$work/bench_sqlite" "$root/bench/sqlite.c" -lsqlite3 -lpthread

if have initdb && have pg_ctl && have pg_config; then
  say "starting a private PostgreSQL server..."
  cc -O2 -I"$(pg_config --includedir)" -o "$work/bench_postgres" "$root/bench/postgres.c" \
    -L"$(pg_config --libdir)" -lpq -lpthread
  initdb -D "$pgdata" -U bench --auth=trust --no-sync --encoding=UTF8 >/dev/null
  pg_ctl -D "$pgdata" -l "$work/postgres.log" -w start -o "-c listen_addresses='' \
    -c unix_socket_directories='$sockets' -p 54399 -c shared_buffers=64MB \
    -c synchronous_commit=off -c max_parallel_workers_per_gather=0" >/dev/null
  engines+=(postgres)
else
  say "PostgreSQL not found; skipping it"
fi

if have mariadbd && have mariadb-install-db && have mariadb_config; then
  say "starting a private MariaDB server..."
  # shellcheck disable=SC2046 # mariadb_config prints several flags
  cc -O2 $(mariadb_config --cflags) -o "$work/bench_mariadb" "$root/bench/mariadb.c" $(mariadb_config --libs) -lpthread
  mariadb-install-db --no-defaults --datadir="$work/mariadb" --auth-root-authentication-method=normal \
    --skip-test-db >"$work/mariadb-install.log" 2>&1
  mariadbd --no-defaults --datadir="$work/mariadb" --socket="$sockets/mariadb.sock" --skip-networking \
    --pid-file="$work/mariadb.pid" --log-error="$work/mariadb.log" --skip-log-bin \
    --innodb-buffer-pool-size=64M --innodb-flush-log-at-trx-commit=2 &
  mariadb_pid=$!
  for _ in $(seq 300); do
    mariadb-admin --no-defaults --socket="$sockets/mariadb.sock" -uroot ping >/dev/null 2>&1 && break
    sleep 0.1
  done
  engines+=(mariadb)
else
  say "MariaDB not found; skipping it"
fi

# Where Homebrew puts libraries, if it's installed (harmless elsewhere).
brew_flags=()
if have brew; then
  brew_prefix=$(brew --prefix)
  brew_flags=(-I"$brew_prefix/include" -L"$brew_prefix/lib")
fi
# RocksDB is key-value only; DuckDB is SQL only.
if [[ $suite == kv ]]; then
  rocksdb_version=$(awk '/define ROCKSDB_(MAJOR|MINOR|PATCH)/ { printf "%s%s", sep, $3; sep = "." }' \
    "${brew_prefix:-/usr}/include/rocksdb/version.h" 2>/dev/null || true)
  if cc -O2 ${brew_flags[@]+"${brew_flags[@]}"} -DROCKSDB_VERSION="\"${rocksdb_version:-?}\"" \
    -o "$work/bench_rocksdb" "$root/bench/rocksdb.c" -lrocksdb -lpthread 2>/dev/null; then
    engines+=(rocksdb)
  else
    say "RocksDB not found; skipping it"
  fi
elif cc -O2 ${brew_flags[@]+"${brew_flags[@]}"} -o "$work/bench_duckdb" "$root/bench/duckdb.c" -lduckdb 2>/dev/null; then
  engines+=(duckdb)
else
  say "DuckDB not found; skipping it"
fi

run_engine() {
  case "$1" in
    spirodb)
      local flags=(--num "$num" --threads "$threads" --path "$work/spirodb")
      [[ $suite == sql ]] && flags+=(--sql)
      "$root/target/release/spiro-bench" "${flags[@]}"
      rm -rf "$work/spirodb"
      ;;
    sqlite)
      rm -rf "$work/sqlite" && mkdir -p "$work/sqlite"
      "$work/bench_sqlite" "$suite" "$work/sqlite" "$num" "$threads"
      ;;
    postgres) "$work/bench_postgres" "$suite" "host=$sockets port=54399 user=bench dbname=postgres" "$num" "$threads" ;;
    mariadb) "$work/bench_mariadb" "$suite" "$sockets/mariadb.sock" "$num" "$threads" ;;
    rocksdb)
      rm -rf "$work/rocksdb" && mkdir -p "$work/rocksdb"
      "$work/bench_rocksdb" kv "$work/rocksdb" "$num" "$threads"
      ;;
    duckdb)
      rm -rf "$work/duckdb" && mkdir -p "$work/duckdb"
      "$work/bench_duckdb" sql "$work/duckdb" "$num"
      ;;
  esac
}

mkdir -p "$work/results"
for ((round = 1; round <= rounds; round++)); do
  for engine in "${engines[@]}"; do
    say "round $round of $rounds: $engine"
    run_engine "$engine" >>"$work/results/$engine.txt"
  done
done

results=()
echo
for engine in "${engines[@]}"; do
  head -1 "$work/results/$engine.txt"
  results+=("$work/results/$engine.txt")
done
echo
echo "$suite suite, n = $num, best of $rounds rounds: microseconds per operation (lower is better)"
echo
awk -f "$root/bench/summarize.awk" "${results[@]}"
