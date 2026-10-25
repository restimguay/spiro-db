//! Batched aggregation, for GROUP BY and aggregate queries on one table.
//!
//! The row-at-a-time executor evaluates expressions on boxed values and finds
//! each row's group in an ordered map. Here rows are gathered in batches of
//! [`BATCH`], and the columns the query reads become plain arrays: integers
//! and reals unboxed, with a validity mask for NULLs. Expressions over them
//! run as tight loops, and groups are found by hashing, with a fast path for
//! a single integer key.
//!
//! Whatever has no fast path (text functions, CASE, an integer overflow, a
//! division by zero) is evaluated row by row with the ordinary evaluator, so
//! results and errors are the same as the row-at-a-time executor's.
//!
//! A full scan of a large table is also split by key range across threads.
//! Each aggregates its share, and the partial groups are merged.

use std::any::Any;
use std::borrow::Cow;
use std::collections::VecDeque;
use std::collections::{BTreeMap, HashMap};
use std::hash::{BuildHasherDefault, Hasher};
use std::ops::Bound;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;

use crate::db::ReadView;
use crate::error::Result;

use super::ast::BinaryOp;
use super::catalog::TableSchema;
use super::columnar::{self, Chunk, ColumnView, Kind};
use super::encoding::{RawValue, encode_key, visit_row};
use super::exec::{ReadSpec, read};
use super::expr::{Accumulator, Aggregate, BExpr, Env};
use super::plan::{Access, GroupPlan, SelectPlan};
use super::txn::Txn;
use super::value::{DataType, Value, cmp_f64};

/// Rows per batch: enough to amortize the per-batch work, few enough that a
/// batch's columns stay in the CPU cache.
const BATCH: usize = 1024;

/// A scan is split across threads only if each part gets at least this many
/// data blocks (about 4 KiB each); smaller scans aren't worth the threads.
const MIN_BLOCKS_PER_PART: usize = if cfg!(test) { 2 } else { 16 };

/// Parts per thread. Threads that finish early (on the faster cores) take
/// more parts, so no thread is left with a long tail.
const PARTS_PER_THREAD: usize = 8;

/// Aggregated groups, ready for HAVING and projection: each group's key
/// values and accumulators, in the order of the encoded keys.
pub type Groups = Vec<(Vec<Value>, Vec<Accumulator>)>;

#[cfg(test)]
thread_local! {
    /// Lets tests compare against the row-at-a-time executor.
    pub static DISABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the batched path runs this query.
pub fn applies(plan: &SelectPlan) -> bool {
    #[cfg(test)]
    if DISABLED.with(|d| d.get()) {
        return false;
    }
    plan.base.is_some() && plan.joins.is_empty() && plan.grouping.is_some() && plan.is_simple()
}

/// Runs the query's read, filters and aggregation. Only for plans that
/// [`applies`] accepts.
pub fn aggregate(txn: &Txn, plan: &Arc<SelectPlan>, params: &[Value]) -> Result<Groups> {
    let base = plan.base.as_ref().expect("a FROM table");
    if let Some(job) = split(txn, plan, params) {
        return job.run();
    }
    let mut worker = Worker::new(plan, params);
    if let Access::Scan = base.access {
        let (lower, upper) = base.schema.rows_range();
        match txn.shared_view(&lower, &upper) {
            // Column chunks where the files have them.
            Some(view) => columnar::scan(&view, &base.schema, &base.needed, &lower, &upper, &mut worker)?,
            // The transaction's own writes must be merged in: rows it is.
            None => txn.scan(lower, upper, &mut |_, data| {
                worker.push_stored(data)?;
                Ok(true)
            })?,
        }
        worker.flush()?;
        return merge(vec![worker.groups]);
    }
    let spec = ReadSpec::new(&base.schema, &base.access, Some(&base.needed));
    read(txn, &spec, &[], &Env::new(params, Some(txn)), &mut |_, row| {
        worker.narrowed = spec.narrowed.get();
        worker.push(row)?;
        Ok(true)
    })?;
    worker.flush()?;
    merge(vec![worker.groups])
}

type Range = (Bound<Vec<u8>>, Bound<Vec<u8>>);

/// A full scan shared out among threads: the key ranges they take turns
/// claiming, and the partial groups they hand back.
struct Job {
    plan: Arc<SelectPlan>,
    params: Vec<Value>,
    view: ReadView,
    ranges: Vec<Range>,
    /// How many helpers from the pool to ask for, besides the calling thread.
    helpers: usize,
    next: AtomicUsize,
    failed: AtomicBool,
    state: Mutex<JobState>,
    finished: Condvar,
}

#[derive(Default)]
struct JobState {
    /// Set once the calling thread is done: helpers that start later have
    /// nothing left to do, and aren't waited for.
    closed: bool,
    /// Helpers still working.
    running: usize,
    results: Vec<Result<GroupTable>>,
    panic: Option<Box<dyn Any + Send>>,
}

/// For a full scan big enough to share out, the job that does it.
fn split(txn: &Txn, plan: &Arc<SelectPlan>, params: &[Value]) -> Option<Arc<Job>> {
    let (base, group) = (plan.base.as_ref()?, plan.grouping.as_ref()?);
    // DISTINCT aggregates remember the values they've seen, which don't merge.
    if !matches!(base.access, Access::Scan) || group.aggregates.iter().any(|a| a.distinct) {
        return None;
    }
    let mut threads = Pool::get().threads + 1;
    if txn.scan_threads() > 0 {
        threads = threads.min(txn.scan_threads());
    }
    if threads < 2 {
        return None;
    }
    let (lower, upper) = base.schema.rows_range();
    let view = txn.shared_view(&lower, &upper)?;
    let points = view.split_points(&lower, &upper);
    let parts = (points.len() / MIN_BLOCKS_PER_PART).min(threads * PARTS_PER_THREAD);
    if parts < 2 {
        return None;
    }
    let mut ranges = Vec::with_capacity(parts);
    let mut start = lower;
    for k in 1..parts {
        let point = points[k * points.len() / parts].to_vec();
        ranges.push((start, Bound::Excluded(point.clone())));
        start = Bound::Included(point);
    }
    ranges.push((start, upper));
    drop(points);
    Some(Arc::new(Job {
        plan: plan.clone(),
        params: params.to_vec(),
        view,
        helpers: threads.min(parts) - 1,
        ranges,
        next: AtomicUsize::new(0),
        failed: AtomicBool::new(false),
        state: Mutex::default(),
        finished: Condvar::new(),
    }))
}

impl Job {
    /// Runs the job on this thread and on idle pool threads.
    fn run(self: &Arc<Self>) -> Result<Groups> {
        let pool = Pool::get();
        pool.submit(self, self.helpers);
        let mine = self.work();
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        drop(state);
        pool.withdraw(self);
        let mut state = self.state.lock().unwrap();
        while state.running > 0 {
            state = self.finished.wait(state).unwrap();
        }
        if let Some(panic) = state.panic.take() {
            std::panic::resume_unwind(panic);
        }
        let mut tables = vec![mine?];
        for result in std::mem::take(&mut state.results) {
            tables.push(result?);
        }
        merge(tables)
    }

    /// On a pool thread: joins in, unless the job is already done.
    fn help(&self) {
        {
            let mut state = self.state.lock().unwrap();
            if state.closed {
                return;
            }
            state.running += 1;
        }
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| self.work()));
        let mut state = self.state.lock().unwrap();
        match outcome {
            Ok(result) => state.results.push(result),
            Err(panic) => {
                self.failed.store(true, Ordering::Relaxed);
                state.panic = Some(panic);
            }
        }
        state.running -= 1;
        self.finished.notify_all();
    }

    /// Claims ranges until none are left, aggregating them.
    fn work(&self) -> Result<GroupTable> {
        let mut worker = Worker::new(&self.plan, &self.params);
        let result = (|| {
            while !self.failed.load(Ordering::Relaxed) {
                let Some((lower, upper)) = self.ranges.get(self.next.fetch_add(1, Ordering::Relaxed)) else {
                    break;
                };
                let base = self.plan.base.as_ref().expect("a FROM table");
                columnar::scan(&self.view, &base.schema, &base.needed, lower, upper, &mut worker)?;
            }
            worker.flush()
        })();
        if result.is_err() {
            self.failed.store(true, Ordering::Relaxed);
        }
        result.map(|()| worker.groups)
    }
}

/// Threads kept for splitting scans, so a query doesn't pay to start them.
struct Pool {
    threads: usize,
    queue: Mutex<VecDeque<Arc<Job>>>,
    ready: Condvar,
}

impl Pool {
    fn get() -> &'static Pool {
        static POOL: OnceLock<Pool> = OnceLock::new();
        let pool = POOL.get_or_init(|| {
            // The calling thread works too, so one fewer than the cores.
            let threads = thread::available_parallelism().map_or(1, |n| n.get()) - 1;
            Pool { threads, queue: Mutex::default(), ready: Condvar::new() }
        });
        static STARTED: std::sync::Once = std::sync::Once::new();
        STARTED.call_once(|| {
            for _ in 0..pool.threads {
                let _ = thread::Builder::new().name("spiro-scan".into()).spawn(|| pool.serve());
            }
        });
        pool
    }

    fn serve(&self) {
        loop {
            let job = {
                let mut queue = self.queue.lock().unwrap();
                loop {
                    match queue.pop_front() {
                        Some(job) => break job,
                        None => queue = self.ready.wait(queue).unwrap(),
                    }
                }
            };
            job.help();
        }
    }

    fn submit(&self, job: &Arc<Job>, helpers: usize) {
        if helpers == 0 {
            return;
        }
        self.queue.lock().unwrap().extend(std::iter::repeat_n(job.clone(), helpers));
        if helpers == 1 { self.ready.notify_one() } else { self.ready.notify_all() }
    }

    /// Takes back the job's requests that no thread picked up.
    fn withdraw(&self, job: &Arc<Job>) {
        self.queue.lock().unwrap().retain(|queued| !Arc::ptr_eq(queued, job));
    }
}

/// Combines the groups found by each worker, ordered by encoded key as the
/// row-at-a-time executor orders them.
fn merge(tables: Vec<GroupTable>) -> Result<Groups> {
    if let [_] = tables.as_slice() {
        let mut groups = tables.into_iter().next().unwrap().into_groups();
        groups.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        return Ok(groups.into_iter().map(|(_, keys, accumulators)| (keys, accumulators)).collect());
    }
    let mut merged: BTreeMap<Vec<u8>, (Vec<Value>, Vec<Accumulator>)> = BTreeMap::new();
    for table in tables {
        for (encoded, keys, accumulators) in table.into_groups() {
            match merged.get_mut(&encoded) {
                Some((_, existing)) => {
                    for (into, from) in existing.iter_mut().zip(accumulators) {
                        into.merge(from)?;
                    }
                }
                None => {
                    merged.insert(encoded, (keys, accumulators));
                }
            }
        }
    }
    Ok(merged.into_values().collect())
}

// ------------------------------------------------------------------ worker

/// Gathers rows into batches and aggregates each full batch.
struct Worker<'a> {
    plan: &'a SelectPlan,
    group: &'a GroupPlan,
    params: &'a [Value],
    needed: &'a [bool],
    batch: Batch,
    groups: GroupTable,
    /// Whether the read was narrowed by its access path, so only the
    /// residual base conditions need checking.
    narrowed: bool,
    /// For the simplest queries: how to aggregate straight from stored
    /// columns, without gathering a batch first.
    direct: Option<Direct>,
    /// Decoded values of one stored row, for `direct`.
    scratch: Vec<Slot>,
    /// Each row's group in a run, for `direct`; reused from run to run.
    ids: Vec<u32>,
    /// Rows per group in a block, and the groups with any, for counting.
    counts: Vec<usize>,
    touched: Vec<u32>,
    /// Group keys of a block, for `direct`.
    keys: Vec<i64>,
}

/// A decoded number, or NULL.
#[derive(Clone, Copy)]
enum Slot {
    Null,
    Integer(i64),
    Real(f64),
}

/// An aggregation simple enough to feed from stored columns directly: no
/// filter, at most one integer group key (a column, or a column combined
/// with an integer constant), and aggregates of plain columns or `count(*)`.
struct Direct {
    /// Per aggregate, the column it reads (`None` for `count(*)`).
    args: Vec<Option<usize>>,
    key: Option<(usize, Option<(BinaryOp, i64)>)>,
}

impl Direct {
    fn of(plan: &SelectPlan) -> Option<Direct> {
        let (base, group) = (plan.base.as_ref()?, plan.grouping.as_ref()?);
        if plan.base_filter.is_some() || plan.filter.is_some() || group.aggregates.iter().any(|a| a.distinct) {
            return None;
        }
        let args = group
            .aggregates
            .iter()
            .map(|a| match &a.arg {
                None => Some(None),
                Some(BExpr::Column(c)) => Some(Some(*c)),
                Some(_) => None,
            })
            .collect::<Option<Vec<_>>>()?;
        let integer = |c: usize| base.schema.columns[c].ty == DataType::Integer;
        let key = match group.keys.as_slice() {
            [] => None,
            [BExpr::Column(c)] if integer(*c) => Some((*c, None)),
            [BExpr::Binary(op, a, b)] => match (&**a, &**b) {
                (BExpr::Column(c), BExpr::Literal(Value::Integer(n)))
                    if integer(*c)
                        && matches!(
                            op,
                            BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem
                        )
                        && (*n != 0 || !matches!(op, BinaryOp::Div | BinaryOp::Rem)) =>
                {
                    Some((*c, Some((*op, *n))))
                }
                _ => return None,
            },
            _ => return None,
        };
        Some(Direct { args, key })
    }

    /// Turns column values into group keys in place. Returns `false` if any
    /// computation overflows (possibly for a row that would be skipped: the
    /// ordinary path then sorts it out).
    fn apply_key(&self, values: &mut [i64]) -> bool {
        match self.key.and_then(|(_, op)| op) {
            None => true,
            // Never overflows: the divisor is neither 0 nor -1.
            Some((BinaryOp::Rem, n)) if n != -1 => {
                values.iter_mut().for_each(|x| *x %= n);
                true
            }
            Some((BinaryOp::Add, n)) => {
                let mut overflow = false;
                values.iter_mut().for_each(|x| {
                    let (r, o) = x.overflowing_add(n);
                    (*x, overflow) = (r, overflow | o);
                });
                !overflow
            }
            Some((BinaryOp::Sub, n)) => {
                let mut overflow = false;
                values.iter_mut().for_each(|x| {
                    let (r, o) = x.overflowing_sub(n);
                    (*x, overflow) = (r, overflow | o);
                });
                !overflow
            }
            Some((BinaryOp::Mul, n)) => {
                let mut overflow = false;
                values.iter_mut().for_each(|x| {
                    let (r, o) = x.overflowing_mul(n);
                    (*x, overflow) = (r, overflow | o);
                });
                !overflow
            }
            Some(_) => values.iter_mut().all(|x| match self.key_of(*x) {
                Some(key) => {
                    *x = key;
                    true
                }
                None => false,
            }),
        }
    }

    /// The group key for a column value; `None` if computing it fails (an
    /// overflow), which the ordinary path then reports.
    fn key_of(&self, x: i64) -> Option<i64> {
        match self.key.and_then(|(_, op)| op) {
            None => Some(x),
            Some((BinaryOp::Add, n)) => x.checked_add(n),
            Some((BinaryOp::Sub, n)) => x.checked_sub(n),
            Some((BinaryOp::Mul, n)) => x.checked_mul(n),
            Some((BinaryOp::Div, n)) => x.checked_div(n),
            Some((_, n)) => x.checked_rem(n),
        }
    }
}

impl<'a> Worker<'a> {
    fn new(plan: &'a SelectPlan, params: &'a [Value]) -> Self {
        let base = plan.base.as_ref().expect("a FROM table");
        let group = plan.grouping.as_ref().expect("aggregation");
        Worker {
            plan,
            group,
            params,
            needed: &base.needed,
            batch: Batch::new(&base.schema, &base.needed),
            groups: GroupTable::new(group),
            narrowed: false,
            direct: Direct::of(plan),
            scratch: vec![Slot::Null; base.schema.columns.len()],
            ids: Vec::new(),
            counts: Vec::new(),
            touched: Vec::new(),
            keys: Vec::new(),
        }
    }

    /// A run of chunk rows, aggregated straight from its columns. Returns
    /// `false`, having done nothing, if a group key can't be computed.
    fn direct_chunk(&mut self, chunk: &Chunk, rows: std::ops::Range<usize>, skip: &[usize]) -> Result<bool> {
        let Some(direct) = &self.direct else { return Ok(false) };
        let groups = &mut self.groups;
        // Whether row `start + i` is to be used; `skip` is walked in step.
        let kept = || {
            let mut s = 0;
            move |row: usize| {
                if skip.get(s) == Some(&row) {
                    s += 1;
                    false
                } else {
                    true
                }
            }
        };
        let Some((key_column, _)) = direct.key else {
            // One group: each aggregate takes the whole run at once.
            for (a, arg) in direct.args.iter().enumerate() {
                let accumulator = groups.accumulator(0, a);
                let Some(c) = arg else {
                    accumulator.add_rows(rows.len() - skip.len());
                    continue;
                };
                let Some(view) = chunk.column(*c) else { continue }; // all NULL: skipped
                let valid = view.validity(rows.clone());
                let present = |i: usize| valid.is_none_or(|v| v[i] != 0);
                let mut keep = kept();
                match view.kind() {
                    Kind::Nulls => {}
                    Kind::Integer => accumulator.add_integers_from(
                        view.words(rows.clone())
                            .enumerate()
                            .map(|(i, w)| (keep(rows.start + i) && present(i)).then_some(w as i64)),
                    )?,
                    Kind::Real => accumulator.add_reals_from(
                        view.words(rows.clone())
                            .enumerate()
                            .map(|(i, w)| (keep(rows.start + i) && present(i)).then_some(f64::from_bits(w))),
                    )?,
                    Kind::Other => {
                        for i in rows.clone().filter(|&i| keep(i)) {
                            accumulator.add_value(Cow::Owned(view.value(i)?))?;
                        }
                    }
                }
            }
            return Ok(true);
        };
        // Grouped: every group key first, so a key that can't be computed
        // leaves nothing half done.
        // A block at a time, so the group ids stay in cache.
        if rows.len() > DIRECT_BLOCK {
            let mut start = rows.start;
            let mut s = 0;
            while start < rows.end {
                let end = (start + DIRECT_BLOCK).min(rows.end);
                let n = skip[s..].partition_point(|&row| row < end);
                if !self.direct_chunk(chunk, start..end, &skip[s..s + n])? {
                    // Earlier blocks are done: the rest goes the ordinary way,
                    // which reports what went wrong.
                    self.batch_chunk(chunk, start..rows.end, &skip[s..])?;
                    return Ok(true);
                }
                (start, s) = (end, s + n);
            }
            return Ok(true);
        }
        // Only `count(*)`: one pass, counting each group's rows in a
        // reused array, then adding each count once.
        if direct.args.iter().all(Option::is_none) {
            let view = match chunk.column(key_column) {
                Some(view) if view.kind() == Kind::Integer => Some(view),
                Some(view) if view.kind() != Kind::Nulls => return Ok(false),
                _ => None,
            };
            // Keys first, as below: a key that can't be computed must leave
            // nothing done. A block is short, so this stays in cache.
            let ids = &mut self.ids;
            ids.clear();
            let mut keep = kept();
            match view {
                Some(view) => {
                    // The keys for the whole block in simple loops, which the
                    // compiler keeps tight; then each key's group.
                    let keys = &mut self.keys;
                    keys.clear();
                    keys.extend(view.words(rows.clone()).map(|w| w as i64));
                    if !direct.apply_key(keys) {
                        return Ok(false);
                    }
                    let valid = view.validity(rows.clone());
                    for (i, &key) in keys.iter().enumerate() {
                        let id = if !keep(rows.start + i) {
                            SKIPPED
                        } else if valid.is_some_and(|v| v[i] == 0) {
                            groups.null_group()
                        } else {
                            groups.int_group(key)
                        };
                        ids.push(id);
                    }
                }
                None => {
                    for row in rows.clone() {
                        ids.push(if keep(row) { groups.null_group() } else { SKIPPED });
                    }
                }
            }
            let (counts, touched) = (&mut self.counts, &mut self.touched);
            for &id in ids.iter().filter(|&&id| id != SKIPPED) {
                let id = id as usize;
                if counts.len() <= id {
                    counts.resize(id + 1, 0);
                }
                if counts[id] == 0 {
                    touched.push(id as u32);
                }
                counts[id] += 1;
            }
            for &id in touched.iter() {
                let n = std::mem::take(&mut counts[id as usize]);
                for a in 0..direct.args.len() {
                    groups.accumulator(id, a).add_rows(n);
                }
            }
            touched.clear();
            return Ok(true);
        }
        // Skipped rows get no group: they mustn't create one.
        let ids = &mut self.ids;
        ids.clear();
        let mut keep = kept();
        match chunk.column(key_column) {
            Some(view) if view.kind() == Kind::Integer => {
                let valid = view.validity(rows.clone());
                for (i, w) in view.words(rows.clone()).enumerate() {
                    if !keep(rows.start + i) {
                        ids.push(SKIPPED);
                    } else if valid.is_some_and(|v| v[i] == 0) {
                        ids.push(groups.null_group());
                    } else {
                        let Some(key) = direct.key_of(w as i64) else { return Ok(false) };
                        ids.push(groups.int_group(key));
                    }
                }
            }
            Some(view) if view.kind() != Kind::Nulls => return Ok(false),
            _ => {
                for row in rows.clone() {
                    ids.push(if keep(row) { groups.null_group() } else { SKIPPED });
                }
            }
        }
        let ids = &*ids;
        for (a, arg) in direct.args.iter().enumerate() {
            let Some(c) = arg else {
                ids.iter().filter(|&&id| id != SKIPPED).for_each(|&id| groups.accumulator(id, a).add_row());
                continue;
            };
            let Some(view) = chunk.column(*c) else { continue }; // all NULL: skipped
            let valid = view.validity(rows.clone());
            let present = |i: usize| valid.is_none_or(|v| v[i] != 0);
            match view.kind() {
                Kind::Nulls => {}
                Kind::Integer => {
                    for (i, w) in view.words(rows.clone()).enumerate() {
                        if ids[i] != SKIPPED && present(i) {
                            groups.accumulator(ids[i], a).add_integer(w as i64)?;
                        }
                    }
                }
                Kind::Real => {
                    for (i, w) in view.words(rows.clone()).enumerate() {
                        if ids[i] != SKIPPED && present(i) {
                            groups.accumulator(ids[i], a).add_real(f64::from_bits(w))?;
                        }
                    }
                }
                Kind::Other => {
                    for (i, row) in rows.clone().enumerate() {
                        if ids[i] != SKIPPED {
                            groups.accumulator(ids[i], a).add_value(Cow::Owned(view.value(row)?))?;
                        }
                    }
                }
            }
        }
        Ok(true)
    }

    /// One stored row, aggregated straight from its needed values. Returns
    /// `false`, having done nothing, if its group key can't be computed or
    /// it holds values other than numbers.
    fn direct_row(&mut self, data: &[u8]) -> Result<bool> {
        let Some(direct) = &self.direct else { return Ok(false) };
        let scratch = &mut self.scratch;
        let mut other = false;
        visit_row(data, self.needed, |i, value| {
            scratch[i] = match value {
                RawValue::Null => Slot::Null,
                RawValue::Integer(x) => Slot::Integer(x),
                RawValue::Real(x) => Slot::Real(x),
                _ => {
                    other = true;
                    Slot::Null
                }
            }
        })?;
        if other {
            return Ok(false);
        }
        let groups = &mut self.groups;
        let id = match direct.key {
            None => 0,
            Some((c, _)) => match scratch[c] {
                Slot::Null => groups.null_group(),
                Slot::Integer(x) => match direct.key_of(x) {
                    Some(key) => groups.int_group(key),
                    None => return Ok(false),
                },
                Slot::Real(_) => return Ok(false),
            },
        };
        for (a, arg) in direct.args.iter().enumerate() {
            let accumulator = groups.accumulator(id, a);
            match arg.map(|c| scratch[c]) {
                None => accumulator.add_row(),
                Some(Slot::Null) => {}
                Some(Slot::Integer(x)) => accumulator.add_integer(x)?,
                Some(Slot::Real(x)) => accumulator.add_real(x)?,
            }
        }
        Ok(true)
    }

    /// Adds a decoded row.
    fn push(&mut self, row: &mut [Value]) -> Result<()> {
        self.batch.push(row);
        if self.batch.len == BATCH { self.flush() } else { Ok(()) }
    }

    /// Adds a row as stored, decoding only the columns the query reads.
    fn push_stored(&mut self, data: &[u8]) -> Result<()> {
        if self.direct_row(data)? {
            return Ok(());
        }
        self.batch.push_stored(data, self.needed)?;
        if self.batch.len == BATCH { self.flush() } else { Ok(()) }
    }

    /// Adds a chunk's rows, but those in `skip`: straight to the
    /// accumulators if the query allows, otherwise a batch's worth at a time.
    fn push_chunk(&mut self, chunk: &Chunk, rows: std::ops::Range<usize>, skip: &[usize]) -> Result<()> {
        if self.direct_chunk(chunk, rows.clone(), skip)? {
            return Ok(());
        }
        self.batch_chunk(chunk, rows, skip)
    }

    /// Adds a chunk's rows, but those in `skip`, to batches.
    fn batch_chunk(&mut self, chunk: &Chunk, rows: std::ops::Range<usize>, skip: &[usize]) -> Result<()> {
        // The live stretches between skipped rows.
        let ends = skip.iter().copied().chain(std::iter::once(rows.end));
        let mut start = rows.start;
        for end in ends {
            while start < end {
                let take = (BATCH - self.batch.len).min(end - start);
                self.batch.push_chunk(chunk, start..start + take)?;
                start += take;
                if self.batch.len == BATCH {
                    self.flush()?;
                }
            }
            start = end + 1;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        let plan = self.plan;
        let base_filter = if self.narrowed { &plan.base_residual } else { &plan.base_filter };
        for filter in [base_filter, &plan.filter].into_iter().flatten() {
            if self.batch.len == 0 {
                break;
            }
            let verdicts = eval(filter, &self.batch, self.params)?;
            if let Some(keep) = selected(&verdicts, self.batch.len)? {
                self.batch.retain(&keep);
            }
        }
        if self.batch.len > 0 {
            let keys = self.group.keys.iter().map(|k| eval(k, &self.batch, self.params)).collect::<Result<Vec<_>>>()?;
            if self.group.keys.is_empty() {
                // One group: whole columns go to each accumulator at once.
                for (a, aggregate) in self.group.aggregates.iter().enumerate() {
                    let accumulator = self.groups.accumulator(0, a);
                    let Some(arg) = &aggregate.arg else {
                        accumulator.add_rows(self.batch.len);
                        continue;
                    };
                    match eval(arg, &self.batch, self.params)? {
                        Vector::Int(xs, valid) => accumulator.add_integers(&xs, valid.as_deref())?,
                        Vector::Real(xs, valid) => accumulator.add_reals(&xs, valid.as_deref())?,
                        values => {
                            for i in 0..self.batch.len {
                                accumulator.add_value(Cow::Owned(values.get(i)))?;
                            }
                        }
                    }
                }
                self.batch.clear();
                return Ok(());
            }
            let ids = self.groups.assign(self.group, &keys, self.batch.len);
            for (a, aggregate) in self.group.aggregates.iter().enumerate() {
                let Some(arg) = &aggregate.arg else {
                    for &id in &ids {
                        self.groups.accumulator(id, a).add_row();
                    }
                    continue;
                };
                let values = eval(arg, &self.batch, self.params)?;
                // NULLs are skipped, as aggregates skip them.
                match &values {
                    Vector::Int(xs, valid) => {
                        for (i, &id) in ids.iter().enumerate() {
                            if is_valid(valid, i) {
                                self.groups.accumulator(id, a).add_integer(xs[i])?;
                            }
                        }
                    }
                    Vector::Real(xs, valid) => {
                        for (i, &id) in ids.iter().enumerate() {
                            if is_valid(valid, i) {
                                self.groups.accumulator(id, a).add_real(xs[i])?;
                            }
                        }
                    }
                    _ => {
                        for (i, &id) in ids.iter().enumerate() {
                            let value = match &values {
                                Vector::Bool(xs, valid) if is_valid(valid, i) => Cow::Owned(Value::Boolean(xs[i])),
                                Vector::Bool(..) => continue,
                                Vector::Const(v) => Cow::Borrowed(v),
                                Vector::Any(vs) => Cow::Borrowed(&vs[i]),
                                Vector::Int(..) | Vector::Real(..) => unreachable!(),
                            };
                            self.groups.accumulator(id, a).add_value(value)?;
                        }
                    }
                }
            }
        }
        self.batch.clear();
        Ok(())
    }
}

// ------------------------------------------------------------------- batch

/// One stored column of a batch.
enum Column {
    Int(Vec<i64>, Vec<bool>),
    Real(Vec<f64>, Vec<bool>),
    /// Any other type, or a typed column that met a value it didn't expect.
    Any(Vec<Value>),
}

impl Column {
    fn for_type(ty: DataType) -> Column {
        match ty {
            DataType::Integer => Column::Int(Vec::new(), Vec::new()),
            DataType::Real => Column::Real(Vec::new(), Vec::new()),
            _ => Column::Any(Vec::new()),
        }
    }

    fn push(&mut self, value: Value) {
        match (&mut *self, value) {
            (Column::Int(xs, valid), Value::Integer(x)) => {
                xs.push(x);
                valid.push(true);
            }
            (Column::Real(xs, valid), Value::Real(x)) => {
                xs.push(x);
                valid.push(true);
            }
            (Column::Int(xs, valid), Value::Null) => {
                xs.push(0);
                valid.push(false);
            }
            (Column::Real(xs, valid), Value::Null) => {
                xs.push(0.0);
                valid.push(false);
            }
            (Column::Any(vs), v) => vs.push(v),
            (_, v) => {
                self.make_any();
                self.push(v);
            }
        }
    }

    fn push_raw(&mut self, value: RawValue) -> Result<()> {
        match (&mut *self, value) {
            (Column::Int(xs, valid), RawValue::Integer(x)) => {
                xs.push(x);
                valid.push(true);
            }
            (Column::Real(xs, valid), RawValue::Real(x)) => {
                xs.push(x);
                valid.push(true);
            }
            (Column::Int(xs, valid), RawValue::Null) => {
                xs.push(0);
                valid.push(false);
            }
            (Column::Real(xs, valid), RawValue::Null) => {
                xs.push(0.0);
                valid.push(false);
            }
            (_, v) => self.push(v.to_value()?),
        }
        Ok(())
    }

    fn push_nulls(&mut self, n: usize) {
        match self {
            Column::Int(xs, valid) => {
                xs.resize(xs.len() + n, 0);
                valid.resize(valid.len() + n, false);
            }
            Column::Real(xs, valid) => {
                xs.resize(xs.len() + n, 0.0);
                valid.resize(valid.len() + n, false);
            }
            Column::Any(vs) => vs.resize(vs.len() + n, Value::Null),
        }
    }

    fn extend_from(&mut self, view: &ColumnView, rows: std::ops::Range<usize>) -> Result<()> {
        let extend_valid = |valid: &mut Vec<bool>| match view.validity(rows.clone()) {
            Some(bytes) => valid.extend(bytes.iter().map(|&b| b != 0)),
            None => valid.resize(valid.len() + rows.len(), true),
        };
        match (&mut *self, view.kind()) {
            (_, Kind::Nulls) => self.push_nulls(rows.len()),
            (Column::Int(xs, valid), Kind::Integer) => {
                xs.extend(view.words(rows.clone()).map(|w| w as i64));
                extend_valid(valid);
            }
            (Column::Real(xs, valid), Kind::Real) => {
                xs.extend(view.words(rows.clone()).map(f64::from_bits));
                extend_valid(valid);
            }
            (column, _) => {
                for i in rows {
                    column.push(view.value(i)?);
                }
            }
        }
        Ok(())
    }

    fn make_any(&mut self) {
        let len = self.len();
        let values = (0..len).map(|i| self.get(i)).collect();
        *self = Column::Any(values);
    }

    fn len(&self) -> usize {
        match self {
            Column::Int(xs, _) => xs.len(),
            Column::Real(xs, _) => xs.len(),
            Column::Any(vs) => vs.len(),
        }
    }

    fn get(&self, i: usize) -> Value {
        match self {
            Column::Int(xs, valid) => {
                if valid[i] {
                    Value::Integer(xs[i])
                } else {
                    Value::Null
                }
            }
            Column::Real(xs, valid) => {
                if valid[i] {
                    Value::Real(xs[i])
                } else {
                    Value::Null
                }
            }
            Column::Any(vs) => vs[i].clone(),
        }
    }

    fn vector(&self) -> Vector {
        let mask = |valid: &Vec<bool>| (!valid.iter().all(|&v| v)).then(|| valid.clone());
        match self {
            Column::Int(xs, valid) => Vector::Int(xs.clone(), mask(valid)),
            Column::Real(xs, valid) => Vector::Real(xs.clone(), mask(valid)),
            Column::Any(vs) => Vector::Any(vs.clone()),
        }
    }

    fn retain(&mut self, keep: &[u32]) {
        fn gather<T: Clone>(v: &mut Vec<T>, keep: &[u32]) {
            for (to, &from) in keep.iter().enumerate() {
                v[to] = v[from as usize].clone();
            }
            v.truncate(keep.len());
        }
        match self {
            Column::Int(xs, valid) => {
                gather(xs, keep);
                gather(valid, keep);
            }
            Column::Real(xs, valid) => {
                gather(xs, keep);
                gather(valid, keep);
            }
            Column::Any(vs) => gather(vs, keep),
        }
    }

    fn clear(&mut self) {
        match self {
            Column::Int(xs, valid) => {
                xs.clear();
                valid.clear();
            }
            Column::Real(xs, valid) => {
                xs.clear();
                valid.clear();
            }
            Column::Any(vs) => vs.clear(),
        }
    }
}

/// Rows gathered column by column. Only the columns the query reads are kept.
struct Batch {
    len: usize,
    columns: Vec<Option<Column>>,
}

impl columnar::Sink for Worker<'_> {
    fn row(&mut self, data: &[u8]) -> Result<()> {
        self.push_stored(data)
    }

    fn rows(&mut self, rows: &[&[u8]]) -> Result<()> {
        for row in rows {
            self.push_stored(row)?;
        }
        Ok(())
    }

    fn chunk(&mut self, chunk: &Chunk, rows: std::ops::Range<usize>, skip: &[usize]) -> Result<()> {
        self.push_chunk(chunk, rows, skip)
    }
}

impl Batch {
    fn new(table: &TableSchema, needed: &[bool]) -> Batch {
        let columns = table.columns.iter().zip(needed).map(|(c, &n)| n.then(|| Column::for_type(c.ty))).collect();
        Batch { len: 0, columns }
    }

    fn push(&mut self, row: &mut [Value]) {
        for (column, value) in self.columns.iter_mut().zip(row) {
            if let Some(column) = column {
                column.push(std::mem::replace(value, Value::Null));
            }
        }
        self.len += 1;
    }

    fn push_stored(&mut self, data: &[u8], needed: &[bool]) -> Result<()> {
        let mut result = Ok(());
        visit_row(data, needed, |i, value| {
            if let (Some(column), Ok(())) = (&mut self.columns[i], &result) {
                result = column.push_raw(value);
            }
        })?;
        result?;
        self.len += 1;
        Ok(())
    }

    /// Adds a run of a chunk's rows: integer and real columns are copied as
    /// they lie, without decoding row by row.
    fn push_chunk(&mut self, chunk: &Chunk, rows: std::ops::Range<usize>) -> Result<()> {
        for (c, column) in self.columns.iter_mut().enumerate() {
            let Some(column) = column else { continue };
            match chunk.column(c) {
                None => column.push_nulls(rows.len()),
                Some(view) => column.extend_from(&view, rows.clone())?,
            }
        }
        self.len += rows.len();
        Ok(())
    }

    fn retain(&mut self, keep: &[u32]) {
        self.columns.iter_mut().flatten().for_each(|c| c.retain(keep));
        self.len = keep.len();
    }

    fn clear(&mut self) {
        self.columns.iter_mut().flatten().for_each(Column::clear);
        self.len = 0;
    }

    /// Evaluates `e` one row at a time with the ordinary evaluator.
    fn row_by_row(&self, e: &BExpr, params: &[Value]) -> Result<Vector> {
        let mut row = vec![Value::Null; self.columns.len()];
        let mut out = Vec::with_capacity(self.len);
        for i in 0..self.len {
            for (slot, column) in row.iter_mut().zip(&self.columns) {
                if let Some(column) = column {
                    *slot = column.get(i);
                }
            }
            out.push(e.eval(&row, &Env::new(params, None))?);
        }
        Ok(Vector::Any(out))
    }
}

// ---------------------------------------------------------------- vectors

/// An expression's value for every row of a batch.
enum Vector {
    /// Values, and which are not NULL (`None`: all of them).
    Int(Vec<i64>, Option<Vec<bool>>),
    Real(Vec<f64>, Option<Vec<bool>>),
    Bool(Vec<bool>, Option<Vec<bool>>),
    /// The same value for every row.
    Const(Value),
    Any(Vec<Value>),
}

fn is_valid(valid: &Option<Vec<bool>>, i: usize) -> bool {
    valid.as_ref().is_none_or(|v| v[i])
}

/// Rows where both operands are not NULL.
fn both_valid(a: &Option<Vec<bool>>, b: &Option<Vec<bool>>) -> Option<Vec<bool>> {
    match (a, b) {
        (None, None) => None,
        (Some(v), None) | (None, Some(v)) => Some(v.clone()),
        (Some(a), Some(b)) => Some(a.iter().zip(b).map(|(&a, &b)| a && b).collect()),
    }
}

impl Vector {
    fn get(&self, i: usize) -> Value {
        match self {
            Vector::Int(xs, valid) => {
                if is_valid(valid, i) {
                    Value::Integer(xs[i])
                } else {
                    Value::Null
                }
            }
            Vector::Real(xs, valid) => {
                if is_valid(valid, i) {
                    Value::Real(xs[i])
                } else {
                    Value::Null
                }
            }
            Vector::Bool(xs, valid) => {
                if is_valid(valid, i) {
                    Value::Boolean(xs[i])
                } else {
                    Value::Null
                }
            }
            Vector::Const(v) => v.clone(),
            Vector::Any(vs) => vs[i].clone(),
        }
    }

    fn validity(&self) -> Option<&Option<Vec<bool>>> {
        match self {
            Vector::Int(_, valid) | Vector::Real(_, valid) | Vector::Bool(_, valid) => Some(valid),
            _ => None,
        }
    }
}

/// Evaluates `e` for every row of the batch.
fn eval(e: &BExpr, batch: &Batch, params: &[Value]) -> Result<Vector> {
    // Any trouble on the fast path (an overflow, a division by zero, an
    // error under AND or OR, which may not need to evaluate both sides) is
    // settled by the ordinary evaluator, which reports errors exactly.
    match fast(e, batch, params) {
        Ok(Some(v)) => Ok(v),
        Ok(None) | Err(_) => batch.row_by_row(e, params),
    }
}

fn fast(e: &BExpr, batch: &Batch, params: &[Value]) -> Result<Option<Vector>> {
    let n = batch.len;
    Ok(match e {
        BExpr::Literal(v) => Some(Vector::Const(v.clone())),
        BExpr::Param(_) => Some(Vector::Const(e.eval(&[], &Env::new(params, None))?)),
        BExpr::Column(c) => Some(match &batch.columns[*c] {
            Some(column) => column.vector(),
            None => Vector::Const(Value::Null), // not read, as in the row executor
        }),
        BExpr::Neg(a) => match eval(a, batch, params)? {
            Vector::Int(xs, valid) => {
                let mut ok = true;
                let out = xs
                    .iter()
                    .enumerate()
                    .map(|(i, &x)| {
                        let (r, overflow) = x.overflowing_neg();
                        ok &= !overflow || !is_valid(&valid, i);
                        r
                    })
                    .collect();
                ok.then_some(Vector::Int(out, valid))
            }
            Vector::Real(xs, valid) => Some(Vector::Real(xs.iter().map(|x| -x).collect(), valid)),
            _ => None,
        },
        BExpr::Not(a) => {
            truths(&eval(a, batch, params)?, n).map(|(xs, valid)| Vector::Bool(xs.iter().map(|x| !x).collect(), valid))
        }
        BExpr::IsNull(a, negated) => {
            let a = eval(a, batch, params)?;
            Some(match &a {
                Vector::Const(v) => Vector::Const(Value::Boolean(v.is_null() != *negated)),
                Vector::Any(vs) => Vector::Bool(vs.iter().map(|v| v.is_null() != *negated).collect(), None),
                _ => {
                    let valid = a.validity().unwrap();
                    Vector::Bool((0..n).map(|i| !is_valid(valid, i) != *negated).collect(), None)
                }
            })
        }
        BExpr::Binary(op, a, b) => {
            let (a, b) = (eval(a, batch, params)?, eval(b, batch, params)?);
            match op {
                BinaryOp::And | BinaryOp::Or => logic(*op, &a, &b, n),
                op if op.is_comparison() => compare(*op, &a, &b, n),
                BinaryOp::Concat => None,
                op => arithmetic(*op, &a, &b, n),
            }
        }
        _ => None,
    })
}

/// Truth values for AND, OR and NOT: booleans, or integers (nonzero is true).
fn truths(v: &Vector, n: usize) -> Option<(Vec<bool>, Option<Vec<bool>>)> {
    match v {
        Vector::Bool(xs, valid) => Some((xs.clone(), valid.clone())),
        Vector::Int(xs, valid) => Some((xs.iter().map(|&x| x != 0).collect(), valid.clone())),
        Vector::Const(Value::Boolean(b)) => Some((vec![*b; n], None)),
        Vector::Const(Value::Integer(x)) => Some((vec![*x != 0; n], None)),
        Vector::Const(Value::Null) => Some((vec![false; n], Some(vec![false; n]))),
        _ => None,
    }
}

fn logic(op: BinaryOp, a: &Vector, b: &Vector, n: usize) -> Option<Vector> {
    let ((ax, av), (bx, bv)) = (truths(a, n)?, truths(b, n)?);
    let mut out = Vec::with_capacity(n);
    let mut valid = vec![true; n];
    for i in 0..n {
        let (a, b) = (is_valid(&av, i).then_some(ax[i]), is_valid(&bv, i).then_some(bx[i]));
        let result = match op {
            BinaryOp::And => match (a, b) {
                (Some(false), _) | (_, Some(false)) => Some(false),
                (Some(true), Some(true)) => Some(true),
                _ => None,
            },
            _ => match (a, b) {
                (Some(true), _) | (_, Some(true)) => Some(true),
                (Some(false), Some(false)) => Some(false),
                _ => None,
            },
        };
        out.push(result.unwrap_or(false));
        valid[i] = result.is_some();
    }
    let valid = (!valid.iter().all(|&v| v)).then_some(valid);
    Some(Vector::Bool(out, valid))
}

/// A numeric operand: a column of values or one value for every row.
enum Num<'a, T> {
    Lanes(&'a [T]),
    Splat(T),
}

impl<T: Copy> Num<'_, T> {
    #[inline(always)]
    fn at(&self, i: usize) -> T {
        match self {
            Num::Lanes(xs) => xs[i],
            Num::Splat(x) => *x,
        }
    }
}

fn ints(v: &Vector) -> Option<(Num<'_, i64>, &Option<Vec<bool>>)> {
    const NONE: &Option<Vec<bool>> = &None;
    match v {
        Vector::Int(xs, valid) => Some((Num::Lanes(xs), valid)),
        Vector::Const(Value::Integer(x)) => Some((Num::Splat(*x), NONE)),
        _ => None,
    }
}

/// Two operands as reals, and which rows have both.
type RealOperands = (Vec<f64>, Vec<f64>, Option<Vec<bool>>);

/// Integers and reals as reals; `None` unless at least one side is real.
fn reals(a: &Vector, b: &Vector, n: usize) -> Option<RealOperands> {
    let lanes = |v: &Vector| -> Option<(Vec<f64>, Option<Vec<bool>>)> {
        match v {
            Vector::Real(xs, valid) => Some((xs.clone(), valid.clone())),
            Vector::Int(xs, valid) => Some((xs.iter().map(|&x| x as f64).collect(), valid.clone())),
            Vector::Const(Value::Real(x)) => Some((vec![*x; n], None)),
            Vector::Const(Value::Integer(x)) => Some((vec![*x as f64; n], None)),
            _ => None,
        }
    };
    let is_real = |v: &Vector| matches!(v, Vector::Real(..) | Vector::Const(Value::Real(_)));
    if !is_real(a) && !is_real(b) {
        return None;
    }
    let ((ax, av), (bx, bv)) = (lanes(a)?, lanes(b)?);
    Some((ax, bx, both_valid(&av, &bv)))
}

fn is_null(v: &Vector) -> bool {
    matches!(v, Vector::Const(Value::Null))
}

fn arithmetic(op: BinaryOp, a: &Vector, b: &Vector, n: usize) -> Option<Vector> {
    if is_null(a) || is_null(b) {
        // NULL with anything is NULL, before any type check.
        return Some(Vector::Const(Value::Null));
    }
    if let (Some((x, xv)), Some((y, yv))) = (ints(a), ints(b)) {
        let valid = both_valid(xv, yv);
        let mut out = Vec::with_capacity(n);
        let mut ok = true;
        // One loop per operator, so each is a tight loop over the lanes.
        macro_rules! lanes {
            ($f:expr) => {
                for i in 0..n {
                    let r: Option<i64> = $f(x.at(i), y.at(i));
                    ok &= r.is_some() || !is_valid(&valid, i);
                    out.push(r.unwrap_or(0));
                }
            };
        }
        match op {
            BinaryOp::Add => lanes!(i64::checked_add),
            BinaryOp::Sub => lanes!(i64::checked_sub),
            BinaryOp::Mul => lanes!(i64::checked_mul),
            BinaryOp::Div => lanes!(i64::checked_div),
            _ => lanes!(i64::checked_rem),
        }
        return ok.then_some(Vector::Int(out, valid));
    }
    let (x, y, valid) = reals(a, b, n)?;
    if matches!(op, BinaryOp::Div | BinaryOp::Rem) && (0..n).any(|i| y[i] == 0.0 && is_valid(&valid, i)) {
        return None; // division by zero: let the row evaluator report it
    }
    let out = match op {
        BinaryOp::Add => x.iter().zip(&y).map(|(a, b)| a + b).collect(),
        BinaryOp::Sub => x.iter().zip(&y).map(|(a, b)| a - b).collect(),
        BinaryOp::Mul => x.iter().zip(&y).map(|(a, b)| a * b).collect(),
        BinaryOp::Div => x.iter().zip(&y).map(|(a, b)| a / b).collect(),
        _ => x.iter().zip(&y).map(|(a, b)| a % b).collect(),
    };
    Some(Vector::Real(out, valid))
}

fn compare(op: BinaryOp, a: &Vector, b: &Vector, n: usize) -> Option<Vector> {
    if is_null(a) || is_null(b) {
        return Some(Vector::Const(Value::Null));
    }
    let test = |o: std::cmp::Ordering| match op {
        BinaryOp::Eq => o.is_eq(),
        BinaryOp::NotEq => o.is_ne(),
        BinaryOp::Lt => o.is_lt(),
        BinaryOp::LtEq => o.is_le(),
        BinaryOp::Gt => o.is_gt(),
        _ => o.is_ge(),
    };
    if let (Some((x, xv)), Some((y, yv))) = (ints(a), ints(b)) {
        let out = (0..n).map(|i| test(x.at(i).cmp(&y.at(i)))).collect();
        return Some(Vector::Bool(out, both_valid(xv, yv)));
    }
    let (x, y, valid) = reals(a, b, n)?;
    Some(Vector::Bool((0..n).map(|i| test(cmp_f64(x[i], y[i]))).collect(), valid))
}

/// The rows a filter keeps (only TRUE passes), or `None` for all of them.
fn selected(verdicts: &Vector, n: usize) -> Result<Option<Vec<u32>>> {
    let keep: Vec<u32> = match verdicts {
        Vector::Bool(xs, valid) => (0..n).filter(|&i| xs[i] && is_valid(valid, i)).map(|i| i as u32).collect(),
        Vector::Const(v) => return Ok(if v.truth()? == Some(true) { None } else { Some(Vec::new()) }),
        _ => {
            let mut keep = Vec::new();
            for i in 0..n {
                if verdicts.get(i).truth()? == Some(true) {
                    keep.push(i as u32);
                }
            }
            keep
        }
    };
    Ok((keep.len() < n).then_some(keep))
}

// ------------------------------------------------------------------ groups

/// A fast hash for group keys. Keys come from the data being aggregated,
/// not from an attacker choosing collisions, so SipHash's defense isn't needed.
#[derive(Default)]
struct FastHasher(u64);

impl Hasher for FastHasher {
    fn write(&mut self, bytes: &[u8]) {
        let (chunks, rest) = bytes.as_chunks::<8>();
        for chunk in chunks {
            self.write_u64(u64::from_le_bytes(*chunk));
        }
        let mut last = [0u8; 8];
        last[..rest.len()].copy_from_slice(rest);
        self.write_u64(u64::from_le_bytes(last) ^ bytes.len() as u64);
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0.rotate_left(5) ^ n).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }

    fn write_i64(&mut self, n: i64) {
        self.write_u64(n as u64);
    }

    fn finish(&self) -> u64 {
        // Mix the high bits down: hash tables index by the low bits.
        let z = self.0;
        (z ^ (z >> 29)).wrapping_mul(0xbf58_476d_1ce4_e5b9) ^ (z >> 32)
    }
}

type Fast = BuildHasherDefault<FastHasher>;

/// The group of a row that is passed over.
const SKIPPED: u32 = u32::MAX;

/// Rows per block when grouping straight from a chunk.
const DIRECT_BLOCK: usize = 2048;

/// Slots in a group table's cache of recent integer keys (a power of two).
const RECENT: usize = 256;

/// Integer keys below this get a group table's array, not its hash table.
const DENSE: usize = 1024;

/// The groups one worker has found, and their accumulators.
struct GroupTable {
    /// Groups whose single key is an integer, by that integer.
    by_int: HashMap<i64, u32, Fast>,
    /// The group whose single key is NULL.
    null_group: Option<u32>,
    /// Every other group, by its encoded key.
    by_bytes: HashMap<Vec<u8>, u32, Fast>,
    /// Groups of small non-negative integer keys (below `DENSE`), by key:
    /// one array lookup, as for `GROUP BY x % 10`.
    dense: Vec<u32>,
    /// The integer keys found most recently, by a hash of the key: most
    /// lookups end here, without a probe of `by_int`.
    recent: Box<[(i64, u32); RECENT]>,
    keys: Vec<Vec<Value>>,
    /// `aggregates` accumulators per group, group after group.
    accumulators: Vec<Accumulator>,
    aggregates: usize,
    template: Vec<Aggregate>,
}

impl GroupTable {
    fn new(group: &GroupPlan) -> Self {
        let mut table = GroupTable {
            by_int: HashMap::default(),
            null_group: None,
            by_bytes: HashMap::default(),
            dense: Vec::new(),
            recent: Box::new([(0, u32::MAX); RECENT]),
            keys: Vec::new(),
            accumulators: Vec::new(),
            aggregates: group.aggregates.len(),
            template: group.aggregates.clone(),
        };
        if group.keys.is_empty() {
            // Without GROUP BY there is exactly one group, even with no rows.
            table.add_group(Vec::new());
        }
        table
    }

    fn add_group(&mut self, keys: Vec<Value>) -> u32 {
        self.keys.push(keys);
        self.accumulators.extend(self.template.iter().map(Accumulator::new));
        (self.keys.len() - 1) as u32
    }

    fn accumulator(&mut self, group: u32, aggregate: usize) -> &mut Accumulator {
        &mut self.accumulators[group as usize * self.aggregates + aggregate]
    }

    fn int_group(&mut self, key: i64) -> u32 {
        if (0..DENSE as i64).contains(&key) {
            let k = key as usize;
            if let Some(&id) = self.dense.get(k)
                && id != u32::MAX
            {
                return id;
            }
            let id = self.int_group_slow(key);
            if self.dense.len() <= k {
                self.dense.resize(k + 1, u32::MAX);
            }
            self.dense[k] = id;
            return id;
        }
        self.int_group_slow(key)
    }

    fn int_group_slow(&mut self, key: i64) -> u32 {
        let slot = ((key as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> (64 - RECENT.trailing_zeros())) as usize;
        let (recent_key, recent_id) = self.recent[slot];
        if recent_key == key && recent_id != u32::MAX {
            return recent_id;
        }
        let id = match self.by_int.get(&key) {
            Some(&id) => id,
            None => {
                let id = self.add_group(vec![Value::Integer(key)]);
                self.by_int.insert(key, id);
                id
            }
        };
        self.recent[slot] = (key, id);
        id
    }

    fn null_group(&mut self) -> u32 {
        match self.null_group {
            Some(id) => id,
            None => {
                let id = self.add_group(vec![Value::Null]);
                self.null_group = Some(id);
                id
            }
        }
    }

    /// The group of each row, given its key values.
    fn assign(&mut self, group: &GroupPlan, keys: &[Vector], n: usize) -> Vec<u32> {
        if group.keys.is_empty() {
            return vec![0; n];
        }
        let mut ids = Vec::with_capacity(n);
        if let [Vector::Int(xs, valid)] = keys {
            for (i, &x) in xs.iter().enumerate() {
                ids.push(if is_valid(valid, i) { self.int_group(x) } else { self.null_group() });
            }
            return ids;
        }
        let mut encoded = Vec::new();
        for i in 0..n {
            let values: Vec<Value> = keys.iter().map(|k| k.get(i)).collect();
            // A single integer or NULL key goes where the fast path puts it,
            // so a batch that takes the slow path finds the same groups.
            match values.as_slice() {
                [Value::Integer(x)] => {
                    ids.push(self.int_group(*x));
                    continue;
                }
                [Value::Null] => {
                    ids.push(self.null_group());
                    continue;
                }
                _ => {}
            }
            encoded.clear();
            values.iter().for_each(|v| encode_key(&mut encoded, v));
            let id = match self.by_bytes.get(encoded.as_slice()) {
                Some(&id) => id,
                None => {
                    let id = self.add_group(values);
                    self.by_bytes.insert(encoded.clone(), id);
                    id
                }
            };
            ids.push(id);
        }
        ids
    }

    /// Every group as (encoded key, key values, accumulators).
    fn into_groups(self) -> Vec<(Vec<u8>, Vec<Value>, Vec<Accumulator>)> {
        let mut accumulators = self.accumulators.into_iter();
        self.keys
            .into_iter()
            .map(|keys| {
                let mut encoded = Vec::new();
                keys.iter().for_each(|v| encode_key(&mut encoded, v));
                (encoded, keys, accumulators.by_ref().take(self.aggregates).collect())
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::DISABLED;
    use crate::options::Options;
    use crate::sql::{Database, QueryResult, Session, Value};
    use crate::testutil::TempDir;

    /// The result with batching on, and with the row-at-a-time executor.
    fn both(session: &mut Session, sql: &str) -> (Result<QueryResult, String>, Result<QueryResult, String>) {
        let mut run = |disabled: bool| {
            DISABLED.with(|d| d.set(disabled));
            let result = session.execute(sql).map_err(|e| e.to_string());
            DISABLED.with(|d| d.set(false));
            result
        };
        (run(false), run(true))
    }

    /// Equal, allowing for reals summed in a different order by parallel parts.
    fn same(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Real(x), Value::Real(y)) => (x - y).abs() <= 1e-9 * x.abs().max(y.abs()).max(1.0),
            _ => a == b,
        }
    }

    fn check(session: &mut Session, sql: &str) {
        match both(session, sql) {
            (Ok(QueryResult::Rows(batched)), Ok(QueryResult::Rows(rows))) => {
                assert_eq!(batched.columns, rows.columns, "{sql}");
                assert_eq!(batched.rows.len(), rows.rows.len(), "{sql}");
                for (x, y) in batched.rows.iter().zip(&rows.rows) {
                    assert!(x.iter().zip(y).all(|(a, b)| same(a, b)), "{sql}\n  batched: {x:?}\n  rows:    {y:?}");
                }
            }
            (Err(batched), Err(rows)) => assert_eq!(batched, rows, "{sql}"),
            (batched, rows) => panic!("{sql}\n  batched: {batched:?}\n  rows:    {rows:?}"),
        }
    }

    const QUERIES: &[&str] = &[
        "SELECT count(*) FROM t",
        "SELECT count(*), sum(a), avg(a), min(a), max(a), count(a) FROM t",
        "SELECT sum(b), avg(b), min(b), max(b), count(b) FROM t",
        "SELECT a % 7, count(*), sum(b) FROM t GROUP BY 1",
        "SELECT a % 7 AS k, count(*) FROM t GROUP BY 1 ORDER BY 2 DESC, 1",
        "SELECT c, count(*), max(a), min(b) FROM t GROUP BY c",
        "SELECT c, d, count(*) FROM t GROUP BY c, d",
        "SELECT d, avg(b) FROM t GROUP BY d HAVING count(*) > 10",
        "SELECT a, count(*) FROM t WHERE b > 0 AND (a < 10 OR c = 'beta') GROUP BY a",
        "SELECT count(*) FROM t WHERE a IS NULL",
        "SELECT count(*) FROM t WHERE b IS NOT NULL AND NOT (a > 0)",
        "SELECT count(*) FROM t WHERE d",
        "SELECT count(*) FROM t WHERE a",
        "SELECT a * 2 + 1, count(*) FROM t WHERE a BETWEEN -5 AND 5 GROUP BY 1",
        "SELECT a + b, count(*) FROM t GROUP BY 1",
        "SELECT -a, sum(-b) FROM t GROUP BY 1",
        "SELECT a / 3, a % 3, count(*) FROM t WHERE a <> 0 GROUP BY 1, 2",
        "SELECT b / 2.5, count(*) FROM t WHERE b < -10 GROUP BY 1",
        "SELECT a < b, a = 3, b >= 1.5, count(*) FROM t GROUP BY 1, 2, 3",
        "SELECT upper(c), length(c), count(*) FROM t GROUP BY 1, 2",
        "SELECT count(DISTINCT a), sum(DISTINCT a), count(DISTINCT c) FROM t",
        "SELECT c, count(DISTINCT a) FROM t GROUP BY c",
        "SELECT sum(a / 0) FROM t",
        "SELECT sum(b / 0) FROM t",
        "SELECT sum(a / a) FROM t WHERE a <> 0",
        "SELECT sum(a / a) FROM t WHERE a <> 0 AND a / a = 1",
        "SELECT count(*) FROM t WHERE a = 0 OR 10 / a > 2",
        "SELECT sum(e) FROM t",
        "SELECT sum(e - e), max(e), min(e) FROM t",
        "SELECT a, count(*) FROM t WHERE a + 9223372036854775800 > 0 GROUP BY a",
        "SELECT sum(c) FROM t",
        "SELECT count(*) FROM t WHERE c LIKE 'a%'",
        "SELECT min(c), max(c), min(d), max(d) FROM t",
        "SELECT b, count(*) FROM t GROUP BY b",
        "SELECT CASE WHEN a > 0 THEN 'pos' WHEN a < 0 THEN 'neg' END, count(*) FROM t GROUP BY 1",
        "SELECT id % 100, sum(a) FROM t GROUP BY 1",
        "SELECT count(*), sum(a) FROM t WHERE id BETWEEN 100 AND 5000",
        "SELECT a, count(*) FROM t WHERE a = 3 GROUP BY a",
        "SELECT count(*), avg(a), sum(a), max(c) FROM t WHERE a > 1000",
        "SELECT a, count(*) FROM t WHERE a > 1000 GROUP BY a",
        "SELECT coalesce(a, -1) % 5, count(*) FROM t GROUP BY 1",
        "SELECT a IS NULL, b IS NULL, count(*) FROM t GROUP BY 1, 2",
        "SELECT count(*) FROM t WHERE a > ? ",
    ];

    fn fill(db: &Database) {
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b REAL, c TEXT, d BOOLEAN, e INTEGER)").unwrap();
        db.execute("CREATE INDEX t_a ON t (a)").unwrap();
        let insert = db.prepare("INSERT INTO t VALUES (?, ?, ?, ?, ?, ?)").unwrap();
        let mut session = db.session();
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut rand = move |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n
        };
        session.execute("BEGIN").unwrap();
        for id in 0..20_000i64 {
            let nulls: Vec<bool> = (0..4).map(|_| rand(7) == 0).collect();
            let maybe = |i: usize, v: Value| if nulls[i] { Value::Null } else { v };
            let a = maybe(0, Value::Integer(rand(101) as i64 - 50));
            let b = maybe(1, Value::Real((rand(20_001) as f64 - 10_000.0) / 64.0));
            let c = maybe(2, Value::Text(["alpha", "beta", "gamma", "", "a\u{0}b"][rand(5) as usize].into()));
            let d = maybe(3, Value::Boolean(rand(2) == 0));
            let e = Value::Integer(i64::MAX / 4 + rand(1000) as i64);
            session.execute_prepared(&insert, &[Value::Integer(id), a, b, c, d, e]).unwrap();
        }
        session.execute("COMMIT").unwrap();
    }

    #[test]
    fn batched_results_match_the_row_executor() {
        // A small memtable puts most rows in tables, so full scans are split
        // across threads; the rest stay in the memtable.
        let dir = TempDir::new("vector");
        let db = Database::open_with(dir.path(), Options { memtable_size: 256 << 10, ..Options::default() }).unwrap();
        fill(&db);
        db.execute("UPDATE t SET a = a + 1 WHERE id % 13 = 0").unwrap();
        db.execute("DELETE FROM t WHERE id % 17 = 0").unwrap();
        let mut session = db.session();
        for sql in QUERIES {
            let sql = sql.replace('?', "3");
            check(&mut session, &sql);
        }

        // A transaction's own uncommitted writes are visible to it.
        session.execute("BEGIN").unwrap();
        session.execute("INSERT INTO t (id, a, b, c, d, e) VALUES (100000, 7, 1.5, 'beta', TRUE, 1)").unwrap();
        session.execute("DELETE FROM t WHERE id < 50").unwrap();
        for sql in QUERIES {
            check(&mut session, &sql.replace('?', "3"));
        }
        session.execute("ROLLBACK").unwrap();
    }

    #[test]
    fn scans_without_column_sections_agree() {
        // Files written without sections are read row by row.
        let dir = TempDir::new("vector-no-sections");
        let opts = Options { memtable_size: 256 << 10, column_sections: false, ..Options::default() };
        let db = Database::open_with(dir.path(), opts).unwrap();
        fill(&db);
        db.kv().flush().unwrap();
        db.execute("UPDATE t SET a = a + 1 WHERE id % 13 = 0").unwrap();
        let mut session = db.session();
        for sql in QUERIES {
            check(&mut session, &sql.replace('?', "3"));
        }
    }

    #[test]
    fn full_scans_are_split_and_agree() {
        let dir = TempDir::new("vector-split");
        let db = Database::open_with(dir.path(), Options { memtable_size: 256 << 10, ..Options::default() }).unwrap();
        fill(&db);
        db.kv().flush().unwrap();
        let plan_splits = {
            let txn = db.read_txn();
            let plan = std::sync::Arc::new(
                super::super::plan::plan_select(
                    &txn,
                    match &crate::sql::parser::parse_statement("SELECT a, count(*) FROM t GROUP BY a").unwrap().0 {
                        crate::sql::ast::Statement::Select(s) => s,
                        _ => unreachable!(),
                    },
                    &super::super::expr::Binder::new(&[], &txn),
                )
                .unwrap(),
            );
            super::split(&txn, &plan, &[]).map(|job| job.ranges.len())
        };
        assert!(plan_splits.is_some_and(|parts| parts >= 2), "{plan_splits:?}");
        let mut session = db.session();
        for sql in QUERIES {
            check(&mut session, &sql.replace('?', "-10"));
        }
    }
}
