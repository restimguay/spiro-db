//! CHECK constraints and foreign keys: what a row must satisfy before a
//! statement may leave it stored.

use std::sync::Arc;

use super::ast::FkAction;
use super::catalog::{self, IndexSchema, ROW_PREFIX_LEN, TableSchema};
use super::encoding::{decode_row, encode_key, encode_keys, skip_key};
use super::expr::{BExpr, Binder, Env, Scope};
use super::txn::Txn;
use super::value::{Value, sql_error};
use crate::error::{Error, Result};

/// A table's own constraints, bound for evaluation: its CHECKs and the
/// rows its foreign keys must find.
#[derive(Default)]
pub struct Rules {
    checks: Vec<(String, BExpr)>,
    parents: Vec<Parent>,
}

/// A foreign key, from the referencing side.
struct Parent {
    /// The referencing columns, in the order of the parent key's columns.
    columns: Vec<usize>,
    table: Arc<TableSchema>,
    key: ParentKey,
    label: String,
}

/// How a referenced row is found: by its primary key or a unique index.
pub enum ParentKey {
    Primary,
    Unique(Arc<IndexSchema>),
}

fn positions(table: &TableSchema, names: &[String]) -> Result<Vec<usize>> {
    names
        .iter()
        .map(|n| table.column_index(n).ok_or_else(|| sql_error(format!("no such column: {}.{n}", table.name))))
        .collect()
}

impl ParentKey {
    /// The key that the `references` columns of `parent` form, with the
    /// position in `references` of each of the key's columns.
    pub fn of(
        parent: &TableSchema,
        indexes: &[Arc<IndexSchema>],
        references: &[usize],
    ) -> Option<(ParentKey, Vec<usize>)> {
        let order = |key: &[usize]| -> Option<Vec<usize>> {
            if key.len() != references.len() {
                return None;
            }
            key.iter().map(|k| references.iter().position(|r| r == k)).collect()
        };
        if let Some(order) = order(&parent.primary_key) {
            return Some((ParentKey::Primary, order));
        }
        indexes.iter().filter(|i| i.unique).find_map(|i| order(&i.columns).map(|o| (ParentKey::Unique(i.clone()), o)))
    }

    fn columns<'a>(&'a self, table: &'a TableSchema) -> &'a [usize] {
        match self {
            ParentKey::Primary => &table.primary_key,
            ParentKey::Unique(index) => &index.columns,
        }
    }

    /// Whether `table` has a row with these key values, in key order.
    pub fn exists(&self, txn: &Txn, table: &TableSchema, values: &[Value]) -> Result<bool> {
        let mut converted = Vec::with_capacity(values.len());
        for (v, &c) in values.iter().zip(self.columns(table)) {
            match v.clone().assign_to(table.columns[c].ty) {
                Ok(v) if !v.is_null() => converted.push(v),
                _ => return Ok(false),
            }
        }
        match self {
            ParentKey::Primary => {
                let mut key = table.row_prefix();
                encode_keys(&mut key, &converted);
                Ok(txn.get(&key)?.is_some())
            }
            ParentKey::Unique(index) => {
                let (lower, upper) = catalog::values_range(index, &converted);
                txn.any_in(lower, upper)
            }
        }
    }
}

impl Rules {
    pub fn of(txn: &Txn, table: &Arc<TableSchema>) -> Result<Rules> {
        let mut rules = Rules::default();
        if table.checks.is_empty() && table.foreign_keys.is_empty() {
            return Ok(rules);
        }
        let binder = Binder { params: &[], txn: None, outer: None, ctes: None };
        let scope = Scope::of_table(table, &table.name);
        for check in &table.checks {
            let label = match &check.name {
                Some(name) => name.clone(),
                None => format!("{}: {}", table.name, check.expr),
            };
            rules.checks.push((label, binder.bind(&check.expr, &scope, "CHECK")?));
        }
        for fk in &table.foreign_keys {
            let parent = if fk.table == table.name { table.clone() } else { catalog::require_table(txn, &fk.table)? };
            let indexes = catalog::indexes_of(txn, &parent.name)?;
            let (columns, references) = (positions(table, &fk.columns)?, positions(&parent, &fk.references)?);
            let (key, order) = ParentKey::of(&parent, &indexes, &references).ok_or_else(|| {
                sql_error(format!("{}({}) is no longer a primary key or unique", parent.name, fk.references.join(", ")))
            })?;
            let label = fk.name.clone().unwrap_or_else(|| {
                format!(
                    "{}({}) references {}({})",
                    table.name,
                    fk.columns.join(", "),
                    fk.table,
                    fk.references.join(", ")
                )
            });
            rules.parents.push(Parent {
                columns: order.iter().map(|&j| columns[j]).collect(),
                table: parent,
                key,
                label,
            });
        }
        Ok(rules)
    }

    pub fn has_parents(&self) -> bool {
        !self.parents.is_empty()
    }

    /// Checks the row against the CHECK constraints. Unknown (NULL) passes.
    pub fn check(&self, row: &[Value]) -> Result<()> {
        if self.checks.is_empty() {
            return Ok(());
        }
        let env = Env::new(&[], None);
        for (label, check) in &self.checks {
            match check.eval(row, &env)? {
                Value::Boolean(true) | Value::Null => {}
                Value::Boolean(false) => return Err(Error::Constraint(format!("CHECK constraint failed: {label}"))),
                v => return Err(sql_error(format!("CHECK {label} gave {}, not a boolean", v.literal()))),
            }
        }
        Ok(())
    }

    /// Checks that every row the row refers to exists. A reference with a
    /// NULL in it refers to nothing and passes.
    pub fn check_parents(&self, txn: &Txn, row: &[Value]) -> Result<()> {
        for parent in &self.parents {
            let values: Vec<Value> = parent.columns.iter().map(|&c| row[c].clone()).collect();
            if values.iter().any(Value::is_null) {
                continue;
            }
            if !parent.key.exists(txn, &parent.table, &values)? {
                return Err(Error::Constraint(format!("FOREIGN KEY constraint failed: {}", parent.label)));
            }
        }
        Ok(())
    }
}

/// A foreign key, from the referenced side.
pub struct Child {
    pub table: Arc<TableSchema>,
    pub indexes: Arc<[Arc<IndexSchema>]>,
    /// The referencing columns, each beside the referenced column in
    /// `references` it matches.
    pub columns: Vec<usize>,
    pub references: Vec<usize>,
    pub on_delete: FkAction,
    pub on_update: FkAction,
    label: String,
}

/// Every foreign key that refers to `table`.
pub fn children(txn: &Txn, table: &TableSchema) -> Result<Vec<Child>> {
    let mut out = Vec::new();
    for child in catalog::children_of(txn, &table.name)?.iter() {
        let indexes = catalog::indexes_of(txn, &child.name)?;
        for fk in child.foreign_keys.iter().filter(|fk| fk.table == table.name) {
            out.push(Child {
                table: child.clone(),
                indexes: indexes.clone(),
                columns: positions(child, &fk.columns)?,
                references: positions(table, &fk.references)?,
                on_delete: fk.on_delete,
                on_update: fk.on_update,
                label: fk.name.clone().unwrap_or_else(|| format!("{}({})", child.name, fk.columns.join(", "))),
            });
        }
    }
    Ok(out)
}

impl Child {
    pub fn violation(&self, parent: &str) -> Error {
        Error::Constraint(format!(
            "FOREIGN KEY constraint failed: a row of {parent} is still referenced by {}",
            self.label
        ))
    }

    /// The rows (with their keys) that refer to the referenced values
    /// `values`, given in the order of `references`.
    pub fn rows(&self, txn: &Txn, values: &[Value]) -> Result<Vec<(Vec<u8>, Vec<Value>)>> {
        let table = &self.table;
        let mut wanted = Vec::with_capacity(values.len());
        for (v, &c) in values.iter().zip(&self.columns) {
            match v.clone().assign_to(table.columns[c].ty) {
                Ok(v) if !v.is_null() => wanted.push(v),
                _ => return Ok(Vec::new()),
            }
        }
        let width = table.columns.len();
        let mut rows = Vec::new();
        // Values for a key whose first columns are the referencing ones.
        let leading = |key: &[usize]| -> Option<Vec<Value>> {
            let first = key.get(..self.columns.len())?;
            first.iter().map(|k| self.columns.iter().position(|c| c == k).map(|j| wanted[j].clone())).collect()
        };
        if let Some(values) = leading(&table.primary_key) {
            let mut prefix = table.row_prefix();
            encode_keys(&mut prefix, &values);
            let (lower, upper) = catalog::prefix_range(prefix);
            txn.scan(lower, upper, &mut |key, data| {
                rows.push((key.to_vec(), decode_row(data, width)?));
                Ok(true)
            })?;
        } else if let Some((index, values)) = self.indexes.iter().find_map(|i| leading(&i.columns).map(|v| (i, v))) {
            let (lower, upper) = catalog::values_range(index, &values);
            let mut keys = Vec::new();
            txn.scan(lower, upper, &mut |entry, _| {
                // The entry is the indexed values followed by the row's primary key.
                let mut rest = &entry[index.prefix().len()..];
                for _ in &index.columns {
                    skip_key(&mut rest)?;
                }
                let mut key = table.row_prefix();
                key.extend_from_slice(rest);
                keys.push(key);
                Ok(true)
            })?;
            for key in keys {
                if let Some(data) = txn.get(&key)? {
                    rows.push((key, decode_row(&data, width)?));
                }
            }
        } else {
            let (lower, upper) = table.rows_range();
            txn.scan(lower, upper, &mut |key, data| {
                let row = decode_row(data, width)?;
                if self.columns.iter().zip(&wanted).all(|(&c, v)| row[c] == *v) {
                    rows.push((key.to_vec(), row));
                }
                Ok(true)
            })?;
        }
        debug_assert!(rows.iter().all(|(key, _)| key.len() > ROW_PREFIX_LEN));
        Ok(rows)
    }
}

/// Encodes values for comparing them, NULLs included.
pub fn encoded(values: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    values.iter().for_each(|v| encode_key(&mut out, v));
    out
}
