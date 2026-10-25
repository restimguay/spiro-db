//! Query planning: choosing how to read each table, and binding a SELECT into
//! a plan the executor can run.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::ast::{BinaryOp, Expr};
use super::ast::{
    Cte, Frame, FrameBound, JoinKind, OrderBy, Select, SelectItem, SetOp, Sort, TableRef, WindowSpec, With,
    is_aggregate,
};
use super::catalog::{self, Column, IndexSchema, TableSchema, ViewSchema};
use super::expr::{Aggregate, BExpr, Binder, Grouping, Scope, ScopeColumn};
use super::txn::Txn;
use super::value::{DataType, Value, sql_error};
use crate::error::Result;

/// One end of a key range.
#[derive(Clone, Debug)]
pub struct KeyBound {
    pub value: BExpr,
    pub inclusive: bool,
}

/// How rows of a table are found. Key values are expressions over the outer
/// row (empty for the first table of a query), evaluated when the read starts.
#[derive(Clone, Debug)]
pub enum Access {
    Scan,
    /// Equality on the leading primary-key columns; all of them for a point lookup.
    PrimaryKey(Vec<BExpr>),
    /// A range over a single-column primary key.
    PrimaryKeyRange {
        lower: Option<KeyBound>,
        upper: Option<KeyBound>,
    },
    /// Equality on the leading columns of an index.
    Index {
        index: Arc<IndexSchema>,
        values: Vec<BExpr>,
    },
    /// A range over an index's first column.
    IndexRange {
        index: Arc<IndexSchema>,
        lower: Option<KeyBound>,
        upper: Option<KeyBound>,
    },
}

/// Whether `index` holds every column in `needed`: its own columns, plus the
/// primary key that ends each entry. Such reads never touch the rows.
pub fn index_covers(index: &IndexSchema, table: &TableSchema, needed: &[bool]) -> bool {
    needed.iter().enumerate().all(|(c, &need)| !need || index.columns.contains(&c) || table.primary_key.contains(&c))
}

impl Access {
    /// Whether rows come out in primary-key order.
    pub fn in_primary_key_order(&self) -> bool {
        matches!(self, Access::Scan | Access::PrimaryKey(_) | Access::PrimaryKeyRange { .. })
    }

    pub fn describe(&self, table: &TableSchema, binding: &str) -> String {
        let name = if binding == table.name { table.name.clone() } else { format!("{} AS {binding}", table.name) };
        let col = |c: usize| table.columns[c].name.as_str();
        let range = |c: usize, lower: &Option<KeyBound>, upper: &Option<KeyBound>| {
            let mut parts = Vec::new();
            if let Some(b) = lower {
                parts.push(format!("{} {} ?", col(c), if b.inclusive { ">=" } else { ">" }));
            }
            if let Some(b) = upper {
                parts.push(format!("{} {} ?", col(c), if b.inclusive { "<=" } else { "<" }));
            }
            parts.join(" AND ")
        };
        let equalities =
            |cols: &[usize]| cols.iter().map(|&c| format!("{} = ?", col(c))).collect::<Vec<_>>().join(" AND ");
        match self {
            Access::Scan => format!("SCAN {name}"),
            Access::PrimaryKey(values) => {
                format!("SEARCH {name} USING PRIMARY KEY ({})", equalities(&table.primary_key[..values.len()]))
            }
            Access::PrimaryKeyRange { lower, upper } => {
                format!("SEARCH {name} USING PRIMARY KEY ({})", range(table.primary_key[0], lower, upper))
            }
            Access::Index { index, values } => {
                format!("SEARCH {name} USING INDEX {} ({})", index.name, equalities(&index.columns[..values.len()]))
            }
            Access::IndexRange { index, lower, upper } => {
                format!("SEARCH {name} USING INDEX {} ({})", index.name, range(index.columns[0], lower, upper))
            }
        }
    }
}

/// Picks the cheapest way to read `table`, whose columns start at `offset` in
/// the row, using conditions that must hold for every row it returns.
///
/// Any condition used to narrow the read is still evaluated on each row, so
/// the choice only affects speed, never results.
pub fn choose_access(table: &TableSchema, indexes: &[Arc<IndexSchema>], offset: usize, conditions: &[BExpr]) -> Access {
    choose_access_enforcing(table, indexes, offset, conditions).0
}

/// Like [`choose_access`], also returning the positions of the conditions
/// the access path fully enforces: whenever the read is narrowed by the path
/// (rather than falling back to a scan), every row it returns satisfies them.
pub fn choose_access_enforcing(
    table: &TableSchema,
    indexes: &[Arc<IndexSchema>],
    offset: usize,
    conditions: &[BExpr],
) -> (Access, Vec<usize>) {
    let width = table.columns.len();
    let own_column = |e: &BExpr| match e {
        BExpr::Column(i) if (offset..offset + width).contains(i) => Some(i - offset),
        _ => None,
    };
    // A key value may only depend on the outer row, which is known before the read.
    let known_early = |e: &BExpr| e.max_column().is_none_or(|m| m < offset);

    // Each bound remembers which condition supplied it.
    let mut equal: Vec<Option<(BExpr, usize)>> = vec![None; width];
    let mut lower: Vec<Option<(KeyBound, usize)>> = vec![None; width];
    let mut upper: Vec<Option<(KeyBound, usize)>> = vec![None; width];
    for (i, condition) in conditions.iter().enumerate() {
        match condition {
            BExpr::Binary(op, left, right) if op.is_comparison() => {
                let (column, op, value) = match (own_column(left), own_column(right)) {
                    (Some(c), _) if known_early(right) => (c, *op, right),
                    (_, Some(c)) if known_early(left) => (c, op.flipped(), left),
                    _ => continue,
                };
                let bound = |inclusive| Some((KeyBound { value: (**value).clone(), inclusive }, i));
                match op {
                    BinaryOp::Eq => equal[column] = Some(((**value).clone(), i)),
                    BinaryOp::Gt => lower[column] = bound(false),
                    BinaryOp::GtEq => lower[column] = bound(true),
                    BinaryOp::Lt => upper[column] = bound(false),
                    BinaryOp::LtEq => upper[column] = bound(true),
                    _ => {}
                }
            }
            BExpr::Between(e, low, high, false) if known_early(low) && known_early(high) => {
                if let Some(c) = own_column(e) {
                    lower[c] = Some((KeyBound { value: (**low).clone(), inclusive: true }, i));
                    upper[c] = Some((KeyBound { value: (**high).clone(), inclusive: true }, i));
                }
            }
            _ => {}
        }
    }

    let leading_equalities = |cols: &[usize]| cols.iter().take_while(|&&c| equal[c].is_some()).count();
    let values = |cols: &[usize]| cols.iter().map(|&c| equal[c].clone().unwrap().0).collect::<Vec<_>>();
    let sources = |cols: &[usize]| cols.iter().map(|&c| equal[c].as_ref().unwrap().1).collect::<Vec<_>>();
    // A condition is enforced only if every bound it supplied is used: a
    // BETWEEN whose upper bound was overridden by a later condition isn't.
    let range_sources = |c: usize| -> Vec<usize> {
        let (lo, hi) = (lower[c].as_ref().map(|b| b.1), upper[c].as_ref().map(|b| b.1));
        let mut used: Vec<usize> = lo.into_iter().chain(hi).collect();
        used.dedup();
        used.retain(|&i| !matches!(conditions[i], BExpr::Between(..)) || (lo == Some(i) && hi == Some(i)));
        used
    };
    let bound = |b: &Option<(KeyBound, usize)>| b.as_ref().map(|b| b.0.clone());

    let pk = &table.primary_key;
    let pk_prefix = leading_equalities(pk);
    if !pk.is_empty() && pk_prefix == pk.len() {
        return (Access::PrimaryKey(values(pk)), sources(pk));
    }
    // Prefer the index pinning the most columns; a fully pinned unique index wins ties.
    let best_index = indexes
        .iter()
        .map(|index| {
            let pinned = leading_equalities(&index.columns);
            (2 * pinned + usize::from(index.unique && pinned == index.columns.len()), pinned, index)
        })
        .filter(|&(_, pinned, _)| pinned > 0)
        .max_by_key(|&(score, ..)| score);
    if let Some((_, pinned, index)) = best_index
        && pinned >= pk_prefix
    {
        let cols = &index.columns[..pinned];
        return (Access::Index { index: index.clone(), values: values(cols) }, sources(cols));
    }
    if pk_prefix > 0 {
        return (Access::PrimaryKey(values(&pk[..pk_prefix])), sources(&pk[..pk_prefix]));
    }
    if let [c] = pk[..]
        && (lower[c].is_some() || upper[c].is_some())
    {
        return (Access::PrimaryKeyRange { lower: bound(&lower[c]), upper: bound(&upper[c]) }, range_sources(c));
    }
    for index in indexes {
        let c = index.columns[0];
        if lower[c].is_some() || upper[c].is_some() {
            let access = Access::IndexRange { index: index.clone(), lower: bound(&lower[c]), upper: bound(&upper[c]) };
            return (access, range_sources(c));
        }
    }
    (Access::Scan, Vec::new())
}

pub struct TablePlan {
    pub schema: Arc<TableSchema>,
    /// The name the table goes by in the query.
    pub binding: String,
    /// Where its columns start in the combined row.
    pub offset: usize,
    pub access: Access,
    /// The columns the query reads; the rest needn't be decoded.
    pub needed: Vec<bool>,
    /// For `FROM (SELECT ...)` or a common table expression: what produces
    /// the rows, in place of a table.
    pub derived: Option<Derived>,
}

/// Rows that stand in for a table's.
#[derive(Clone)]
pub enum Derived {
    /// A query in FROM, or a common table expression.
    Query(Arc<SelectPlan>),
    /// A `WITH RECURSIVE` query.
    Recursive(Arc<Recursive>),
    /// A recursive query's reference to itself in its recursive part: the
    /// rows of the previous round, numbered as the `Recursive`.
    Work(usize),
}

/// `anchor UNION [ALL] step`, where `step` reads the rows the previous
/// round added until a round adds none.
pub struct Recursive {
    pub id: usize,
    pub anchor: Arc<SelectPlan>,
    pub step: Arc<SelectPlan>,
    /// UNION ALL: keep duplicates (and don't look for them).
    pub all: bool,
}

static NEXT_RECURSIVE: AtomicUsize = AtomicUsize::new(0);

/// The common table expressions a query can name, innermost `WITH` first.
pub struct CteScope<'a> {
    entries: Vec<(String, Vec<String>, Derived)>,
    parent: Option<&'a CteScope<'a>>,
}

impl CteScope<'_> {
    fn find(&self, name: &str) -> Option<(&[String], &Derived)> {
        match self.entries.iter().rev().find(|(n, _, _)| n == name) {
            Some((_, columns, derived)) => Some((columns, derived)),
            None => self.parent?.find(name),
        }
    }
}

impl TablePlan {
    fn describe(&self) -> String {
        match &self.derived {
            Some(Derived::Query(_)) => return format!("SCAN SUBQUERY AS {}", self.binding),
            Some(Derived::Recursive(_)) => return format!("SCAN RECURSIVE QUERY AS {}", self.binding),
            Some(Derived::Work(_)) => return format!("SCAN PREVIOUS ROUND OF {}", self.binding),
            None => {}
        }
        let mut text = self.access.describe(&self.schema, &self.binding);
        if let Access::Index { index, .. } | Access::IndexRange { index, .. } = &self.access
            && index_covers(index, &self.schema, &self.needed)
        {
            text.push_str(" (index only)");
        }
        text
    }
}

pub struct JoinPlan {
    pub kind: JoinKind,
    pub table: TablePlan,
    pub on: Option<BExpr>,
}

#[derive(Clone, Debug)]
pub enum SortKey {
    /// An output column, by position.
    Output(usize),
    /// An expression over the input (or, in aggregate queries, the aggregated) row.
    Expr(BExpr),
}

pub struct GroupPlan {
    pub keys: Vec<BExpr>,
    pub aggregates: Vec<Aggregate>,
}

pub struct SelectPlan {
    /// The first table in FROM, if any.
    pub base: Option<TablePlan>,
    /// Conditions on the base table alone, checked as it is read.
    pub base_filter: Option<BExpr>,
    /// The base conditions the access path doesn't enforce: all that needs
    /// checking when the read was actually narrowed by the path.
    pub base_residual: Option<BExpr>,
    pub joins: Vec<JoinPlan>,
    /// The rest of WHERE, checked once all tables are joined.
    pub filter: Option<BExpr>,
    pub grouping: Option<GroupPlan>,
    pub having: Option<BExpr>,
    pub projection: Vec<BExpr>,
    pub columns: Vec<String>,
    pub distinct: bool,
    pub order_by: Vec<(SortKey, Sort)>,
    /// Rows already arrive in ORDER BY order, so no sort is needed.
    pub presorted: bool,
    /// Constant expressions (they may be parameters), evaluated per run.
    pub limit: Option<BExpr>,
    pub offset: Option<BExpr>,
    /// Projected columns that can be moved out of the source row rather than
    /// copied: plain column references that nothing else in the output reads.
    pub movable: Vec<bool>,
    /// For a compound query (UNION, INTERSECT, EXCEPT): its parts. Only the
    /// columns, ORDER BY, LIMIT and OFFSET above apply then.
    pub compound: Option<Box<Compound>>,
    /// Window functions, whose values are appended to each row (after
    /// grouping, if any) before the projection and ORDER BY read them.
    pub windows: Vec<WindowFunc>,
}

/// The table name that stands for window function values while binding.
pub const WINDOW_TABLE: &str = "\u{0}window";
/// Where window values are numbered from in an aggregate query's row,
/// until the row's width is known.
pub const WINDOW_BASE: usize = usize::MAX / 2;

/// A window function: `kind(args) OVER (PARTITION BY ... ORDER BY ... frame)`.
pub struct WindowFunc {
    pub kind: WindowKind,
    pub args: Vec<BExpr>,
    pub partition: Vec<BExpr>,
    pub order: Vec<(BExpr, Sort)>,
    pub frame: Frame,
}

pub enum WindowKind {
    RowNumber,
    Rank,
    DenseRank,
    PercentRank,
    CumeDist,
    Ntile,
    Lag,
    Lead,
    FirstValue,
    LastValue,
    NthValue,
    /// An aggregate over the frame.
    Aggregate(Aggregate),
}

/// Replaces each window function in `e` (outside aggregates and
/// subqueries) with a placeholder column, adding it to `found`.
fn extract_windows(e: &mut Expr, named: &[(String, WindowSpec)], found: &mut Vec<Expr>) -> Result<()> {
    match e {
        Expr::Window { over, .. } => {
            resolve_window(over, named)?;
            let k = match found.iter().position(|w| w == e) {
                Some(k) => k,
                None => {
                    found.push(e.clone());
                    found.len() - 1
                }
            };
            *e = Expr::Column { table: Some(WINDOW_TABLE.into()), name: k.to_string() };
            Ok(())
        }
        // A window inside an aggregate is reported when the aggregate is bound.
        Expr::Function { name, .. } if is_aggregate(name) => Ok(()),
        _ => {
            let mut result = Ok(());
            e.for_each_child(&mut |child| {
                if result.is_ok() {
                    result = extract_windows(child, named, found);
                }
            });
            result
        }
    }
}

/// Fills in a window that builds on one named in the WINDOW clause.
fn resolve_window(over: &mut WindowSpec, named: &[(String, WindowSpec)]) -> Result<()> {
    let mut steps = 0;
    while let Some(base) = over.base.take() {
        steps += 1;
        let (_, spec) = named
            .iter()
            .find(|(name, _)| *name == base)
            .filter(|_| steps <= named.len())
            .ok_or_else(|| sql_error(format!("no such window: {base}")))?;
        match (over.partition_by.is_empty(), spec.partition_by.is_empty()) {
            (true, _) => over.partition_by = spec.partition_by.clone(),
            (false, true) => {}
            (false, false) => return Err(sql_error(format!("a window built on {base} can't set its PARTITION BY"))),
        }
        match (over.order_by.is_empty(), spec.order_by.is_empty()) {
            (true, _) => over.order_by = spec.order_by.clone(),
            (false, true) => {}
            (false, false) => return Err(sql_error(format!("a window built on {base} can't replace its ORDER BY"))),
        }
        if over.frame.is_none() {
            over.frame = spec.frame;
        }
        over.base = spec.base.clone();
    }
    Ok(())
}

/// Binds a window function, its arguments and its window with `bind`.
fn bind_window(w: &Expr, bind: &mut dyn FnMut(&Expr) -> Result<BExpr>) -> Result<WindowFunc> {
    let Expr::Window { name, args, star, filter, over } = w else { unreachable!("only windows are extracted") };
    let n = args.len();
    let arity = |ok: bool| if ok { Ok(()) } else { Err(sql_error(format!("wrong number of arguments to {name}()"))) };
    let kind = match name.as_str() {
        "row_number" | "rank" | "dense_rank" | "percent_rank" | "cume_dist" => {
            arity(n == 0)?;
            match name.as_str() {
                "row_number" => WindowKind::RowNumber,
                "rank" => WindowKind::Rank,
                "dense_rank" => WindowKind::DenseRank,
                "percent_rank" => WindowKind::PercentRank,
                _ => WindowKind::CumeDist,
            }
        }
        "ntile" => {
            arity(n == 1)?;
            WindowKind::Ntile
        }
        "lag" | "lead" => {
            arity((1..=3).contains(&n))?;
            if name == "lag" { WindowKind::Lag } else { WindowKind::Lead }
        }
        "first_value" | "last_value" => {
            arity(n == 1)?;
            if name == "first_value" { WindowKind::FirstValue } else { WindowKind::LastValue }
        }
        "nth_value" => {
            arity(n == 2)?;
            WindowKind::NthValue
        }
        name if is_aggregate(name) => {
            WindowKind::Aggregate(Aggregate::build(name, args, false, *star, filter.as_deref(), bind)?)
        }
        _ => return Err(sql_error(format!("{name}() is not a window function"))),
    };
    if *star && !matches!(kind, WindowKind::Aggregate(_)) {
        return Err(sql_error(format!("{name}(*) is not allowed")));
    }
    if filter.is_some() && !matches!(kind, WindowKind::Aggregate(_)) {
        return Err(sql_error(format!("FILTER is only for aggregates, not {name}()")));
    }
    let args = match kind {
        WindowKind::Aggregate(_) => Vec::new(),
        _ => args.iter().map(&mut *bind).collect::<Result<_>>()?,
    };
    let partition = over.partition_by.iter().map(&mut *bind).collect::<Result<_>>()?;
    let order = over.order_by.iter().map(|o| Ok((bind(&o.expr)?, o.sort()))).collect::<Result<Vec<_>>>()?;
    // Without a frame: up to the current row and its peers when ordered,
    // otherwise the whole partition (every row is then a peer).
    let end = if order.is_empty() { FrameBound::UnboundedFollowing } else { FrameBound::CurrentRow };
    let frame = over.frame.unwrap_or(Frame { rows: false, start: FrameBound::UnboundedPreceding, end });
    Ok(WindowFunc { kind, args, partition, order, frame })
}

/// The parts of a compound query, combined left to right.
pub struct Compound {
    pub first: Arc<SelectPlan>,
    pub parts: Vec<(SetOp, bool, Arc<SelectPlan>)>,
}

impl SelectPlan {
    /// Whether the query, or any part of it, runs a subquery, reads a query
    /// in FROM, or combines parts: the batched paths leave those alone.
    pub fn is_simple(&self) -> bool {
        let exprs = self
            .base_filter
            .iter()
            .chain(&self.filter)
            .chain(&self.having)
            .chain(&self.projection)
            .chain(self.joins.iter().filter_map(|j| j.on.as_ref()))
            .chain(
                self.grouping
                    .iter()
                    .flat_map(|g| g.keys.iter().chain(g.aggregates.iter().filter_map(|a| a.arg.as_ref()))),
            );
        self.compound.is_none()
            && self.base.iter().chain(self.joins.iter().map(|j| &j.table)).all(|t| t.derived.is_none())
            && self.windows.is_empty()
            && !exprs.into_iter().any(BExpr::has_subquery)
    }
}

impl SelectPlan {
    /// The plan as text, one step per line, for EXPLAIN.
    pub fn explain(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(compound) = &self.compound {
            lines.extend(compound.first.explain());
            for (op, all, part) in &compound.parts {
                lines.push(format!("{op}{}", if *all { " ALL" } else { "" }));
                lines.extend(part.explain());
            }
            if !self.order_by.is_empty() {
                lines.push("ORDER BY".into());
            }
            return lines;
        }
        match &self.base {
            Some(t) => lines.push(t.describe()),
            None => lines.push("CONSTANT ROW".into()),
        }
        if self.base_residual.is_some() {
            lines.push("  FILTER".into());
        }
        for join in &self.joins {
            let kind = if join.kind == JoinKind::Left { "LEFT JOIN" } else { "JOIN" };
            lines.push(format!("{kind} {}", join.table.describe()));
        }
        if self.filter.is_some() {
            lines.push("FILTER".into());
        }
        if let Some(g) = &self.grouping {
            lines.push(format!("AGGREGATE ({} group keys, {} aggregates)", g.keys.len(), g.aggregates.len()));
        }
        if self.having.is_some() {
            lines.push("FILTER (HAVING)".into());
        }
        if !self.windows.is_empty() {
            lines.push(format!("WINDOW ({} functions)", self.windows.len()));
        }
        if self.distinct {
            lines.push("DISTINCT".into());
        }
        if !self.order_by.is_empty() {
            lines.push(if self.presorted { "ORDER BY (rows already in primary key order)" } else { "SORT" }.into());
        }
        let shown = |e: &Option<BExpr>| match e {
            Some(BExpr::Literal(v)) => Some(v.to_string()),
            Some(_) => Some("?".to_string()),
            None => None,
        };
        match (shown(&self.limit), shown(&self.offset)) {
            (Some(l), None) => lines.push(format!("LIMIT {l}")),
            (Some(l), Some(o)) => lines.push(format!("LIMIT {l} OFFSET {o}")),
            (None, Some(o)) => lines.push(format!("OFFSET {o}")),
            (None, None) => {}
        }
        lines
    }
}

fn table_plan(
    txn: &Txn,
    table: &TableRef,
    binder: &Binder,
    offset: usize,
) -> Result<(TablePlan, Arc<[Arc<IndexSchema>]>)> {
    let binding = table.binding().to_string();
    let found = match &table.subquery {
        // A query in FROM can't read the columns of a query around it.
        Some(query) => {
            let plan = plan_select(txn, query, &Binder { outer: None, ..*binder })?;
            Some((plan.columns.clone(), Derived::Query(Arc::new(plan))))
        }
        None => match binder.ctes.and_then(|ctes| ctes.find(&table.name)) {
            Some((columns, derived)) => Some((columns.to_vec(), derived.clone())),
            None if catalog::get_table(txn, &table.name)?.is_none() => match catalog::get_view(txn, &table.name)? {
                Some(view) => Some(plan_view(txn, &view, binder)?),
                None => None,
            },
            None => None,
        },
    };
    if let Some((names, derived)) = found {
        let names = renamed(&binding, names, &table.columns)?;
        let columns = names.into_iter().map(|name| Column::new(name, DataType::Text)).collect();
        let schema = Arc::new(TableSchema::new(u32::MAX, binding.clone(), columns));
        let needed = vec![false; schema.columns.len()];
        let derived = Some(derived);
        return Ok((TablePlan { schema, binding, offset, access: Access::Scan, needed, derived }, Arc::new([])));
    }
    let schema = catalog::require_table(txn, &table.name)?;
    let indexes = catalog::indexes_of(txn, &table.name)?;
    let needed = vec![false; schema.columns.len()];
    Ok((TablePlan { schema, binding, offset, access: Access::Scan, needed, derived: None }, indexes))
}

/// Plans `a UNION b EXCEPT c ...`: each part on its own, then how they
/// combine, and the ORDER BY, LIMIT and OFFSET of the result.
fn plan_compound(txn: &Txn, select: &Select, binder: &Binder) -> Result<SelectPlan> {
    let mut core = select.clone();
    core.compound.clear();
    core.order_by.clear();
    (core.limit, core.offset) = (None, None);
    let first = plan_select(txn, &core, binder)?;
    let mut parts = Vec::new();
    for (op, all, part) in &select.compound {
        let plan = plan_select(txn, part, binder)?;
        if plan.columns.len() != first.columns.len() {
            return Err(sql_error(format!(
                "each part of {op} must return the same number of columns ({} and {})",
                first.columns.len(),
                plan.columns.len()
            )));
        }
        parts.push((*op, *all, Arc::new(plan)));
    }
    let columns = first.columns.clone();
    // ORDER BY names result columns, by position or name.
    let order_by = select
        .order_by
        .iter()
        .map(|o| {
            let column = match &o.expr {
                Expr::Literal(Value::Integer(n)) => {
                    usize::try_from(*n).ok().filter(|n| (1..=columns.len()).contains(n)).map(|n| n - 1)
                }
                Expr::Column { table: None, name } => columns.iter().position(|c| c == name),
                _ => None,
            };
            let column = column.ok_or_else(|| {
                sql_error(format!(
                    "ORDER BY of a {} query must name a result column, not {}",
                    select.compound[0].0, o.expr
                ))
            })?;
            Ok((SortKey::Output(column), o.sort()))
        })
        .collect::<Result<Vec<_>>>()?;
    let count =
        |e: &Option<Expr>, clause: &str| e.as_ref().map(|e| binder.bind(e, &Scope::default(), clause)).transpose();
    Ok(SelectPlan {
        base: None,
        base_filter: None,
        base_residual: None,
        joins: Vec::new(),
        filter: None,
        grouping: None,
        having: None,
        projection: Vec::new(),
        columns,
        distinct: false,
        order_by,
        presorted: false,
        limit: count(&select.limit, "LIMIT")?,
        offset: count(&select.offset, "OFFSET")?,
        movable: Vec::new(),
        compound: Some(Box::new(Compound { first: Arc::new(first), parts })),
        windows: Vec::new(),
    })
}

/// The most views a query may reach through other views.
const MAX_VIEW_DEPTH: usize = 32;

thread_local! {
    static VIEW_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Plans a view's query, which sees neither the query around it nor its CTEs.
fn plan_view(txn: &Txn, view: &ViewSchema, binder: &Binder) -> Result<(Vec<String>, Derived)> {
    let depth = VIEW_DEPTH.get();
    if depth >= MAX_VIEW_DEPTH {
        return Err(sql_error(format!(
            "views nest more than {MAX_VIEW_DEPTH} deep; does {} refer to itself?",
            view.name
        )));
    }
    VIEW_DEPTH.set(depth + 1);
    let plan = plan_select(txn, &view.query, &Binder { outer: None, ctes: None, ..*binder });
    VIEW_DEPTH.set(depth);
    let plan = plan?;
    let columns = renamed(&view.name, plan.columns.clone(), &view.columns)?;
    Ok((columns, Derived::Query(Arc::new(plan))))
}

/// Column names `given` in place of a query's own.
fn renamed(name: &str, own: Vec<String>, given: &[String]) -> Result<Vec<String>> {
    if given.is_empty() {
        return Ok(own);
    }
    if given.len() != own.len() {
        return Err(sql_error(format!("{name} has {} columns, but {} names are given", own.len(), given.len())));
    }
    Ok(given.to_vec())
}

/// Plans a query with a WITH clause: each common table expression in turn,
/// able to name those before it, then the query itself.
fn plan_with(txn: &Txn, select: &Select, with: &With, binder: &Binder) -> Result<SelectPlan> {
    let mut scope = CteScope { entries: Vec::new(), parent: binder.ctes };
    for cte in &with.ctes {
        if scope.entries.iter().any(|(name, _, _)| *name == cte.name) {
            return Err(sql_error(format!("WITH names {} more than once", cte.name)));
        }
        let entry = plan_cte(txn, cte, with.recursive, &Binder { outer: None, ctes: Some(&scope), ..*binder })?;
        scope.entries.push(entry);
    }
    let mut query = select.clone();
    query.with = None;
    plan_select(txn, &query, &Binder { ctes: Some(&scope), ..*binder })
}

fn plan_cte(txn: &Txn, cte: &Cte, recursive: bool, binder: &Binder) -> Result<(String, Vec<String>, Derived)> {
    let names_itself = |query: &Select| {
        let mut found = false;
        query.visit_tables(&mut |t| found |= t.subquery.is_none() && t.name == cte.name);
        found
    };
    let query = &cte.query;
    if !recursive || !names_itself(query) {
        let plan = plan_select(txn, query, binder)?;
        let columns = renamed(&cte.name, plan.columns.clone(), &cte.columns)?;
        return Ok((cte.name.clone(), columns, Derived::Query(Arc::new(plan))));
    }
    // anchor UNION [ALL] step, where only the step reads the query itself.
    let shape = || sql_error(format!("recursive query {} must be: anchor UNION [ALL] recursive part", cte.name));
    let Some(((op, all, step), anchor_parts)) = query.compound.split_last() else { return Err(shape()) };
    if *op != SetOp::Union {
        return Err(shape());
    }
    if !query.order_by.is_empty() || query.limit.is_some() || query.offset.is_some() {
        return Err(sql_error(format!("recursive query {} can't have its own ORDER BY or LIMIT", cte.name)));
    }
    let mut anchor = query.clone();
    anchor.compound = anchor_parts.to_vec();
    if names_itself(&anchor) {
        return Err(sql_error(format!("only the last part of recursive query {} may refer to it", cte.name)));
    }
    let anchor = plan_select(txn, &anchor, binder)?;
    let columns = renamed(&cte.name, anchor.columns.clone(), &cte.columns)?;
    let id = NEXT_RECURSIVE.fetch_add(1, Ordering::Relaxed);
    let work = CteScope { entries: vec![(cte.name.clone(), columns.clone(), Derived::Work(id))], parent: binder.ctes };
    let step = plan_select(txn, step, &Binder { ctes: Some(&work), ..*binder })?;
    if step.columns.len() != columns.len() {
        return Err(sql_error(format!(
            "each part of recursive query {} must return the same number of columns ({} and {})",
            cte.name,
            columns.len(),
            step.columns.len()
        )));
    }
    let recursive = Recursive { id, anchor: Arc::new(anchor), step: Arc::new(step), all: *all };
    Ok((cte.name.clone(), columns, Derived::Recursive(Arc::new(recursive))))
}

pub fn plan_select(txn: &Txn, select: &Select, binder: &Binder) -> Result<SelectPlan> {
    if let Some(with) = &select.with {
        return plan_with(txn, select, with, binder);
    }
    if !select.compound.is_empty() {
        return plan_compound(txn, select, binder);
    }
    // FROM: load the tables and lay out the combined row.
    let mut scope = Scope::default();
    let mut base = None;
    let mut joined = Vec::new();
    if let Some(from) = &select.from {
        let (plan, indexes) = table_plan(txn, &from.base, binder, 0)?;
        scope.add_table(&plan.schema, &plan.binding);
        base = Some((plan, indexes));
        for join in &from.joins {
            let binding = join.table.binding();
            if scope.has_table(binding) {
                return Err(sql_error(format!("table name {binding} is used more than once; give it an alias")));
            }
            let (plan, indexes) = table_plan(txn, &join.table, binder, scope.columns.len())?;
            scope.add_table(&plan.schema, &plan.binding);
            joined.push((join, plan, indexes));
        }
    }

    // WHERE: conditions on the base table alone are checked (and used to
    // narrow the read) as it is scanned; the rest wait until after the joins.
    let base_width = base.as_ref().map_or(0, |(p, _): &(TablePlan, _)| p.schema.columns.len());
    let (mut base_conditions, mut rest) = (Vec::new(), Vec::new());
    if let Some(filter) = &select.filter {
        for condition in binder.bind(filter, &scope, "WHERE")?.into_conjuncts() {
            if condition.max_column().is_none_or(|m| m < base_width) {
                base_conditions.push(condition);
            } else {
                rest.push(condition);
            }
        }
    }
    let mut enforced = Vec::new();
    let base = base.map(|(mut plan, indexes)| {
        (plan.access, enforced) = choose_access_enforcing(&plan.schema, &indexes, 0, &base_conditions);
        plan
    });
    let base_residual = BExpr::conjoin(
        base_conditions.iter().enumerate().filter(|(i, _)| !enforced.contains(i)).map(|(_, c)| c.clone()).collect(),
    );

    let mut joins = Vec::new();
    for (join, mut plan, indexes) in joined {
        let prefix = Scope { columns: scope.columns[..plan.offset + plan.schema.columns.len()].to_vec() };
        let on = join.on.as_ref().map(|on| binder.bind(on, &prefix, "ON")).transpose()?;
        let conditions = on.clone().map(BExpr::into_conjuncts).unwrap_or_default();
        plan.access = choose_access(&plan.schema, &indexes, plan.offset, &conditions);
        joins.push(JoinPlan { kind: join.kind, table: plan, on });
    }

    // Expand wildcards into column references, so they bind like any column.
    let mut items: Vec<(Expr, Option<String>)> = Vec::new();
    for item in &select.items {
        match item {
            SelectItem::Wildcard | SelectItem::TableWildcard(_) => {
                let wanted = match item {
                    SelectItem::TableWildcard(t) if !scope.has_table(t) => {
                        return Err(sql_error(format!("no such table: {t}")));
                    }
                    SelectItem::TableWildcard(t) => Some(t),
                    _ => None,
                };
                if select.from.is_none() {
                    return Err(sql_error("SELECT * needs a FROM clause"));
                }
                for c in scope.columns.iter().filter(|c| wanted.is_none_or(|t| c.table == *t)) {
                    items.push((Expr::Column { table: Some(c.table.clone()), name: c.name.clone() }, None));
                }
            }
            SelectItem::Expr { expr, alias } => items.push((expr.clone(), alias.clone())),
        }
    }
    let columns: Vec<String> = items
        .iter()
        .map(|(expr, alias)| match (alias, expr) {
            (Some(alias), _) => alias.clone(),
            (None, Expr::Column { name, .. }) => name.clone(),
            (None, e) => e.to_string(),
        })
        .collect();

    // Window functions become placeholder columns, whose values are worked
    // out once WHERE, GROUP BY and HAVING are done.
    let mut window_exprs: Vec<Expr> = Vec::new();
    for (e, _) in &mut items {
        extract_windows(e, &select.windows, &mut window_exprs)?;
    }
    let mut ordering: Vec<OrderBy> = select.order_by.clone();
    for o in &mut ordering {
        extract_windows(&mut o.expr, &select.windows, &mut window_exprs)?;
    }

    let aggregated = !select.group_by.is_empty()
        || select.having.is_some()
        || items.iter().any(|(e, _)| e.contains_aggregate())
        || ordering.iter().any(|o| o.expr.contains_aggregate())
        || window_exprs.iter().any(Expr::contains_aggregate);

    // ORDER BY may name an output column by position or alias.
    let output_reference = |expr: &Expr| -> Result<Option<usize>> {
        match expr {
            Expr::Literal(Value::Integer(n)) => match usize::try_from(*n) {
                Ok(n) if (1..=items.len()).contains(&n) => Ok(Some(n - 1)),
                _ => Err(sql_error(format!("position {n} is not in the select list"))),
            },
            Expr::Column { table: None, name } => {
                Ok(items.iter().position(|(_, alias)| alias.as_deref() == Some(name)))
            }
            _ => Ok(None),
        }
    };

    let (grouping, having, mut projection, mut order_by, windows);
    if aggregated {
        let mut g = Grouping::default();
        for key in &select.group_by {
            // GROUP BY may also refer to select items by position or alias.
            let key = match output_reference(key)? {
                Some(i) if !matches!(key, Expr::Column { .. }) || scope.resolve(None, &columns[i]).is_err() => {
                    &items[i].0
                }
                _ => key,
            };
            g.keys.push(binder.bind(key, &scope, "GROUP BY")?);
        }
        projection = items.iter().map(|(e, _)| binder.bind_grouped(e, &scope, &mut g)).collect::<Result<Vec<_>>>()?;
        having = select.having.as_ref().map(|h| binder.bind_grouped(h, &scope, &mut g)).transpose()?;
        order_by = ordering
            .iter()
            .map(|o| {
                let key = match output_reference(&o.expr)? {
                    Some(i) => SortKey::Output(i),
                    None => SortKey::Expr(binder.bind_grouped(&o.expr, &scope, &mut g)?),
                };
                Ok((key, o.sort()))
            })
            .collect::<Result<Vec<_>>>()?;
        windows = window_exprs
            .iter()
            .map(|w| bind_window(w, &mut |e| binder.bind_grouped(e, &scope, &mut g)))
            .collect::<Result<Vec<_>>>()?;
        // Window values follow the group keys and aggregates in the row.
        let width = g.keys.len() + g.aggregates.len();
        let place = |i: usize| if i >= WINDOW_BASE { width + i - WINDOW_BASE } else { i };
        projection.iter_mut().for_each(|e| e.map_columns(&place));
        for (key, _) in &mut order_by {
            if let SortKey::Expr(e) = key {
                e.map_columns(&place);
            }
        }
        grouping = Some(GroupPlan { keys: g.keys, aggregates: g.aggregates.into_iter().map(|(_, a)| a).collect() });
    } else {
        grouping = None;
        having = None;
        // Window values follow the row's columns.
        let mut output_scope = scope.clone();
        output_scope
            .columns
            .extend((0..window_exprs.len()).map(|k| ScopeColumn { table: WINDOW_TABLE.into(), name: k.to_string() }));
        projection =
            items.iter().map(|(e, _)| binder.bind(e, &output_scope, "the select list")).collect::<Result<Vec<_>>>()?;
        order_by = ordering
            .iter()
            .map(|o| {
                let key = match output_reference(&o.expr)? {
                    Some(i) => SortKey::Output(i),
                    None => SortKey::Expr(binder.bind(&o.expr, &output_scope, "ORDER BY")?),
                };
                Ok((key, o.sort()))
            })
            .collect::<Result<Vec<_>>>()?;
        windows = window_exprs
            .iter()
            .map(|w| bind_window(w, &mut |e| binder.bind(e, &scope, "OVER")))
            .collect::<Result<Vec<_>>>()?;
    }

    let count =
        |e: &Option<Expr>, clause: &str| e.as_ref().map(|e| binder.bind(e, &Scope::default(), clause)).transpose();
    let limit = count(&select.limit, "LIMIT")?;
    let offset = count(&select.offset, "OFFSET")?;

    // A single-table scan in primary-key order already satisfies ORDER BY <pk>.
    let presorted = match (&base, &order_by[..]) {
        (Some(t), [(key, sort)])
            if !sort.desc && joins.is_empty() && grouping.is_none() && !select.distinct && windows.is_empty() =>
        {
            let sorted_column = match key {
                SortKey::Output(i) => projection.get(*i),
                SortKey::Expr(e) => Some(e),
            };
            t.access.in_primary_key_order()
                && matches!((&t.schema.primary_key[..], sorted_column), ([pk], Some(BExpr::Column(c))) if pk == c)
        }
        _ => false,
    };

    // Note which columns the query reads, so the rest are never decoded.
    let mut used = vec![false; scope.columns.len() + windows.len()];
    let mut source_exprs: Vec<&BExpr> = base_conditions.iter().chain(&rest).collect();
    source_exprs.extend(joins.iter().filter_map(|j| j.on.as_ref()));
    match &grouping {
        Some(g) => {
            source_exprs.extend(&g.keys);
            source_exprs.extend(g.aggregates.iter().flat_map(|a| a.arg.iter().chain(&a.filter)));
        }
        None => {
            source_exprs.extend(&projection);
            for w in &windows {
                source_exprs.extend(w.args.iter().chain(&w.partition).chain(w.order.iter().map(|(e, _)| e)));
                if let WindowKind::Aggregate(aggregate) = &w.kind {
                    source_exprs.extend(aggregate.arg.iter().chain(&aggregate.filter));
                }
            }
            source_exprs.extend(order_by.iter().filter_map(|(k, _)| match k {
                SortKey::Expr(e) => Some(e),
                SortKey::Output(_) => None,
            }));
        }
    }
    source_exprs.iter().for_each(|e| e.mark_columns(&mut used));
    let mut base = base;
    for t in base.iter_mut().chain(joins.iter_mut().map(|j| &mut j.table)) {
        t.needed = used[t.offset..t.offset + t.schema.columns.len()].to_vec();
    }

    // A plain column that appears once in the output, and nowhere in ORDER
    // BY, can be moved out of the row instead of copied.
    let movable = if grouping.is_some() {
        vec![false; projection.len()]
    } else {
        let mut reads = vec![0usize; scope.columns.len() + windows.len()];
        let mut count_reads = |e: &BExpr| {
            let mut seen = vec![false; reads.len()];
            e.mark_columns(&mut seen);
            seen.iter().enumerate().filter(|&(_, &s)| s).for_each(|(c, _)| reads[c] += 1);
        };
        projection.iter().for_each(&mut count_reads);
        order_by.iter().for_each(|(k, _)| {
            if let SortKey::Expr(e) = k {
                count_reads(e);
            }
        });
        projection.iter().map(|e| matches!(e, BExpr::Column(c) if reads[*c] == 1)).collect()
    };

    Ok(SelectPlan {
        base,
        base_filter: BExpr::conjoin(base_conditions),
        base_residual,
        joins,
        filter: BExpr::conjoin(rest),
        grouping,
        having,
        projection,
        columns,
        distinct: select.distinct,
        order_by,
        presorted,
        limit,
        offset,
        movable,
        compound: None,
        windows,
    })
}
