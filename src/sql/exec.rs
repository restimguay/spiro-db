//! Statement execution.

use std::cell::Cell;
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::ops::Bound;
use std::sync::{Arc, Mutex};

use crate::coding::Reader;
use crate::error::{Error, Result, corruption};

use super::ast::{CreateIndex, CreateTable, Delete, Expr, Insert, InsertSource, JoinKind, Statement, Update};
use super::catalog::{self, Column, IndexSchema, ROW_PREFIX_LEN, TableSchema};
use super::encoding::{decode_key, decode_row_into, encode_key, encode_keys, encode_row, prefix_end, skip_key};
use super::expr::{Accumulator, BExpr, Binder, Scope};
use super::notify::MAX_PAYLOAD;
use super::plan::{
    Access, GroupPlan, JoinPlan, KeyBound, SelectPlan, SortKey, choose_access, index_covers, plan_select,
};
use super::txn::Txn;
use super::value::{Value, sql_error};
use super::{QueryResult, Rows};

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
    let binder = Binder { params };
    match statement {
        Statement::Select(select) => {
            let plan = plans.plan(txn, Planned::select, Planned::Select, || plan_select(txn, select, &binder))?;
            Ok(QueryResult::Rows(run_select(txn, &plan, params)?))
        }
        Statement::Explain(inner) => explain(txn, inner, &binder),
        Statement::CreateTable(create) => create_table(txn, create, &binder),
        Statement::DropTable { name, if_exists } => drop_table(txn, name, *if_exists),
        Statement::CreateIndex(create) => create_index(txn, create),
        Statement::DropIndex { name, if_exists } => drop_index(txn, name, *if_exists),
        Statement::Insert(insert) => insert_rows(txn, insert, &binder, plans),
        Statement::Update(update) => update_rows(txn, update, &binder),
        Statement::Delete(delete) => delete_rows(txn, delete, &binder),
        Statement::Notify { channel, payload } => {
            let payload = match payload {
                None => String::new(),
                Some(e) => match binder.constant(e, "NOTIFY")? {
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
        Statement::Begin | Statement::Commit | Statement::Rollback | Statement::Listen(_) | Statement::Unlisten(_) => {
            Err(sql_error("this statement is handled by the session"))
        }
    }
}

fn explain(txn: &Txn, statement: &Statement, binder: &Binder) -> Result<QueryResult> {
    let lines = match statement {
        Statement::Select(select) => plan_select(txn, select, binder)?.explain(),
        Statement::Update(Update { table, filter, .. }) | Statement::Delete(Delete { table, filter }) => {
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
type Sink<'a> = &'a mut dyn FnMut(&[u8], &mut [Value]) -> Result<bool>;

/// Where a read looks and what it decodes.
struct ReadSpec<'a> {
    table: &'a TableSchema,
    access: &'a Access,
    /// Only these columns are decoded (the rest read as NULL), and an index
    /// that holds them all answers without touching the rows. `None`: all.
    needed: Option<&'a [bool]>,
    /// Set by the read when the access path narrowed it, rather than falling
    /// back to a full scan; the conditions the path enforces then hold.
    narrowed: Cell<bool>,
}

impl<'a> ReadSpec<'a> {
    fn new(table: &'a TableSchema, access: &'a Access, needed: Option<&'a [bool]>) -> Self {
        ReadSpec { table, access, needed, narrowed: Cell::new(false) }
    }
}

/// Reads the rows `spec` selects. `outer` is the row that key expressions
/// are evaluated against.
fn read(txn: &Txn, spec: &ReadSpec, outer: &[Value], params: &[Value], sink: Sink) -> Result<()> {
    let (table, access, needed) = (spec.table, spec.access, spec.needed);
    // Key values are converted to their column's type. A value that can't be
    // (NULL, or a type that could never compare equal) falls back to a full
    // scan, and the row filter then gives the right answer.
    let key_values = |exprs: &[BExpr], cols: &[usize]| -> Result<Option<Vec<Value>>> {
        let mut values = Vec::with_capacity(exprs.len());
        for (e, &c) in exprs.iter().zip(cols) {
            match e.eval(outer, params)?.assign_to(table.columns[c].ty) {
                Ok(v) if !v.is_null() => values.push(v),
                _ => return Ok(None),
            }
        }
        Ok(Some(values))
    };
    let range = |prefix: Vec<u8>, column: usize, lower: &Option<KeyBound>, upper: &Option<KeyBound>| {
        let ty = table.columns[column].ty;
        let encoded = |b: &KeyBound| -> Result<Option<Vec<u8>>> {
            Ok(match b.value.eval(outer, params)?.assign_to(ty) {
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
    params: &[Value],
) -> Result<Vec<(Vec<u8>, Vec<Value>)>> {
    let mut rows = Vec::new();
    read(txn, &ReadSpec::new(table, access, None), &[], params, &mut |key, row| {
        if filter.map_or(Ok(true), |f| f.matches(row, params))? {
            rows.push((key.to_vec(), row.to_vec()));
        }
        Ok(true)
    })?;
    Ok(rows)
}

// ---------------------------------------------------------------- SELECT

/// Evaluates a LIMIT or OFFSET expression.
fn count(e: &Option<BExpr>, params: &[Value], clause: &str) -> Result<Option<usize>> {
    let Some(e) = e else { return Ok(None) };
    match e.eval(&[], params)? {
        Value::Integer(n) if n >= 0 => Ok(Some(n as usize)),
        v => Err(sql_error(format!("{clause} must be a non-negative integer, got {}", v.literal()))),
    }
}

pub fn run_select(txn: &Txn, plan: &SelectPlan, params: &[Value]) -> Result<Rows> {
    let limit = count(&plan.limit, params, "LIMIT")?;
    let offset = count(&plan.offset, params, "OFFSET")?.unwrap_or(0);
    // Without aggregation, DISTINCT or a sort, rows can be emitted as they are
    // read, and reading can stop as soon as LIMIT is satisfied.
    let streaming = plan.grouping.is_none() && !plan.distinct && (plan.order_by.is_empty() || plan.presorted);
    let wanted = if streaming { limit.map(|l| l.saturating_add(offset)) } else { None };

    // Each output row with the values of its ORDER BY expressions.
    let mut out: Vec<(Vec<Value>, Vec<Value>)> = Vec::new();
    // Aggregation state: one set of accumulators without GROUP BY, otherwise
    // one per group, keyed by the encoded group key.
    let new_accumulators = |g: &GroupPlan| g.aggregates.iter().map(Accumulator::new).collect::<Vec<_>>();
    let mut single = plan.grouping.as_ref().filter(|g| g.keys.is_empty()).map(new_accumulators);
    let mut groups: BTreeMap<Vec<u8>, (Vec<Value>, Vec<Accumulator>)> = BTreeMap::new();
    let (mut key_values, mut key_bytes) = (Vec::new(), Vec::new());

    for_each_row(txn, plan, params, &mut |row| {
        if let Some(filter) = &plan.filter
            && !filter.matches(row, params)?
        {
            return Ok(true);
        }
        let Some(g) = &plan.grouping else {
            out.push(project(plan, row, params)?);
            return Ok(wanted.is_none_or(|n| out.len() < n));
        };
        let accumulators = match &mut single {
            Some(accumulators) => accumulators,
            None => {
                key_values.clear();
                key_bytes.clear();
                for k in &g.keys {
                    let v = k.eval(row, params)?;
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
            acc.add(aggregate, row, params)?;
        }
        Ok(true)
    })?;

    if plan.grouping.is_some() {
        // Without GROUP BY there is exactly one group, even with no input rows.
        let finished = single.map(|accumulators| (Vec::new(), accumulators)).into_iter().chain(groups.into_values());
        for (mut row, accumulators) in finished {
            row.extend(accumulators.into_iter().map(Accumulator::finish));
            if let Some(having) = &plan.having
                && !having.matches(&row, params)?
            {
                continue;
            }
            out.push(project(plan, &mut row, params)?);
        }
    }
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
            for (i, (key, desc)) in plan.order_by.iter().enumerate() {
                let (a, b) = match key {
                    SortKey::Output(c) => (&a_row[*c], &b_row[*c]),
                    SortKey::Expr(_) => (&a_keys[i], &b_keys[i]),
                };
                let ordering = if *desc { b.total_cmp(a) } else { a.total_cmp(b) };
                if ordering.is_ne() {
                    return ordering;
                }
            }
            std::cmp::Ordering::Equal
        });
    }
    let rows = out.into_iter().skip(offset).take(limit.unwrap_or(usize::MAX)).map(|(row, _)| row).collect();
    Ok(Rows { columns: plan.columns.clone(), rows })
}

/// Computes an output row and its ORDER BY values. Columns marked movable
/// are taken out of `row` rather than copied.
fn project(plan: &SelectPlan, row: &mut [Value], params: &[Value]) -> Result<(Vec<Value>, Vec<Value>)> {
    // Sort values first: they may read columns before any are moved out.
    let sort_values = plan
        .order_by
        .iter()
        .map(|(key, _)| match key {
            SortKey::Expr(e) => e.eval(row, params),
            SortKey::Output(_) => Ok(Value::Null),
        })
        .collect::<Result<Vec<_>>>()?;
    let mut projected = Vec::with_capacity(plan.projection.len());
    for (e, &movable) in plan.projection.iter().zip(&plan.movable) {
        projected.push(match e {
            BExpr::Column(c) if movable => std::mem::replace(&mut row[*c], Value::Null),
            e => e.eval(row, params)?,
        });
    }
    Ok((projected, sort_values))
}

/// Feeds every row produced by FROM (joins included) and the base-table
/// filter to `emit`, which returns `false` to stop.
fn for_each_row(
    txn: &Txn,
    plan: &SelectPlan,
    params: &[Value],
    emit: &mut dyn FnMut(&mut [Value]) -> Result<bool>,
) -> Result<()> {
    let Some(base) = &plan.base else {
        // No FROM: a single row with no columns.
        if plan.base_filter.as_ref().map_or(Ok(true), |f| f.matches(&[], params))? {
            emit(&mut [])?;
        }
        return Ok(());
    };
    // Joined tables read in full are read once and kept here.
    let mut cache: Vec<Option<Vec<Vec<Value>>>> = vec![None; plan.joins.len()];
    let spec = ReadSpec::new(&base.schema, &base.access, Some(&base.needed));
    read(txn, &spec, &[], params, &mut |_, row| {
        // Conditions the access path enforces needn't be checked again,
        // unless the read fell back to a full scan.
        let filter = if spec.narrowed.get() { &plan.base_residual } else { &plan.base_filter };
        if let Some(filter) = filter
            && !filter.matches(row, params)?
        {
            return Ok(true);
        }
        if plan.joins.is_empty() {
            return emit(row);
        }
        join(txn, &plan.joins, &mut cache, row, params, emit)
    })
}

/// Nested-loop join of `row` with the remaining `joins`.
fn join(
    txn: &Txn,
    joins: &[JoinPlan],
    cache: &mut [Option<Vec<Vec<Value>>>],
    row: &mut [Value],
    params: &[Value],
    emit: &mut dyn FnMut(&mut [Value]) -> Result<bool>,
) -> Result<bool> {
    let (Some((j, rest)), Some((cached, rest_cache))) = (joins.split_first(), cache.split_first_mut()) else {
        return emit(row);
    };
    let table = &j.table;
    let collect = |access: &Access, outer: &[Value]| -> Result<Vec<Vec<Value>>> {
        let mut rows = Vec::new();
        let spec = ReadSpec::new(&table.schema, access, Some(&table.needed));
        read(txn, &spec, outer, params, &mut |_, r| {
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
            && !on.matches(&combined, params)?
        {
            continue;
        }
        matched = true;
        if !join(txn, rest, rest_cache, &mut combined, params, emit)? {
            return Ok(false);
        }
    }
    if !matched && j.kind == JoinKind::Left {
        combined.clear();
        combined.extend_from_slice(row);
        combined.resize(row.len() + table.schema.columns.len(), Value::Null);
        return join(txn, rest, rest_cache, &mut combined, params, emit);
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

fn create_table(txn: &mut Txn, create: &CreateTable, binder: &Binder) -> Result<QueryResult> {
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
        let default = match &def.default {
            None => None,
            Some(e) => Some(binder.constant(e, "DEFAULT")?.assign_to(def.ty).map_err(|v| {
                sql_error(format!("default for column {} is {}, which is not {}", def.name, v.literal(), def.ty))
            })?),
        };
        columns.push(Column { name: def.name.clone(), ty: def.ty, not_null: def.not_null, default });
    }
    let mut table =
        TableSchema { id: catalog::next_id(txn)?, name: create.name.clone(), columns, primary_key: Vec::new() };

    let inline: Vec<usize> = create.columns.iter().enumerate().filter(|(_, d)| d.primary_key).map(|(i, _)| i).collect();
    table.primary_key = match (inline.len(), create.primary_key.is_empty()) {
        (0, _) => column_positions(&table, &create.primary_key)?,
        (1, true) => inline,
        _ => return Err(sql_error(format!("table {} has more than one primary key", create.name))),
    };
    for &c in &table.primary_key {
        table.columns[c].not_null = true;
    }
    catalog::put_table(txn, &table);

    // UNIQUE constraints are enforced by unique indexes.
    let mut uniques: Vec<Vec<usize>> =
        create.columns.iter().enumerate().filter(|(_, d)| d.unique).map(|(i, _)| vec![i]).collect();
    for names in &create.unique {
        uniques.push(column_positions(&table, names)?);
    }
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
    let rows = matching_rows(txn, &table, &Access::Scan, None, &[])?;
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

/// Keeps future row ids clear of one chosen explicitly.
fn observe_row_id(txn: &mut Txn, table: &TableSchema, id: i64) -> Result<()> {
    let next = txn.sequence(&table.sequence_key(), stored_counter)?;
    if id >= *next {
        *next = id.saturating_add(1);
    }
    Ok(())
}

/// Validates `row` and writes it, with its index entries. `key` is the row's
/// existing key, for rewriting a row of a table without a primary key.
fn write_row(
    txn: &mut Txn,
    table: &TableSchema,
    indexes: &[Arc<IndexSchema>],
    mut row: Vec<Value>,
    key: Option<Vec<u8>>,
    inserting: bool,
) -> Result<()> {
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
    if let Some(c) = table.auto_increment_column() {
        match row[c] {
            Value::Null if inserting => row[c] = Value::Integer(next_row_id(txn, table)?),
            Value::Integer(id) => observe_row_id(txn, table, id)?,
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
            let id = next_row_id(txn, table)?;
            table.row_key(&row, Some(id))
        }
        None => table.row_key(&row, None),
    };
    if txn.get(&key)?.is_some() {
        return Err(unique_violation(table, &table.primary_key));
    }
    for index in indexes {
        add_index_entry(txn, table, index, &row, &key[ROW_PREFIX_LEN..])?;
    }
    txn.put(key, encode_row(&row));
    Ok(())
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
    table: Arc<TableSchema>,
    indexes: Arc<[Arc<IndexSchema>]>,
    /// The column each supplied value goes into.
    targets: Vec<usize>,
}

fn plan_insert(txn: &Txn, insert: &Insert) -> Result<InsertPlan> {
    let table = catalog::require_table(txn, &insert.table)?;
    let indexes = catalog::indexes_of(txn, &table.name)?;
    let targets = match &insert.columns {
        Some(names) => column_positions(&table, names)?,
        None => (0..table.columns.len()).collect(),
    };
    Ok(InsertPlan { table, indexes, targets })
}

fn insert_rows(txn: &mut Txn, insert: &Insert, binder: &Binder, plans: &PlanCache) -> Result<QueryResult> {
    let plan = plans.plan(txn, Planned::insert, Planned::Insert, || plan_insert(txn, insert))?;
    let InsertPlan { table, indexes, targets } = &*plan;
    let insert_one = |txn: &mut Txn, values: &mut dyn Iterator<Item = Result<Value>>, supplied: usize| {
        if supplied != targets.len() {
            return Err(sql_error(format!("{supplied} values supplied for {} columns", targets.len())));
        }
        let mut row: Vec<Value> = table.columns.iter().map(|c| c.default.clone().unwrap_or(Value::Null)).collect();
        for (&target, value) in targets.iter().zip(values) {
            row[target] = value?;
        }
        write_row(txn, table, indexes, row, None, true)
    };
    // Parameters and literals, by far the most common values, skip binding.
    let value = |e: &Expr| match e {
        Expr::Param(i) if *i < binder.params.len() => Ok(binder.params[*i].clone()),
        Expr::Literal(v) => Ok(v.clone()),
        e => binder.constant(e, "VALUES"),
    };
    let count = match &insert.source {
        InsertSource::Values(rows) => {
            for exprs in rows {
                insert_one(txn, &mut exprs.iter().map(value), exprs.len())?;
            }
            rows.len()
        }
        InsertSource::Select(select) => {
            let rows = run_select(txn, &plan_select(txn, select, binder)?, binder.params)?.rows;
            for row in &rows {
                insert_one(txn, &mut row.iter().cloned().map(Ok), row.len())?;
            }
            rows.len()
        }
    };
    Ok(QueryResult::Changed { command: "INSERT", rows: count as u64 })
}

fn update_rows(txn: &mut Txn, update: &Update, binder: &Binder) -> Result<QueryResult> {
    let table = catalog::require_table(txn, &update.table)?;
    let indexes = catalog::indexes_of(txn, &table.name)?;
    let scope = Scope::of_table(&table, &table.name);
    let mut assignments: Vec<(usize, BExpr)> = Vec::new();
    for (name, expr) in &update.assignments {
        let c = column_positions(&table, std::slice::from_ref(name))?[0];
        if assignments.iter().any(|(seen, _)| *seen == c) {
            return Err(sql_error(format!("column {name} is assigned more than once")));
        }
        assignments.push((c, binder.bind(expr, &scope, "UPDATE")?));
    }
    let (access, filter) = write_target(&table, &indexes, update.filter.as_ref(), &scope, binder)?;
    let matched = matching_rows(txn, &table, &access, filter.as_ref(), binder.params)?;

    // Every SET expression sees the row as it was before the statement.
    let mut updated = Vec::with_capacity(matched.len());
    for (key, old) in matched {
        let mut new = old.clone();
        for (c, e) in &assignments {
            new[*c] = e.eval(&old, binder.params)?;
        }
        updated.push((key, old, new));
    }
    // Remove every old version before writing any new one, so updates that
    // shuffle keys between rows (SET id = id + 1) don't collide midway.
    for (key, old, _) in &updated {
        remove_row(txn, &indexes, key, old);
    }
    let count = updated.len() as u64;
    for (key, _, new) in updated {
        let keep_key = table.primary_key.is_empty().then_some(key);
        write_row(txn, &table, &indexes, new, keep_key, false)?;
    }
    Ok(QueryResult::Changed { command: "UPDATE", rows: count })
}

fn delete_rows(txn: &mut Txn, delete: &Delete, binder: &Binder) -> Result<QueryResult> {
    let table = catalog::require_table(txn, &delete.table)?;
    let indexes = catalog::indexes_of(txn, &table.name)?;
    let scope = Scope::of_table(&table, &table.name);
    let (access, filter) = write_target(&table, &indexes, delete.filter.as_ref(), &scope, binder)?;
    let matched = matching_rows(txn, &table, &access, filter.as_ref(), binder.params)?;
    for (key, row) in &matched {
        remove_row(txn, &indexes, key, row);
    }
    Ok(QueryResult::Changed { command: "DELETE", rows: matched.len() as u64 })
}
