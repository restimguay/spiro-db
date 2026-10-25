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
