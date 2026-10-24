// Run from the repository root after `cargo build --release`:
//   node examples/node/demo.mjs

import { SpiroDB, sql } from "./spirodb.mjs";

const db = new SpiroDB(process.env.SPIRO_DATA ?? "./demo-data", {
  binary: process.env.SPIRO ?? "./target/release/spiro",
});

await db.query(`CREATE TABLE IF NOT EXISTS users (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  age INTEGER
)`);
for (const [name, age] of [["Ada", 36], ["Alan", 41], ["Miles O'Brien", 52]]) {
  await db.query(sql`INSERT INTO users (name, age) VALUES (${name}, ${age})`);
}

const users = await db.query("SELECT * FROM users WHERE age > 40 ORDER BY age");
console.log(users); // a plain JavaScript array of objects
console.log(users.map((u) => u.name));

const [{ average }] = await db.query("SELECT avg(age) AS average FROM users");
console.log("average age:", average);

await db.close();
