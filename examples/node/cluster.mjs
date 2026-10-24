// SpiroDB cluster client for Node.js: one object for a primary and its
// replicas, routing every statement for you. No npm dependencies.
//
//   import { SpiroCluster } from "./cluster.mjs";
//   const db = await SpiroCluster.connect(["10.0.0.1:7878", "10.0.0.2:7878", "10.0.0.3:7878"]);
//   await db.query("INSERT INTO users (name) VALUES (?)", ["Ada"]); // primary
//   const rows = await db.query("SELECT * FROM users");             // a replica
//   // rows: [{ id: 1, name: "Ada" }] -- your own writes are always visible
//
// - SELECT, EXPLAIN and WITH go to the replicas, round-robin. Each carries
//   the sequence number of this client's latest write; a replica that hasn't
//   applied it yet says so and the read goes to the primary instead.
// - Writes, transactions (BEGIN ... COMMIT) and LISTEN go to the primary.
// - The primary is whichever server says so with the highest epoch (the
//   latest promotion). When it stops answering, the client looks again.
//   Reads are retried; a write whose connection broke rejects, since it may
//   or may not have committed.

import { EventEmitter } from "node:events";
import { connect } from "node:net";
import { createInterface } from "node:readline";

/** One connection to one server, speaking JSON lines. */
class Connection extends EventEmitter {
  #socket;
  #pending = [];

  static open(addr, timeoutMs = 2000) {
    const at = addr.lastIndexOf(":");
    const socket = connect({ host: addr.slice(0, at), port: Number(addr.slice(at + 1)), noDelay: true });
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => socket.destroy(new Error(`${addr}: connection timed out`)), timeoutMs);
      socket.once("connect", () => {
        clearTimeout(timer);
        resolve(new Connection(addr, socket));
      });
      socket.once("error", (error) => {
        clearTimeout(timer);
        reject(error);
      });
    });
  }

  constructor(addr, socket) {
    super();
    this.addr = addr;
    this.#socket = socket;
    createInterface({ input: socket }).on("line", (line) => {
      const message = JSON.parse(line);
      if ("notification" in message) this.emit("notification", message.notification);
      else this.#pending.shift()?.resolve(message);
    });
    const fail = (error) => {
      this.closed = true;
      const reason = error ?? new Error(`${addr} closed the connection`);
      for (const { reject } of this.#pending.splice(0)) reject(reason);
    };
    socket.on("error", fail);
    socket.on("close", () => fail());
  }

  request(message) {
    if (this.closed) return Promise.reject(new Error(`${this.addr} is not connected`));
    return new Promise((resolve, reject) => {
      this.#pending.push({ resolve, reject });
      this.#socket.write(JSON.stringify(message) + "\n");
    });
  }

  close() {
    this.closed = true;
    this.#socket.end();
  }
}

/**
 * Emits "notification" events, `{ channel, payload, sender }`, for channels
 * this client LISTENs to.
 */
export class SpiroCluster extends EventEmitter {
  #nodes;
  #primary = null;
  #replicas = [];
  #next = 0;
  #lastSeq = 0;
  #inTransaction = false;

  /** Where the last statement ran: `{ primary: addr }` or `{ replica: addr }`. */
  lastTarget = null;

  /** @param {string[]} nodes every server's `host:port`, in any order */
  static async connect(nodes) {
    const cluster = new SpiroCluster(nodes);
    await cluster.discover();
    return cluster;
  }

  constructor(nodes) {
    super();
    this.#nodes = [...nodes];
  }

  get primary() {
    return this.#primary?.addr ?? null;
  }

  get replicas() {
    return this.#replicas.map((c) => c.addr);
  }

  /** The sequence number of this client's latest commit. */
  get lastSeq() {
    return this.#lastSeq;
  }

  /** Asks every server for its role and reconnects accordingly. */
  async discover() {
    this.#primary?.close();
    for (const c of this.#replicas) c.close();
    this.#primary = null;
    this.#replicas = [];
    this.#inTransaction = false;
    const found = await Promise.all(
      this.#nodes.map(async (addr) => {
        try {
          const conn = await Connection.open(addr);
          return { conn, status: await conn.request({ status: true }) };
        } catch {
          return null;
        }
      }),
    );
    let best = null;
    for (const node of found.filter(Boolean)) {
      if (node.status.role === "replica") this.#replicas.push(node.conn);
      else if (node.status.role === "primary" && (!best || node.status.epoch > best.status.epoch)) {
        best?.conn.close();
        best = node;
      } else node.conn.close();
    }
    if (best) {
      this.#primary = best.conn;
      this.#primary.on("notification", (n) => this.emit("notification", n));
    }
    if (!this.#primary && this.#replicas.length === 0) throw new Error("no SpiroDB server is reachable");
  }

  /**
   * Runs one SQL statement. A query resolves to an array of row objects;
   * anything else to an object such as `{ command: "INSERT", rows: 2 }`.
   * @param {string} statement
   * @param {Array<null | boolean | number | string>} params values for `?` placeholders
   */
  async query(statement, params = []) {
    const keyword = firstKeyword(statement);
    const request = { sql: statement };
    if (params.length) request.params = params;
    if (READS.has(keyword) && !this.#inTransaction) {
      const reply = await this.#readFromReplica({ ...request, min_seq: this.#lastSeq });
      if (reply) return result(reply);
      return result(await this.#onPrimary(request, true));
    }
    const reply = await this.#onPrimary(request, false);
    if (!("error" in reply)) {
      if (reply.seq) this.#lastSeq = Math.max(this.#lastSeq, reply.seq);
      if (keyword === "BEGIN") this.#inTransaction = true;
      if (keyword === "COMMIT" || keyword === "ROLLBACK") this.#inTransaction = false;
    }
    return result(reply);
  }

  async #readFromReplica(request) {
    for (let tries = this.#replicas.length; tries > 0 && this.#replicas.length > 0; tries--) {
      const replica = this.#replicas[this.#next++ % this.#replicas.length];
      try {
        const reply = await replica.request(request);
        // "behind": hasn't applied our latest write; "readonly": not a read after all.
        if (reply.kind === "readonly") return null;
        if (reply.kind === "behind") continue;
        this.lastTarget = { replica: replica.addr };
        return reply;
      } catch {
        this.#replicas = this.#replicas.filter((c) => c !== replica);
      }
    }
    return null;
  }

  async #onPrimary(request, retry) {
    for (let attempt = 0; attempt < 2; attempt++) {
      if ((!this.#primary || this.#primary.closed) && !this.#inTransaction) {
        await this.discover().catch(() => {});
      }
      const primary = this.#primary;
      if (!primary) throw new Error("no primary is reachable");
      try {
        const reply = await primary.request(request);
        if (reply.kind === "readonly" && attempt === 0 && !this.#inTransaction) {
          this.#primary = null; // it was demoted; nothing ran, so find the real one
          continue;
        }
        this.lastTarget = { primary: primary.addr };
        return reply;
      } catch (error) {
        const wasInTransaction = this.#inTransaction;
        this.#primary = null;
        this.#inTransaction = false;
        if (!retry || wasInTransaction || attempt === 1) throw error;
      }
    }
    throw new Error("no primary is reachable");
  }

  listen(channel) {
    return this.query(`LISTEN ${identifier(channel)}`);
  }

  unlisten(channel) {
    return this.query(channel === undefined ? "UNLISTEN *" : `UNLISTEN ${identifier(channel)}`);
  }

  notify(channel, payload = "") {
    return this.query(`NOTIFY ${identifier(channel)}, ?`, [String(payload)]);
  }

  close() {
    this.#primary?.close();
    for (const c of this.#replicas) c.close();
  }
}

const READS = new Set(["SELECT", "EXPLAIN", "WITH"]);

/** The statement's first word, skipping comments. */
function firstKeyword(statement) {
  const text = statement.replace(/^(\s+|--[^\n]*\n?|\/\*[\s\S]*?\*\/)*/, "");
  return (/^[A-Za-z]+/.exec(text)?.[0] ?? "").toUpperCase();
}

function result(reply) {
  if ("error" in reply) {
    const error = new Error(reply.error);
    error.kind = reply.kind;
    throw error;
  }
  if ("columns" in reply) {
    return reply.rows.map((row) => Object.fromEntries(reply.columns.map((name, i) => [name, row[i]])));
  }
  const { seq, ...rest } = reply; // seq is the client's business
  return rest;
}

function identifier(name) {
  return `"${String(name).replaceAll('"', '""')}"`;
}
