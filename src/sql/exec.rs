//! Statement execution.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::ops::Bound;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use crate::coding::Reader;
use crate::error::{Error, Result, corruption};

use super::ast::{
    AlterAction, CheckDef, ColumnDef, ConflictAction, CreateIndex, CreateTable, Delete, Expr, FkAction, ForeignKeyDef,
    Insert, InsertSource, JoinKind, Select, SelectItem, SetOp, Statement, Update,
};
use super::catalog::{self, Column, IndexSchema, ROW_PREFIX_LEN, TableSchema, ViewSchema};
use super::datetime::{Kind, Moment};
use super::encoding::{
    decode_key, decode_row, decode_row_into, encode_key, encode_keys, encode_row, prefix_end, skip_key,
};
use super::expr::{Accumulator, BExpr, Binder, Env, Scope};
use super::integrity::{self, Child, ParentKey, Rules};
use super::notify::MAX_PAYLOAD;
use super::plan::{
    Access, Compound, Derived, GroupPlan, JoinPlan, KeyBound, Recursive, SelectPlan, SortKey, TablePlan, choose_access,
    index_covers, plan_select,
};
use super::txn::Txn;
use super::value::{DataType, Value, sql_error};
use super::{QueryResult, Rows};
use super::{vector, window};

/// A prepared statement's plan, reused across executions while the schema
/// it was planned against stays unchanged.
#[derive(Default)]
pub struct PlanCache(Mutex<Option<CachedPlan>>);

struct CachedPlan {
    /// Which database's schema cache the plan was built against, and at what version.
    cache_id: usize,
    version: u64,
    plan: Planned,
}

enum Planned {
    Select(Arc<SelectPlan>),
    Insert(Arc<InsertPlan>),
}

impl Planned {
    fn select(&self) -> Option<&Arc<SelectPlan>> {
        match self {
            Planned::Select(plan) => Some(plan),
            _ => None,
        }
    }

    fn insert(&self) -> Option<&Arc<InsertPlan>> {
        match self {
            Planned::Insert(plan) => Some(plan),
            _ => None,
        }
    }
}

impl fmt::Debug for PlanCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PlanCache")
    }
}

impl PlanCache {
    /// The cached plan if it's current, otherwise a fresh one from `make`.
    fn plan<T>(
        &self,
        txn: &Txn,
        cached: fn(&Planned) -> Option<&Arc<T>>,
        wrap: fn(Arc<T>) -> Planned,
        make: impl FnOnce() -> Result<T>,
    ) -> Result<Arc<T>> {
        // A transaction that changed the schema plans against its own view of it.
        let Some((cache_id, version)) = txn.schema_version() else {
            return Ok(Arc::new(make()?));
        };
        if let Some(entry) = &*self.0.lock().unwrap()
            && entry.cache_id == cache_id
            && entry.version == version
            && let Some(plan) = cached(&entry.plan)
        {
            return Ok(plan.clone());
        }
        // The version was read before planning, so a schema change that races
        // with planning leaves this plan tagged stale.
        let plan = Arc::new(make()?);
        *self.0.lock().unwrap() = Some(CachedPlan { cache_id, version, plan: wrap(plan.clone()) });
        Ok(plan)
    }
}

pub fn execute(txn: &mut Txn, statement: &Statement, params: &[Value], plans: &PlanCache) -> Result<QueryResult> {
    match statement {
        Statement::Select(select) => {
            let txn: &Txn = txn;
            let binder = Binder::new(params, txn);
            let plan = plans.plan(txn, Planned::select, Planned::Select, || plan_select(txn, select, &binder))?;
            Ok(QueryResult::Rows(run_select(txn, &plan, params)?))
        }
        Statement::Explain(inner) => explain(txn, inner, &Binder::new(params, txn)),
        // Subqueries can't appear in DEFAULT: no transaction to run them in.
        Statement::CreateTable(create) => {
            create_table(txn, create, &Binder { params, txn: None, outer: None, ctes: None })
        }
        Statement::DropTable { name, if_exists } => drop_table(txn, name, *if_exists),
        Statement::CreateIndex(create) => create_index(txn, create),
        Statement::AlterTable { table, action } => alter_table(txn, table, action, params),
        Statement::DropIndex { name, if_exists } => drop_index(txn, name, *if_exists),
        Statement::Insert(insert) => insert_rows(txn, insert, params, plans),
        Statement::Update(update) => update_rows(txn, update, params),
        Statement::Delete(delete) => delete_rows(txn, delete, params),
        Statement::Notify { channel, payload } => {
            let payload = match payload {
                None => String::new(),
                Some(e) => match Binder::new(params, txn).constant(e, "NOTIFY")? {
                    Value::Text(s) => s,
                    v => return Err(sql_error(format!("a NOTIFY payload must be TEXT, got {}", v.type_name()))),
                },
            };
            if payload.len() > MAX_PAYLOAD {
                return Err(sql_error(format!("a NOTIFY payload can be at most {MAX_PAYLOAD} bytes")));
            }
            txn.notify(channel.clone(), payload);
            Ok(QueryResult::Done("NOTIFY"))
        }
        Statement::CreateView { .. } => create_view(txn, statement),
        Statement::DropView { name, if_exists } => {
            if catalog::get_view(txn, name)?.is_none() {
                if *if_exists {
                    return Ok(QueryResult::Done("DROP VIEW"));
                }
                return Err(sql_error(format!("no such view: {name}")));
            }
            catalog::delete_view(txn, name);
            Ok(QueryResult::Done("DROP VIEW"))
        }
        Statement::Begin
        | Statement::Commit
        | Statement::Rollback
        | Statement::Savepoint(_)
        | Statement::Release(_)
        | Statement::RollbackTo(_)
        | Statement::Listen(_)
        | Statement::Unlisten(_) => Err(sql_error("this statement is handled by the session")),
    }
}

fn explain(txn: &Txn, statement: &Statement, binder: &Binder) -> Result<QueryResult> {
    let lines = match statement {
        Statement::Select(select) => plan_select(txn, select, binder)?.explain(),
        Statement::Update(Update { table, filter, .. }) | Statement::Delete(Delete { table, filter, .. }) => {
            let (schema, indexes) = (catalog::require_table(txn, table)?, catalog::indexes_of(txn, table)?);
            let scope = Scope::of_table(&schema, &schema.name);
            let (access, filter) = write_target(&schema, &indexes, filter.as_ref(), &scope, binder)?;
            let mut lines = vec![access.describe(&schema, &schema.name)];
            if filter.is_some() {
                lines.push("  FILTER".into());
            }
            let verb = if matches!(statement, Statement::Update(_)) { "UPDATE" } else { "DELETE FROM" };
            lines.push(format!("{verb} {table}"));
            lines
        }
        _ => return Err(sql_error("EXPLAIN supports SELECT, UPDATE and DELETE")),
    };
    let rows = lines.into_iter().map(|line| vec![Value::Text(line)]).collect();
    Ok(QueryResult::Rows(Rows { columns: vec!["plan".into()], rows }))
}

// ---------------------------------------------------------------- reading

/// Receives each row read, with its key. Both are lent for the duration of
/// the call; the row may be modified (its values moved out). Returning
/// `false` stops the read.
pub(super) type Sink<'a> = &'a mut dyn FnMut(&[u8], &mut [Value]) -> Result<bool>;

/// Where a read looks and what it decodes.
pub(super) struct ReadSpec<'a> {
    table: &'a TableSchema,
    access: &'a Access,
    /// Only these columns are decoded (the rest read as NULL), and an index
    /// that holds them all answers without touching the rows. `None`: all.
    needed: Option<&'a [bool]>,
    /// Set by the read when the access path narrowed it, rather than falling
    /// back to a full scan; the conditions the path enforces then hold.
    pub(super) narrowed: Cell<bool>,
    /// For a query in FROM or a common table expression: what to read
    /// instead of a table.
    derived: Option<&'a Derived>,
}

impl<'a> ReadSpec<'a> {
    pub(super) fn new(table: &'a TableSchema, access: &'a Access, needed: Option<&'a [bool]>) -> Self {
        ReadSpec { table, access, needed, narrowed: Cell::new(false), derived: None }
    }

    fn of(table: &'a TablePlan) -> Self {
        ReadSpec { derived: table.derived.as_ref(), ..ReadSpec::new(&table.schema, &table.access, Some(&table.needed)) }
    }
}

/// Reads the rows `spec` selects. `outer` is the row that key expressions
/// are evaluated against.
pub(super) fn read(txn: &Txn, spec: &ReadSpec, outer: &[Value], env: &Env, sink: Sink) -> Result<()> {
    match spec.derived {
        // A query in FROM: its rows, as if read from a table.
        Some(Derived::Query(query)) => {
            for mut row in run_select(txn, query, env.params)?.rows {
                if !sink(&[], &mut row)? {
                    break;
                }
            }
            return Ok(());
        }
        Some(Derived::Recursive(query)) => return recursive_rows(txn, query, env.params, sink),
        Some(Derived::Work(id)) => {
            let Some((_, rows)) = env.work.as_ref().filter(|(work, _)| work == id) else {
                return Err(sql_error(format!(
                    "{} can refer to itself only in the FROM clause of its recursive part, not in a subquery",
                    spec.table.name
                )));
            };
            for row in rows.iter() {
                if !sink(&[], &mut row.clone())? {
                    break;
                }
            }
            return Ok(());
        }
        None => {}
    }
    let (table, access, needed) = (spec.table, spec.access, spec.needed);
    // Key values are converted to their column's type. A value that can't be
    // (NULL, or a type that could never compare equal) falls back to a full
    // scan, and the row filter then gives the right answer.
    let key_values = |exprs: &[BExpr], cols: &[usize]| -> Result<Option<Vec<Value>>> {
        let mut values = Vec::with_capacity(exprs.len());
        for (e, &c) in exprs.iter().zip(cols) {
            match e.eval(outer, env)?.assign_to(table.columns[c].ty) {
                Ok(v) if !v.is_null() => values.push(v),
                _ => return Ok(None),
            }
        }
        Ok(Some(values))
    };
    let range = |prefix: Vec<u8>, column: usize, lower: &Option<KeyBound>, upper: &Option<KeyBound>| {
        let ty = table.columns[column].ty;
        let encoded = |b: &KeyBound| -> Result<Option<Vec<u8>>> {
            Ok(match b.value.eval(outer, env)?.assign_to(ty) {
                Ok(v) if !v.is_null() => {
                    let mut k = prefix.clone();
                    encode_key(&mut k, &v);
                    Some(k)
                }
                _ => None,
            })
        };
        // Index entries continue past the value with the primary key, so an
        // exclusive lower or inclusive upper bound must cover all of them.
        let lower = match lower {
            None => {
                let mut k = prefix.clone();
                k.push(0x01); // past NULLs, which no range matches
                Bound::Included(k)
            }
            Some(b) => match encoded(b)? {
                None => return Ok(None),
                Some(k) if b.inclusive => Bound::Included(k),
                Some(k) => prefix_end(&k).map_or(Bound::Unbounded, Bound::Included),
            },
        };
        let upper = match upper {
            None => prefix_end(&prefix).map_or(Bound::Unbounded, Bound::Excluded),
            Some(b) => match encoded(b)? {
                None => return Ok(None),
                Some(k) if b.inclusive => prefix_end(&k).map_or(Bound::Unbounded, Bound::Excluded),
                Some(k) => Bound::Excluded(k),
            },
        };
        Ok::<_, Error>(Some((lower, upper)))
    };
    match access {
        Access::Scan => scan_rows(txn, table, needed, table.rows_range(), sink),
        Access::PrimaryKey(exprs) => {
            let Some(values) = key_values(exprs, &table.primary_key)? else {
                return scan_rows(txn, table, needed, table.rows_range(), sink);
            };
            spec.narrowed.set(true);
            let mut key = table.row_prefix();
            encode_keys(&mut key, &values);
            if values.len() < table.primary_key.len() {
                let end = prefix_end(&key).map_or(Bound::Unbounded, Bound::Excluded);
                return scan_rows(txn, table, needed, (Bound::Included(key), end), sink);
            }
            if let Some(data) = txn.get(&key)? {
                let mut row = Vec::with_capacity(table.columns.len());
                decode_row_into(&data, table.columns.len(), needed, &mut row)?;
                sink(&key, &mut row)?;
            }
            Ok(())
        }
        Access::PrimaryKeyRange { lower, upper } => {
            match range(table.row_prefix(), table.primary_key[0], lower, upper)? {
                Some(bounds) => {
                    spec.narrowed.set(true);
                    scan_rows(txn, table, needed, bounds, sink)
                }
                None => scan_rows(txn, table, needed, table.rows_range(), sink),
            }
        }
        Access::Index { index, values: exprs } => match key_values(exprs, &index.columns)? {
            Some(values) => {
                spec.narrowed.set(true);
                index_rows(txn, table, index, needed, catalog::values_range(index, &values), sink)
            }
            None => scan_rows(txn, table, needed, table.rows_range(), sink),
        },
        Access::IndexRange { index, lower, upper } => match range(index.prefix(), index.columns[0], lower, upper)? {
            Some(bounds) => {
                spec.narrowed.set(true);
                index_rows(txn, table, index, needed, bounds, sink)
            }
            None => scan_rows(txn, table, needed, table.rows_range(), sink),
        },
    }
}

fn scan_rows(
    txn: &Txn,
    table: &TableSchema,
    needed: Option<&[bool]>,
    (lower, upper): (Bound<Vec<u8>>, Bound<Vec<u8>>),
    sink: Sink,
) -> Result<()> {
    // One row buffer serves the whole scan.
    let mut row = Vec::with_capacity(table.columns.len());
    txn.scan(lower, upper, &mut |key, data| {
        decode_row_into(data, table.columns.len(), needed, &mut row)?;
        sink(key, &mut row)
    })
}

/// Reads rows through index entries in `[lower, upper]`.
fn index_rows(
    txn: &Txn,
    table: &TableSchema,
    index: &IndexSchema,
    needed: Option<&[bool]>,
    (lower, upper): (Bound<Vec<u8>>, Bound<Vec<u8>>),
    sink: Sink,
) -> Result<()> {
    let width = table.columns.len();
    let skip = index.prefix().len();
    let covered = needed.is_some_and(|needed| index_covers(index, table, needed));
    let mut key = table.row_prefix();
    let mut row = Vec::with_capacity(width);
    txn.scan(lower, upper, &mut |entry, _| {
        // The entry is the indexed values followed by the row's primary key.
        let mut rest = &entry[skip..];
        if covered {
            row.clear();
            row.resize(width, Value::Null);
            for &c in &index.columns {
                row[c] = decode_key(&mut rest)?;
            }
        } else {
            for _ in &index.columns {
                skip_key(&mut rest)?;
            }
        }
        key.truncate(ROW_PREFIX_LEN);
        key.extend_from_slice(rest);
        if covered {
            for &c in &table.primary_key {
                row[c] = decode_key(&mut rest)?;
            }
        } else {
            let data =
                txn.get(&key)?.ok_or_else(|| corruption(format!("index {} points at a missing row", index.name)))?;
            decode_row_into(&data, width, needed, &mut row)?;
        }
        sink(&key, &mut row)
    })
}

/// The access path and bound WHERE clause for an UPDATE or DELETE.
fn write_target(
    table: &TableSchema,
    indexes: &[Arc<IndexSchema>],
    filter: Option<&Expr>,
    scope: &Scope,
    binder: &Binder,
) -> Result<(Access, Option<BExpr>)> {
    let filter = filter.map(|f| binder.bind(f, scope, "WHERE")).transpose()?;
    let conditions = filter.clone().map(BExpr::into_conjuncts).unwrap_or_default();
    Ok((choose_access(table, indexes, 0, &conditions), filter))
}

/// Every complete row (with its key) that `access` finds and `filter` accepts.
fn matching_rows(
    txn: &Txn,
    table: &TableSchema,
    access: &Access,
    filter: Option<&BExpr>,
    env: &Env,
) -> Result<Vec<(Vec<u8>, Vec<Value>)>> {
    let mut rows = Vec::new();
    read(txn, &ReadSpec::new(table, access, None), &[], env, &mut |key, row| {
        if filter.map_or(Ok(true), |f| f.matches(row, env))? {
            rows.push((key.to_vec(), row.to_vec()));
        }
        Ok(true)
    })?;
    Ok(rows)
}

// ---------------------------------------------------------------- SELECT

/// Evaluates a LIMIT or OFFSET expression.
fn count(e: &Option<BExpr>, env: &Env, clause: &str) -> Result<Option<usize>> {
    let Some(e) = e else { return Ok(None) };
    match e.eval(&[], env)? {
        Value::Integer(n) if n >= 0 => Ok(Some(n as usize)),
        v => Err(sql_error(format!("{clause} must be a non-negative integer, got {}", v.literal()))),
    }
}

pub fn run_select(txn: &Txn, plan: &Arc<SelectPlan>, params: &[Value]) -> Result<Rows> {
    select_rows(txn, plan, &Env::new(params, Some(txn)))
}

fn select_rows(txn: &Txn, plan: &Arc<SelectPlan>, env: &Env) -> Result<Rows> {
    let limit = count(&plan.limit, env, "LIMIT")?;
    let offset = count(&plan.offset, env, "OFFSET")?.unwrap_or(0);
    if let Some(compound) = &plan.compound {
        let out = compound_rows(txn, compound, env.params)?.into_iter().map(|row| (row, Vec::new())).collect();
        return Ok(finish(plan, out, limit, offset));
    }
    // Without aggregation, DISTINCT or a sort, rows can be emitted as they are
    // read, and reading can stop as soon as LIMIT is satisfied.
    let streaming = plan.grouping.is_none()
        && plan.windows.is_empty()
        && !plan.distinct
        && (plan.order_by.is_empty() || plan.presorted);
    let wanted = if streaming { limit.map(|l| l.saturating_add(offset)) } else { None };

    // Each output row with the values of its ORDER BY expressions.
    let mut out: Vec<(Vec<Value>, Vec<Value>)> = Vec::new();
    // With window functions: the rows, kept until all are in.
    let mut windowed: Vec<Vec<Value>> = Vec::new();
    // Aggregation state: one set of accumulators without GROUP BY, otherwise
    // one per group, keyed by the encoded group key.
    let new_accumulators = |g: &GroupPlan| g.aggregates.iter().map(Accumulator::new).collect::<Vec<_>>();
    let mut single = plan.grouping.as_ref().filter(|g| g.keys.is_empty()).map(new_accumulators);
    let mut groups: BTreeMap<Vec<u8>, (Vec<Value>, Vec<Accumulator>)> = BTreeMap::new();
    let (mut key_values, mut key_bytes) = (Vec::new(), Vec::new());
    // Aggregating one table runs in batches, and may be split across threads.
    let batched = vector::applies(plan);

    let mut visit = |row: &mut [Value]| {
        if let Some(filter) = &plan.filter
            && !filter.matches(row, env)?
        {
            return Ok(true);
        }
        let Some(g) = &plan.grouping else {
            if !plan.windows.is_empty() {
                windowed.push(row.to_vec());
                return Ok(true);
            }
            out.push(project(plan, row, env)?);
            return Ok(wanted.is_none_or(|n| out.len() < n));
        };
        let accumulators = match &mut single {
            Some(accumulators) => accumulators,
            None => {
                key_values.clear();
                key_bytes.clear();
                for k in &g.keys {
                    let v = k.eval(row, env)?;
                    encode_key(&mut key_bytes, &v);
                    key_values.push(v);
                }
                if !groups.contains_key(key_bytes.as_slice()) {
                    groups.insert(key_bytes.clone(), (key_values.clone(), new_accumulators(g)));
                }
                &mut groups.get_mut(key_bytes.as_slice()).unwrap().1
            }
        };
        for (acc, aggregate) in accumulators.iter_mut().zip(&g.aggregates) {
            acc.add(aggregate, row, env)?;
        }
        Ok(true)
    };
    if !batched {
        for_each_row(txn, plan, env, &mut visit)?;
    }

    if plan.grouping.is_some() {
        let finished: Vec<(Vec<Value>, Vec<Accumulator>)> = if batched {
            vector::aggregate(txn, plan, env.params)?
        } else {
            // Without GROUP BY there is exactly one group, even with no input rows.
            single.map(|accumulators| (Vec::new(), accumulators)).into_iter().chain(groups.into_values()).collect()
        };
        for (mut row, accumulators) in finished {
            row.extend(accumulators.into_iter().map(Accumulator::finish));
            if let Some(having) = &plan.having
                && !having.matches(&row, env)?
            {
                continue;
            }
            if !plan.windows.is_empty() {
                windowed.push(row);
                continue;
            }
            out.push(project(plan, &mut row, env)?);
        }
    }
    if !plan.windows.is_empty() {
        window::compute(&plan.windows, &mut windowed, env)?;
        for row in &mut windowed {
            out.push(project(plan, row, env)?);
        }
    }
    Ok(finish(plan, out, limit, offset))
}

/// DISTINCT, ORDER BY, OFFSET and LIMIT, applied to the output rows (each
/// with the values of its ORDER BY expressions).
fn finish(plan: &SelectPlan, mut out: Vec<(Vec<Value>, Vec<Value>)>, limit: Option<usize>, offset: usize) -> Rows {
    if plan.distinct {
        let mut seen = HashSet::new();
        out.retain(|(row, _)| {
            let mut key = Vec::new();
            encode_keys(&mut key, row);
            seen.insert(key)
        });
    }
    if !plan.order_by.is_empty() && !plan.presorted {
        out.sort_by(|(a_row, a_keys), (b_row, b_keys)| {
            for (i, (key, sort)) in plan.order_by.iter().enumerate() {
                let (a, b) = match key {
                    SortKey::Output(c) => (&a_row[*c], &b_row[*c]),
                    SortKey::Expr(_) => (&a_keys[i], &b_keys[i]),
                };
                let ordering = sort.compare(a, b);
                if ordering.is_ne() {
                    return ordering;
                }
            }
            std::cmp::Ordering::Equal
        });
    }
    let rows = out.into_iter().skip(offset).take(limit.unwrap_or(usize::MAX)).map(|(row, _)| row).collect();
    Rows { columns: plan.columns.clone(), rows }
}

/// The rows of a compound query: its parts, combined left to right.
fn compound_rows(txn: &Txn, compound: &Compound, params: &[Value]) -> Result<Vec<Vec<Value>>> {
    let mut rows = run_select(txn, &compound.first, params)?.rows;
    for (op, all, part) in &compound.parts {
        rows = combine(*op, *all, rows, run_select(txn, part, params)?.rows);
    }
    Ok(rows)
}

/// Rounds a recursive query may take before it is taken to be endless.
const MAX_ROUNDS: usize = 1_000_000;

/// Runs a recursive query: the anchor, then the recursive part over each
/// round's new rows until a round adds none. Rows go to `sink` as they are
/// found, so a LIMIT above can stop a query that would never end.
fn recursive_rows(txn: &Txn, query: &Recursive, params: &[Value], sink: Sink) -> Result<()> {
    let mut seen = HashSet::new();
    let mut new = |row: &[Value]| {
        query.all || {
            let mut k = Vec::new();
            encode_keys(&mut k, row);
            seen.insert(k)
        }
    };
    let mut round = Vec::new();
    for row in run_select(txn, &query.anchor, params)?.rows {
        if new(&row) {
            if !sink(&[], &mut row.clone())? {
                return Ok(());
            }
            round.push(row);
        }
    }
    for _ in 0..MAX_ROUNDS {
        if round.is_empty() {
            return Ok(());
        }
        let mut env = Env::new(params, Some(txn));
        env.work = Some((query.id, Rc::new(std::mem::take(&mut round))));
        for row in select_rows(txn, &query.step, &env)?.rows {
            if new(&row) {
                if !sink(&[], &mut row.clone())? {
                    return Ok(());
                }
                round.push(row);
            }
        }
    }
    Err(sql_error(format!("recursive query ran for more than {MAX_ROUNDS} rounds; does it ever stop?")))
}

/// UNION, INTERSECT or EXCEPT of two lists of rows. Without ALL, the result
/// has no duplicates; with it, INTERSECT and EXCEPT match rows one for one.
/// Rows count as equal when every value is (NULLs included), as in DISTINCT.
fn combine(op: SetOp, all: bool, left: Vec<Vec<Value>>, right: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    let key = |row: &[Value]| {
        let mut k = Vec::new();
        encode_keys(&mut k, row);
        k
    };
    let distinct = |rows: Vec<Vec<Value>>| {
        let mut seen = HashSet::new();
        rows.into_iter().filter(|r| seen.insert(key(r))).collect::<Vec<_>>()
    };
    let mut counts: HashMap<Vec<u8>, usize> = HashMap::new();
    if op != SetOp::Union {
        for row in &right {
            *counts.entry(key(row)).or_default() += 1;
        }
    }
    match (op, all) {
        (SetOp::Union, true) => left.into_iter().chain(right).collect(),
        (SetOp::Union, false) => distinct(left.into_iter().chain(right).collect()),
        (SetOp::Intersect, false) => distinct(left.into_iter().filter(|r| counts.contains_key(&key(r))).collect()),
        (SetOp::Except, false) => distinct(left.into_iter().filter(|r| !counts.contains_key(&key(r))).collect()),
        (SetOp::Intersect, true) => left
            .into_iter()
            .filter(|r| {
                counts.get_mut(&key(r)).is_some_and(|n| {
                    *n > 0 && {
                        *n -= 1;
                        true
                    }
                })
            })
            .collect(),
        (SetOp::Except, true) => left
            .into_iter()
            .filter(|r| match counts.get_mut(&key(r)) {
                Some(n) if *n > 0 => {
                    *n -= 1;
                    false
                }
                _ => true,
            })
            .collect(),
    }
}

/// Runs a SELECT, handing each result row to `f`, which returns `false` to
/// stop. Rows that can be produced as they are read (no grouping, DISTINCT
/// or sort) go through one reused buffer, so nothing is allocated per row;
/// the rest are collected first.
pub fn stream(
    txn: &Txn,
    statement: &Statement,
    params: &[Value],
    plans: &PlanCache,
    f: &mut dyn FnMut(&[Value]) -> Result<bool>,
) -> Result<()> {
    let Statement::Select(select) = statement else {
        return Err(sql_error("only a SELECT can be streamed"));
    };
    let binder = Binder::new(params, txn);
    let plan = plans.plan(txn, Planned::select, Planned::Select, || plan_select(txn, select, &binder))?;
    stream_plan(txn, &plan, params, f)
}

/// Runs a planned SELECT, handing each result row to `f`; see `stream`.
pub fn stream_plan(
    txn: &Txn,
    plan: &Arc<SelectPlan>,
    params: &[Value],
    f: &mut dyn FnMut(&[Value]) -> Result<bool>,
) -> Result<()> {
    let env = Env::new(params, Some(txn));
    let streaming = plan.grouping.is_none()
        && plan.compound.is_none()
        && plan.windows.is_empty()
        && !plan.distinct
        && (plan.order_by.is_empty() || plan.presorted);
    if !streaming {
        for row in &select_rows(txn, plan, &env)?.rows {
            if !f(row)? {
                break;
            }
        }
        return Ok(());
    }
    let limit = count(&plan.limit, &env, "LIMIT")?;
    let mut skip = count(&plan.offset, &env, "OFFSET")?.unwrap_or(0);
    if limit == Some(0) {
        return Ok(());
    }
    let (mut out, mut emitted) = (vec![Value::Null; plan.projection.len()], 0);
    // `SELECT *`, or the first columns in order: the row itself is the result.
    let prefix = plan.projection.iter().enumerate().all(|(i, e)| matches!(e, BExpr::Column(c) if *c == i));
    for_each_row(txn, plan, &env, &mut |row| {
        if let Some(filter) = &plan.filter
            && !filter.matches(row, &env)?
        {
            return Ok(true);
        }
        if skip > 0 {
            skip -= 1;
            return Ok(true);
        }
        emitted += 1;
        if prefix {
            return Ok(f(&row[..out.len()])? && limit.is_none_or(|limit| emitted < limit));
        }
        for (slot, e) in out.iter_mut().zip(&plan.projection) {
            match (e, &row) {
                // Copied into the string already in the slot, not a new one.
                (BExpr::Column(c), row) => match (&mut *slot, &row[*c]) {
                    (Value::Text(to), Value::Text(from)) => to.clone_from(from),
                    (slot, value) => *slot = value.clone(),
                },
                (e, row) => *slot = e.eval(row, &env)?,
            }
        }
        Ok(f(&out)? && limit.is_none_or(|limit| emitted < limit))
    })
}

/// Computes an output row and its ORDER BY values. Columns marked movable
/// are taken out of `row` rather than copied.
fn project(plan: &SelectPlan, row: &mut [Value], env: &Env) -> Result<(Vec<Value>, Vec<Value>)> {
    // Sort values first: they may read columns before any are moved out.
    let sort_values = plan
        .order_by
        .iter()
        .map(|(key, _)| match key {
            SortKey::Expr(e) => e.eval(row, env),
            SortKey::Output(_) => Ok(Value::Null),
        })
        .collect::<Result<Vec<_>>>()?;
    let mut projected = Vec::with_capacity(plan.projection.len());
    for (e, &movable) in plan.projection.iter().zip(&plan.movable) {
        projected.push(match e {
            BExpr::Column(c) if movable => std::mem::replace(&mut row[*c], Value::Null),
            e => e.eval(row, env)?,
        });
    }
    Ok((projected, sort_values))
}

/// Feeds every row produced by FROM (joins included) and the base-table
/// filter to `emit`, which returns `false` to stop.
fn for_each_row(
    txn: &Txn,
    plan: &SelectPlan,
    env: &Env,
    emit: &mut dyn FnMut(&mut [Value]) -> Result<bool>,
) -> Result<()> {
    let Some(base) = &plan.base else {
        // No FROM: a single row with no columns.
        if plan.base_filter.as_ref().map_or(Ok(true), |f| f.matches(&[], env))? {
            emit(&mut [])?;
        }
        return Ok(());
    };
    // Joined tables read in full are read once and kept here.
    let mut cache: Vec<Option<Vec<Vec<Value>>>> = vec![None; plan.joins.len()];
    let spec = ReadSpec::of(base);
    read(txn, &spec, &[], env, &mut |_, row| {
        // Conditions the access path enforces needn't be checked again,
        // unless the read fell back to a full scan.
        let filter = if spec.narrowed.get() { &plan.base_residual } else { &plan.base_filter };
        if let Some(filter) = filter
            && !filter.matches(row, env)?
        {
            return Ok(true);
        }
        if plan.joins.is_empty() {
            return emit(row);
        }
        join(txn, &plan.joins, &mut cache, row, env, emit)
    })
}

/// Nested-loop join of `row` with the remaining `joins`.
fn join(
    txn: &Txn,
    joins: &[JoinPlan],
    cache: &mut [Option<Vec<Vec<Value>>>],
    row: &mut [Value],
    env: &Env,
    emit: &mut dyn FnMut(&mut [Value]) -> Result<bool>,
) -> Result<bool> {
    let (Some((j, rest)), Some((cached, rest_cache))) = (joins.split_first(), cache.split_first_mut()) else {
        return emit(row);
    };
    let table = &j.table;
    let collect = |access: &Access, outer: &[Value]| -> Result<Vec<Vec<Value>>> {
        let mut rows = Vec::new();
        let spec =
            ReadSpec { derived: table.derived.as_ref(), ..ReadSpec::new(&table.schema, access, Some(&table.needed)) };
        read(txn, &spec, outer, env, &mut |_, r| {
            rows.push(r.to_vec());
            Ok(true)
        })?;
        Ok(rows)
    };
    let looked_up;
    let candidates: &[Vec<Value>] = if matches!(table.access, Access::Scan) {
        if cached.is_none() {
            *cached = Some(collect(&Access::Scan, &[])?);
        }
        cached.as_deref().unwrap()
    } else {
        looked_up = collect(&table.access, row)?;
        &looked_up
    };

    let mut matched = false;
    let mut combined = Vec::with_capacity(row.len() + table.schema.columns.len());
    for right in candidates {
        combined.clear();
        combined.extend_from_slice(row);
        combined.extend_from_slice(right);
        if let Some(on) = &j.on
            && !on.matches(&combined, env)?
        {
            continue;
        }
        matched = true;
        if !join(txn, rest, rest_cache, &mut combined, env, emit)? {
            return Ok(false);
        }
    }
    if !matched && j.kind == JoinKind::Left {
        combined.clear();
        combined.extend_from_slice(row);
        combined.resize(row.len() + table.schema.columns.len(), Value::Null);
        return join(txn, rest, rest_cache, &mut combined, env, emit);
    }
    Ok(true)
}

// ---------------------------------------------------------------- DDL

fn column_positions(table: &TableSchema, names: &[String]) -> Result<Vec<usize>> {
    let mut positions = Vec::with_capacity(names.len());
    for name in names {
        let c = table.column_index(name).ok_or_else(|| sql_error(format!("no such column: {}.{name}", table.name)))?;
        if positions.contains(&c) {
            return Err(sql_error(format!("column {name} is listed more than once")));
        }
        positions.push(c);
    }
    Ok(positions)
}

/// Checks that CHECK constraints can be evaluated against rows of `table`.
fn validate_checks(table: &TableSchema, checks: &[CheckDef]) -> Result<()> {
    let binder = Binder { params: &[], txn: None, outer: None, ctes: None };
    let scope = Scope::of_table(table, &table.name);
    for check in checks {
        let mut params = false;
        check.expr.walk(&mut |e| params |= matches!(e, Expr::Param(_)));
        if params {
            return Err(sql_error("a CHECK constraint can't use parameters"));
        }
        binder.bind(&check.expr, &scope, "CHECK")?;
    }
    Ok(())
}

/// Resolves a foreign key of `table`: the table it refers to must exist and
/// the referenced columns (by default its primary key) be its primary key
/// or unique. `uniques` are `table`'s own unique column sets, for a key
/// that refers to its own table.
fn foreign_key(txn: &Txn, table: &TableSchema, uniques: &[Vec<usize>], fk: &ForeignKeyDef) -> Result<ForeignKeyDef> {
    let columns = column_positions(table, &fk.columns)?;
    let stored = if fk.table == table.name { None } else { Some(catalog::require_table(txn, &fk.table)?) };
    let parent = stored.as_deref().unwrap_or(table);
    let references: Vec<String> = if fk.references.is_empty() {
        if parent.primary_key.is_empty() {
            return Err(sql_error(format!(
                "{} has no primary key, so name the columns that REFERENCES {} refers to",
                parent.name, parent.name
            )));
        }
        parent.primary_key.iter().map(|&c| parent.columns[c].name.clone()).collect()
    } else {
        fk.references.clone()
    };
    if references.len() != columns.len() {
        return Err(sql_error(format!(
            "foreign key ({}) has {} columns but references {}",
            fk.columns.join(", "),
            columns.len(),
            references.len()
        )));
    }
    let positions = column_positions(parent, &references)?;
    let mut sorted = positions.clone();
    sorted.sort_unstable();
    let same = |key: &[usize]| {
        let mut key = key.to_vec();
        key.sort_unstable();
        key == sorted
    };
    let unique = match &stored {
        None => same(&table.primary_key) || uniques.iter().any(|u| same(u)),
        Some(parent) => ParentKey::of(parent, &catalog::indexes_of(txn, &parent.name)?, &positions).is_some(),
    };
    if !unique {
        return Err(sql_error(format!(
            "{}({}) is not a primary key or unique, so a foreign key can't refer to it",
            parent.name,
            references.join(", ")
        )));
    }
    Ok(ForeignKeyDef { references, ..fk.clone() })
}

/// Renames column `from` to `to` where `expr` reads it from `table`.
fn rename_column_in(expr: &mut Expr, table: &str, from: &str, to: &str) {
    expr.rewrite(&mut |e| {
        if let Expr::Column { table: t, name } = e
            && name == from
            && t.as_deref().is_none_or(|t| t == table)
        {
            *name = to.to_string();
        }
    });
}

/// A column's DEFAULT: its value, and whether it is the clock
/// (`CURRENT_TIMESTAMP` and friends), which each INSERT reads anew.
fn column_default(def: &ColumnDef, binder: &Binder) -> Result<(Option<Value>, bool)> {
    let Some(e) = &def.default else { return Ok((None, false)) };
    let clock = matches!(e, Expr::Function { name, args, .. }
        if args.is_empty() && matches!(name.as_str(), "now" | "current_timestamp" | "localtimestamp" | "current_date" | "current_time" | "localtime"));
    if clock && !(def.ty.is_temporal() || def.ty == DataType::Text) {
        return Err(sql_error(format!("column {} is {}, so its default can't be the current time", def.name, def.ty)));
    }
    let value =
        if clock { Value::Text(Moment::now().format(Kind::of(def.ty))?) } else { binder.constant(e, "DEFAULT")? };
    let value = value.assign_to(def.ty).map_err(|v| {
        sql_error(format!("default for column {} is {}, which is not {}", def.name, v.literal(), def.ty))
    })?;
    Ok((Some(value), clock))
}

fn create_view(txn: &mut Txn, statement: &Statement) -> Result<QueryResult> {
    let Statement::CreateView { name, columns, query, sql, or_replace, if_not_exists } = statement else {
        unreachable!("only CREATE VIEW comes here")
    };
    if catalog::get_table(txn, name)?.is_some() {
        return Err(sql_error(format!("a table named {name} already exists")));
    }
    if catalog::get_view(txn, name)?.is_some() {
        if *if_not_exists {
            return Ok(QueryResult::Done("CREATE VIEW"));
        }
        if !*or_replace {
            return Err(sql_error(format!("view {name} already exists")));
        }
    }
    // Planning the query checks it; it may not reach this view, even through others.
    let plan = plan_select(txn, query, &Binder::new(&[], txn))?;
    if !columns.is_empty() && columns.len() != plan.columns.len() {
        return Err(sql_error(format!(
            "view {name} has {} columns, but {} names are given",
            plan.columns.len(),
            columns.len()
        )));
    }
    if reaches_view(txn, query, name, 0)? {
        return Err(sql_error(format!("view {name} can't refer to itself")));
    }
    let view = ViewSchema { name: name.clone(), columns: columns.clone(), sql: sql.clone(), query: (**query).clone() };
    catalog::put_view(txn, &view);
    Ok(QueryResult::Done("CREATE VIEW"))
}

/// Whether `query` reads view `name`, directly or through other views.
fn reaches_view(txn: &Txn, query: &Select, name: &str, depth: usize) -> Result<bool> {
    let mut tables = Vec::new();
    query.visit_tables(&mut |t| {
        if t.subquery.is_none() {
            tables.push(t.name.clone());
        }
    });
    for table in tables {
        if table == name || depth > 32 {
            return Ok(true);
        }
        if catalog::get_table(txn, &table)?.is_none()
            && let Some(view) = catalog::get_view(txn, &table)?
            && reaches_view(txn, &view.query, name, depth + 1)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn create_table(txn: &mut Txn, create: &CreateTable, binder: &Binder) -> Result<QueryResult> {
    if catalog::get_view(txn, &create.name)?.is_some() {
        return Err(sql_error(format!("a view named {} already exists", create.name)));
    }
    if catalog::get_table(txn, &create.name)?.is_some() {
        if create.if_not_exists {
            return Ok(QueryResult::Done("CREATE TABLE"));
        }
        return Err(sql_error(format!("table {} already exists", create.name)));
    }
    let mut columns: Vec<Column> = Vec::new();
    for def in &create.columns {
        if columns.iter().any(|c| c.name == def.name) {
            return Err(sql_error(format!("duplicate column name: {}", def.name)));
        }
        let (default, default_now) = column_default(def, binder)?;
        let default = if default_now { None } else { default };
        columns.push(Column { name: def.name.clone(), ty: def.ty, not_null: def.not_null, default, default_now });
    }
    let mut table = TableSchema::new(catalog::next_id(txn)?, create.name.clone(), columns);

    let inline: Vec<usize> = create.columns.iter().enumerate().filter(|(_, d)| d.primary_key).map(|(i, _)| i).collect();
    table.primary_key = match (inline.len(), create.primary_key.is_empty()) {
        (0, _) => column_positions(&table, &create.primary_key)?,
        (1, true) => inline,
        _ => return Err(sql_error(format!("table {} has more than one primary key", create.name))),
    };
    for &c in &table.primary_key {
        table.columns[c].not_null = true;
    }

    // UNIQUE constraints are enforced by unique indexes.
    let mut uniques: Vec<Vec<usize>> =
        create.columns.iter().enumerate().filter(|(_, d)| d.unique).map(|(i, _)| vec![i]).collect();
    for names in &create.unique {
        uniques.push(column_positions(&table, names)?);
    }
    validate_checks(&table, &create.checks)?;
    table.checks = create.checks.clone();
    for fk in &create.foreign_keys {
        let fk = foreign_key(txn, &table, &uniques, fk)?;
        table.foreign_keys.push(fk);
    }
    catalog::put_table(txn, &table);
    for columns in uniques {
        if columns == table.primary_key {
            continue;
        }
        let names: Vec<&str> = columns.iter().map(|&c| table.columns[c].name.as_str()).collect();
        let name = format!("{}_{}_key", table.name, names.join("_"));
        if catalog::get_index(txn, &name)?.is_some() {
            return Err(sql_error(format!("index {name} already exists")));
        }
        let index = IndexSchema { id: catalog::next_id(txn)?, name, table: table.name.clone(), columns, unique: true };
        catalog::put_index(txn, &index);
    }
    Ok(QueryResult::Done("CREATE TABLE"))
}

fn alter_table(txn: &mut Txn, name: &str, action: &AlterAction, params: &[Value]) -> Result<QueryResult> {
    let mut table = (*catalog::require_table(txn, name)?).clone();
    let indexes = catalog::indexes_of(txn, name)?;
    let position = |table: &TableSchema, column: &str| {
        table.column_index(column).ok_or_else(|| sql_error(format!("no such column: {}.{column}", table.name)))
    };
    match action {
        AlterAction::AddColumn(def) => {
            if table.column_index(&def.name).is_some() {
                return Err(sql_error(format!("column {} already exists in {}", def.name, table.name)));
            }
            if def.primary_key {
                return Err(sql_error("a PRIMARY KEY column can't be added to an existing table"));
            }
            // Existing rows get a clock default's current value.
            let (default, default_now) = column_default(def, &Binder { params, txn: None, outer: None, ctes: None })?;
            let rows = matching_rows(txn, &table, &Access::Scan, None, &Env::new(&[], Some(txn)))?;
            if def.not_null && default.as_ref().is_none_or(Value::is_null) && !rows.is_empty() {
                return Err(sql_error(format!(
                    "column {} is NOT NULL, so existing rows need a DEFAULT for it",
                    def.name
                )));
            }
            table.columns.push(Column {
                name: def.name.clone(),
                ty: def.ty,
                not_null: def.not_null,
                default: if default_now { None } else { default.clone() },
                default_now,
            });
            validate_checks(&table, &def.checks)?;
            table.checks.extend(def.checks.iter().cloned());
            if let Some(fk) = &def.references {
                let mut uniques: Vec<Vec<usize>> =
                    indexes.iter().filter(|i| i.unique).map(|i| i.columns.clone()).collect();
                uniques.extend(def.unique.then(|| vec![table.columns.len() - 1]));
                let fk = foreign_key(txn, &table, &uniques, fk)?;
                table.foreign_keys.push(fk);
            }
            // Existing rows, with the new column's value, must satisfy its constraints.
            if !def.checks.is_empty() || def.references.is_some() {
                let rules = Rules::of(txn, &Arc::new(table.clone()))?;
                for (_, row) in &rows {
                    let mut row = row.clone();
                    row.push(default.clone().unwrap_or(Value::Null));
                    rules.check(&row)?;
                    rules.check_parents(txn, &row)?;
                }
            }
            catalog::put_table(txn, &table);
            // Rows written before the column existed read as NULL in it; a
            // default has to be written into them.
            if let Some(value) = default.filter(|v| !v.is_null()) {
                for (key, mut row) in rows {
                    row.push(value.clone());
                    txn.put(key, encode_row(&row));
                }
            }
            if def.unique {
                let column = table.columns.len() - 1;
                let index_name = format!("{}_{}_key", table.name, def.name);
                let index = IndexSchema {
                    id: catalog::next_id(txn)?,
                    name: index_name,
                    table: table.name.clone(),
                    columns: vec![column],
                    unique: true,
                };
                for (key, row) in &matching_rows(txn, &table, &Access::Scan, None, &Env::new(&[], Some(txn)))? {
                    add_index_entry(txn, &table, &index, row, &key[ROW_PREFIX_LEN..])?;
                }
                catalog::put_index(txn, &index);
            }
        }
        AlterAction::DropColumn(column) => {
            let c = position(&table, column)?;
            if table.primary_key.contains(&c) {
                return Err(sql_error(format!("column {column} is part of the primary key, so it can't be dropped")));
            }
            if table.columns.len() == 1 {
                return Err(sql_error(format!("column {column} is the table's only column")));
            }
            let mut in_check = false;
            for check in &table.checks {
                check.expr.walk(&mut |e| {
                    in_check |= matches!(e, Expr::Column { table: t, name }
                        if name == column && t.as_deref().is_none_or(|t| t == table.name));
                });
            }
            if in_check {
                return Err(sql_error(format!(
                    "column {column} is used by a CHECK constraint, so it can't be dropped"
                )));
            }
            let referenced = catalog::children_of(txn, &table.name)?.iter().any(|child| {
                child.foreign_keys.iter().any(|fk| fk.table == table.name && fk.references.contains(column))
            });
            if referenced || table.foreign_keys.iter().any(|fk| fk.columns.contains(column)) {
                return Err(sql_error(format!("column {column} is part of a foreign key, so it can't be dropped")));
            }
            let rows = matching_rows(txn, &table, &Access::Scan, None, &Env::new(&[], Some(txn)))?;
            // Indexes on the column go with it; the others shift past it.
            let shift = |p: usize| if p > c { p - 1 } else { p };
            for index in indexes.iter() {
                if index.columns.contains(&c) {
                    delete_range(txn, catalog::index_range(index))?;
                    catalog::delete_index(txn, &index.name);
                } else {
                    let mut moved = (**index).clone();
                    moved.columns = moved.columns.iter().map(|&p| shift(p)).collect();
                    catalog::put_index(txn, &moved);
                }
            }
            table.columns.remove(c);
            table.primary_key = table.primary_key.iter().map(|&p| shift(p)).collect();
            catalog::put_table(txn, &table);
            for (key, mut row) in rows {
                row.remove(c);
                txn.put(key, encode_row(&row));
            }
        }
        AlterAction::RenameTable(new) => {
            if catalog::get_table(txn, new)?.is_some() {
                return Err(sql_error(format!("table {new} already exists")));
            }
            // Rows and index entries are keyed by the table's id, not its name.
            let old = std::mem::replace(&mut table.name, new.clone());
            for child in catalog::children_of(txn, &old)?.iter().filter(|c| c.name != old) {
                let mut child = (**child).clone();
                child.foreign_keys.iter_mut().filter(|fk| fk.table == old).for_each(|fk| fk.table = new.clone());
                catalog::put_table(txn, &child);
            }
            table.foreign_keys.iter_mut().filter(|fk| fk.table == old).for_each(|fk| fk.table = new.clone());
            for check in &mut table.checks {
                check.expr.rewrite(&mut |e| {
                    if let Expr::Column { table: Some(t), .. } = e
                        && *t == old
                    {
                        *t = new.clone();
                    }
                });
            }
            catalog::delete_table(txn, &old);
            catalog::put_table(txn, &table);
            for index in indexes.iter() {
                let mut renamed = (**index).clone();
                renamed.table = new.clone();
                catalog::put_index(txn, &renamed);
            }
        }
        AlterAction::RenameColumn { from, to } => {
            let c = position(&table, from)?;
            if table.column_index(to).is_some() {
                return Err(sql_error(format!("column {to} already exists in {}", table.name)));
            }
            table.columns[c].name = to.clone();
            for check in &mut table.checks {
                rename_column_in(&mut check.expr, &table.name, from, to);
            }
            let rename =
                |names: &mut Vec<String>| names.iter_mut().filter(|n| *n == from).for_each(|n| *n = to.clone());
            for child in catalog::children_of(txn, &table.name)?.iter().filter(|c| c.name != table.name) {
                let mut child = (**child).clone();
                child
                    .foreign_keys
                    .iter_mut()
                    .filter(|fk| fk.table == table.name)
                    .for_each(|fk| rename(&mut fk.references));
                catalog::put_table(txn, &child);
            }
            let own = table.name.clone();
            for fk in &mut table.foreign_keys {
                rename(&mut fk.columns);
                if fk.table == own {
                    rename(&mut fk.references);
                }
            }
            catalog::put_table(txn, &table);
        }
    }
    Ok(QueryResult::Done("ALTER TABLE"))
}

fn delete_range(txn: &mut Txn, (lower, upper): (Bound<Vec<u8>>, Bound<Vec<u8>>)) -> Result<()> {
    let mut keys = Vec::new();
    txn.scan(lower, upper, &mut |key, _| {
        keys.push(key.to_vec());
        Ok(true)
    })?;
    keys.into_iter().for_each(|k| txn.delete(k));
    Ok(())
}

fn drop_table(txn: &mut Txn, name: &str, if_exists: bool) -> Result<QueryResult> {
    let Some(table) = catalog::get_table(txn, name)? else {
        return if if_exists {
            Ok(QueryResult::Done("DROP TABLE"))
        } else {
            Err(sql_error(format!("no such table: {name}")))
        };
    };
    if let Some(child) = catalog::children_of(txn, name)?.iter().find(|c| c.name != name) {
        return Err(sql_error(format!(
            "table {name} is referenced by a foreign key of {}; drop that first",
            child.name
        )));
    }
    for index in catalog::indexes_of(txn, name)?.iter() {
        delete_range(txn, catalog::index_range(index))?;
        catalog::delete_index(txn, &index.name);
    }
    delete_range(txn, table.rows_range())?;
    txn.forget_sequence(&table.sequence_key());
    catalog::delete_table(txn, name);
    Ok(QueryResult::Done("DROP TABLE"))
}

fn create_index(txn: &mut Txn, create: &CreateIndex) -> Result<QueryResult> {
    if catalog::get_index(txn, &create.name)?.is_some() {
        if create.if_not_exists {
            return Ok(QueryResult::Done("CREATE INDEX"));
        }
        return Err(sql_error(format!("index {} already exists", create.name)));
    }
    let table = catalog::require_table(txn, &create.table)?;
    let columns = column_positions(&table, &create.columns)?;
    let index = IndexSchema {
        id: catalog::next_id(txn)?,
        name: create.name.clone(),
        table: table.name.clone(),
        columns,
        unique: create.unique,
    };
    let rows = matching_rows(txn, &table, &Access::Scan, None, &Env::new(&[], Some(txn)))?;
    for (key, row) in &rows {
        add_index_entry(txn, &table, &index, row, &key[ROW_PREFIX_LEN..])?;
    }
    catalog::put_index(txn, &index);
    Ok(QueryResult::Done("CREATE INDEX"))
}

fn drop_index(txn: &mut Txn, name: &str, if_exists: bool) -> Result<QueryResult> {
    let Some(index) = catalog::get_index(txn, name)? else {
        return if if_exists {
            Ok(QueryResult::Done("DROP INDEX"))
        } else {
            Err(sql_error(format!("no such index: {name}")))
        };
    };
    delete_range(txn, catalog::index_range(&index))?;
    catalog::delete_index(txn, name);
    Ok(QueryResult::Done("DROP INDEX"))
}

// ---------------------------------------------------------------- DML

fn unique_violation(table: &TableSchema, columns: &[usize]) -> Error {
    let names: Vec<String> = columns.iter().map(|&c| format!("{}.{}", table.name, table.columns[c].name)).collect();
    Error::Constraint(format!("UNIQUE constraint failed: {}", names.join(", ")))
}

fn add_index_entry(txn: &mut Txn, table: &TableSchema, index: &IndexSchema, row: &[Value], pk: &[u8]) -> Result<()> {
    if index.unique {
        // As in standard SQL, NULLs never collide with each other.
        let values = index.values_of(row);
        if !values.iter().any(Value::is_null) {
            let (lower, upper) = catalog::values_range(index, &values);
            if txn.any_in(lower, upper)? {
                return Err(unique_violation(table, &index.columns));
            }
        }
    }
    txn.put(index.entry_key(row, pk), Vec::new());
    Ok(())
}

/// Reads a stored row-id counter; tables start at 1.
fn stored_counter(stored: Option<&[u8]>) -> Result<i64> {
    Ok(match stored {
        Some(data) => Reader::new(data).varint()? as i64,
        None => 1,
    })
}

/// Hands out the next auto-assigned row id (or INTEGER PRIMARY KEY value).
fn next_row_id(txn: &mut Txn, table: &TableSchema) -> Result<i64> {
    let next = txn.sequence(&table.sequence_key(), stored_counter)?;
    let id = *next;
    *next = id.checked_add(1).ok_or_else(|| sql_error("row ids exhausted"))?;
    Ok(id)
}

/// Keeps future row ids clear of one chosen explicitly. Returns whether the
/// id is new: at or past the counter, which every row id ever written has
/// moved beyond, so no row can have it.
fn observe_row_id(txn: &mut Txn, table: &TableSchema, id: i64) -> Result<bool> {
    let next = txn.sequence(&table.sequence_key(), stored_counter)?;
    let fresh = id >= *next;
    if fresh {
        *next = id.saturating_add(1);
    }
    Ok(fresh)
}

/// A table being written: its definition, indexes and constraints.
pub struct Target {
    table: Arc<TableSchema>,
    indexes: Arc<[Arc<IndexSchema>]>,
    rules: Rules,
}

impl Target {
    fn new(txn: &Txn, table: Arc<TableSchema>, indexes: Arc<[Arc<IndexSchema>]>) -> Result<Arc<Target>> {
        let rules = Rules::of(txn, &table)?;
        Ok(Arc::new(Target { table, indexes, rules }))
    }

    fn of(txn: &Txn, name: &str) -> Result<Arc<Target>> {
        let Some(table) = catalog::get_table(txn, name)? else {
            if catalog::get_view(txn, name)?.is_some() {
                return Err(sql_error(format!("{name} is a view, which can't be written to")));
            }
            return Err(sql_error(format!("no such table: {name}")));
        };
        let indexes = catalog::indexes_of(txn, &table.name)?;
        Target::new(txn, table, indexes)
    }
}

/// Rows whose references to other rows are checked once the statement's
/// writes, cascades included, are done; so a row may refer to one written
/// after it.
#[derive(Default)]
struct Deferred(Vec<(Arc<Target>, Vec<u8>)>);

impl Deferred {
    fn verify(self, txn: &Txn) -> Result<()> {
        for (target, key) in self.0 {
            // A row deleted since refers to nothing.
            if let Some(data) = txn.get(&key)? {
                target.rules.check_parents(txn, &decode_row(&data, target.table.columns.len())?)?;
            }
        }
        Ok(())
    }
}

/// Validates `row` and writes it, with its index entries, returning the row
/// as stored. `key` is the row's existing key, for rewriting a row of a
/// table without a primary key.
fn write_row(
    txn: &mut Txn,
    target: &Arc<Target>,
    row: Vec<Value>,
    key: Option<Vec<u8>>,
    inserting: bool,
    deferred: &mut Deferred,
) -> Result<Vec<Value>> {
    let prepared = prepare_row(txn, &target.table, row, key, inserting)?;
    store_row(txn, target, prepared, deferred)
}

/// A row ready to store: converted to its columns' types, with its key.
struct Prepared {
    row: Vec<Value>,
    key: Vec<u8>,
    /// Whether the key is certainly unused, so needs no uniqueness check.
    fresh: bool,
}

/// Converts `row` to its columns' types, assigns automatic ids, checks
/// NOT NULL and works out the key.
fn prepare_row(
    txn: &mut Txn,
    table: &TableSchema,
    mut row: Vec<Value>,
    key: Option<Vec<u8>>,
    inserting: bool,
) -> Result<Prepared> {
    for (value, column) in row.iter_mut().zip(&table.columns) {
        *value = std::mem::replace(value, Value::Null).assign_to(column.ty).map_err(|v| {
            sql_error(format!(
                "column {}.{} is {}; cannot store {} {}",
                table.name,
                column.name,
                column.ty,
                v.type_name(),
                v.literal()
            ))
        })?;
    }
    // A row id the counter hasn't reached yet can't be taken, so the row
    // needs no uniqueness check: as SQLite knows for its rowid tables.
    let mut fresh = false;
    if let Some(c) = table.auto_increment_column() {
        match row[c] {
            Value::Null if inserting => {
                row[c] = Value::Integer(next_row_id(txn, table)?);
                fresh = true;
            }
            Value::Integer(id) => fresh = observe_row_id(txn, table, id)?,
            _ => {}
        }
    }
    for (value, column) in row.iter().zip(&table.columns) {
        if column.not_null && value.is_null() {
            return Err(Error::Constraint(format!("NOT NULL constraint failed: {}.{}", table.name, column.name)));
        }
    }
    let key = match key {
        Some(key) => key,
        None if table.primary_key.is_empty() => {
            fresh = true;
            let id = next_row_id(txn, table)?;
            table.row_key(&row, Some(id))
        }
        None => table.row_key(&row, None),
    };
    Ok(Prepared { row, key, fresh })
}

/// Writes a prepared row and its index entries, enforcing CHECK and
/// uniqueness; its references are checked at the end of the statement.
fn store_row(txn: &mut Txn, target: &Arc<Target>, prepared: Prepared, deferred: &mut Deferred) -> Result<Vec<Value>> {
    let Prepared { row, key, fresh } = prepared;
    let table = &target.table;
    target.rules.check(&row)?;
    if !fresh && txn.get(&key)?.is_some() {
        return Err(unique_violation(table, &table.primary_key));
    }
    for index in target.indexes.iter() {
        add_index_entry(txn, table, index, &row, &key[ROW_PREFIX_LEN..])?;
    }
    if target.rules.has_parents() {
        deferred.0.push((target.clone(), key.clone()));
    }
    txn.put(key, encode_row(&row));
    Ok(row)
}

/// The most levels of ON DELETE and ON UPDATE actions a statement may set off.
const MAX_CASCADE: usize = 1000;

/// A changed row: its key, the stored version and the new one.
type Change = (Vec<u8>, Vec<Value>, Vec<Value>);

/// Rewrites rows of `target`, then does what the foreign keys that refer to
/// them ask. Returns the rows as stored.
fn update_found(
    txn: &mut Txn,
    target: &Arc<Target>,
    changes: Vec<Change>,
    deferred: &mut Deferred,
    depth: usize,
) -> Result<Vec<Vec<Value>>> {
    // Remove every old version before writing any new one, so updates that
    // shuffle keys between rows (SET id = id + 1) don't collide midway.
    for (key, old, _) in &changes {
        remove_row(txn, &target.indexes, key, old);
    }
    let table = &target.table;
    let (mut olds, mut stored) = (Vec::with_capacity(changes.len()), Vec::with_capacity(changes.len()));
    for (key, old, new) in changes {
        let keep_key = table.primary_key.is_empty().then_some(key);
        stored.push(write_row(txn, target, new, keep_key, false, deferred)?);
        olds.push(old);
    }
    for child in integrity::children(txn, table)? {
        let mut follow = Vec::new();
        for (old, new) in olds.iter().zip(&stored) {
            let before: Vec<Value> = child.references.iter().map(|&c| old[c].clone()).collect();
            let after: Vec<Value> = child.references.iter().map(|&c| new[c].clone()).collect();
            if integrity::encoded(&before) == integrity::encoded(&after) {
                continue;
            }
            let found = child.rows(txn, &before)?;
            if found.is_empty() {
                continue;
            }
            match child.on_update {
                // Fine if another row now has the old key, as when keys shuffle.
                FkAction::NoAction => {
                    let (key, order) = ParentKey::of(table, &target.indexes, &child.references)
                        .ok_or_else(|| child.violation(&table.name))?;
                    let values: Vec<Value> = order.iter().map(|&j| before[j].clone()).collect();
                    if !key.exists(txn, table, &values)? {
                        return Err(child.violation(&table.name));
                    }
                }
                FkAction::Restrict => return Err(child.violation(&table.name)),
                action => follow.extend(found.into_iter().map(|(key, old)| {
                    let new = referencing_values(&child, &old, action, &after);
                    (key, old, new)
                })),
            }
        }
        if !follow.is_empty() {
            let target = cascade_target(txn, &child, depth)?;
            update_found(txn, &target, follow, deferred, depth + 1)?;
        }
    }
    Ok(stored)
}

/// Deletes rows of `target`, then does what the foreign keys that refer to
/// them ask. Returns the deleted rows.
fn delete_found(
    txn: &mut Txn,
    target: &Arc<Target>,
    rows: Vec<(Vec<u8>, Vec<Value>)>,
    deferred: &mut Deferred,
    depth: usize,
) -> Result<Vec<Vec<Value>>> {
    for (key, row) in &rows {
        remove_row(txn, &target.indexes, key, row);
    }
    let table = &target.table;
    for child in integrity::children(txn, table)? {
        let (mut gone, mut changed) = (Vec::new(), Vec::new());
        for (_, row) in &rows {
            let values: Vec<Value> = child.references.iter().map(|&c| row[c].clone()).collect();
            let found = child.rows(txn, &values)?;
            if found.is_empty() {
                continue;
            }
            match child.on_delete {
                FkAction::NoAction | FkAction::Restrict => return Err(child.violation(&table.name)),
                FkAction::Cascade => gone.extend(found),
                action => changed.extend(found.into_iter().map(|(key, old)| {
                    let new = referencing_values(&child, &old, action, &[]);
                    (key, old, new)
                })),
            }
        }
        if !gone.is_empty() {
            let target = cascade_target(txn, &child, depth)?;
            delete_found(txn, &target, gone, deferred, depth + 1)?;
        }
        if !changed.is_empty() {
            let target = cascade_target(txn, &child, depth)?;
            update_found(txn, &target, changed, deferred, depth + 1)?;
        }
    }
    Ok(rows.into_iter().map(|(_, row)| row).collect())
}

fn cascade_target(txn: &Txn, child: &Child, depth: usize) -> Result<Arc<Target>> {
    if depth >= MAX_CASCADE {
        return Err(sql_error(format!("foreign key actions nest more than {MAX_CASCADE} levels deep")));
    }
    Target::new(txn, child.table.clone(), child.indexes.clone())
}

/// A referencing row after its referenced row changed: its key columns
/// set to the new key (CASCADE), NULL or their defaults.
fn referencing_values(child: &Child, old: &[Value], action: FkAction, new_key: &[Value]) -> Vec<Value> {
    let mut row = old.to_vec();
    for (i, &c) in child.columns.iter().enumerate() {
        row[c] = match action {
            FkAction::Cascade => new_key[i].clone(),
            FkAction::SetDefault => child.table.columns[c].default_value(Some(Moment::now())),
            _ => Value::Null,
        };
    }
    row
}

/// The stored row (and its key) that a prepared row would collide with on
/// its primary key or on one of the unique indexes in `checked`.
fn find_conflict(
    txn: &Txn,
    table: &TableSchema,
    check_key: bool,
    checked: &[&IndexSchema],
    prepared: &Prepared,
) -> Result<Option<(Vec<u8>, Vec<Value>)>> {
    let width = table.columns.len();
    if check_key
        && !prepared.fresh
        && let Some(data) = txn.get(&prepared.key)?
    {
        return Ok(Some((prepared.key.clone(), decode_row(&data, width)?)));
    }
    for index in checked {
        let values = index.values_of(&prepared.row);
        if values.iter().any(Value::is_null) {
            continue;
        }
        let (lower, upper) = catalog::values_range(index, &values);
        let mut found = None;
        txn.scan(lower, upper, &mut |entry, _| {
            found = Some(entry.to_vec());
            Ok(false)
        })?;
        let Some(entry) = found else { continue };
        // The entry is the indexed values followed by the row's primary key.
        let mut rest = &entry[index.prefix().len()..];
        for _ in &index.columns {
            skip_key(&mut rest)?;
        }
        let mut key = table.row_prefix();
        key.extend_from_slice(rest);
        let data = txn.get(&key)?.ok_or_else(|| corruption(format!("index {} points at a missing row", index.name)))?;
        return Ok(Some((key, decode_row(&data, width)?)));
    }
    Ok(None)
}

/// Binds RETURNING items against the target table's columns.
fn bind_returning(table: &TableSchema, items: &[SelectItem], binder: &Binder) -> Result<(Vec<String>, Vec<BExpr>)> {
    let scope = Scope::of_table(table, &table.name);
    let (mut columns, mut exprs) = (Vec::new(), Vec::new());
    for item in items {
        match item {
            SelectItem::Wildcard | SelectItem::TableWildcard(_) => {
                if let SelectItem::TableWildcard(t) = item
                    && *t != table.name
                {
                    return Err(sql_error(format!("no such table: {t}")));
                }
                for (i, c) in table.columns.iter().enumerate() {
                    columns.push(c.name.clone());
                    exprs.push(BExpr::Column(i));
                }
            }
            SelectItem::Expr { expr, alias } => {
                columns.push(match (alias, expr) {
                    (Some(alias), _) => alias.clone(),
                    (None, Expr::Column { name, .. }) => name.clone(),
                    (None, e) => e.to_string(),
                });
                exprs.push(binder.bind(expr, &scope, "RETURNING")?);
            }
        }
    }
    Ok((columns, exprs))
}

/// The result of a write: its RETURNING rows, or how many rows it changed.
fn written(
    txn: &Txn,
    command: &'static str,
    table: &TableSchema,
    items: &[SelectItem],
    params: &[Value],
    count: usize,
    rows: Vec<Vec<Value>>,
) -> Result<QueryResult> {
    if items.is_empty() {
        return Ok(QueryResult::Changed { command, rows: count as u64 });
    }
    let (columns, exprs) = bind_returning(table, items, &Binder::new(params, txn))?;
    let env = Env::new(params, Some(txn));
    let rows = rows
        .iter()
        .map(|row| exprs.iter().map(|e| e.eval(row, &env)).collect::<Result<Vec<_>>>())
        .collect::<Result<Vec<_>>>()?;
    Ok(QueryResult::Rows(Rows { columns, rows }))
}

fn remove_row(txn: &mut Txn, indexes: &[Arc<IndexSchema>], key: &[u8], row: &[Value]) {
    for index in indexes {
        txn.delete(index.entry_key(row, &key[ROW_PREFIX_LEN..]));
    }
    txn.delete(key.to_vec());
}

/// What an INSERT needs to know about its target, worked out once per
/// prepared statement.
pub struct InsertPlan {
    target: Arc<Target>,
    /// The column each supplied value goes into.
    columns: Vec<usize>,
}

fn plan_insert(txn: &Txn, insert: &Insert) -> Result<InsertPlan> {
    let target = Target::of(txn, &insert.table)?;
    let columns = match &insert.columns {
        Some(names) => column_positions(&target.table, names)?,
        None => (0..target.table.columns.len()).collect(),
    };
    Ok(InsertPlan { target, columns })
}

/// How an INSERT ... ON CONFLICT handles a row that collides.
struct Upsert {
    /// Whether a taken primary key counts as a conflict.
    check_key: bool,
    /// The unique indexes whose values count.
    checked: Vec<Arc<IndexSchema>>,
    /// DO UPDATE's assignments and WHERE, over the stored row followed by
    /// the proposed one (`excluded`); none for DO NOTHING.
    update: Option<(Assignments, Option<BExpr>)>,
}

/// Bound SET assignments: each column with its new value.
type Assignments = Vec<(usize, BExpr)>;

fn plan_upsert(
    insert: &Insert,
    table: &TableSchema,
    indexes: &[Arc<IndexSchema>],
    binder: &Binder,
) -> Result<Option<Upsert>> {
    let Some(on_conflict) = &insert.on_conflict else { return Ok(None) };
    let unique = indexes.iter().filter(|i| i.unique);
    let (check_key, checked): (bool, Vec<_>) = if on_conflict.target.is_empty() {
        (!table.primary_key.is_empty(), unique.cloned().collect())
    } else {
        let mut wanted = column_positions(table, &on_conflict.target)?;
        wanted.sort_unstable();
        let same = |columns: &[usize]| {
            let mut columns = columns.to_vec();
            columns.sort_unstable();
            columns == wanted
        };
        let checked: Vec<_> = unique.filter(|i| same(&i.columns)).cloned().collect();
        let check_key = !table.primary_key.is_empty() && same(&table.primary_key);
        if !check_key && checked.is_empty() {
            return Err(sql_error(format!(
                "ON CONFLICT ({}) matches no primary key or unique index",
                on_conflict.target.join(", ")
            )));
        }
        (check_key, checked)
    };
    let update = match &on_conflict.action {
        ConflictAction::Nothing => None,
        ConflictAction::Update { assignments, filter } => {
            let mut scope = Scope::of_table(table, &table.name);
            scope.add_table(table, "excluded");
            // A bare column name means the stored row's, as in SQLite and MySQL.
            let qualify = |e: &Expr| {
                let mut e = e.clone();
                e.rewrite(&mut |e| {
                    if let Expr::Column { table: t @ None, .. } = e {
                        *t = Some(table.name.clone());
                    }
                });
                e
            };
            let assignments: Vec<_> = assignments.iter().map(|(c, e)| (c.clone(), qualify(e))).collect();
            let assignments = bind_assignments(table, &assignments, &scope, binder)?;
            let filter = filter.as_ref().map(|f| binder.bind(&qualify(f), &scope, "WHERE")).transpose()?;
            Some((assignments, filter))
        }
    };
    Ok(Some(Upsert { check_key, checked, update }))
}

/// Binds SET assignments to the columns they write.
fn bind_assignments(
    table: &TableSchema,
    assignments: &[(String, Expr)],
    scope: &Scope,
    binder: &Binder,
) -> Result<Assignments> {
    let mut bound: Assignments = Vec::new();
    for (name, expr) in assignments {
        let c = column_positions(table, std::slice::from_ref(name))?[0];
        if bound.iter().any(|(seen, _)| *seen == c) {
            return Err(sql_error(format!("column {name} is assigned more than once")));
        }
        bound.push((c, binder.bind(expr, scope, "SET")?));
    }
    Ok(bound)
}

fn insert_rows(txn: &mut Txn, insert: &Insert, params: &[Value], plans: &PlanCache) -> Result<QueryResult> {
    let plan = plans.plan(txn, Planned::insert, Planned::Insert, || plan_insert(txn, insert))?;
    let InsertPlan { target, columns } = &*plan;
    let table = &target.table;
    let upsert = plan_upsert(insert, table, &target.indexes, &Binder::new(params, txn))?;
    let returning = !insert.returning.is_empty();
    let now = table.columns.iter().any(|c| c.default_now).then(Moment::now);
    let (mut count, mut returned, mut deferred) = (0, Vec::new(), Deferred::default());
    let mut insert_one = |txn: &mut Txn, values: &mut dyn Iterator<Item = Result<Value>>, supplied: usize| {
        if supplied != columns.len() {
            return Err(sql_error(format!("{supplied} values supplied for {} columns", columns.len())));
        }
        let mut row: Vec<Value> = table.columns.iter().map(|c| c.default_value(now)).collect();
        for (&column, value) in columns.iter().zip(values) {
            row[column] = value?;
        }
        let stored = match &upsert {
            None => write_row(txn, target, row, None, true, &mut deferred)?,
            Some(upsert) => {
                let prepared = prepare_row(txn, table, row, None, true)?;
                let checked: Vec<&IndexSchema> = upsert.checked.iter().map(|i| &**i).collect();
                match (find_conflict(txn, table, upsert.check_key, &checked, &prepared)?, &upsert.update) {
                    (None, _) => store_row(txn, target, prepared, &mut deferred)?,
                    (Some(_), None) => return Ok(()),
                    (Some((key, old)), Some((assignments, filter))) => {
                        let mut both = old.clone();
                        both.extend(prepared.row);
                        let env = Env::new(params, Some(txn));
                        if !filter.as_ref().map_or(Ok(true), |f| f.matches(&both, &env))? {
                            return Ok(());
                        }
                        let mut new = old.clone();
                        for (c, e) in assignments {
                            new[*c] = e.eval(&both, &env)?;
                        }
                        drop(env);
                        let mut stored = update_found(txn, target, vec![(key, old, new)], &mut deferred, 0)?;
                        stored.pop().unwrap_or_default()
                    }
                }
            }
        };
        count += 1;
        if returning {
            returned.push(stored);
        }
        Ok(())
    };
    match &insert.source {
        // Parameters and literals, by far the most common values, skip binding.
        InsertSource::Values(rows) if rows.iter().flatten().all(|e| matches!(e, Expr::Param(_) | Expr::Literal(_))) => {
            let value = |e: &Expr| match e {
                Expr::Param(i) if *i < params.len() => Ok(params[*i].clone()),
                Expr::Literal(v) => Ok(v.clone()),
                e => Err(sql_error(format!("no value for {e}"))),
            };
            for exprs in rows {
                insert_one(txn, &mut exprs.iter().map(value), exprs.len())?;
            }
        }
        InsertSource::Values(rows) => {
            // Every value first (they may run subqueries), then the writes.
            let binder = Binder::new(params, txn);
            let values = rows
                .iter()
                .map(|exprs| exprs.iter().map(|e| binder.constant(e, "VALUES")).collect::<Result<Vec<_>>>())
                .collect::<Result<Vec<_>>>()?;
            for row in values {
                let supplied = row.len();
                insert_one(txn, &mut row.into_iter().map(Ok), supplied)?;
            }
        }
        InsertSource::Select(select) => {
            let plan = Arc::new(plan_select(txn, select, &Binder::new(params, txn))?);
            let rows = run_select(txn, &plan, params)?.rows;
            for row in &rows {
                insert_one(txn, &mut row.iter().cloned().map(Ok), row.len())?;
            }
        }
    }
    deferred.verify(txn)?;
    written(txn, "INSERT", table, &insert.returning, params, count, returned)
}

fn update_rows(txn: &mut Txn, update: &Update, params: &[Value]) -> Result<QueryResult> {
    let binder = &Binder::new(params, txn);
    let target = Target::of(txn, &update.table)?;
    let (table, indexes) = (&target.table, &target.indexes);
    let scope = Scope::of_table(table, &table.name);
    let assignments = bind_assignments(table, &update.assignments, &scope, binder)?;
    let (access, filter) = write_target(table, indexes, update.filter.as_ref(), &scope, binder)?;
    let env = Env::new(params, Some(txn));
    let matched = matching_rows(txn, table, &access, filter.as_ref(), &env)?;

    // Every SET expression sees the row as it was before the statement.
    let mut updated = Vec::with_capacity(matched.len());
    for (key, old) in matched {
        let mut new = old.clone();
        for (c, e) in &assignments {
            new[*c] = e.eval(&old, &env)?;
        }
        updated.push((key, old, new));
    }
    drop(env);
    let count = updated.len();
    let mut deferred = Deferred::default();
    let returned = update_found(txn, &target, updated, &mut deferred, 0)?;
    deferred.verify(txn)?;
    written(txn, "UPDATE", table, &update.returning, params, count, returned)
}

fn delete_rows(txn: &mut Txn, delete: &Delete, params: &[Value]) -> Result<QueryResult> {
    let target = Target::of(txn, &delete.table)?;
    let (table, indexes) = (&target.table, &target.indexes);
    let scope = Scope::of_table(table, &table.name);
    let (access, filter) = write_target(table, indexes, delete.filter.as_ref(), &scope, &Binder::new(params, txn))?;
    let matched = matching_rows(txn, table, &access, filter.as_ref(), &Env::new(params, Some(txn)))?;
    let count = matched.len();
    let mut deferred = Deferred::default();
    let returned = delete_found(txn, &target, matched, &mut deferred, 0)?;
    deferred.verify(txn)?;
    written(txn, "DELETE", table, &delete.returning, params, count, returned)
}
