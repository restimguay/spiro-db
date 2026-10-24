// SpiroDB for Node.js: query results come back as plain JavaScript arrays.
//
// Runs the `spiro` shell in JSON mode as a child process and talks to it one
// statement at a time; each statement sent gets exactly one JSON line back.
// No npm dependencies.
//
//   import { SpiroDB, sql } from "./spirodb.mjs";
//   const db = new SpiroDB("./data");
//   const rows = await db.query(sql`SELECT * FROM users WHERE name = ${name}`);
//   // rows: [{ id: 1, name: "Ada", age: 36 }, ...]

import { spawn } from "node:child_process";
import { EventEmitter } from "node:events";
import { createInterface } from "node:readline";

/**
 * Emits "notification" events, `{ channel, payload, sender }`, for messages
 * on channels this connection LISTENs to.
 */
export class SpiroDB extends EventEmitter {
  #proc;
  #pending = [];
  #closed;

  /**
   * @param {string} path the database directory (created if missing)
   * @param {{ binary?: string }} options path to the `spiro` executable
   */
  constructor(path, { binary = "spiro" } = {}) {
    super();
    this.#proc = spawn(binary, [path, "--json"], { stdio: ["pipe", "pipe", "pipe"] });
    let stderr = "";
    this.#proc.stderr.on("data", (chunk) => (stderr += chunk));
    this.#proc.on("error", () => {}); // reported through #closed instead
    createInterface({ input: this.#proc.stdout }).on("line", (line) => {
      const value = JSON.parse(line);
      // Notifications arrive between replies and answer no query.
      if (!Array.isArray(value) && "notification" in value) {
        this.emit("notification", value.notification);
        return;
      }
      const { resolve, reject } = this.#pending.shift();
      if (!Array.isArray(value) && "error" in value) reject(new Error(value.error));
      else resolve(value);
    });
    this.#closed = new Promise((resolve) => {
      this.#proc.on("close", (code) => {
        const error = new Error(stderr.trim() || `spiro exited with code ${code}`);
        for (const { reject } of this.#pending.splice(0)) reject(error);
        resolve();
      });
    });
  }

  /**
   * Runs one SQL statement. A query resolves to an array of row objects;
   * anything else to an object such as `{ command: "INSERT", rows: 2 }`.
   * Send one statement per call: each call reads exactly one reply.
   * @param {string} statement
   * @returns {Promise<object[] | { command: string, rows?: number }>}
   */
  query(statement) {
    return new Promise((resolve, reject) => {
      this.#pending.push({ resolve, reject });
      this.#proc.stdin.write(statement.trim().replace(/;*$/, ";") + "\n");
    });
  }

  /** Starts receiving "notification" events for `channel`. */
  listen(channel) {
    return this.query(`LISTEN ${identifier(channel)}`);
  }

  /** Stops listening on `channel`, or on every channel if none is given. */
  unlisten(channel) {
    return this.query(channel === undefined ? "UNLISTEN *" : `UNLISTEN ${identifier(channel)}`);
  }

  /** Sends `payload` to every connection listening on `channel`, on commit. */
  notify(channel, payload = "") {
    return this.query(`NOTIFY ${identifier(channel)}, ${literal(String(payload))}`);
  }

  /** Commits nothing on its own: open transactions are rolled back. */
  async close() {
    this.#proc.stdin.end();
    await this.#closed;
  }
}

/**
 * A tagged template that inlines values as safely quoted SQL literals:
 * sql`SELECT * FROM users WHERE name = ${name} AND age > ${age}`
 */
export function sql(strings, ...values) {
  return strings.reduce((out, text, i) => out + literal(values[i - 1]) + text);
}

function identifier(name) {
  return `"${String(name).replaceAll('"', '""')}"`;
}

function literal(value) {
  if (value === null || value === undefined) return "NULL";
  switch (typeof value) {
    case "number":
      if (!Number.isFinite(value)) throw new TypeError(`cannot store ${value} in SQL`);
      return String(value);
    case "bigint":
      return value.toString();
    case "boolean":
      return value ? "TRUE" : "FALSE";
    case "string":
      // SpiroDB strings have no backslash escapes; doubling quotes is enough.
      return `'${value.replaceAll("'", "''")}'`;
    default:
      throw new TypeError(`cannot use a ${typeof value} as a SQL value`);
  }
}
