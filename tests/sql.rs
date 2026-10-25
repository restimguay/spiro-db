use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use spirodb::Error;
use spirodb::Options;
use spirodb::sql::{Database, QueryResult, Value};

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> TempDir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let unique = format!("spirodb-sql-{name}-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
        let path = std::env::temp_dir().join(unique);
        let _ = fs::remove_dir_all(&path);
        TempDir(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn open(name: &str) -> (TempDir, Database) {
    let dir = TempDir::new(name);
    let db = Database::open(&dir.0).unwrap();
    (dir, db)
}

fn run(db: &Database, sql: &str) {
    for statement in sql.split(';').filter(|s| !s.trim().is_empty()) {
        db.execute(statement).unwrap_or_else(|e| panic!("{statement}: {e}"));
    }
}

fn query(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    db.query(sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).rows
}

fn error(db: &Database, sql: &str) -> String {
    match db.execute(sql) {
        Ok(result) => panic!("{sql} should fail, but returned {result}"),
        Err(e) => e.to_string(),
    }
}

/// Builds expected rows: `rows![[1, "a"], [2, "b"]]`.
macro_rules! rows {
    ($([$($v:expr),* $(,)?]),* $(,)?) => { vec![$(vec![$(Value::from($v)),*]),*] };
}

const NULL: Option<i64> = None;

fn people(db: &Database) {
    run(
        db,
        "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT NOT NULL, age INTEGER, city TEXT);
         INSERT INTO people (name, age, city) VALUES
            ('Ada', 36, 'London'), ('Alan', 41, 'London'), ('Grace', 85, 'New York'),
            ('Linus', 54, 'Portland'), ('Margaret', NULL, 'Boston');",
    );
}

#[test]
fn create_insert_select() {
    let (_dir, db) = open("basics");
    people(&db);
    let result = db.query("SELECT * FROM people WHERE id = 1", &[]).unwrap();
    assert_eq!(result.columns, ["id", "name", "age", "city"]);
    assert_eq!(result.rows, rows![[1, "Ada", 36, "London"]]);
    assert_eq!(
        query(&db, "SELECT name FROM people WHERE age > 40 ORDER BY age DESC"),
        rows![["Grace"], ["Linus"], ["Alan"]]
    );
    assert_eq!(query(&db, "SELECT id FROM people ORDER BY id LIMIT 2 OFFSET 1"), rows![[2], [3]]);
    assert_eq!(query(&db, "SELECT name, age FROM people WHERE age IS NULL"), rows![["Margaret", NULL]]);
    assert_eq!(query(&db, "SELECT 1 + 2 AS three, 'a' || 'b'"), rows![[3, "ab"]]);
    let columns = db.query("SELECT age * 2, upper(name) AS shout FROM people LIMIT 1", &[]).unwrap().columns;
    assert_eq!(columns, ["age * 2", "shout"]);
    // Auto-assigned ids continue after explicit ones.
    run(&db, "INSERT INTO people (id, name) VALUES (100, 'Explicit'); INSERT INTO people (name) VALUES ('Next')");
    assert_eq!(query(&db, "SELECT id FROM people WHERE name = 'Next'"), rows![[101]]);
}

#[test]
fn expressions_and_null_semantics() {
    let (_dir, db) = open("expressions");
    people(&db);
    let one = |sql: &str| query(&db, &format!("SELECT {sql}")).remove(0).remove(0);
    assert_eq!(one("7 / 2"), Value::Integer(3));
    assert_eq!(one("7 / 2.0"), Value::Real(3.5));
    assert_eq!(one("-7 % 3"), Value::Integer(-1));
    assert_eq!(one("NULL = NULL"), Value::Null);
    assert_eq!(one("NULL IS NULL"), Value::Boolean(true));
    assert_eq!(one("NULL OR TRUE"), Value::Boolean(true));
    assert_eq!(one("2 BETWEEN 1 AND 3"), Value::Boolean(true));
    assert_eq!(one("3 NOT IN (1, 2)"), Value::Boolean(true));
    assert_eq!(one("3 IN (1, NULL)"), Value::Null);
    assert_eq!(one("'Hello' LIKE 'H%o'"), Value::Boolean(true));
    assert_eq!(one("'Hello' LIKE 'h%'"), Value::Boolean(false));
    assert_eq!(one("'Hello' ILIKE 'h%'"), Value::Boolean(true));
    assert_eq!(one("CASE WHEN 1 > 2 THEN 'a' WHEN 2 > 1 THEN 'b' END"), Value::Text("b".into()));
    assert_eq!(one("CASE 3 WHEN 1 THEN 'one' ELSE 'other' END"), Value::Text("other".into()));
    assert_eq!(one("CAST('42' AS INTEGER) + 1"), Value::Integer(43));
    assert_eq!(one("CAST(3.9 AS INTEGER)"), Value::Integer(3));
    assert_eq!(one("coalesce(NULL, NULL, 'x')"), Value::Text("x".into()));
    assert_eq!(one("length('héllo')"), Value::Integer(5));
    assert_eq!(one("round(2.567, 2)"), Value::Real(2.57));
    assert_eq!(one("substr('spirodb', 1, 5)"), Value::Text("spiro".into()));
    assert_eq!(one("typeof(1.5)"), Value::Text("real".into()));
    // Rows where the condition is NULL are filtered out, and NOT doesn't bring them back.
    assert_eq!(query(&db, "SELECT count(*) FROM people WHERE age > 50"), rows![[2]]);
    assert_eq!(query(&db, "SELECT count(*) FROM people WHERE NOT age > 50"), rows![[2]]);
    assert!(error(&db, "SELECT 1 / 0").contains("division by zero"));
    assert!(error(&db, "SELECT 'a' + 1").contains("cannot apply +"));
    assert!(error(&db, "SELECT nope(1)").contains("no such function"));
}

#[test]
fn aggregates_and_grouping() {
    let (_dir, db) = open("aggregates");
    people(&db);
    assert_eq!(
        query(&db, "SELECT count(*), count(age), sum(age), min(age), max(age) FROM people"),
        rows![[5, 4, 216, 36, 85]]
    );
    assert_eq!(query(&db, "SELECT avg(age) FROM people"), rows![[54.0]]);
    assert_eq!(
        query(&db, "SELECT city, count(*) AS n FROM people GROUP BY city ORDER BY n DESC, city"),
        rows![["London", 2], ["Boston", 1], ["New York", 1], ["Portland", 1]]
    );
    assert_eq!(query(&db, "SELECT city FROM people GROUP BY 1 HAVING count(*) > 1"), rows![["London"]]);
    assert_eq!(
        query(&db, "SELECT upper(city) AS c, max(age) FROM people GROUP BY c HAVING max(age) > 50 ORDER BY c"),
        rows![["NEW YORK", 85], ["PORTLAND", 54]]
    );
    assert_eq!(query(&db, "SELECT count(DISTINCT city) FROM people"), rows![[4]]);
    // With no GROUP BY, an empty input still yields one row.
    assert_eq!(query(&db, "SELECT count(*), sum(age) FROM people WHERE id > 99"), rows![[0, NULL]]);
    assert!(error(&db, "SELECT name, count(*) FROM people").contains("must appear in GROUP BY"));
    assert!(error(&db, "SELECT * FROM people WHERE count(*) > 1").contains("not allowed in WHERE"));
    assert_eq!(
        query(&db, "SELECT DISTINCT city FROM people WHERE city LIKE '%o%' ORDER BY city"),
        rows![["Boston"], ["London"], ["New York"], ["Portland"]]
    );
}

#[test]
fn joins() {
    let (_dir, db) = open("joins");
    people(&db);
    run(
        &db,
        "CREATE TABLE orders (id INTEGER PRIMARY KEY, person_id INTEGER, total REAL);
         CREATE INDEX orders_person ON orders (person_id);
         INSERT INTO orders (person_id, total) VALUES (1, 10.5), (1, 20.0), (3, 99.99), (42, 1.0);",
    );
    assert_eq!(
        query(&db, "SELECT p.name, o.total FROM people p JOIN orders o ON o.person_id = p.id ORDER BY o.total"),
        rows![["Ada", 10.5], ["Ada", 20.0], ["Grace", 99.99]]
    );
    assert_eq!(
        query(
            &db,
            "SELECT p.name, count(o.id), coalesce(sum(o.total), 0.0) FROM people p \
             LEFT JOIN orders o ON o.person_id = p.id GROUP BY p.name ORDER BY p.name"
        ),
        rows![["Ada", 2, 30.5], ["Alan", 0, 0.0], ["Grace", 1, 99.99], ["Linus", 0, 0.0], ["Margaret", 0, 0.0]]
    );
    // The join looks orders up through the index rather than scanning them.
    let plan = query(&db, "EXPLAIN SELECT * FROM people p JOIN orders o ON o.person_id = p.id WHERE p.age < 40");
    assert_eq!(plan[0], rows![["SCAN people AS p"]][0]);
    assert!(
        plan.iter().any(|r| r[0] == Value::from("JOIN SEARCH orders AS o USING INDEX orders_person (person_id = ?)"))
    );
    assert_eq!(query(&db, "SELECT count(*) FROM people, orders"), rows![[20]]);
    assert!(
        error(&db, "SELECT id FROM people JOIN orders ON person_id = people.id").contains("ambiguous column name: id")
    );
    assert!(error(&db, "SELECT * FROM people JOIN people ON 1 = 1").contains("more than once"));
}

#[test]
fn indexes_are_used_and_maintained() {
    let (_dir, db) = open("indexes");
    people(&db);
    run(&db, "CREATE INDEX people_age ON people (age); CREATE UNIQUE INDEX people_name ON people (name)");
    let plan = |sql: &str| query(&db, &format!("EXPLAIN {sql}"))[0][0].to_string();
    assert_eq!(plan("SELECT * FROM people WHERE id = 3"), "SEARCH people USING PRIMARY KEY (id = ?)");
    assert_eq!(plan("SELECT * FROM people WHERE id > 3"), "SEARCH people USING PRIMARY KEY (id > ?)");
    assert_eq!(plan("SELECT * FROM people WHERE age = 41"), "SEARCH people USING INDEX people_age (age = ?)");
    assert_eq!(
        plan("SELECT * FROM people WHERE age BETWEEN 40 AND 60"),
        "SEARCH people USING INDEX people_age (age >= ? AND age <= ?)"
    );
    assert_eq!(
        plan("SELECT * FROM people WHERE name = 'Ada' AND age = 36"),
        "SEARCH people USING INDEX people_name (name = ?)"
    );
    assert_eq!(plan("SELECT * FROM people WHERE city = 'London'"), "SCAN people");
    assert_eq!(plan("UPDATE people SET age = 1 WHERE age < 40"), "SEARCH people USING INDEX people_age (age < ?)");

    assert_eq!(
        query(&db, "SELECT name FROM people WHERE age BETWEEN 40 AND 60 ORDER BY age"),
        rows![["Alan"], ["Linus"]]
    );
    assert_eq!(query(&db, "SELECT name FROM people WHERE age > 50.5 ORDER BY name"), rows![["Grace"], ["Linus"]]);
    // Updates move index entries: the old value no longer finds the row.
    run(&db, "UPDATE people SET age = age + 100 WHERE name = 'Ada'");
    assert_eq!(query(&db, "SELECT name FROM people WHERE age = 36"), Vec::<Vec<Value>>::new());
    assert_eq!(query(&db, "SELECT name FROM people WHERE age = 136"), rows![["Ada"]]);
    run(&db, "DELETE FROM people WHERE age > 80");
    assert_eq!(query(&db, "SELECT name FROM people WHERE age > 80"), Vec::<Vec<Value>>::new());
    assert_eq!(query(&db, "SELECT count(*) FROM people"), rows![[3]]);
    assert!(error(&db, "INSERT INTO people (name) VALUES ('Alan')").contains("UNIQUE constraint failed: people.name"));
    run(&db, "INSERT INTO people (name, city) VALUES ('Zed', 'Portland')");
    assert!(error(&db, "CREATE UNIQUE INDEX dup_city ON people (city)").contains("UNIQUE constraint failed"));
    run(&db, "DROP INDEX people_age");
    assert_eq!(plan("SELECT * FROM people WHERE age = 41"), "SCAN people");
}

#[test]
fn conditions_enforced_by_keys_are_not_lost() {
    let (_dir, db) = open("enforced");
    people(&db);
    let ids = |cond: &str| query(&db, &format!("SELECT id FROM people WHERE {cond} ORDER BY id"));
    assert_eq!(ids("id >= 2 AND id >= 4"), rows![[4], [5]], "only one bound is enforced");
    assert_eq!(ids("id BETWEEN 1 AND 5 AND id < 3"), rows![[1], [2]]);
    assert_eq!(ids("id < 3 AND id BETWEEN 2 AND 5"), rows![[2]], "a partly overridden bound still counts");
    assert_eq!(ids("id = 2 AND id = 3"), Vec::<Vec<Value>>::new());
    assert_eq!(ids("id >= 2.5"), rows![[3], [4], [5]], "a bound that isn't a key value falls back to a scan");
    assert_eq!(ids("id > NULL"), Vec::<Vec<Value>>::new());
    assert_eq!(ids("3 < id AND name != 'Linus'"), rows![[5]]);

    let filtered =
        |sql: &str| query(&db, &format!("EXPLAIN {sql}")).iter().any(|r| r[0].to_string().trim() == "FILTER");
    assert!(!filtered("SELECT * FROM people WHERE id > 3"), "the key range alone answers it");
    assert!(filtered("SELECT * FROM people WHERE id > 3 AND name LIKE 'A%'"));
    assert!(filtered("SELECT * FROM people WHERE age > 3"), "no index on age");
}

#[test]
fn updates_and_deletes() {
    let (_dir, db) = open("updates");
    people(&db);
    assert_eq!(db.execute("UPDATE people SET city = 'Cambridge' WHERE name LIKE 'A%'").unwrap().rows_affected(), 2);
    assert_eq!(query(&db, "SELECT name FROM people WHERE city = 'Cambridge' ORDER BY name"), rows![["Ada"], ["Alan"]]);
    // Every SET sees the old row, and shifting all keys up by one doesn't collide midway.
    run(&db, "UPDATE people SET id = id + 1, age = id");
    assert_eq!(query(&db, "SELECT id, age FROM people ORDER BY id"), rows![[2, 1], [3, 2], [4, 3], [5, 4], [6, 5]]);
    assert_eq!(db.execute("DELETE FROM people WHERE id >= 5").unwrap().rows_affected(), 2);
    assert_eq!(db.execute("DELETE FROM people").unwrap().rows_affected(), 3);
    assert_eq!(query(&db, "SELECT count(*) FROM people"), rows![[0]]);
    assert!(error(&db, "UPDATE people SET nope = 1").contains("no such column"));
}

#[test]
fn constraints_and_types() {
    let (_dir, db) = open("constraints");
    run(
        &db,
        "CREATE TABLE accounts (
            owner TEXT, bank TEXT, balance REAL DEFAULT 0, email TEXT UNIQUE, active BOOLEAN DEFAULT TRUE,
            PRIMARY KEY (owner, bank));
         INSERT INTO accounts (owner, bank, email) VALUES ('ada', 'first', 'ada@x.org');
         INSERT INTO accounts (owner, bank, balance) VALUES ('ada', 'second', 5);",
    );
    assert_eq!(
        query(&db, "SELECT owner, bank, balance, active FROM accounts ORDER BY bank"),
        rows![["ada", "first", 0.0, true], ["ada", "second", 5.0, true]]
    );
    assert!(
        error(&db, "INSERT INTO accounts (owner, bank) VALUES ('ada', 'first')")
            .contains("UNIQUE constraint failed: accounts.owner, accounts.bank")
    );
    assert!(
        error(&db, "INSERT INTO accounts (owner) VALUES ('bob')").contains("NOT NULL constraint failed: accounts.bank")
    );
    assert!(
        error(&db, "INSERT INTO accounts (owner, bank, email) VALUES ('bob', 'x', 'ada@x.org')")
            .contains("accounts.email")
    );
    assert!(
        error(&db, "INSERT INTO accounts (owner, bank, balance) VALUES ('bob', 'x', 'lots')")
            .contains("cannot store TEXT 'lots'")
    );
    // NULLs never collide in a UNIQUE column.
    run(&db, "INSERT INTO accounts (owner, bank) VALUES ('bob', 'x'), ('carol', 'x')");
    assert_eq!(query(&db, "SELECT count(*) FROM accounts WHERE email IS NULL"), rows![[3]]);
    assert!(error(&db, "CREATE TABLE accounts (x INTEGER)").contains("already exists"));
    run(&db, "CREATE TABLE IF NOT EXISTS accounts (x INTEGER)");
    assert!(
        error(&db, "CREATE TABLE t (a INTEGER PRIMARY KEY, b INTEGER PRIMARY KEY)")
            .contains("more than one primary key")
    );
    let schema = db.schema("accounts").unwrap().unwrap();
    assert!(
        schema.contains("PRIMARY KEY (owner, bank)") && schema.contains("CREATE UNIQUE INDEX accounts_email_key"),
        "{schema}"
    );
}

#[test]
fn transactions() {
    let (_dir, db) = open("transactions");
    people(&db);
    let mut session = db.session();
    session.execute("BEGIN").unwrap();
    session.execute("INSERT INTO people (name) VALUES ('Pending')").unwrap();
    assert_eq!(session.query("SELECT count(*) FROM people", &[]).unwrap().rows, rows![[6]], "own writes are visible");
    assert_eq!(query(&db, "SELECT count(*) FROM people"), rows![[5]], "others don't see them yet");
    // A failing statement is undone on its own; the transaction carries on.
    assert!(session.execute("INSERT INTO people (name) VALUES ('ok'), (NULL)").is_err());
    assert_eq!(session.query("SELECT count(*) FROM people", &[]).unwrap().rows, rows![[6]]);
    session.execute("COMMIT").unwrap();
    assert_eq!(query(&db, "SELECT count(*) FROM people"), rows![[6]]);

    session.execute("BEGIN").unwrap();
    session.execute("DELETE FROM people").unwrap();
    session.execute("ROLLBACK").unwrap();
    assert_eq!(query(&db, "SELECT count(*) FROM people"), rows![[6]]);

    // Writers wait for an open transaction, and give up after the busy timeout.
    db.set_busy_timeout(Duration::from_millis(50));
    session.execute("BEGIN").unwrap();
    assert!(error(&db, "INSERT INTO people (name) VALUES ('blocked')").contains("locked"));
    assert_eq!(query(&db, "SELECT count(*) FROM people"), rows![[6]], "reads don't wait");
    session.execute("UPDATE people SET age = 0").unwrap();
    drop(session); // rolls back and releases the lock
    assert_eq!(query(&db, "SELECT count(*) FROM people WHERE age = 0"), rows![[0]]);
    run(&db, "INSERT INTO people (name) VALUES ('unblocked')");
    assert!(db.execute("COMMIT").unwrap_err().to_string().contains("no transaction"));
}

#[test]
fn schema_changes_are_isolated_until_commit() {
    let (_dir, db) = open("schema-cache");
    run(&db, "CREATE TABLE t (a INTEGER); INSERT INTO t VALUES (1)");
    let columns = |rows: spirodb::sql::Rows| rows.columns;
    assert_eq!(columns(db.query("SELECT * FROM t", &[]).unwrap()), ["a"]); // warms the schema cache

    let mut session = db.session();
    session.execute("BEGIN").unwrap();
    session.execute("DROP TABLE t").unwrap();
    session.execute("CREATE TABLE t (a INTEGER, b TEXT)").unwrap();
    session.execute("CREATE TABLE u (x INTEGER)").unwrap();
    assert_eq!(columns(session.query("SELECT * FROM t", &[]).unwrap()), ["a", "b"], "the transaction sees its own DDL");
    assert_eq!(columns(db.query("SELECT * FROM t", &[]).unwrap()), ["a"], "others don't, yet");
    assert!(error(&db, "SELECT * FROM u").contains("no such table"));
    session.execute("COMMIT").unwrap();
    assert_eq!(columns(db.query("SELECT * FROM t", &[]).unwrap()), ["a", "b"]);
    assert_eq!(query(&db, "SELECT count(*) FROM t"), rows![[0]]);

    session.execute("BEGIN").unwrap();
    session.execute("DROP TABLE u").unwrap();
    session.execute("ROLLBACK").unwrap();
    assert_eq!(query(&db, "SELECT count(*) FROM u"), rows![[0]], "rolled-back DDL leaves no trace");

    run(&db, "CREATE INDEX t_b ON t (b)");
    assert!(query(&db, "EXPLAIN SELECT * FROM t WHERE b = 'x'")[0][0].to_string().contains("USING INDEX t_b"));
}

#[test]
fn everything_survives_reopen() {
    let dir = TempDir::new("reopen");
    {
        let db = Database::open(&dir.0).unwrap();
        people(&db);
        run(&db, "CREATE INDEX people_city ON people (city)");
    }
    let db = Database::open(&dir.0).unwrap();
    assert_eq!(db.table_names().unwrap(), ["people"]);
    assert_eq!(query(&db, "SELECT name FROM people WHERE city = 'London' ORDER BY name"), rows![["Ada"], ["Alan"]]);
    assert!(query(&db, "EXPLAIN SELECT * FROM people WHERE city = 'x'")[0][0].to_string().contains("people_city"));
    run(&db, "INSERT INTO people (name) VALUES ('After')");
    assert_eq!(query(&db, "SELECT id FROM people WHERE name = 'After'"), rows![[6]]);
}

#[test]
fn parameters_and_prepared_statements() {
    let (_dir, db) = open("params");
    people(&db);
    let rows = db
        .query("SELECT name FROM people WHERE age > ? AND city = ? ORDER BY name", &[40.into(), "London".into()])
        .unwrap();
    assert_eq!(rows.rows, rows![["Alan"]]);
    assert_eq!(db.query("SELECT $2 - $1", &[1.into(), 10.into()]).unwrap().rows, rows![[9]]);
    let insert = db.prepare("INSERT INTO people (name, age) VALUES (?, ?)").unwrap();
    assert_eq!(insert.param_count(), 2);
    let mut session = db.session();
    for i in 0..10 {
        session.execute_prepared(&insert, &[format!("p{i}").into(), Value::Integer(i)]).unwrap();
    }
    assert_eq!(query(&db, "SELECT count(*) FROM people WHERE name LIKE 'p%'"), rows![[10]]);
    // Parameters are values, never SQL: this is just an odd name.
    db.execute_with("INSERT INTO people (name) VALUES (?)", &["x'); DROP TABLE people; --".into()]).unwrap();
    assert_eq!(query(&db, "SELECT count(*) FROM people"), rows![[16]]);
    assert!(db.execute_with("SELECT ?", &[]).unwrap_err().to_string().contains("expected 1 parameters"));
}

#[test]
fn prepared_plans_follow_schema_changes() {
    let (_dir, db) = open("plan-cache");
    run(&db, "CREATE TABLE t (a INTEGER, b TEXT); INSERT INTO t VALUES (1, 'x'), (2, 'z')");
    let select = db.prepare("SELECT * FROM t WHERE a = ?").unwrap();
    let rows_of = |s: &mut spirodb::sql::Session, a: i64| {
        s.execute_prepared(&select, &[a.into()]).unwrap().rows().unwrap().rows.clone()
    };
    let mut session = db.session();
    assert_eq!(rows_of(&mut session, 1), rows![[1, "x"]]);
    assert_eq!(rows_of(&mut session, 2), rows![[2, "z"]], "the cached plan takes new parameters");

    // A transaction sees its own schema change; others keep the old table.
    session.execute("BEGIN").unwrap();
    session.execute("DROP TABLE t").unwrap();
    session.execute("CREATE TABLE t (a INTEGER, b TEXT, c REAL)").unwrap();
    session.execute("INSERT INTO t VALUES (1, 'y', 2.5)").unwrap();
    assert_eq!(rows_of(&mut session, 1), rows![[1, "y", 2.5]]);
    assert_eq!(rows_of(&mut db.session(), 1), rows![[1, "x"]]);
    session.execute("COMMIT").unwrap();
    assert_eq!(rows_of(&mut db.session(), 1), rows![[1, "y", 2.5]], "the commit retires the old plan");

    run(&db, "INSERT INTO t VALUES (2, 'a', 0.5), (3, 'b', 1.5)");
    let limited = db.prepare("SELECT a FROM t ORDER BY a LIMIT ? OFFSET ?").unwrap();
    let mut s = db.session();
    assert_eq!(s.execute_prepared(&limited, &[2.into(), 1.into()]).unwrap().rows().unwrap().rows, rows![[2], [3]]);
    assert_eq!(s.execute_prepared(&limited, &[1.into(), 0.into()]).unwrap().rows().unwrap().rows, rows![[1]]);
    let negative = s.execute_prepared(&limited, &[(-1).into(), 0.into()]).unwrap_err().to_string();
    assert!(negative.contains("LIMIT must be a non-negative integer"), "{negative}");

    // A statement prepared on one database and run on another gets a fresh plan.
    let (_other_dir, other) = open("plan-cache-other");
    run(&other, "CREATE TABLE t (a INTEGER, b TEXT); INSERT INTO t VALUES (1, 'other')");
    assert_eq!(rows_of(&mut other.session(), 1), rows![[1, "other"]]);
}

#[test]
fn drop_and_recreate() {
    let (_dir, db) = open("drop");
    people(&db);
    run(&db, "CREATE INDEX people_age ON people (age); DROP TABLE people");
    assert!(error(&db, "SELECT * FROM people").contains("no such table: people"));
    assert!(error(&db, "DROP INDEX people_age").contains("no such index"));
    run(&db, "DROP TABLE IF EXISTS people; CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT)");
    assert_eq!(query(&db, "SELECT count(*) FROM people"), rows![[0]]);
    run(&db, "INSERT INTO people (name) VALUES ('fresh')");
    assert_eq!(query(&db, "SELECT id FROM people"), rows![[1]], "row ids start over");
}

#[test]
fn insert_from_select_and_tables_without_primary_keys() {
    let (_dir, db) = open("insert-select");
    people(&db);
    run(&db, "CREATE TABLE log (who TEXT, years INTEGER)");
    let result = db.execute("INSERT INTO log SELECT name, age FROM people WHERE age IS NOT NULL").unwrap();
    assert_eq!(result, QueryResult::Changed { command: "INSERT", rows: 4 });
    run(&db, "INSERT INTO log VALUES ('Ada', 36)"); // duplicates are fine without a key
    assert_eq!(query(&db, "SELECT who, count(*) FROM log GROUP BY who HAVING count(*) > 1"), rows![["Ada", 2]]);
    run(&db, "UPDATE log SET years = years + 1 WHERE who = 'Ada'");
    assert_eq!(query(&db, "SELECT years FROM log WHERE who = 'Ada'"), rows![[37], [37]]);
}

#[test]
fn json_output() {
    let (_dir, db) = open("json");
    people(&db);
    let rows = db.query("SELECT id, name, age FROM people WHERE id IN (1, 5) ORDER BY id", &[]).unwrap();
    assert_eq!(rows.to_json(), r#"[{"id":1,"name":"Ada","age":36},{"id":5,"name":"Margaret","age":null}]"#);
    assert_eq!(db.query("SELECT * FROM people WHERE id = 0", &[]).unwrap().to_json(), "[]");
    let changed = db.execute("UPDATE people SET age = 1 WHERE id = 1").unwrap();
    assert_eq!(changed.to_json(), r#"{"command":"UPDATE","rows":1}"#);
}

#[test]
fn helpful_errors() {
    let (_dir, db) = open("errors");
    people(&db);
    assert!(matches!(db.execute("SELEC 1"), Err(Error::Syntax(_))));
    assert!(error(&db, "SELECT name FROM people WHERE").contains("line 1, column 30"));
    assert!(error(&db, "SELECT nope FROM people").contains("no such column: nope"));
    assert!(error(&db, "SELECT * FROM nope").contains("no such table: nope"));
    assert!(error(&db, "SELECT x.name FROM people").contains("no such table: x"));
    assert!(error(&db, "INSERT INTO people (name, name) VALUES ('a', 'b')").contains("more than once"));
    assert!(error(&db, "INSERT INTO people (name) VALUES ('a', 'b')").contains("2 values supplied for 1 columns"));
    assert!(error(&db, "SELECT * FROM people ORDER BY 9").contains("position 9"));
    assert!(error(&db, "SELECT 1; SELECT 2").contains("expected one statement"));
    assert!(matches!(db.execute("INSERT INTO people (id, name) VALUES (1, 'dup')"), Err(Error::Constraint(_))));
}

#[test]
fn script_execution() {
    let (_dir, db) = open("script");
    let results = db
        .session()
        .execute_script("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1), (2); SELECT sum(x) FROM t;")
        .unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(results[2].rows().unwrap().rows, rows![[3]]);
}

#[test]
fn concurrent_writers_get_unique_ids() {
    let (_dir, db) = open("concurrent");
    run(&db, "CREATE TABLE events (id INTEGER PRIMARY KEY, worker INTEGER, n INTEGER)");
    let workers: Vec<_> = (0..4)
        .map(|w| {
            let db = db.clone();
            thread::spawn(move || {
                for n in 0..200 {
                    db.execute_with("INSERT INTO events (worker, n) VALUES (?, ?)", &[w.into(), Value::Integer(n)])
                        .unwrap();
                }
            })
        })
        .collect();
    for w in workers {
        w.join().unwrap();
    }
    assert_eq!(
        query(&db, "SELECT count(*), count(DISTINCT id), min(id), max(id) FROM events"),
        rows![[800, 800, 1, 800]]
    );
}

/// SplitMix64, for reproducible random tests.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        (z ^ (z >> 31)) % n
    }
}

/// Runs the same random workload against an indexed table and an identical
/// unindexed one, on a store tuned to flush and compact constantly. Index
/// choice must never change a result.
#[test]
fn indexes_never_change_results() {
    let dir = TempDir::new("differential");
    let opts = Options {
        memtable_size: 32 << 10,
        l0_compaction_trigger: 2,
        l0_stop_writes_trigger: 8,
        target_file_size: 64 << 10,
        block_size: 512,
        ..Options::default()
    };
    let db = Database::open_with(&dir.0, opts).unwrap();
    let schema = "(id INTEGER PRIMARY KEY, a INTEGER, b TEXT, c REAL)";
    run(&db, &format!("CREATE TABLE indexed {schema}; CREATE TABLE plain {schema}"));
    run(&db, "CREATE INDEX indexed_a ON indexed (a); CREATE INDEX indexed_b_a ON indexed (b, a)");

    let mut rng = Rng(11);
    let mut session = db.session();
    session.execute("BEGIN").unwrap();
    for _ in 0..3000 {
        let values = [
            Value::Integer(rng.below(5000) as i64),
            if rng.below(10) == 0 { Value::Null } else { Value::Integer(rng.below(100) as i64) },
            Value::Text(format!("k{}", rng.below(50))),
            Value::Real(rng.below(1000) as f64 / 10.0),
        ];
        for table in ["indexed", "plain"] {
            let sql = format!("INSERT INTO {table} VALUES (?, ?, ?, ?)");
            match session.execute_with(&sql, &values) {
                Ok(_) => {}
                Err(Error::Constraint(_)) => {} // duplicate id: rejected by both
                Err(e) => panic!("{e}"),
            }
        }
    }
    session.execute("COMMIT").unwrap();

    let condition = |rng: &mut Rng| -> String {
        let n = rng.below(100);
        match rng.below(9) {
            0 => format!("a = {n}"),
            1 => format!("a > {n}"),
            2 => format!("a BETWEEN {n} AND {}", n + rng.below(20)),
            3 => format!("b = 'k{}'", rng.below(50)),
            4 => format!("b = 'k{}' AND a < {n}", rng.below(50)),
            5 => format!("id = {}", rng.below(5000)),
            6 => format!("id >= {} AND id < {}", n * 40, n * 50),
            7 => format!("a <= {n}.5"),
            _ => "a IS NULL".to_string(),
        }
    };
    for round in 0..300 {
        let cond = condition(&mut rng);
        if round % 10 == 0 {
            let update = format!("UPDATE {{}} SET a = a + 1, b = 'k{}' WHERE {cond}", rng.below(50));
            let delete = format!("DELETE FROM {{}} WHERE {}", condition(&mut rng));
            for template in [update, delete] {
                let counts: Vec<u64> = ["indexed", "plain"]
                    .iter()
                    .map(|t| db.execute(&template.replace("{}", t)).unwrap().rows_affected())
                    .collect();
                assert_eq!(counts[0], counts[1], "{template}");
            }
        }
        // `*` reads whole rows; the narrower lists are answered from the
        // indexes alone whenever an index covers them.
        for columns in ["*", "id, a", "a, id, b", "count(*)", "b, count(*), max(a)"] {
            let tail = match columns {
                "count(*)" => "",
                c if c.contains("count") => "GROUP BY b ORDER BY b",
                _ => "ORDER BY id",
            };
            let select = format!("SELECT {columns} FROM {{}} WHERE {cond} {tail}");
            let indexed = query(&db, &select.replace("{}", "indexed"));
            let plain = query(&db, &select.replace("{}", "plain"));
            assert_eq!(indexed, plain, "round {round}: {select}");
        }
    }
    let plan = query(&db, "EXPLAIN SELECT id, a FROM indexed WHERE a = 5")[0][0].to_string();
    assert!(plan.ends_with("USING INDEX indexed_a (a = ?) (index only)"), "{plan}");
    let plan = query(&db, "EXPLAIN SELECT id, c FROM indexed WHERE a = 5")[0][0].to_string();
    assert!(!plan.contains("index only"), "c isn't in the index: {plan}");
    let stats = db.kv().stats();
    assert!(stats.flushes > 0, "the workload should exercise flushes: {stats:?}");
}

#[test]
fn listen_and_notify() {
    use spirodb::sql::Notification;
    let (_dir, db) = open("notify");
    let mut listener = db.session();
    let mut sender = db.session();
    let note = |channel: &str, payload: &str, sender: u64| Notification {
        channel: channel.into(),
        payload: payload.into(),
        sender,
    };
    listener.execute("LISTEN orders").unwrap();
    assert_eq!(listener.listening(), ["orders"]);

    sender.execute("NOTIFY orders, 'first'").unwrap();
    sender.execute("NOTIFY other, 'ignored'").unwrap();
    sender.execute_with("NOTIFY Orders, $1", &["from a parameter".into()]).unwrap();
    assert_eq!(
        listener.notifications(),
        vec![note("orders", "first", sender.id()), note("orders", "from a parameter", sender.id())]
    );

    // Delivered at commit, in order, once per distinct payload; never on rollback.
    sender.execute("BEGIN").unwrap();
    sender.execute("NOTIFY orders, 'a'").unwrap();
    sender.execute("NOTIFY orders, 'a'").unwrap();
    sender.execute("NOTIFY orders").unwrap();
    assert!(listener.notifications().is_empty(), "nothing before commit");
    sender.execute("COMMIT").unwrap();
    assert_eq!(listener.notifications(), vec![note("orders", "a", sender.id()), note("orders", "", sender.id())]);
    sender.execute("BEGIN").unwrap();
    sender.execute("NOTIFY orders, 'lost'").unwrap();
    sender.execute("ROLLBACK").unwrap();
    assert!(listener.notifications().is_empty());

    // A failed statement's notification is dropped; the transaction's others survive.
    sender.execute("BEGIN").unwrap();
    sender.execute("NOTIFY orders, 'kept'").unwrap();
    assert!(sender.execute("NOTIFY orders, 1 / 0").is_err());
    assert!(sender.execute("NOTIFY orders, 42").unwrap_err().to_string().contains("must be TEXT"));
    sender.execute("COMMIT").unwrap();
    assert_eq!(listener.notifications(), vec![note("orders", "kept", sender.id())]);

    // A session hears itself; LISTEN in a transaction counts from its commit.
    sender.execute("BEGIN").unwrap();
    sender.execute("LISTEN orders").unwrap();
    assert!(sender.listening().is_empty());
    sender.execute("NOTIFY orders, 'self'").unwrap();
    sender.execute("COMMIT").unwrap();
    assert_eq!(sender.notifications(), vec![note("orders", "self", sender.id())]);
    assert_eq!(listener.notifications(), vec![note("orders", "self", sender.id())]);

    listener.execute("UNLISTEN *").unwrap();
    sender.execute("NOTIFY orders, 'after'").unwrap();
    assert!(listener.notifications().is_empty());
    assert!(db.execute(&format!("NOTIFY orders, '{}'", "x".repeat(8001))).is_err());

    // Another thread can wait for notifications while the session keeps working.
    listener.execute("LISTEN jobs").unwrap();
    let receiver = listener.notification_receiver();
    let waiter = thread::spawn(move || receiver.recv_timeout(Duration::from_secs(10)));
    db.execute("NOTIFY jobs, 'wake up'").unwrap();
    assert_eq!(waiter.join().unwrap().map(|n| n.payload), Some("wake up".to_string()));
}

#[test]
fn integer_primary_keys_stay_unique_without_a_lookup_for_new_ids() {
    // New ids skip the existence check, trusting the row-id counter, so
    // every way an id can arrive must still be caught when it repeats.
    let (dir, db) = open("fresh-ids");
    run(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
    let dup = |db: &Database, sql: &str| assert!(error(db, sql).contains("UNIQUE constraint failed: t.id"), "{sql}");
    run(&db, "INSERT INTO t VALUES (10, 'a')");
    run(&db, "INSERT INTO t VALUES (5, 'b')"); // below the counter: checked, free
    dup(&db, "INSERT INTO t VALUES (5, 'c')");
    dup(&db, "INSERT INTO t VALUES (10, 'c')");
    run(&db, "INSERT INTO t (v) VALUES ('auto')"); // 11
    dup(&db, "INSERT INTO t VALUES (11, 'c')");

    // Within one transaction, and against rows it deleted.
    let mut s = db.session();
    s.execute("BEGIN").unwrap();
    s.execute("INSERT INTO t VALUES (20, 'x')").unwrap();
    assert!(s.execute("INSERT INTO t VALUES (20, 'y')").is_err());
    s.execute("DELETE FROM t WHERE id = 5").unwrap();
    s.execute("INSERT INTO t VALUES (5, 'again')").unwrap();
    s.execute("COMMIT").unwrap();

    // An UPDATE that moves a row to a new, high id moves the counter too.
    run(&db, "UPDATE t SET id = 100 WHERE id = 10");
    dup(&db, "INSERT INTO t VALUES (100, 'c')");
    run(&db, "INSERT INTO t (v) VALUES ('after')");
    assert_eq!(query(&db, "SELECT id FROM t WHERE v = 'after'"), vec![vec![Value::Integer(101)]]);

    // A rolled-back insert leaves its id free, and the counter survives reopening.
    s.execute("BEGIN").unwrap();
    s.execute("INSERT INTO t VALUES (500, 'gone')").unwrap();
    s.execute("ROLLBACK").unwrap();
    drop(s);
    run(&db, "INSERT INTO t VALUES (500, 'kept')");
    drop(db);
    let db = Database::open(&dir.0).unwrap();
    dup(&db, "INSERT INTO t VALUES (500, 'c')");
    dup(&db, "INSERT INTO t VALUES (101, 'c')");
    let ids = query(&db, "SELECT id FROM t ORDER BY id");
    assert_eq!(ids, rows![[5], [11], [20], [100], [101], [500]]);

    // Tables with only the hidden row id never collide.
    run(&db, "CREATE TABLE h (v INTEGER)");
    run(&db, "INSERT INTO h VALUES (1), (1), (1)");
    assert_eq!(query(&db, "SELECT count(*) FROM h"), vec![vec![Value::Integer(3)]]);
}

#[test]
fn query_each_streams_the_same_rows_as_query() {
    let (_dir, db) = open("query-each");
    run(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, n INTEGER)");
    let insert = db.prepare("INSERT INTO t VALUES (?, ?, ?)").unwrap();
    let mut session = db.session();
    for i in 0..500i64 {
        // Names of varying lengths, so a reused string grows and shrinks.
        let name = if i % 7 == 0 { Value::Null } else { Value::Text("x".repeat((i % 13) as usize) + &i.to_string()) };
        session.execute_prepared(&insert, &[Value::Integer(i), name, Value::Integer(i % 10)]).unwrap();
    }
    for sql in [
        "SELECT * FROM t",
        "SELECT id, name FROM t WHERE id >= 100 AND id < 200",
        "SELECT name, id FROM t WHERE n = 3",
        "SELECT id * 2, upper(name) FROM t WHERE name LIKE 'xx%' LIMIT 7 OFFSET 3",
        "SELECT id FROM t LIMIT 0",
        "SELECT n, count(*) FROM t GROUP BY n",
        "SELECT name FROM t ORDER BY name DESC LIMIT 20",
        "SELECT DISTINCT n FROM t",
    ] {
        let prepared = db.prepare(sql).unwrap();
        let mut streamed = Vec::new();
        session
            .query_each(&prepared, &[], |row| {
                streamed.push(row.to_vec());
                true
            })
            .unwrap();
        assert_eq!(streamed, query(&db, sql), "{sql}");
    }
    // Stopping early, inside a transaction that sees its own writes.
    session.execute("BEGIN").unwrap();
    session.execute("INSERT INTO t VALUES (1000, 'mine', 0)").unwrap();
    let all = db.prepare("SELECT id FROM t WHERE n = 0").unwrap();
    let mut seen = Vec::new();
    session
        .query_each(&all, &[], |row| {
            seen.push(row[0].clone());
            seen.len() < 3
        })
        .unwrap();
    assert_eq!(seen, rows![[0], [10], [20]].concat());
    let mut last = None;
    session
        .query_each(&all, &[], |row| {
            last = Some(row[0].clone());
            true
        })
        .unwrap();
    assert_eq!(last, Some(Value::Integer(1000)));
    session.execute("ROLLBACK").unwrap();
}

/// Users and their orders, for subquery tests.
fn shop(db: &Database) {
    run(
        db,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, city TEXT);
         CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER, total REAL);
         INSERT INTO users VALUES (1, 'Ada', 'London'), (2, 'Alan', 'London'), (3, 'Grace', 'NYC'), (4, 'Edsger', NULL);
         INSERT INTO orders VALUES (1, 1, 10.0), (2, 1, 25.5), (3, 3, 7.25), (4, 3, 100.0), (5, 3, 1.0), (6, NULL, 3.0)",
    );
}

#[test]
fn scalar_subqueries() {
    let (_dir, db) = open("subq-scalar");
    shop(&db);
    assert_eq!(query(&db, "SELECT (SELECT max(total) FROM orders)"), rows![[100.0]]);
    assert_eq!(
        query(&db, "SELECT name FROM users WHERE id = (SELECT user_id FROM orders WHERE total = 100.0)"),
        rows![["Grace"]]
    );
    // No rows: NULL. More than one: an error.
    assert_eq!(query(&db, "SELECT (SELECT id FROM orders WHERE total > 1000)"), vec![vec![Value::Null]]);
    assert!(error(&db, "SELECT (SELECT id FROM orders)").contains("more than one row"));
    assert!(error(&db, "SELECT (SELECT id, total FROM orders WHERE id = 1)").contains("one column"));
    // Correlated: each user's order count and largest order.
    assert_eq!(
        query(
            &db,
            "SELECT name, (SELECT count(*) FROM orders o WHERE o.user_id = u.id),
                    (SELECT max(total) FROM orders WHERE user_id = u.id)
             FROM users u ORDER BY id"
        ),
        rows![["Ada", 2, 25.5], ["Alan", 0, NULL], ["Grace", 3, 100.0], ["Edsger", 0, NULL]]
    );
    // In WHERE, comparing with a correlated aggregate.
    assert_eq!(
        query(&db, "SELECT id FROM orders o WHERE total > (SELECT avg(total) FROM orders WHERE user_id = o.user_id)"),
        rows![[2], [4]]
    );
}

#[test]
fn in_and_exists_subqueries() {
    let (_dir, db) = open("subq-in");
    shop(&db);
    assert_eq!(
        query(&db, "SELECT name FROM users WHERE id IN (SELECT user_id FROM orders) ORDER BY id"),
        rows![["Ada"], ["Grace"]]
    );
    // NOT IN with a NULL in the subquery: unknown for everyone, as in SQL.
    assert_eq!(
        query(&db, "SELECT name FROM users WHERE id NOT IN (SELECT user_id FROM orders)"),
        Vec::<Vec<Value>>::new()
    );
    assert_eq!(
        query(
            &db,
            "SELECT name FROM users WHERE id NOT IN (SELECT user_id FROM orders WHERE user_id IS NOT NULL) ORDER BY id"
        ),
        rows![["Alan"], ["Edsger"]]
    );
    // An empty subquery: IN is false, NOT IN true, even for NULL.
    assert_eq!(
        query(&db, "SELECT count(*) FROM users WHERE city NOT IN (SELECT name FROM users WHERE id > 99)"),
        rows![[4]]
    );
    // 1 and 1.0 are equal, as in comparisons.
    assert_eq!(query(&db, "SELECT count(*) FROM orders WHERE total IN (SELECT 10)"), rows![[1]]);
    // Correlated EXISTS and NOT EXISTS.
    assert_eq!(
        query(
            &db,
            "SELECT name FROM users u WHERE EXISTS (SELECT 1 FROM orders WHERE user_id = u.id AND total > 20) ORDER BY id"
        ),
        rows![["Ada"], ["Grace"]]
    );
    assert_eq!(
        query(
            &db,
            "SELECT name FROM users u WHERE NOT EXISTS (SELECT * FROM orders o WHERE o.user_id = u.id) ORDER BY id"
        ),
        rows![["Alan"], ["Edsger"]]
    );
    // Subqueries with parameters.
    let prepared =
        db.prepare("SELECT name FROM users WHERE id IN (SELECT user_id FROM orders WHERE total > ?)").unwrap();
    let rows = db.session().execute_prepared(&prepared, &[Value::Real(50.0)]).unwrap();
    assert_eq!(rows.rows().unwrap().rows, rows![["Grace"]]);
}

#[test]
fn queries_in_from() {
    let (_dir, db) = open("subq-from");
    shop(&db);
    assert_eq!(
        query(
            &db,
            "SELECT t.user_id, t.n FROM (SELECT user_id, count(*) AS n FROM orders GROUP BY user_id) AS t
             WHERE t.n > 1 ORDER BY t.n DESC"
        ),
        rows![[3, 3], [1, 2]]
    );
    // Joined with a table, by alias.
    assert_eq!(
        query(
            &db,
            "SELECT u.name, big.total FROM users u JOIN (SELECT user_id, total FROM orders WHERE total >= 25) big
             ON big.user_id = u.id ORDER BY big.total"
        ),
        rows![["Ada", 25.5], ["Grace", 100.0]]
    );
    assert_eq!(query(&db, "SELECT count(*) FROM (SELECT DISTINCT city FROM users) AS c"), rows![[3]]);
}

#[test]
fn union_intersect_except() {
    let (_dir, db) = open("set-ops");
    run(
        &db,
        "CREATE TABLE a (x INTEGER); CREATE TABLE b (x INTEGER);
         INSERT INTO a VALUES (1), (2), (2), (3), (NULL);
         INSERT INTO b VALUES (2), (3), (3), (4), (NULL)",
    );
    let sorted = |sql: &str| query(&db, &format!("{sql} ORDER BY 1"));
    assert_eq!(sorted("SELECT x FROM a UNION SELECT x FROM b"), rows![[NULL], [1], [2], [3], [4]]);
    assert_eq!(query(&db, "SELECT count(*) FROM (SELECT x FROM a UNION ALL SELECT x FROM b) AS u"), rows![[10]]);
    assert_eq!(sorted("SELECT x FROM a INTERSECT SELECT x FROM b"), rows![[NULL], [2], [3]]);
    assert_eq!(sorted("SELECT x FROM a INTERSECT ALL SELECT x FROM b"), rows![[NULL], [2], [3]]);
    assert_eq!(sorted("SELECT x FROM a EXCEPT SELECT x FROM b"), rows![[1]]);
    assert_eq!(sorted("SELECT x FROM a EXCEPT ALL SELECT x FROM b"), rows![[1], [2]]);
    // INTERSECT binds first: a UNION (b INTERSECT a).
    assert_eq!(sorted("SELECT 9 UNION SELECT x FROM b INTERSECT SELECT x FROM a"), rows![[NULL], [2], [3], [9]]);
    // ORDER BY and LIMIT apply to the whole; parts in parentheses keep their own.
    assert_eq!(query(&db, "SELECT x FROM a UNION SELECT x FROM b ORDER BY x DESC LIMIT 2"), rows![[4], [3]]);
    assert_eq!(
        query(
            &db,
            "(SELECT x FROM a WHERE x IS NOT NULL ORDER BY x LIMIT 1) UNION ALL (SELECT x FROM b ORDER BY x DESC LIMIT 1)"
        ),
        rows![[1], [4]]
    );
    assert_eq!(
        query(&db, "SELECT 'a' AS label, 1 UNION ALL SELECT 'b', 2 ORDER BY label DESC"),
        rows![["b", 2], ["a", 1]]
    );
    assert!(error(&db, "SELECT x FROM a UNION SELECT x, x FROM b").contains("same number of columns"));
    assert!(error(&db, "SELECT x FROM a UNION SELECT x FROM b ORDER BY x + 1").contains("result column"));
    // In INSERT ... SELECT.
    run(&db, "CREATE TABLE c (x INTEGER); INSERT INTO c SELECT x FROM a UNION SELECT x FROM b");
    assert_eq!(query(&db, "SELECT count(*) FROM c"), rows![[5]]);
}

#[test]
fn subqueries_in_writes() {
    let (_dir, db) = open("subq-writes");
    shop(&db);
    run(&db, "UPDATE users SET city = (SELECT city FROM users WHERE id = 1) WHERE city IS NULL");
    assert_eq!(query(&db, "SELECT city FROM users WHERE id = 4"), rows![["London"]]);
    run(&db, "DELETE FROM orders WHERE user_id NOT IN (SELECT id FROM users WHERE city = 'London') OR user_id IS NULL");
    assert_eq!(query(&db, "SELECT id FROM orders ORDER BY id"), rows![[1], [2]]);
    run(
        &db,
        "INSERT INTO orders (user_id, total) VALUES ((SELECT id FROM users WHERE name = 'Alan'), (SELECT sum(total) FROM orders))",
    );
    assert_eq!(query(&db, "SELECT user_id, total FROM orders WHERE id = 7"), rows![[2, 35.5]]);
    // Inside a transaction, subqueries see its own writes.
    let mut s = db.session();
    s.execute("BEGIN").unwrap();
    s.execute("INSERT INTO users VALUES (5, 'Barbara', 'Boston')").unwrap();
    let rows =
        s.execute("SELECT count(*) FROM users WHERE id IN (SELECT id FROM users WHERE city = 'Boston')").unwrap();
    assert_eq!(rows.rows().unwrap().rows, rows![[1]]);
    s.execute("ROLLBACK").unwrap();
}

#[test]
fn alter_table() {
    let (dir, db) = open("alter");
    run(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score INTEGER); CREATE INDEX t_score ON t (score);");
    run(&db, "INSERT INTO t (name, score) VALUES ('a', 10), ('b', 20)");
    let by_name = db.prepare("SELECT id FROM t WHERE name = 'b'").unwrap();
    db.session().execute_prepared(&by_name, &[]).unwrap();

    // ADD COLUMN: existing rows get the default (or NULL without one).
    run(&db, "ALTER TABLE t ADD COLUMN level INTEGER NOT NULL DEFAULT 1");
    run(&db, "ALTER TABLE t ADD note TEXT");
    run(&db, "INSERT INTO t (name, score, level, note) VALUES ('c', 30, 5, 'new')");
    assert_eq!(
        query(&db, "SELECT name, level, note FROM t ORDER BY id"),
        rows![["a", 1, NULL], ["b", 1, NULL], ["c", 5, "new"]]
    );
    assert!(error(&db, "ALTER TABLE t ADD COLUMN must INTEGER NOT NULL").contains("need a DEFAULT"));
    assert!(error(&db, "ALTER TABLE t ADD COLUMN name TEXT").contains("already exists"));
    assert!(error(&db, "INSERT INTO t (name, level) VALUES ('x', NULL)").contains("NOT NULL"));

    // UNIQUE builds an index, and fails on duplicates.
    run(&db, "ALTER TABLE t ADD COLUMN code TEXT UNIQUE");
    run(&db, "UPDATE t SET code = name");
    assert!(error(&db, "INSERT INTO t (name, code) VALUES ('d', 'a')").contains("UNIQUE"));
    assert!(error(&db, "ALTER TABLE t ADD COLUMN flag INTEGER UNIQUE DEFAULT 0").contains("UNIQUE"));

    // RENAME COLUMN and RENAME TO.
    run(&db, "ALTER TABLE t RENAME COLUMN score TO points; ALTER TABLE t RENAME TO items");
    assert!(error(&db, "SELECT * FROM t").contains("no such table"));
    assert_eq!(query(&db, "SELECT name FROM items WHERE points = 20"), rows![["b"]]);
    let explain = query(&db, "EXPLAIN SELECT name FROM items WHERE points = 20");
    assert!(format!("{explain:?}").contains("t_score"), "{explain:?}");
    // The prepared statement re-plans against the new schema (and fails, since t is gone).
    assert!(db.session().execute_prepared(&by_name, &[]).is_err());

    // DROP COLUMN: indexes on it go, the others keep working.
    run(&db, "ALTER TABLE items DROP COLUMN name");
    assert_eq!(
        query(&db, "SELECT id, points, level FROM items WHERE points >= 20 ORDER BY id"),
        rows![[2, 20, 1], [3, 30, 5]]
    );
    run(&db, "ALTER TABLE items DROP COLUMN points");
    assert_eq!(query(&db, "SELECT count(*) FROM items WHERE level = 1"), rows![[2]]);
    assert!(error(&db, "ALTER TABLE items DROP COLUMN id").contains("primary key"));
    assert!(error(&db, "ALTER TABLE items DROP COLUMN nope").contains("no such column"));

    // All of it survives reopening.
    drop(db);
    let db = Database::open(&dir.0).unwrap();
    assert_eq!(
        query(&db, "SELECT id, level, note, code FROM items ORDER BY id"),
        rows![[1, 1, NULL, "a"], [2, 1, NULL, "b"], [3, 5, "new", "c"]]
    );
    assert_eq!(
        db.schema("items").unwrap().unwrap(),
        "CREATE TABLE items (id INTEGER PRIMARY KEY, level INTEGER NOT NULL DEFAULT 1, note TEXT, code TEXT);\n\
         CREATE UNIQUE INDEX t_code_key ON items (code);"
    );
}

#[test]
fn upsert() {
    let (_dir, db) = open("upsert");
    run(&db, "CREATE TABLE kv (k TEXT PRIMARY KEY, v INTEGER, hits INTEGER DEFAULT 0)");
    run(&db, "INSERT INTO kv (k, v) VALUES ('a', 1), ('b', 2)");

    // DO NOTHING skips taken keys and inserts the rest.
    let result = db.execute("INSERT INTO kv (k, v) VALUES ('a', 10), ('c', 3) ON CONFLICT DO NOTHING").unwrap();
    assert_eq!(result.to_string(), "INSERT 1");
    run(&db, "INSERT OR IGNORE INTO kv (k, v) VALUES ('b', 20)");
    assert_eq!(query(&db, "SELECT k, v FROM kv ORDER BY k"), rows![["a", 1], ["b", 2], ["c", 3]]);

    // DO UPDATE sees the stored row and the proposed one as excluded.
    run(
        &db,
        "INSERT INTO kv (k, v) VALUES ('a', 5), ('d', 4) ON CONFLICT (k) DO UPDATE SET v = kv.v + excluded.v, hits = hits + 1",
    );
    assert_eq!(
        query(&db, "SELECT k, v, hits FROM kv ORDER BY k"),
        rows![["a", 6, 1], ["b", 2, 0], ["c", 3, 0], ["d", 4, 0]]
    );
    // ... unless its WHERE rejects the row.
    run(
        &db,
        "INSERT INTO kv (k, v) VALUES ('a', 100), ('b', 100) ON CONFLICT (k) DO UPDATE SET v = excluded.v WHERE kv.v < 5",
    );
    assert_eq!(query(&db, "SELECT k, v FROM kv WHERE k IN ('a', 'b') ORDER BY k"), rows![["a", 6], ["b", 100]]);
    // MySQL's spelling.
    run(&db, "INSERT INTO kv (k, v) VALUES ('c', 30) ON DUPLICATE KEY UPDATE v = VALUES(v) + 1");
    assert_eq!(query(&db, "SELECT v FROM kv WHERE k = 'c'"), rows![[31]]);

    // Unique indexes conflict too, and the target picks which ones count.
    run(&db, "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT UNIQUE, name TEXT)");
    run(&db, "INSERT INTO users (email, name) VALUES ('x@a', 'X'), ('y@a', 'Y')");
    run(
        &db,
        "INSERT INTO users (email, name) VALUES ('x@a', 'X2') ON CONFLICT (email) DO UPDATE SET name = excluded.name",
    );
    assert_eq!(query(&db, "SELECT id, email, name FROM users ORDER BY id"), rows![[1, "x@a", "X2"], [2, "y@a", "Y"]]);
    assert_eq!(query(&db, "SELECT name FROM users WHERE email = 'x@a'"), rows![["X2"]]);
    // A conflict on a constraint the target doesn't name still fails.
    assert!(
        error(&db, "INSERT INTO users (id, email) VALUES (1, 'z@a') ON CONFLICT (email) DO NOTHING").contains("UNIQUE")
    );
    assert!(error(&db, "INSERT INTO users (name) VALUES ('n') ON CONFLICT (name) DO NOTHING").contains("matches no"));
    // An update that moves onto another row's unique value fails.
    assert!(
        error(&db, "INSERT INTO users (id, email) VALUES (2, 'q@a') ON CONFLICT (id) DO UPDATE SET email = 'x@a'")
            .contains("UNIQUE")
    );
    assert_eq!(query(&db, "SELECT count(*) FROM users"), rows![[2]]);
}

#[test]
fn returning() {
    let (_dir, db) = open("returning");
    run(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score INTEGER DEFAULT 0)");
    // Auto-assigned ids and defaults come back.
    let rows = query(&db, "INSERT INTO t (name) VALUES ('a'), ('b') RETURNING *");
    assert_eq!(rows, rows![[1, "a", 0], [2, "b", 0]]);
    let result =
        db.query("INSERT INTO t (name, score) VALUES ('c', 5) RETURNING id, upper(name) AS shout", &[]).unwrap();
    assert_eq!(result.columns, vec!["id", "shout"]);
    assert_eq!(result.rows, rows![[3, "C"]]);

    assert_eq!(
        query(&db, "UPDATE t SET score = score + 10 WHERE id <= 2 RETURNING id, score"),
        rows![[1, 10], [2, 10]]
    );
    assert_eq!(query(&db, "DELETE FROM t WHERE score = 10 RETURNING name"), rows![["a"], ["b"]]);
    assert_eq!(query(&db, "SELECT name FROM t"), rows![["c"]]);

    // An upsert returns the rows it inserted or updated, not the ones it skipped.
    let rows = query(
        &db,
        "INSERT INTO t (id, name) VALUES (3, 'c2'), (4, 'd') ON CONFLICT (id) DO UPDATE SET name = excluded.name RETURNING id, name",
    );
    assert_eq!(rows, rows![[3, "c2"], [4, "d"]]);
    assert!(query(&db, "INSERT INTO t (id, name) VALUES (3, 'x') ON CONFLICT DO NOTHING RETURNING id").is_empty());
    assert!(error(&db, "DELETE FROM t RETURNING nope").contains("no such column"));
}

#[test]
fn dates_and_times() {
    let (dir, db) = open("dates");
    run(
        &db,
        "CREATE TABLE events (id INTEGER PRIMARY KEY, day DATE, at TIMESTAMP, starts TIME,
             created TIMESTAMP DEFAULT CURRENT_TIMESTAMP, logged DATE DEFAULT CURRENT_DATE);
         CREATE INDEX events_at ON events (at)",
    );
    // Values are checked and stored in canonical form, in UTC.
    run(
        &db,
        "INSERT INTO events (day, at, starts) VALUES
             ('2024-03-15', '2024-03-15T09:30:00Z', '09:30'),
             ('2024-02-29', '2024-02-29 23:15:00.5+02:00', '21:15:00'),
             (DATE '2025-01-01', TIMESTAMP '2025-01-01', TIME '00:00:01')",
    );
    assert_eq!(
        query(&db, "SELECT day, at, starts FROM events ORDER BY at"),
        rows![
            ["2024-02-29", "2024-02-29 21:15:00.5", "21:15:00"],
            ["2024-03-15", "2024-03-15 09:30:00", "09:30:00"],
            ["2025-01-01", "2025-01-01 00:00:00", "00:00:01"],
        ]
    );
    assert!(error(&db, "INSERT INTO events (day) VALUES ('2023-02-29')").contains("cannot store"));
    assert!(error(&db, "INSERT INTO events (at) VALUES ('noon')").contains("cannot store"));
    assert!(error(&db, "INSERT INTO events (day) VALUES (20240101)").contains("cannot store"));
    assert!(error(&db, "SELECT CAST('13:00' AS DATE)").contains("cannot cast"));

    // Comparisons and ranges work on the text, and can use an index.
    assert_eq!(query(&db, "SELECT id FROM events WHERE at >= '2024-03-01' AND at < '2024-04-01'"), rows![[1]]);
    let explain = query(&db, "EXPLAIN SELECT id FROM events WHERE at BETWEEN '2024-01-01' AND '2024-12-31'");
    assert!(format!("{explain:?}").contains("events_at"), "{explain:?}");

    // Clock defaults: every row of one statement gets the same time.
    let rows = query(&db, "SELECT DISTINCT created, logged FROM events");
    assert_eq!(rows.len(), 1, "{rows:?}");
    let created = rows[0][0].to_string();
    assert!(created.len() >= 19, "{created}");
    assert_eq!(rows[0][1].to_string(), created[..10]);
    assert!(db.schema("events").unwrap().unwrap().contains("created TIMESTAMP DEFAULT CURRENT_TIMESTAMP"));
    assert_eq!(query(&db, "SELECT now() = current_timestamp, current_date = date('now')"), rows![[true, true]]);

    // Arithmetic and functions.
    assert_eq!(
        query(
            &db,
            "SELECT day + INTERVAL '1 month', at - INTERVAL '30 minutes', starts + INTERVAL 3 HOUR,
                    date_add(day, '2 days'), date_sub(day, INTERVAL '1 year')
             FROM events WHERE id = 2"
        ),
        rows![["2024-03-29", "2024-02-29 20:45:00.5", "00:15:00", "2024-03-02", "2023-02-28"]]
    );
    assert_eq!(
        query(
            &db,
            "SELECT EXTRACT(year FROM at), date_part('month', at), extract(dow FROM day), extract(second FROM at),
                    date_trunc('month', at), strftime('%d/%m/%Y %H:%M', at), datediff('2024-03-15', day)
             FROM events WHERE id = 2"
        ),
        rows![[2024, 2, 4, 0.5, "2024-02-01 00:00:00", "29/02/2024 21:15", 15]]
    );
    assert_eq!(
        query(
            &db,
            "SELECT date('2024-03-15 10:00', 'start of month', '+1 month', '-1 day'), time('2024-03-15 10:20:30'),
                    datetime(0, 'unixepoch'), unixepoch('1970-01-02'), julianday('2000-01-01 12:00')"
        ),
        rows![["2024-03-31", "10:20:30", "1970-01-01 00:00:00", 86400, 2_451_545.0]]
    );
    assert_eq!(
        query(&db, "SELECT count(*), min(day), max(at) FROM events GROUP BY date_trunc('year', at) ORDER BY 2"),
        rows![[2, "2024-02-29", "2024-03-15 09:30:00"], [1, "2025-01-01", "2025-01-01 00:00:00"]]
    );
    assert!(error(&db, "SELECT INTERVAL '3 fortnights'").contains("invalid interval"));
    assert!(error(&db, "SELECT date('2024-01-01', 'localtime')").contains("UTC"));
    assert!(error(&db, "CREATE TABLE bad (n INTEGER DEFAULT CURRENT_TIMESTAMP)").contains("current time"));

    // ALTER TABLE ADD COLUMN with a clock default fills existing rows.
    run(&db, "ALTER TABLE events ADD COLUMN seen TIMESTAMP DEFAULT now()");
    assert_eq!(query(&db, "SELECT count(*) FROM events WHERE seen IS NULL"), rows![[0]]);

    // Types and clock defaults survive a reopen.
    drop(db);
    let db = Database::open(&dir.0).unwrap();
    assert!(error(&db, "INSERT INTO events (day) VALUES ('soon')").contains("DATE"));
    run(&db, "INSERT INTO events (day) VALUES ('2030-06-01')");
    assert_eq!(query(&db, "SELECT count(*) FROM events WHERE created IS NOT NULL AND id = 4"), rows![[1]]);
}

#[test]
fn common_table_expressions() {
    let (_dir, db) = open("ctes");
    shop(&db);
    // A CTE reads like a table; later ones can use earlier ones.
    assert_eq!(
        query(
            &db,
            "WITH spend AS (SELECT user_id, sum(total) AS total FROM orders GROUP BY user_id),
                  big AS (SELECT user_id FROM spend WHERE total > 100)
             SELECT u.name FROM users u JOIN big ON big.user_id = u.id ORDER BY u.name"
        ),
        query(
            &db,
            "SELECT name FROM users WHERE id IN
                 (SELECT user_id FROM orders GROUP BY user_id HAVING sum(total) > 100) ORDER BY name"
        )
    );
    // Column names, use in subqueries, and a CTE named twice.
    assert_eq!(
        query(
            &db,
            "WITH t(a, b) AS (SELECT 1, 2 UNION ALL SELECT 3, 4) SELECT b, a FROM t WHERE a > (SELECT min(a) FROM t)"
        ),
        rows![[4, 3]]
    );
    assert_eq!(query(&db, "WITH t AS (SELECT 1 AS n) SELECT x.n + y.n FROM t x, t y"), rows![[2]]);
    assert_eq!(query(&db, "SELECT x FROM (SELECT 1, 2) AS s (x, y)"), rows![[1]]);
    // A CTE hides a table of the same name.
    assert_eq!(query(&db, "WITH users AS (SELECT 'nobody' AS name) SELECT name FROM users"), rows![["nobody"]]);
    // Inside INSERT ... SELECT and in parentheses.
    run(&db, "CREATE TABLE squares (n INTEGER PRIMARY KEY, sq INTEGER)");
    run(&db, "INSERT INTO squares WITH n(v) AS (SELECT 1 UNION ALL SELECT 2) SELECT v, v * v FROM n");
    assert_eq!(query(&db, "(WITH x AS (SELECT sq FROM squares) SELECT sum(sq) FROM x)"), rows![[5]]);

    assert!(error(&db, "WITH t AS (SELECT 1), t AS (SELECT 2) SELECT * FROM t").contains("more than once"));
    assert!(error(&db, "WITH t(a, b) AS (SELECT 1) SELECT * FROM t").contains("names are given"));
    // Without RECURSIVE, a CTE can't see itself.
    assert!(error(&db, "WITH t AS (SELECT 1 UNION SELECT n FROM t) SELECT * FROM t").contains("no such table"));
}

#[test]
fn recursive_queries() {
    let (_dir, db) = open("recursive");
    assert_eq!(
        query(
            &db,
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 5) SELECT sum(i), count(*) FROM n"
        ),
        rows![[15, 5]]
    );
    // An endless query stops at the LIMIT above it.
    assert_eq!(
        query(&db, "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i * 2 FROM n) SELECT i FROM n LIMIT 4"),
        rows![[1], [2], [4], [8]]
    );
    // Walking a tree, with depth and path.
    run(
        &db,
        "CREATE TABLE staff (id INTEGER PRIMARY KEY, name TEXT, boss INTEGER);
         INSERT INTO staff VALUES (1, 'ceo', NULL), (2, 'cto', 1), (3, 'cfo', 1), (4, 'dev', 2), (5, 'intern', 4)",
    );
    assert_eq!(
        query(
            &db,
            "WITH RECURSIVE chain AS (
                 SELECT id, name, 0 AS depth, name AS path FROM staff WHERE boss IS NULL
                 UNION ALL
                 SELECT s.id, s.name, c.depth + 1, c.path || '/' || s.name FROM staff s JOIN chain c ON s.boss = c.id
             )
             SELECT name, depth, path FROM chain ORDER BY depth, name"
        ),
        rows![
            ["ceo", 0, "ceo"],
            ["cfo", 1, "ceo/cfo"],
            ["cto", 1, "ceo/cto"],
            ["dev", 2, "ceo/cto/dev"],
            ["intern", 3, "ceo/cto/dev/intern"],
        ]
    );
    // UNION drops rows already found, so a cycle ends.
    run(&db, "CREATE TABLE edges (a INTEGER, b INTEGER); INSERT INTO edges VALUES (1, 2), (2, 3), (3, 1), (3, 4)");
    assert_eq!(
        query(
            &db,
            "WITH RECURSIVE reach(n) AS (SELECT 1 UNION SELECT b FROM edges JOIN reach ON a = n)
             SELECT n FROM reach ORDER BY n"
        ),
        rows![[1], [2], [3], [4]]
    );
    // A recursive CTE used beside a plain one, and inside a subquery.
    assert_eq!(
        query(
            &db,
            "WITH RECURSIVE days(d) AS (SELECT DATE '2024-02-27' UNION ALL SELECT d + INTERVAL '1 day' FROM days WHERE d < '2024-03-01'),
                  wanted AS (SELECT d FROM days WHERE d >= '2024-02-29')
             SELECT (SELECT count(*) FROM days), d FROM wanted ORDER BY d"
        ),
        rows![[4, "2024-02-29"], [4, "2024-03-01"]]
    );

    assert!(
        error(&db, "WITH RECURSIVE t(n) AS (SELECT n FROM t UNION SELECT 1) SELECT * FROM t")
            .contains("only the last part")
    );
    assert!(
        error(&db, "WITH RECURSIVE t(n) AS (SELECT 1 EXCEPT SELECT n FROM t) SELECT * FROM t").contains("anchor UNION")
    );
    assert!(
        error(&db, "WITH RECURSIVE t(n) AS (SELECT 1 UNION SELECT 1, 2 FROM t) SELECT * FROM t")
            .contains("same number")
    );
    assert!(
        error(
            &db,
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION SELECT b FROM edges WHERE a IN (SELECT n FROM t)) SELECT * FROM t"
        )
        .contains("not in a subquery")
    );
}

#[test]
fn window_functions() {
    let (_dir, db) = open("windows");
    run(
        &db,
        "CREATE TABLE sales (id INTEGER PRIMARY KEY, region TEXT, rep TEXT, amount INTEGER);
         INSERT INTO sales (region, rep, amount) VALUES
             ('east', 'ann', 100), ('east', 'bob', 300), ('east', 'cat', 300), ('east', 'dan', 50),
             ('west', 'eve', 200), ('west', 'fay', 400), ('west', 'gus', 100)",
    );
    // Ranking within partitions.
    assert_eq!(
        query(
            &db,
            "SELECT rep, row_number() OVER w, rank() OVER w, dense_rank() OVER w, ntile(2) OVER w
             FROM sales WINDOW w AS (PARTITION BY region ORDER BY amount DESC) ORDER BY region, 2"
        ),
        rows![
            ["bob", 1, 1, 1, 1],
            ["cat", 2, 1, 1, 1],
            ["ann", 3, 3, 2, 2],
            ["dan", 4, 4, 3, 2],
            ["fay", 1, 1, 1, 1],
            ["eve", 2, 2, 2, 1],
            ["gus", 3, 3, 3, 2],
        ]
    );
    // Aggregates over the partition, running totals (peers included), and a sliding frame.
    assert_eq!(
        query(
            &db,
            "SELECT rep, sum(amount) OVER (PARTITION BY region), count(*) OVER (),
                    sum(amount) OVER (ORDER BY amount),
                    avg(amount) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING)
             FROM sales ORDER BY id"
        ),
        rows![
            ["ann", 750, 7, 250, 200.0],
            ["bob", 750, 7, 1050, 700.0 / 3.0],
            ["cat", 750, 7, 1050, 650.0 / 3.0],
            ["dan", 750, 7, 50, 550.0 / 3.0],
            ["eve", 700, 7, 450, 650.0 / 3.0],
            ["fay", 700, 7, 1450, 700.0 / 3.0],
            ["gus", 700, 7, 250, 250.0],
        ]
    );
    // Neighbours and frame values.
    assert_eq!(
        query(
            &db,
            "SELECT rep, lag(rep) OVER o, lead(amount, 2, 0) OVER o, first_value(rep) OVER o,
                    last_value(rep) OVER (o ROWS BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING), nth_value(rep, 2) OVER o
             FROM sales WHERE region = 'west' WINDOW o AS (ORDER BY id)"
        ),
        rows![
            ["eve", NULL, 100, "eve", "gus", NULL],
            ["fay", "eve", 0, "eve", "gus", "fay"],
            ["gus", "fay", 0, "eve", "gus", "fay"],
        ]
    );
    // Over grouped rows, in ORDER BY, and with percent_rank/cume_dist.
    assert_eq!(
        query(
            &db,
            "SELECT region, sum(amount) AS total, rank() OVER (ORDER BY sum(amount) DESC), sum(sum(amount)) OVER ()
             FROM sales GROUP BY region ORDER BY rank() OVER (ORDER BY sum(amount) DESC)"
        ),
        rows![["east", 750, 1, 1450], ["west", 700, 2, 1450]]
    );
    assert_eq!(
        query(
            &db,
            "SELECT percent_rank() OVER (ORDER BY amount), cume_dist() OVER (ORDER BY amount) FROM sales WHERE region = 'east' ORDER BY id"
        ),
        rows![[1.0 / 3.0, 0.5], [2.0 / 3.0, 1.0], [2.0 / 3.0, 1.0], [0.0, 0.25]]
    );
    // Filtering on a window value takes a query around it.
    assert_eq!(
        query(
            &db,
            "SELECT region, rep FROM (SELECT region, rep, row_number() OVER (PARTITION BY region ORDER BY amount DESC, rep) AS n FROM sales) AS ranked
             WHERE n = 1 ORDER BY region"
        ),
        rows![["east", "bob"], ["west", "fay"]]
    );
    let explain = query(&db, "EXPLAIN SELECT rank() OVER (ORDER BY amount) FROM sales");
    assert!(format!("{explain:?}").contains("WINDOW"), "{explain:?}");

    assert!(
        error(&db, "SELECT rep FROM sales WHERE rank() OVER (ORDER BY amount) = 1").contains("only in the select list")
    );
    assert!(error(&db, "SELECT sum(rank() OVER (ORDER BY amount)) FROM sales").contains("only in the select list"));
    assert!(error(&db, "SELECT upper(rep) OVER () FROM sales").contains("not a window function"));
    assert!(error(&db, "SELECT rank() OVER nope FROM sales").contains("no such window"));
    assert!(error(&db, "SELECT ntile(0) OVER () FROM sales").contains("positive integer"));
    assert!(error(&db, "SELECT rank(amount) OVER () FROM sales").contains("wrong number of arguments"));
    assert!(
        error(&db, "SELECT sum(amount) OVER (ORDER BY id RANGE 1 PRECEDING) FROM sales")
            .contains("RANGE with an offset")
    );
    assert!(error(&db, "SELECT region, rank() OVER (ORDER BY amount) FROM sales GROUP BY region").contains("GROUP BY"));
}

#[test]
fn check_constraints() {
    let (dir, db) = open("checks");
    run(
        &db,
        "CREATE TABLE items (id INTEGER PRIMARY KEY, price REAL CHECK (price > 0), qty INTEGER DEFAULT 0,
             name TEXT, CONSTRAINT stock CHECK (qty >= 0 AND qty <= 1000), CHECK (length(name) > 1))",
    );
    run(&db, "INSERT INTO items (price, qty, name) VALUES (9.5, 3, 'pen')");
    // NULL is unknown, which passes.
    run(&db, "INSERT INTO items (price, name) VALUES (NULL, NULL)");
    assert!(
        error(&db, "INSERT INTO items (price, name) VALUES (0, 'cup')")
            .contains("CHECK constraint failed: items: price > 0")
    );
    assert!(
        error(&db, "INSERT INTO items (price, qty, name) VALUES (1, -1, 'cup')")
            .contains("CHECK constraint failed: stock")
    );
    assert!(error(&db, "UPDATE items SET qty = 5000 WHERE id = 1").contains("stock"));
    assert!(error(&db, "INSERT INTO items (price, name) VALUES (1, 'x')").contains("length(name) > 1"));
    assert_eq!(query(&db, "SELECT count(*) FROM items WHERE price IS NOT NULL"), rows![[1]]);
    assert!(error(&db, "CREATE TABLE bad (n INTEGER CHECK (sum(n) > 0))").contains("aggregate"));
    assert!(error(&db, "CREATE TABLE bad (n INTEGER CHECK (m > 0))").contains("no such column"));
    assert!(error(&db, "CREATE TABLE bad (n INTEGER CHECK (n > (SELECT 1)))").contains("subquery"));

    // ALTER TABLE keeps checks working, and checks existing rows for new ones.
    run(&db, "ALTER TABLE items RENAME COLUMN price TO cost");
    assert!(error(&db, "INSERT INTO items (cost, name) VALUES (-3, 'cup')").contains("cost > 0"));
    assert!(error(&db, "ALTER TABLE items DROP COLUMN qty").contains("CHECK"));
    assert!(error(&db, "ALTER TABLE items ADD COLUMN weight INTEGER DEFAULT 0 CHECK (weight > 0)").contains("CHECK"));
    run(&db, "ALTER TABLE items ADD COLUMN weight INTEGER DEFAULT 1 CHECK (weight > 0)");

    // Constraints survive a reopen and show in the schema.
    drop(db);
    let db = Database::open(&dir.0).unwrap();
    let schema = db.schema("items").unwrap().unwrap();
    assert!(schema.contains("CONSTRAINT stock CHECK ((qty >= 0) AND (qty <= 1000))"), "{schema}");
    assert!(error(&db, "INSERT INTO items (cost, name, weight) VALUES (1, 'cup', 0)").contains("weight > 0"));
}

#[test]
fn foreign_keys() {
    let (dir, db) = open("fks");
    run(
        &db,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT UNIQUE);
         CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER REFERENCES users ON DELETE CASCADE, total REAL);
         CREATE TABLE notes (id INTEGER PRIMARY KEY, order_id INTEGER, author TEXT,
             FOREIGN KEY (order_id) REFERENCES orders (id) ON DELETE SET NULL,
             CONSTRAINT by_email FOREIGN KEY (author) REFERENCES users (email) ON UPDATE CASCADE);
         INSERT INTO users VALUES (1, 'a@x'), (2, 'b@x');
         INSERT INTO orders VALUES (10, 1, 5.0), (11, 1, 7.5), (12, 2, 1.0);
         INSERT INTO notes VALUES (100, 10, 'a@x'), (101, 12, 'b@x'), (102, NULL, NULL)",
    );
    // References must exist; NULL refers to nothing.
    assert!(
        error(&db, "INSERT INTO orders VALUES (13, 99, 1.0)")
            .contains("FOREIGN KEY constraint failed: orders(user_id) references users(id)")
    );
    assert!(error(&db, "UPDATE orders SET user_id = 3 WHERE id = 10").contains("FOREIGN KEY"));
    assert!(error(&db, "INSERT INTO notes (order_id, author) VALUES (10, 'z@x')").contains("by_email"));
    run(&db, "INSERT INTO orders VALUES (13, NULL, 2.0)");

    // ON UPDATE CASCADE through a unique column; NO ACTION refuses to orphan rows.
    run(&db, "UPDATE users SET email = 'a@y' WHERE id = 1");
    assert_eq!(query(&db, "SELECT author FROM notes WHERE id = 100"), rows![["a@y"]]);
    assert!(error(&db, "UPDATE users SET id = 5 WHERE id = 2").contains("still referenced by orders(user_id)"));

    // ON DELETE CASCADE, which sets off ON DELETE SET NULL further down.
    assert!(error(&db, "DELETE FROM users WHERE id = 1").contains("still referenced by by_email"));
    run(&db, "UPDATE notes SET author = NULL WHERE id = 100");
    run(&db, "DELETE FROM users WHERE id = 1");
    assert_eq!(query(&db, "SELECT id FROM orders ORDER BY id"), rows![[12], [13]]);
    assert_eq!(query(&db, "SELECT id, order_id FROM notes ORDER BY id"), rows![[100, NULL], [101, 12], [102, NULL]]);
    // A failed statement leaves everything as it was.
    assert!(error(&db, "DELETE FROM users").contains("still referenced by by_email"));
    assert_eq!(query(&db, "SELECT count(*) FROM orders"), rows![[2]]);

    // A table referring to itself: rows of one statement may refer to each other.
    run(
        &db,
        "CREATE TABLE staff (id INTEGER PRIMARY KEY, boss INTEGER REFERENCES staff (id) ON DELETE CASCADE ON UPDATE CASCADE);
         INSERT INTO staff VALUES (2, 1), (1, NULL), (3, 2), (4, 3)",
    );
    run(&db, "UPDATE staff SET id = id + 10");
    assert_eq!(query(&db, "SELECT id, boss FROM staff ORDER BY id"), rows![[11, NULL], [12, 11], [13, 12], [14, 13]]);
    run(&db, "DELETE FROM staff WHERE id = 12");
    assert_eq!(query(&db, "SELECT id FROM staff"), rows![[11]]);

    // Upserts and RESTRICT.
    run(
        &db,
        "CREATE TABLE tags (name TEXT PRIMARY KEY); CREATE TABLE tagged (tag TEXT REFERENCES tags ON DELETE RESTRICT)",
    );
    run(&db, "INSERT INTO tags VALUES ('red'); INSERT INTO tagged VALUES ('red')");
    assert!(error(&db, "INSERT INTO tagged VALUES ('blue') ON CONFLICT DO NOTHING").contains("FOREIGN KEY"));
    assert!(error(&db, "DELETE FROM tags").contains("still referenced"));

    // Schema rules.
    assert!(error(&db, "DROP TABLE users").contains("referenced by a foreign key of"));
    assert!(error(&db, "ALTER TABLE orders DROP COLUMN user_id").contains("foreign key"));
    assert!(
        error(&db, "CREATE TABLE bad (x INTEGER REFERENCES orders (total))").contains("not a primary key or unique")
    );
    assert!(error(&db, "CREATE TABLE bad (x INTEGER REFERENCES nowhere)").contains("no such table"));
    assert!(
        error(&db, "CREATE TABLE bad (x INTEGER, y INTEGER, FOREIGN KEY (x, y) REFERENCES users)")
            .contains("references 1")
    );
    run(&db, "ALTER TABLE users RENAME TO people; ALTER TABLE people RENAME COLUMN id TO pid");
    assert!(db.schema("orders").unwrap().unwrap().contains("REFERENCES people (pid) ON DELETE CASCADE"));
    assert!(error(&db, "INSERT INTO orders VALUES (20, 77, 1.0)").contains("references people(pid)"));

    // Survives a reopen.
    drop(db);
    let db = Database::open(&dir.0).unwrap();
    run(&db, "INSERT INTO orders VALUES (20, 2, 1.0)");
    assert!(error(&db, "INSERT INTO orders VALUES (21, 77, 1.0)").contains("FOREIGN KEY"));
    run(&db, "UPDATE notes SET author = NULL; DELETE FROM people WHERE pid = 2");
    assert_eq!(query(&db, "SELECT count(*) FROM orders WHERE user_id IS NOT NULL"), rows![[0]]);
}

#[test]
fn views() {
    let (dir, db) = open("views");
    shop(&db);
    run(
        &db,
        "CREATE VIEW spenders (user_id, spent) AS
             SELECT user_id, sum(total) FROM orders GROUP BY user_id;
         CREATE VIEW big_spenders AS SELECT u.name, s.spent FROM users u JOIN spenders s ON s.user_id = u.id WHERE s.spent > 100",
    );
    // A view reads like a table, sees later changes, and can be used anywhere a table can.
    let direct = query(
        &db,
        "SELECT name, sum(total) FROM users JOIN orders ON orders.user_id = users.id
         GROUP BY name HAVING sum(total) > 100 ORDER BY name",
    );
    assert_eq!(query(&db, "SELECT name, spent FROM big_spenders ORDER BY name"), direct);
    run(&db, "INSERT INTO orders (user_id, total) VALUES (2, 1000)");
    assert_eq!(query(&db, "SELECT name FROM big_spenders ORDER BY name"), rows![["Alan"], ["Grace"]]);
    assert_eq!(
        query(&db, "WITH t AS (SELECT max(spent) AS m FROM spenders) SELECT count(*) FROM spenders, t WHERE spent = m"),
        rows![[1]]
    );
    assert!(error(&db, "INSERT INTO spenders VALUES (1, 2)").contains("is a view"));
    assert!(error(&db, "CREATE VIEW spenders AS SELECT 1").contains("already exists"));
    assert!(error(&db, "CREATE TABLE spenders (x INTEGER)").contains("view named spenders"));
    assert!(error(&db, "CREATE VIEW users AS SELECT 1").contains("table named users"));
    assert!(error(&db, "CREATE VIEW bad (a, b) AS SELECT 1").contains("names are given"));
    assert!(error(&db, "CREATE VIEW bad AS SELECT * FROM nowhere").contains("no such table"));
    // OR REPLACE, but not into a loop.
    assert!(error(&db, "CREATE OR REPLACE VIEW spenders AS SELECT * FROM big_spenders").contains("refer to itself"));
    run(
        &db,
        "CREATE OR REPLACE VIEW spenders (user_id, spent) AS SELECT user_id, count(*) FROM orders GROUP BY user_id",
    );
    assert!(query(&db, "SELECT count(*) FROM big_spenders WHERE spent > 100") == rows![[0]]);

    // Survives a reopen, and shows in the schema.
    drop(db);
    let db = Database::open(&dir.0).unwrap();
    assert_eq!(
        db.schema("spenders").unwrap().unwrap(),
        "CREATE VIEW spenders (user_id, spent) AS SELECT user_id, count(*) FROM orders GROUP BY user_id;"
    );
    assert_eq!(db.view_names().unwrap(), vec!["big_spenders", "spenders"]);
    run(&db, "DROP VIEW big_spenders; DROP VIEW IF EXISTS big_spenders");
    assert!(error(&db, "SELECT * FROM big_spenders").contains("no such table"));
    assert!(error(&db, "DROP VIEW big_spenders").contains("no such view"));
}

#[test]
fn savepoints() {
    let (_dir, db) = open("savepoints");
    run(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
    let mut s = db.session();
    assert!(s.execute("SAVEPOINT a").unwrap_err().to_string().contains("need a transaction"));
    for sql in [
        "BEGIN",
        "INSERT INTO t (v) VALUES ('kept')",
        "SAVEPOINT a",
        "INSERT INTO t (v) VALUES ('undone')",
        "SAVEPOINT b",
        "UPDATE t SET v = 'changed'",
        "ROLLBACK TO b",
    ] {
        s.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    let rows = s.query("SELECT v FROM t ORDER BY id", &[]).unwrap().rows;
    assert_eq!(rows, rows![["kept"], ["undone"]]);
    // ROLLBACK TO keeps the savepoint; savepoints after it are gone.
    s.execute("INSERT INTO t (v) VALUES ('x')").unwrap();
    s.execute("ROLLBACK TO SAVEPOINT a").unwrap();
    assert!(s.execute("RELEASE b").unwrap_err().to_string().contains("no such savepoint"));
    s.execute("INSERT INTO t (v) VALUES ('after')").unwrap();
    s.execute("ROLLBACK TO a").unwrap();
    s.execute("INSERT INTO t (v) VALUES ('last')").unwrap();
    s.execute("RELEASE SAVEPOINT a").unwrap();
    assert!(s.execute("ROLLBACK TO a").unwrap_err().to_string().contains("no such savepoint"));
    s.execute("COMMIT").unwrap();
    assert_eq!(query(&db, "SELECT v FROM t ORDER BY id"), rows![["kept"], ["last"]]);
    // A whole ROLLBACK still undoes everything, savepoints or not.
    s.execute("BEGIN").unwrap();
    s.execute("SAVEPOINT a").unwrap();
    s.execute("DELETE FROM t").unwrap();
    s.execute("RELEASE a").unwrap();
    s.execute("ROLLBACK").unwrap();
    assert_eq!(query(&db, "SELECT count(*) FROM t"), rows![[2]]);
}

#[test]
fn more_functions() {
    let (_dir, db) = open("functions");
    run(
        &db,
        "CREATE TABLE m (id INTEGER PRIMARY KEY, grp TEXT, x REAL, ok BOOLEAN, name TEXT);
         INSERT INTO m (grp, x, ok, name) VALUES
             ('a', 2, true, 'ann'), ('a', 4, true, NULL), ('a', 4, false, 'cat'), ('a', 5, true, 'dan'),
             ('b', 5, true, 'eve'), ('b', 7, true, 'fay'), ('b', 9, true, 'gus'), ('c', NULL, NULL, NULL)",
    );
    // Aggregates: total, group_concat/string_agg, bool_and/bool_or, variance and stddev.
    assert_eq!(
        query(
            &db,
            "SELECT grp, total(x), group_concat(name), string_agg(name, '|'), bool_and(ok), bool_or(NOT ok),
                    var_pop(x), stddev_samp(x)
             FROM m GROUP BY grp ORDER BY grp"
        ),
        rows![
            ["a", 15.0, "ann,cat,dan", "ann|cat|dan", false, true, 1.1875, 1.2583057392117916],
            ["b", 21.0, "eve,fay,gus", "eve|fay|gus", true, false, 8.0 / 3.0, 2.0],
            ["c", 0.0, NULL, NULL, NULL, NULL, NULL, NULL],
        ]
    );
    // FILTER, in plain aggregates (batched or not) and window aggregates.
    assert_eq!(
        query(&db, "SELECT count(*) FILTER (WHERE x > 4), sum(x) FILTER (WHERE ok), count(*) FROM m"),
        rows![[4, 32.0, 8]]
    );
    assert_eq!(
        query(&db, "SELECT grp, count(*) FILTER (WHERE NOT ok) FROM m GROUP BY grp ORDER BY grp"),
        rows![["a", 1], ["b", 0], ["c", 0]]
    );
    assert_eq!(
        query(&db, "SELECT id, sum(x) FILTER (WHERE grp = 'b') OVER (ORDER BY id) FROM m WHERE id >= 4 ORDER BY id"),
        rows![[4, NULL], [5, 5.0], [6, 12.0], [7, 21.0], [8, 21.0]]
    );
    assert!(error(&db, "SELECT upper(name) FILTER (WHERE ok) FROM m").contains("only for aggregates"));
    assert!(error(&db, "SELECT string_agg(name, grp) FROM m").contains("text literal"));

    // NULLS FIRST / LAST, in ORDER BY and windows.
    assert_eq!(query(&db, "SELECT id FROM m WHERE id >= 6 ORDER BY x"), rows![[8], [6], [7]]);
    assert_eq!(query(&db, "SELECT id FROM m WHERE id >= 6 ORDER BY x NULLS LAST"), rows![[6], [7], [8]]);
    assert_eq!(query(&db, "SELECT id FROM m WHERE id >= 6 ORDER BY x DESC NULLS FIRST"), rows![[8], [7], [6]]);
    assert_eq!(
        query(&db, "SELECT id, row_number() OVER (ORDER BY name NULLS FIRST) FROM m WHERE id <= 3 ORDER BY id"),
        rows![[1, 2], [2, 1], [3, 3]]
    );

    // Scalar functions, including SQL's keyword forms.
    assert_eq!(
        query(
            &db,
            "SELECT position('lo' IN 'hello'), substring('hello' FROM 2 FOR 3), trim(LEADING 'x' FROM 'xxhix'),
                    trim(BOTH FROM '  hi  '), trim('-' FROM '--a--'), concat('a', NULL, 1), lpad('5', 3, '0'),
                    initcap('hello world'), split_part('a.b.c', '.', 2), iif(1 > 2, 'yes', 'no'),
                    greatest(3, NULL, 7), least(2.5, 1), ceil(1.2), floor(-1.2), round(sqrt(16)), power(2, 10),
                    mod(10, 3), abs(sign(-3)), reverse('abc'), repeat('ab', 3), starts_with('spiro', 'sp')"
        ),
        rows![[
            4,
            "ell",
            "hix",
            "hi",
            "a",
            "a1",
            "005",
            "Hello World",
            "b",
            "no",
            7,
            1,
            2.0,
            -2.0,
            4.0,
            1024.0,
            1,
            1,
            "cba",
            "ababab",
            true
        ]]
    );
    assert!(error(&db, "SELECT sqrt(-1)").contains("negative"));
    assert!(error(&db, "SELECT nosuch(1)").contains("no such function"));
}
