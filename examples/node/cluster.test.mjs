// Run from the repository root after `cargo build --release`:
//   node --test examples/node/cluster.test.mjs

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import { SpiroCluster } from "./cluster.mjs";

const binary = process.env.SPIRO_SERVER ?? "./target/release/spiro-server";

/** Starts a server on a free port; resolves once it says where it listens. */
function startServer(args) {
  const dir = mkdtempSync(join(tmpdir(), "spirodb-cluster-"));
  const proc = spawn(binary, [dir, "--listen", "127.0.0.1:0", ...args], { stdio: ["ignore", "ignore", "pipe"] });
  return new Promise((resolve, reject) => {
    let stderr = "";
    proc.stderr.on("data", (chunk) => {
      stderr += chunk;
      const addr = / on (\S+:\d+) as /.exec(stderr)?.[1];
      if (addr) {
        const exited = new Promise((done) => proc.once("exit", done));
        let stopped;
        const stop = () => {
          stopped ??= (proc.kill(), exited.then(() => rmSync(dir, { recursive: true, force: true })));
          return stopped;
        };
        resolve({ addr, stop });
      }
    });
    proc.on("exit", (code) => reject(new Error(stderr || `spiro-server exited with ${code}`)));
  });
}

async function admin(addr, request) {
  const { Socket } = await import("node:net");
  const at = addr.lastIndexOf(":");
  return new Promise((resolve, reject) => {
    const socket = new Socket().connect(Number(addr.slice(at + 1)), addr.slice(0, at));
    socket.on("error", reject);
    socket.once("data", (data) => {
      socket.end();
      resolve(JSON.parse(data.toString()));
    });
    socket.write(JSON.stringify(request) + "\n");
  });
}

async function eventually(check) {
  for (let i = 0; i < 500; i++) {
    if (await check()) return;
    await new Promise((r) => setTimeout(r, 10));
  }
  throw new Error("timed out");
}

test("selects go to replicas, writes to the primary", async () => {
  const primary = await startServer([]);
  const replicas = await Promise.all([startServer(["--replica-of", primary.addr]), startServer(["--replica-of", primary.addr])]);
  const db = await SpiroCluster.connect([replicas[0].addr, primary.addr, replicas[1].addr]);
  try {
    assert.equal(db.primary, primary.addr);
    assert.equal(db.replicas.length, 2);
    await db.query("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT UNIQUE)");
    const readers = new Set();
    for (let i = 0; i < 6; i++) {
      assert.deepEqual(await db.query("INSERT INTO users (name) VALUES (?)", [`u${i}`]), { command: "INSERT", rows: 1 });
      assert.deepEqual(db.lastTarget, { primary: primary.addr });
      // Read-your-writes, from a replica.
      assert.deepEqual(await db.query("SELECT name FROM users WHERE name = ?", [`u${i}`]), [{ name: `u${i}` }]);
      readers.add(db.lastTarget.replica);
    }
    assert.equal(readers.size, 2);
    await assert.rejects(db.query("INSERT INTO users (name) VALUES ('u0')"), { kind: "constraint" });

    await db.query("BEGIN");
    await db.query("INSERT INTO users (name) VALUES ('in txn')");
    assert.equal((await db.query("SELECT * FROM users")).length, 7);
    assert.deepEqual(db.lastTarget, { primary: primary.addr }, "reads in a transaction stay on the primary");
    await db.query("ROLLBACK");
  } finally {
    db.close();
    await Promise.all([primary, ...replicas].map((s) => s.stop()));
  }
});

test("notifications and failover", async () => {
  const primary = await startServer([]);
  const r1 = await startServer(["--replica-of", primary.addr]);
  const db = await SpiroCluster.connect([primary.addr, r1.addr]);
  try {
    const received = [];
    db.on("notification", (n) => received.push(n.payload));
    await db.listen("jobs");
    await db.notify("jobs", "it's ready");
    await eventually(() => received.length === 1);
    assert.deepEqual(received, ["it's ready"]);

    await db.query("CREATE TABLE t (v TEXT)");
    await db.query("INSERT INTO t VALUES ('before')");
    await eventually(async () => (await admin(r1.addr, { status: true })).seq === (await admin(primary.addr, { status: true })).seq);

    await primary.stop();
    assert.deepEqual(await admin(r1.addr, { promote: true }), { command: "PROMOTE", epoch: 2 });
    // The client saw the old primary's connection close, so nothing was sent
    // there: it finds the new primary before writing.
    await db.query("INSERT INTO t VALUES ('after')");
    assert.equal(db.primary, r1.addr);
    assert.deepEqual(await db.query("SELECT v FROM t"), [{ v: "before" }, { v: "after" }]);
  } finally {
    db.close();
    await Promise.allSettled([primary.stop(), r1.stop()]);
  }
});
