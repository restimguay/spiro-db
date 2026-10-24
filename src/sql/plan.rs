//! Query planning: choosing how to read each table, and binding a SELECT into
//! a plan the executor can run.

use std::sync::Arc;

use super::ast::{BinaryOp, Expr};
use super::ast::{JoinKind, Select, SelectItem};
use super::catalog::{self, IndexSchema, TableSchema};
use super::expr::{Aggregate, BExpr, Binder, Grouping, Scope};
use super::txn::Txn;
use super::value::{Value, sql_error};
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
}

impl TablePlan {
    fn describe(&self) -> String {
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
    pub order_by: Vec<(SortKey, bool)>,
    /// Rows already arrive in ORDER BY order, so no sort is needed.
    pub presorted: bool,
    /// Constant expressions (they may be parameters), evaluated per run.
    pub limit: Option<BExpr>,
    pub offset: Option<BExpr>,
    /// Projected columns that can be moved out of the source row rather than
    /// copied: plain column references that nothing else in the output reads.
    pub movable: Vec<bool>,
}

impl SelectPlan {
    /// The plan as text, one step per line, for EXPLAIN.
    pub fn explain(&self) -> Vec<String> {
        let mut lines = Vec::new();
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

fn table_plan(txn: &Txn, name: &str, binding: &str, offset: usize) -> Result<(TablePlan, Arc<[Arc<IndexSchema>]>)> {
    let schema = catalog::require_table(txn, name)?;
    let indexes = catalog::indexes_of(txn, name)?;
    let needed = vec![false; schema.columns.len()];
    Ok((TablePlan { schema, binding: binding.to_string(), offset, access: Access::Scan, needed }, indexes))
}

pub fn plan_select(txn: &Txn, select: &Select, binder: &Binder) -> Result<SelectPlan> {
    // FROM: load the tables and lay out the combined row.
    let mut scope = Scope::default();
    let mut base = None;
    let mut joined = Vec::new();
    if let Some(from) = &select.from {
        let (plan, indexes) = table_plan(txn, &from.base.name, from.base.binding(), 0)?;
        scope.add_table(&plan.schema, &plan.binding);
        base = Some((plan, indexes));
        for join in &from.joins {
            let binding = join.table.binding();
            if scope.has_table(binding) {
                return Err(sql_error(format!("table name {binding} is used more than once; give it an alias")));
            }
            let (plan, indexes) = table_plan(txn, &join.table.name, binding, scope.columns.len())?;
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

    let aggregated = !select.group_by.is_empty()
        || select.having.is_some()
        || items.iter().any(|(e, _)| e.contains_aggregate())
        || select.order_by.iter().any(|o| o.expr.contains_aggregate());

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

    let (grouping, having, projection, order_by);
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
        order_by = select
            .order_by
            .iter()
            .map(|o| {
                let key = match output_reference(&o.expr)? {
                    Some(i) => SortKey::Output(i),
                    None => SortKey::Expr(binder.bind_grouped(&o.expr, &scope, &mut g)?),
                };
                Ok((key, o.desc))
            })
            .collect::<Result<Vec<_>>>()?;
        grouping = Some(GroupPlan { keys: g.keys, aggregates: g.aggregates.into_iter().map(|(_, a)| a).collect() });
    } else {
        grouping = None;
        having = None;
        projection =
            items.iter().map(|(e, _)| binder.bind(e, &scope, "the select list")).collect::<Result<Vec<_>>>()?;
        order_by = select
            .order_by
            .iter()
            .map(|o| {
                let key = match output_reference(&o.expr)? {
                    Some(i) => SortKey::Output(i),
                    None => SortKey::Expr(binder.bind(&o.expr, &scope, "ORDER BY")?),
                };
                Ok((key, o.desc))
            })
            .collect::<Result<Vec<_>>>()?;
    }

    let count =
        |e: &Option<Expr>, clause: &str| e.as_ref().map(|e| binder.bind(e, &Scope::default(), clause)).transpose();
    let limit = count(&select.limit, "LIMIT")?;
    let offset = count(&select.offset, "OFFSET")?;

    // A single-table scan in primary-key order already satisfies ORDER BY <pk>.
    let presorted = match (&base, &order_by[..]) {
        (Some(t), [(key, false)]) if joins.is_empty() && grouping.is_none() && !select.distinct => {
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
    let mut used = vec![false; scope.columns.len()];
    let mut source_exprs: Vec<&BExpr> = base_conditions.iter().chain(&rest).collect();
    source_exprs.extend(joins.iter().filter_map(|j| j.on.as_ref()));
    match &grouping {
        Some(g) => {
            source_exprs.extend(&g.keys);
            source_exprs.extend(g.aggregates.iter().filter_map(|a| a.arg.as_ref()));
        }
        None => {
            source_exprs.extend(&projection);
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
        let mut reads = vec![0usize; scope.columns.len()];
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
    })
}
