//! Catalog tables that describe the database, for tools and ORMs:
//! `information_schema.tables`, `.columns` and `.views`, and SQLite's
//! `sqlite_master` (also `sqlite_schema`).

use super::catalog::{self, TableSchema};
use super::txn::Txn;
use super::value::Value;
use crate::error::Result;

/// The schema every table is in.
const SCHEMA: &str = "main";

/// A table's constraint, as the catalog tables show it.
struct Constraint {
    name: String,
    kind: &'static str,
    columns: Vec<String>,
    /// For a foreign key: the table and columns it refers to.
    references: Option<(String, Vec<String>)>,
}

/// Every table's constraints: primary key, unique indexes, foreign keys and checks.
fn constraints(txn: &Txn) -> Result<Vec<(String, Vec<Constraint>)>> {
    let mut out = Vec::new();
    for table in catalog::list_tables(txn)? {
        let name = |c: usize| table.columns[c].name.clone();
        let mut list = Vec::new();
        if !table.primary_key.is_empty() {
            list.push(Constraint {
                name: format!("{}_pkey", table.name),
                kind: "PRIMARY KEY",
                columns: table.primary_key.iter().map(|&c| name(c)).collect(),
                references: None,
            });
        }
        for index in catalog::indexes_of(txn, &table.name)?.iter().filter(|i| i.unique && i.is_plain()) {
            list.push(Constraint {
                name: index.name.clone(),
                kind: "UNIQUE",
                columns: index.columns.iter().map(|&c| name(c)).collect(),
                references: None,
            });
        }
        for (i, fk) in table.foreign_keys.iter().enumerate() {
            list.push(Constraint {
                name: table.foreign_key_name(i),
                kind: "FOREIGN KEY",
                columns: fk.columns.clone(),
                references: Some((fk.table.clone(), fk.references.clone())),
            });
        }
        for i in 0..table.checks.len() {
            list.push(Constraint { name: table.check_name(i), kind: "CHECK", columns: Vec::new(), references: None });
        }
        out.push((table.name.clone(), list));
    }
    Ok(out)
}

/// The constraint name of `table`'s primary key or unique index on `columns`.
fn key_name(txn: &Txn, table: &TableSchema, columns: &[String]) -> Result<Value> {
    let named = |positions: &[usize]| {
        let mut names: Vec<&str> = positions.iter().map(|&c| table.columns[c].name.as_str()).collect();
        let mut wanted: Vec<&str> = columns.iter().map(String::as_str).collect();
        names.sort_unstable();
        wanted.sort_unstable();
        names == wanted
    };
    if named(&table.primary_key) {
        return Ok(Value::Text(format!("{}_pkey", table.name)));
    }
    let indexes = catalog::indexes_of(txn, &table.name)?;
    Ok(indexes
        .iter()
        .find(|i| i.unique && i.is_plain() && named(&i.columns))
        .map_or(Value::Null, |i| Value::Text(i.name.clone())))
}

/// A catalog table's column names and rows.
type Contents = (Vec<String>, Vec<Vec<Value>>);

/// The columns and rows of the catalog table called `name`, if it is one.
pub fn table(txn: &Txn, name: &str) -> Result<Option<Contents>> {
    let text = |s: &str| Value::Text(s.to_string());
    let (columns, rows): (&[&str], Vec<Vec<Value>>) = match name {
        "information_schema.tables" => {
            let mut rows = Vec::new();
            for table in catalog::list_tables(txn)? {
                rows.push(vec![text(SCHEMA), Value::Text(table.name), text("BASE TABLE")]);
            }
            for view in catalog::list_views(txn)? {
                rows.push(vec![text(SCHEMA), Value::Text(view.name), text("VIEW")]);
            }
            (&["table_schema", "table_name", "table_type"], rows)
        }
        "information_schema.columns" => {
            let mut rows = Vec::new();
            for table in catalog::list_tables(txn)? {
                for (i, column) in table.columns.iter().enumerate() {
                    let default = match (&column.default, column.default_now) {
                        (Some(v), _) => Value::Text(v.literal()),
                        (None, true) => text("CURRENT_TIMESTAMP"),
                        (None, false) => Value::Null,
                    };
                    rows.push(vec![
                        text(SCHEMA),
                        text(&table.name),
                        text(&column.name),
                        Value::Integer(i as i64 + 1),
                        Value::Text(column.ty.to_string()),
                        text(if column.not_null { "NO" } else { "YES" }),
                        default,
                        Value::Boolean(table.primary_key.contains(&i)),
                    ]);
                }
            }
            let columns: &[&str] = &[
                "table_schema",
                "table_name",
                "column_name",
                "ordinal_position",
                "data_type",
                "is_nullable",
                "column_default",
                "is_primary_key",
            ];
            (columns, rows)
        }
        "information_schema.table_constraints" => {
            let mut rows = Vec::new();
            for (table, constraints) in constraints(txn)? {
                for c in constraints {
                    rows.push(vec![text(SCHEMA), Value::Text(c.name), text(SCHEMA), text(&table), text(c.kind)]);
                }
            }
            let columns: &[&str] =
                &["constraint_schema", "constraint_name", "table_schema", "table_name", "constraint_type"];
            (columns, rows)
        }
        "information_schema.key_column_usage" => {
            let mut rows = Vec::new();
            for (table, constraints) in constraints(txn)? {
                for c in constraints.into_iter().filter(|c| c.kind != "CHECK") {
                    for (i, column) in c.columns.iter().enumerate() {
                        let (ref_table, ref_column) = match &c.references {
                            Some((t, cols)) => {
                                (Value::Text(t.clone()), cols.get(i).cloned().map_or(Value::Null, Value::Text))
                            }
                            None => (Value::Null, Value::Null),
                        };
                        rows.push(vec![
                            text(SCHEMA),
                            Value::Text(c.name.clone()),
                            text(SCHEMA),
                            text(&table),
                            text(column),
                            Value::Integer(i as i64 + 1),
                            if c.references.is_some() { Value::Integer(i as i64 + 1) } else { Value::Null },
                            ref_table,
                            ref_column,
                        ]);
                    }
                }
            }
            let columns: &[&str] = &[
                "constraint_schema",
                "constraint_name",
                "table_schema",
                "table_name",
                "column_name",
                "ordinal_position",
                "position_in_unique_constraint",
                "referenced_table_name",
                "referenced_column_name",
            ];
            (columns, rows)
        }
        "information_schema.referential_constraints" => {
            let mut rows = Vec::new();
            for table in catalog::list_tables(txn)? {
                for (i, fk) in table.foreign_keys.iter().enumerate() {
                    // The parent's key the foreign key refers to, by its constraint name.
                    let unique = match catalog::get_table(txn, &fk.table)? {
                        Some(parent) => key_name(txn, &parent, &fk.references)?,
                        None => Value::Null,
                    };
                    rows.push(vec![
                        text(SCHEMA),
                        Value::Text(table.foreign_key_name(i)),
                        text(SCHEMA),
                        unique,
                        text("NONE"),
                        Value::Text(fk.on_update.to_string()),
                        Value::Text(fk.on_delete.to_string()),
                        text(&table.name),
                        text(&fk.table),
                    ]);
                }
            }
            let columns: &[&str] = &[
                "constraint_schema",
                "constraint_name",
                "unique_constraint_schema",
                "unique_constraint_name",
                "match_option",
                "update_rule",
                "delete_rule",
                "table_name",
                "referenced_table_name",
            ];
            (columns, rows)
        }
        "information_schema.check_constraints" => {
            let mut rows = Vec::new();
            for table in catalog::list_tables(txn)? {
                for (i, check) in table.checks.iter().enumerate() {
                    rows.push(vec![
                        text(SCHEMA),
                        Value::Text(table.check_name(i)),
                        Value::Text(check.expr.to_string()),
                    ]);
                }
            }
            (&["constraint_schema", "constraint_name", "check_clause"], rows)
        }
        "information_schema.views" => {
            let views = catalog::list_views(txn)?;
            let rows = views.into_iter().map(|v| vec![text(SCHEMA), Value::Text(v.name), Value::Text(v.sql)]).collect();
            (&["table_schema", "table_name", "view_definition"], rows)
        }
        "sqlite_master" | "sqlite_schema" => {
            let mut rows = Vec::new();
            for table in catalog::list_tables(txn)? {
                let sql = table.to_sql();
                rows.push(vec![
                    text("table"),
                    text(&table.name),
                    text(&table.name),
                    Value::Integer(0),
                    Value::Text(sql),
                ]);
                for index in catalog::indexes_of(txn, &table.name)?.iter() {
                    let sql = index.to_sql(&table);
                    rows.push(vec![
                        text("index"),
                        text(&index.name),
                        text(&table.name),
                        Value::Integer(0),
                        Value::Text(sql),
                    ]);
                }
            }
            for view in catalog::list_views(txn)? {
                let sql = view.to_sql();
                rows.push(vec![text("view"), text(&view.name), text(&view.name), Value::Integer(0), Value::Text(sql)]);
            }
            (&["type", "name", "tbl_name", "rootpage", "sql"], rows)
        }
        _ => return Ok(None),
    };
    Ok(Some((columns.iter().map(|c| c.to_string()).collect(), rows)))
}
