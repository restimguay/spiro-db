//! Catalog tables that describe the database, for tools and ORMs:
//! `information_schema.tables`, `.columns` and `.views`, and SQLite's
//! `sqlite_master` (also `sqlite_schema`).

use super::catalog;
use super::txn::Txn;
use super::value::Value;
use crate::error::Result;

/// The schema every table is in.
const SCHEMA: &str = "main";

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
