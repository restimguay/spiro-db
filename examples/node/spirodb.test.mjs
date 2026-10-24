// Run from the repository root after `cargo build --release`:
//   node --test examples/node/spirodb.test.mjs

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import { SpiroDB, sql } from "./spirodb.mjs";

const binary = process.env.SPIRO ?? "./target/release/spiro";

function withDb(fn) {
  return async () => {
    const dir = mkdtempSync(join(tmpdir(), "spirodb-node-"));
    const db = new SpiroDB(dir, { binary });
    try {
      await fn(db, dir);
    } finally {
      await db.close();
      rmSync(dir, { recursive: true, force: true });
    }
  };
}

test("queries return arrays of row objects", withDb(async (db) => {
  assert.deepEqual(await db.query("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score REAL, ok BOOLEAN)"), {
    command: "CREATE TABLE",
  });
  assert.deepEqual(await db.query("INSERT INTO t (name, score, ok) VALUES ('a', 1.5, TRUE), ('b', NULL, FALSE)"), {
    command: "INSERT",
    rows: 2,
  });
  const rows = await db.query("SELECT * FROM t ORDER BY id");
  assert.ok(Array.isArray(rows));
  assert.deepEqual(rows, [
    { id: 1, name: "a", score: 1.5, ok: true },
    { id: 2, name: "b", score: null, ok: false },
  ]);
  assert.deepEqual(await db.query("SELECT * FROM t WHERE id > 99"), []);
}));

test("errors reject without breaking later queries", withDb(async (db) => {
  await db.query("CREATE TABLE t (id INTEGER PRIMARY KEY)");
  await db.query("INSERT INTO t VALUES (1)");
  await assert.rejects(db.query("INSERT INTO t VALUES (1)"), /UNIQUE constraint failed: t.id/);
  await assert.rejects(db.query("SELEC 1"), /syntax error/);
  assert.deepEqual(await db.query("SELECT count(*) AS n FROM t"), [{ n: 1 }]);
}));

test("sql`` quotes values safely", withDb(async (db) => {
  await db.query("CREATE TABLE t (name TEXT, n INTEGER)");
  const tricky = "x'); DROP TABLE t; --\nline two";
  await db.query(sql`INSERT INTO t VALUES (${tricky}, ${-7})`);
  assert.deepEqual(await db.query(sql`SELECT * FROM t WHERE name = ${tricky}`), [{ name: tricky, n: -7 }]);
  assert.equal(sql`SELECT ${null}, ${true}, ${2.5}, ${10n}`, "SELECT NULL, TRUE, 2.5, 10");
  assert.throws(() => sql`SELECT ${{}}`, TypeError);
}));

test("transactions span queries", withDb(async (db) => {
  await db.query("CREATE TABLE t (x INTEGER)");
  await db.query("BEGIN");
  await db.query("INSERT INTO t VALUES (1)");
  await db.query("ROLLBACK");
  await db.query("BEGIN");
  await db.query("INSERT INTO t VALUES (2)");
  await db.query("COMMIT");
  assert.deepEqual(await db.query("SELECT x FROM t"), [{ x: 2 }]);
}));

test("concurrent calls each get their own answer", withDb(async (db) => {
  await db.query("CREATE TABLE t (id INTEGER PRIMARY KEY)");
  await Promise.all(Array.from({ length: 50 }, (_, i) => db.query(`INSERT INTO t VALUES (${i})`)));
  const answers = await Promise.all(Array.from({ length: 50 }, (_, i) => db.query(`SELECT id FROM t WHERE id = ${i}`)));
  answers.forEach((rows, i) => assert.deepEqual(rows, [{ id: i }]));
}));

test("notifications arrive as events", withDb(async (db) => {
  const received = [];
  db.on("notification", (n) => received.push(n));
  await db.listen("jobs");
  await db.notify("jobs", "it's ready");
  await db.query("BEGIN");
  await db.notify("jobs", "in a transaction");
  assert.deepEqual(received.map((n) => n.payload), ["it's ready"], "not before commit");
  assert.deepEqual(await db.query("SELECT 1 AS one"), [{ one: 1 }], "replies stay in step");
  await db.query("COMMIT");
  await db.notify("other", "not listening");
  await db.unlisten();
  await db.notify("jobs", "after unlisten");
  assert.deepEqual(received.map((n) => [n.channel, n.payload]), [["jobs", "it's ready"], ["jobs", "in a transaction"]]);
  assert.equal(typeof received[0].sender, "number");
}));
