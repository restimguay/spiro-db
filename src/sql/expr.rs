//! Expression binding and evaluation.
//!
//! Binding turns a parsed expression into a [`BExpr`]: column names become
//! positions in the row and parameters become values, so unknown or ambiguous
//! names are reported before a single row is read.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::HashSet;

use super::ast::{BinaryOp, Expr, UnaryOp, is_aggregate};
use super::catalog::TableSchema;
use super::encoding::encode_key;
use super::value::{DataType, Value, sql_error};
use crate::error::Result;

#[derive(Clone, Debug)]
pub struct ScopeColumn {
    /// The name the column's table goes by in the query (its alias, if any).
    pub table: String,
    pub name: String,
}

/// The columns of the rows that expressions are evaluated against, in order.
#[derive(Clone, Debug, Default)]
pub struct Scope {
    pub columns: Vec<ScopeColumn>,
}

impl Scope {
    pub fn of_table(table: &TableSchema, binding: &str) -> Scope {
        let mut scope = Scope::default();
        scope.add_table(table, binding);
        scope
    }

    pub fn add_table(&mut self, table: &TableSchema, binding: &str) {
        let columns = table.columns.iter().map(|c| ScopeColumn { table: binding.to_string(), name: c.name.clone() });
        self.columns.extend(columns);
    }

    pub fn has_table(&self, binding: &str) -> bool {
        self.columns.iter().any(|c| c.table == binding)
    }

    pub fn resolve(&self, table: Option<&str>, name: &str) -> Result<usize> {
        let mut found = None;
        for (i, c) in self.columns.iter().enumerate() {
            if c.name == name && table.is_none_or(|t| c.table == t) {
                if found.is_some() {
                    return Err(sql_error(format!("ambiguous column name: {name}")));
                }
                found = Some(i);
            }
        }
        found.ok_or_else(|| match table {
            Some(t) if !self.has_table(t) => sql_error(format!("no such table: {t}")),
            Some(t) => sql_error(format!("no such column: {t}.{name}")),
            None => sql_error(format!("no such column: {name}")),
        })
    }
}

/// A bound expression, ready to evaluate against a row.
#[derive(Clone, Debug, PartialEq)]
pub enum BExpr {
    Literal(Value),
    Column(usize),
    /// A query parameter, numbered from 0, supplied when the plan runs; so
    /// one plan serves every execution of a prepared statement.
    Param(usize),
    Neg(Box<BExpr>),
    Not(Box<BExpr>),
    Binary(BinaryOp, Box<BExpr>, Box<BExpr>),
    IsNull(Box<BExpr>, bool),
    InList(Box<BExpr>, Vec<BExpr>, bool),
    Between(Box<BExpr>, Box<BExpr>, Box<BExpr>, bool),
    /// value, pattern, negated, case-insensitive
    Like(Box<BExpr>, Box<BExpr>, bool, bool),
    Call(Function, Vec<BExpr>),
    Cast(Box<BExpr>, DataType),
    Case(Option<Box<BExpr>>, Vec<(BExpr, BExpr)>, Option<Box<BExpr>>),
}

impl BExpr {
    /// The highest column position the expression reads, if it reads any.
    pub fn max_column(&self) -> Option<usize> {
        let mut max = None;
        self.visit(&mut |e| {
            if let BExpr::Column(i) = e {
                max = max.max(Some(*i));
            }
        });
        max
    }

    /// Marks every column position the expression reads.
    pub fn mark_columns(&self, used: &mut [bool]) {
        self.visit(&mut |e| {
            if let BExpr::Column(i) = e {
                used[*i] = true;
            }
        });
    }

    fn visit(&self, f: &mut dyn FnMut(&BExpr)) {
        f(self);
        match self {
            BExpr::Literal(_) | BExpr::Column(_) | BExpr::Param(_) => {}
            BExpr::Neg(e) | BExpr::Not(e) | BExpr::IsNull(e, _) | BExpr::Cast(e, _) => e.visit(f),
            BExpr::Binary(_, a, b) | BExpr::Like(a, b, ..) => {
                a.visit(f);
                b.visit(f);
            }
            BExpr::InList(e, list, _) => {
                e.visit(f);
                list.iter().for_each(|x| x.visit(f));
            }
            BExpr::Between(a, b, c, _) => {
                a.visit(f);
                b.visit(f);
                c.visit(f);
            }
            BExpr::Call(_, args) => args.iter().for_each(|x| x.visit(f)),
            BExpr::Case(operand, branches, otherwise) => {
                operand.iter().for_each(|x| x.visit(f));
                for (when, then) in branches {
                    when.visit(f);
                    then.visit(f);
                }
                otherwise.iter().for_each(|x| x.visit(f));
            }
        }
    }

    /// Splits `a AND b AND c` into `[a, b, c]`.
    pub fn into_conjuncts(self) -> Vec<BExpr> {
        match self {
            BExpr::Binary(BinaryOp::And, a, b) => {
                let mut out = a.into_conjuncts();
                out.extend(b.into_conjuncts());
                out
            }
            e => vec![e],
        }
    }

    pub fn conjoin(exprs: Vec<BExpr>) -> Option<BExpr> {
        exprs.into_iter().reduce(|a, b| BExpr::Binary(BinaryOp::And, Box::new(a), Box::new(b)))
    }

    /// Evaluates the expression as a filter: only TRUE passes.
    pub fn matches(&self, row: &[Value], params: &[Value]) -> Result<bool> {
        Ok(self.eval(row, params)?.truth()? == Some(true))
    }

    /// Evaluates the expression against `row`, with `params` supplying the
    /// values of query parameters.
    pub fn eval(&self, row: &[Value], params: &[Value]) -> Result<Value> {
        Ok(match self {
            BExpr::Literal(v) => v.clone(),
            BExpr::Column(i) => row[*i].clone(),
            BExpr::Param(i) => param(params, *i)?.clone(),
            BExpr::Neg(e) => match &*operand(e, row, params)? {
                Value::Null => Value::Null,
                Value::Integer(i) => Value::Integer(i.checked_neg().ok_or_else(|| sql_error("integer overflow"))?),
                Value::Real(r) => Value::Real(-r),
                v => return Err(sql_error(format!("cannot negate {}", v.type_name()))),
            },
            BExpr::Not(e) => operand(e, row, params)?.truth()?.map_or(Value::Null, |b| Value::Boolean(!b)),
            BExpr::Binary(op, a, b) => binary(*op, a, b, row, params)?,
            BExpr::IsNull(e, negated) => Value::Boolean(operand(e, row, params)?.is_null() != *negated),
            BExpr::InList(e, list, negated) => {
                let v = operand(e, row, params)?;
                if v.is_null() {
                    return Ok(Value::Null);
                }
                let mut saw_null = false;
                for item in list {
                    match v.compare(&*operand(item, row, params)?)? {
                        Some(Ordering::Equal) => return Ok(Value::Boolean(!negated)),
                        None => saw_null = true,
                        _ => {}
                    }
                }
                if saw_null { Value::Null } else { Value::Boolean(*negated) }
            }
            BExpr::Between(e, low, high, negated) => {
                let v = operand(e, row, params)?;
                let above = v.compare(&*operand(low, row, params)?)?.map(|o| o != Ordering::Less);
                let below = v.compare(&*operand(high, row, params)?)?.map(|o| o != Ordering::Greater);
                match and3(above, below) {
                    None => Value::Null,
                    Some(b) => Value::Boolean(b != *negated),
                }
            }
            BExpr::Like(e, pattern, negated, case_insensitive) => {
                match (&*operand(e, row, params)?, &*operand(pattern, row, params)?) {
                    (Value::Null, _) | (_, Value::Null) => Value::Null,
                    (Value::Text(s), Value::Text(p)) => Value::Boolean(like(s, p, *case_insensitive) != *negated),
                    (a, b) => {
                        return Err(sql_error(format!("LIKE needs TEXT, got {} and {}", a.type_name(), b.type_name())));
                    }
                }
            }
            BExpr::Call(f, args) => f.call(args.iter().map(|a| a.eval(row, params)).collect::<Result<_>>()?)?,
            BExpr::Cast(e, ty) => operand(e, row, params)?.cast(*ty)?,
            BExpr::Case(case, branches, otherwise) => {
                let case = case.as_ref().map(|o| operand(o, row, params)).transpose()?;
                for (when, then) in branches {
                    let hit = match &case {
                        Some(v) => v.compare(&*operand(when, row, params)?)? == Some(Ordering::Equal),
                        None => when.matches(row, params)?,
                    };
                    if hit {
                        return then.eval(row, params);
                    }
                }
                otherwise.as_ref().map_or(Ok(Value::Null), |e| e.eval(row, params))?
            }
        })
    }
}

fn param(params: &[Value], i: usize) -> Result<&Value> {
    params.get(i).ok_or_else(|| sql_error(format!("no value for parameter ${}", i + 1)))
}

/// Evaluates an operand, borrowing instead of copying when it is a column,
/// literal or parameter. Filters compare values far more often than they
/// produce them, so this saves a copy (and, for text, an allocation) per test.
pub(super) fn operand<'a>(e: &'a BExpr, row: &'a [Value], params: &'a [Value]) -> Result<Cow<'a, Value>> {
    Ok(match e {
        BExpr::Column(i) => Cow::Borrowed(&row[*i]),
        BExpr::Literal(v) => Cow::Borrowed(v),
        BExpr::Param(i) => Cow::Borrowed(param(params, *i)?),
        e => Cow::Owned(e.eval(row, params)?),
    })
}

fn and3(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}

fn binary(op: BinaryOp, a: &BExpr, b: &BExpr, row: &[Value], params: &[Value]) -> Result<Value> {
    let to_value = |t: Option<bool>| t.map_or(Value::Null, Value::Boolean);
    match op {
        BinaryOp::And => {
            let left = operand(a, row, params)?.truth()?;
            if left == Some(false) {
                return Ok(Value::Boolean(false));
            }
            Ok(to_value(and3(left, operand(b, row, params)?.truth()?)))
        }
        BinaryOp::Or => {
            let left = operand(a, row, params)?.truth()?;
            if left == Some(true) {
                return Ok(Value::Boolean(true));
            }
            let right = operand(b, row, params)?.truth()?;
            Ok(to_value(match (left, right) {
                (_, Some(true)) => Some(true),
                (Some(false), Some(false)) => Some(false),
                _ => None,
            }))
        }
        op if op.is_comparison() => {
            let ordering = operand(a, row, params)?.compare(&*operand(b, row, params)?)?;
            Ok(to_value(ordering.map(|o| match op {
                BinaryOp::Eq => o == Ordering::Equal,
                BinaryOp::NotEq => o != Ordering::Equal,
                BinaryOp::Lt => o == Ordering::Less,
                BinaryOp::LtEq => o != Ordering::Greater,
                BinaryOp::Gt => o == Ordering::Greater,
                _ => o != Ordering::Less,
            })))
        }
        BinaryOp::Concat => match (&*operand(a, row, params)?, &*operand(b, row, params)?) {
            (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
            (x, y) => Ok(Value::Text(format!("{x}{y}"))),
        },
        op => arithmetic(op, &*operand(a, row, params)?, &*operand(b, row, params)?),
    }
}

pub fn arithmetic(op: BinaryOp, a: &Value, b: &Value) -> Result<Value> {
    let (x, y) = match (a, b) {
        (Value::Null, _) | (_, Value::Null) => return Ok(Value::Null),
        (Value::Integer(x), Value::Integer(y)) => {
            let result = match op {
                BinaryOp::Add => x.checked_add(*y),
                BinaryOp::Sub => x.checked_sub(*y),
                BinaryOp::Mul => x.checked_mul(*y),
                BinaryOp::Div | BinaryOp::Rem if *y == 0 => return Err(sql_error("division by zero")),
                BinaryOp::Div => x.checked_div(*y),
                _ => x.checked_rem(*y),
            };
            return result.map(Value::Integer).ok_or_else(|| sql_error("integer overflow"));
        }
        (Value::Integer(_) | Value::Real(_), Value::Integer(_) | Value::Real(_)) => {
            (a.as_f64().unwrap(), b.as_f64().unwrap())
        }
        _ => return Err(sql_error(format!("cannot apply {op} to {} and {}", a.type_name(), b.type_name()))),
    };
    if matches!(op, BinaryOp::Div | BinaryOp::Rem) && y == 0.0 {
        return Err(sql_error("division by zero"));
    }
    Ok(Value::Real(match op {
        BinaryOp::Add => x + y,
        BinaryOp::Sub => x - y,
        BinaryOp::Mul => x * y,
        BinaryOp::Div => x / y,
        _ => x % y,
    }))
}

/// SQL LIKE: `%` matches any run of characters, `_` exactly one.
pub fn like(text: &str, pattern: &str, case_insensitive: bool) -> bool {
    let fold = |s: &str| -> Vec<char> {
        if case_insensitive { s.to_lowercase().chars().collect() } else { s.chars().collect() }
    };
    let (t, p) = (fold(text), fold(pattern));
    let (mut ti, mut pi) = (0, 0);
    // Where the last `%` was, and where in the text it was tried from.
    let mut backtrack: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && p[pi] == '%' {
            backtrack = Some((pi, ti));
            pi += 1;
        } else if pi < p.len() && (p[pi] == '_' || p[pi] == t[ti]) {
            ti += 1;
            pi += 1;
        } else if let Some((star, from)) = backtrack {
            // Let the last `%` swallow one more character and try again.
            pi = star + 1;
            ti = from + 1;
            backtrack = Some((star, from + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '%')
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Function {
    Abs,
    Coalesce,
    Length,
    Lower,
    Nullif,
    Replace,
    Round,
    Substr,
    Trim,
    Typeof,
    Upper,
}

impl Function {
    fn resolve(name: &str, args: usize) -> Result<Function> {
        let (function, arity_ok) = match name {
            "abs" => (Function::Abs, args == 1),
            "coalesce" | "ifnull" => (Function::Coalesce, args >= 1),
            "length" | "char_length" => (Function::Length, args == 1),
            "lower" => (Function::Lower, args == 1),
            "nullif" => (Function::Nullif, args == 2),
            "replace" => (Function::Replace, args == 3),
            "round" => (Function::Round, args == 1 || args == 2),
            "substr" | "substring" => (Function::Substr, args == 2 || args == 3),
            "trim" => (Function::Trim, args == 1),
            "typeof" => (Function::Typeof, args == 1),
            "upper" => (Function::Upper, args == 1),
            _ => return Err(sql_error(format!("no such function: {name}"))),
        };
        if !arity_ok {
            return Err(sql_error(format!("wrong number of arguments to {name}()")));
        }
        Ok(function)
    }

    fn call(self, mut args: Vec<Value>) -> Result<Value> {
        let text = |v: &Value| v.to_string();
        if !matches!(self, Function::Coalesce | Function::Nullif | Function::Typeof) && args.iter().any(Value::is_null)
        {
            return Ok(Value::Null);
        }
        Ok(match self {
            Function::Abs => match &args[0] {
                Value::Integer(i) => Value::Integer(i.checked_abs().ok_or_else(|| sql_error("integer overflow"))?),
                Value::Real(r) => Value::Real(r.abs()),
                v => return Err(sql_error(format!("abs() needs a number, got {}", v.type_name()))),
            },
            Function::Coalesce => args.into_iter().find(|v| !v.is_null()).unwrap_or(Value::Null),
            Function::Length => Value::Integer(text(&args[0]).chars().count() as i64),
            Function::Lower => Value::Text(text(&args[0]).to_lowercase()),
            Function::Upper => Value::Text(text(&args[0]).to_uppercase()),
            Function::Trim => Value::Text(text(&args[0]).trim().to_string()),
            Function::Nullif => {
                if args[0].compare(&args[1])? == Some(Ordering::Equal) {
                    Value::Null
                } else {
                    args.swap_remove(0)
                }
            }
            Function::Replace => Value::Text(text(&args[0]).replace(&text(&args[1]), &text(&args[2]))),
            Function::Round => {
                let digits = match args.get(1) {
                    Some(Value::Integer(d)) => *d as i32,
                    Some(v) => return Err(sql_error(format!("round() digits must be INTEGER, got {}", v.type_name()))),
                    None => 0,
                };
                match &args[0] {
                    Value::Integer(i) => Value::Integer(*i),
                    Value::Real(r) => {
                        let scale = 10f64.powi(digits);
                        Value::Real((r * scale).round() / scale)
                    }
                    v => return Err(sql_error(format!("round() needs a number, got {}", v.type_name()))),
                }
            }
            Function::Substr => {
                let chars: Vec<char> = text(&args[0]).chars().collect();
                let (Some(start), len) = (args[1].as_i64(), args.get(2).map(Value::as_i64)) else {
                    return Err(sql_error("substr() positions must be INTEGER"));
                };
                // Positions count from 1. Characters before position 1 eat into the length.
                let mut len = match len {
                    Some(Some(l)) => l.max(0),
                    Some(None) => return Err(sql_error("substr() length must be INTEGER")),
                    None => i64::MAX,
                };
                let mut start = start;
                if start < 1 {
                    len = len.saturating_add(start - 1).max(0);
                    start = 1;
                }
                let from = (start as usize - 1).min(chars.len());
                let to = from.saturating_add(usize::try_from(len).unwrap_or(usize::MAX)).min(chars.len());
                Value::Text(chars[from..to].iter().collect())
            }
            Function::Typeof => Value::Text(args[0].type_name().to_lowercase()),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

/// An aggregate call. `arg` is `None` for `count(*)`.
#[derive(Clone, Debug)]
pub struct Aggregate {
    pub func: AggFunc,
    pub arg: Option<BExpr>,
    pub distinct: bool,
}

/// The grouping of an aggregate query. After aggregation, a row holds the
/// group key values followed by the aggregate results.
#[derive(Default)]
pub struct Grouping {
    pub keys: Vec<BExpr>,
    /// Each aggregate with the expression it came from, so repeats are shared.
    pub aggregates: Vec<(Expr, Aggregate)>,
}

/// Binds expressions. Parameters stay symbolic (`BExpr::Param`), so a plan
/// can be reused with new values; `params` serves only expressions that are
/// evaluated on the spot, such as INSERT values and DEFAULTs.
pub struct Binder<'a> {
    pub params: &'a [Value],
}

impl Binder<'_> {
    /// Binds `expr` against `scope`. `clause` names where it appears, for errors.
    pub fn bind(&self, expr: &Expr, scope: &Scope, clause: &str) -> Result<BExpr> {
        self.tree(expr, &mut |e| match e {
            Expr::Column { table, name } => Ok(Some(BExpr::Column(scope.resolve(table.as_deref(), name)?))),
            Expr::Function { name, .. } if is_aggregate(name) => {
                Err(sql_error(format!("aggregate function {name}() is not allowed in {clause}")))
            }
            _ => Ok(None),
        })
    }

    /// Evaluates an expression that may not refer to any column.
    pub fn constant(&self, expr: &Expr, clause: &str) -> Result<Value> {
        self.bind(expr, &Scope::default(), clause)?.eval(&[], self.params)
    }

    /// Binds an expression of an aggregate query, to be evaluated against the
    /// aggregated rows. Columns may only appear inside aggregates or as part
    /// of a GROUP BY expression.
    pub fn bind_grouped(&self, expr: &Expr, scope: &Scope, grouping: &mut Grouping) -> Result<BExpr> {
        let group_keys = grouping.keys.len();
        self.tree(expr, &mut |e| {
            if !e.contains_aggregate()
                && let Ok(bound) = self.bind(e, scope, "GROUP BY")
                && let Some(i) = grouping.keys.iter().position(|k| *k == bound)
            {
                return Ok(Some(BExpr::Column(i)));
            }
            match e {
                Expr::Function { name, args, distinct, star } if is_aggregate(name) => {
                    let position = match grouping.aggregates.iter().position(|(seen, _)| seen == e) {
                        Some(j) => j,
                        None => {
                            let aggregate = self.aggregate(name, args, *distinct, *star, scope)?;
                            grouping.aggregates.push((e.clone(), aggregate));
                            grouping.aggregates.len() - 1
                        }
                    };
                    Ok(Some(BExpr::Column(group_keys + position)))
                }
                Expr::Column { table, name } => {
                    scope.resolve(table.as_deref(), name)?; // report unknown columns first
                    Err(sql_error(format!("column {e} must appear in GROUP BY or be used in an aggregate function")))
                }
                _ => Ok(None),
            }
        })
    }

    fn aggregate(&self, name: &str, args: &[Expr], distinct: bool, star: bool, scope: &Scope) -> Result<Aggregate> {
        let func = match name {
            "count" => AggFunc::Count,
            "sum" => AggFunc::Sum,
            "avg" => AggFunc::Avg,
            "min" => AggFunc::Min,
            _ => AggFunc::Max,
        };
        if star {
            return match func {
                AggFunc::Count => Ok(Aggregate { func, arg: None, distinct: false }),
                _ => Err(sql_error(format!("{name}(*) is not allowed; only count(*)"))),
            };
        }
        let [arg] = args else { return Err(sql_error(format!("{name}() takes exactly one argument"))) };
        Ok(Aggregate { func, arg: Some(self.bind(arg, scope, "an aggregate's argument")?), distinct })
    }

    /// Binds every node, giving `leaf` the first chance at each one.
    fn tree(&self, expr: &Expr, leaf: &mut dyn FnMut(&Expr) -> Result<Option<BExpr>>) -> Result<BExpr> {
        if let Some(bound) = leaf(expr)? {
            return Ok(bound);
        }
        let mut sub = |e: &Expr| self.tree(e, leaf).map(Box::new);
        Ok(match expr {
            Expr::Literal(v) => BExpr::Literal(v.clone()),
            Expr::Param(i) => BExpr::Param(*i),
            Expr::Column { name, .. } => return Err(sql_error(format!("no such column: {name}"))),
            Expr::Unary { op: UnaryOp::Neg, expr } => BExpr::Neg(sub(expr)?),
            Expr::Unary { op: UnaryOp::Not, expr } => BExpr::Not(sub(expr)?),
            Expr::Binary { op, left, right } => BExpr::Binary(*op, sub(left)?, sub(right)?),
            Expr::IsNull { expr, negated } => BExpr::IsNull(sub(expr)?, *negated),
            Expr::InList { expr, list, negated } => {
                let e = sub(expr)?;
                let list = list.iter().map(|x| sub(x).map(|b| *b)).collect::<Result<_>>()?;
                BExpr::InList(e, list, *negated)
            }
            Expr::Between { expr, low, high, negated } => BExpr::Between(sub(expr)?, sub(low)?, sub(high)?, *negated),
            Expr::Like { expr, pattern, negated, case_insensitive } => {
                BExpr::Like(sub(expr)?, sub(pattern)?, *negated, *case_insensitive)
            }
            Expr::Function { name, args, distinct, star } => {
                if *star || *distinct {
                    return Err(sql_error(format!(
                        "{name}() does not accept {}",
                        if *star { "*" } else { "DISTINCT" }
                    )));
                }
                let function = Function::resolve(name, args.len())?;
                BExpr::Call(function, args.iter().map(|a| sub(a).map(|b| *b)).collect::<Result<_>>()?)
            }
            Expr::Cast { expr, ty } => BExpr::Cast(sub(expr)?, *ty),
            Expr::Case { operand, branches, otherwise } => {
                let operand = operand.as_deref().map(&mut sub).transpose()?;
                let branches = branches.iter().map(|(w, t)| Ok((*sub(w)?, *sub(t)?))).collect::<Result<_>>()?;
                BExpr::Case(operand, branches, otherwise.as_deref().map(&mut sub).transpose()?)
            }
        })
    }
}

/// Running state for one aggregate in one group.
pub struct Accumulator {
    state: State,
    /// Values already counted, for DISTINCT aggregates.
    seen: Option<HashSet<Vec<u8>>>,
}

enum State {
    Count(i64),
    Sum(Value),
    Avg(f64, i64),
    Min(Value),
    Max(Value),
}

impl Accumulator {
    pub fn new(aggregate: &Aggregate) -> Self {
        let state = match aggregate.func {
            AggFunc::Count => State::Count(0),
            AggFunc::Sum => State::Sum(Value::Null),
            AggFunc::Avg => State::Avg(0.0, 0),
            AggFunc::Min => State::Min(Value::Null),
            AggFunc::Max => State::Max(Value::Null),
        };
        Accumulator { state, seen: aggregate.distinct.then(HashSet::new) }
    }

    pub fn add(&mut self, aggregate: &Aggregate, row: &[Value], params: &[Value]) -> Result<()> {
        match &aggregate.arg {
            None => {
                self.add_row();
                Ok(())
            }
            // Borrowed: most values are only counted or summed, never kept.
            Some(arg) => self.add_value(operand(arg, row, params)?),
        }
    }

    /// Counts a row, for `count(*)`.
    pub fn add_row(&mut self) {
        if let State::Count(n) = &mut self.state {
            *n += 1;
        }
    }

    /// Counts `n` rows, for `count(*)`.
    pub fn add_rows(&mut self, n: usize) {
        if let State::Count(count) = &mut self.state {
            *count += n as i64;
        }
    }

    /// `add_integer` for each value whose `valid` flag is set (all of them
    /// if `None`), with the state looked at once rather than per value.
    pub fn add_integers(&mut self, xs: &[i64], valid: Option<&[bool]>) -> Result<()> {
        let keep = |i: usize| valid.is_none_or(|v| v[i]);
        match &mut self.state {
            _ if self.seen.is_some() => {}
            State::Count(n) => {
                *n += valid.map_or(xs.len(), |v| v.iter().filter(|&&b| b).count()) as i64;
                return Ok(());
            }
            State::Sum(Value::Integer(sum)) => {
                let mut total = *sum;
                for (i, &x) in xs.iter().enumerate() {
                    if keep(i) {
                        total = total.checked_add(x).ok_or_else(|| sql_error("integer overflow"))?;
                    }
                }
                *sum = total;
                return Ok(());
            }
            State::Avg(sum, n) => {
                for (i, &x) in xs.iter().enumerate() {
                    if keep(i) {
                        *sum += x as f64;
                        *n += 1;
                    }
                }
                return Ok(());
            }
            _ => {}
        }
        for (i, &x) in xs.iter().enumerate() {
            if keep(i) {
                self.add_integer(x)?;
            }
        }
        Ok(())
    }

    /// `add_integer` for each value that isn't NULL (`None`), with the state
    /// looked at once.
    pub fn add_integers_from(&mut self, values: impl Iterator<Item = Option<i64>>) -> Result<()> {
        match &mut self.state {
            _ if self.seen.is_some() => {}
            State::Count(n) => {
                *n += values.filter(Option::is_some).count() as i64;
                return Ok(());
            }
            State::Sum(Value::Integer(sum)) => {
                let mut total = *sum;
                for x in values.flatten() {
                    total = total.checked_add(x).ok_or_else(|| sql_error("integer overflow"))?;
                }
                *sum = total;
                return Ok(());
            }
            State::Avg(sum, n) => {
                // Summed exactly as integers, then added once: the same
                // result as adding each value to the real sum while the
                // partial sums stay exact (below 2^53), and much faster.
                let (mut total, mut count) = (0i128, 0i64);
                for x in values.flatten() {
                    total += x as i128;
                    count += 1;
                }
                *sum += total as f64;
                *n += count;
                return Ok(());
            }
            _ => {}
        }
        values.flatten().try_for_each(|x| self.add_integer(x))
    }

    /// `add_real` for each value that isn't NULL; see `add_integers_from`.
    pub fn add_reals_from(&mut self, values: impl Iterator<Item = Option<f64>>) -> Result<()> {
        match &mut self.state {
            _ if self.seen.is_some() => {}
            State::Count(n) => {
                *n += values.filter(Option::is_some).count() as i64;
                return Ok(());
            }
            State::Avg(sum, n) => {
                for x in values.flatten() {
                    *sum += x;
                    *n += 1;
                }
                return Ok(());
            }
            State::Sum(Value::Real(sum)) => {
                for x in values.flatten() {
                    *sum += x;
                }
                return Ok(());
            }
            _ => {}
        }
        values.flatten().try_for_each(|x| self.add_real(x))
    }

    /// `add_real` for each value whose `valid` flag is set; see `add_integers`.
    pub fn add_reals(&mut self, xs: &[f64], valid: Option<&[bool]>) -> Result<()> {
        let keep = |i: usize| valid.is_none_or(|v| v[i]);
        match &mut self.state {
            _ if self.seen.is_some() => {}
            State::Avg(sum, n) => {
                for (i, &x) in xs.iter().enumerate() {
                    if keep(i) {
                        *sum += x;
                        *n += 1;
                    }
                }
                return Ok(());
            }
            State::Sum(Value::Real(sum)) => {
                for (i, &x) in xs.iter().enumerate() {
                    if keep(i) {
                        *sum += x;
                    }
                }
                return Ok(());
            }
            _ => {}
        }
        for (i, &x) in xs.iter().enumerate() {
            if keep(i) {
                self.add_real(x)?;
            }
        }
        Ok(())
    }

    /// Adds a non-NULL integer argument: `add_value` without the boxing.
    pub fn add_integer(&mut self, x: i64) -> Result<()> {
        match &mut self.state {
            _ if self.seen.is_some() => return self.add_value(Cow::Owned(Value::Integer(x))),
            State::Count(n) => *n += 1,
            State::Sum(Value::Integer(sum)) => {
                *sum = sum.checked_add(x).ok_or_else(|| sql_error("integer overflow"))?
            }
            State::Avg(sum, n) => {
                *sum += x as f64;
                *n += 1;
            }
            _ => return self.add_value(Cow::Owned(Value::Integer(x))),
        }
        Ok(())
    }

    /// Adds a non-NULL real argument: `add_value` without the boxing.
    pub fn add_real(&mut self, x: f64) -> Result<()> {
        match &mut self.state {
            _ if self.seen.is_some() => return self.add_value(Cow::Owned(Value::Real(x))),
            State::Count(n) => *n += 1,
            State::Sum(Value::Real(sum)) => *sum += x,
            State::Avg(sum, n) => {
                *sum += x;
                *n += 1;
            }
            _ => return self.add_value(Cow::Owned(Value::Real(x))),
        }
        Ok(())
    }

    /// Adds the aggregate's argument as evaluated for one row.
    pub fn add_value(&mut self, value: Cow<Value>) -> Result<()> {
        if value.is_null() {
            return Ok(()); // aggregates skip NULLs
        }
        if let Some(seen) = &mut self.seen {
            let mut key = Vec::new();
            encode_key(&mut key, &value);
            if !seen.insert(key) {
                return Ok(());
            }
        }
        let numeric = |v: &Value, name: &str| match v {
            Value::Integer(_) | Value::Real(_) => Ok(()),
            v => Err(sql_error(format!("{name}() needs numbers, got {}", v.type_name()))),
        };
        match &mut self.state {
            State::Count(n) => *n += 1,
            State::Sum(sum) => {
                numeric(&value, "sum")?;
                *sum = if sum.is_null() { value.into_owned() } else { arithmetic(BinaryOp::Add, sum, &value)? };
            }
            State::Avg(sum, n) => {
                numeric(&value, "avg")?;
                *sum += value.as_f64().unwrap();
                *n += 1;
            }
            State::Min(min) => {
                if min.is_null() || value.total_cmp(min) == Ordering::Less {
                    *min = value.into_owned();
                }
            }
            State::Max(max) => {
                if max.is_null() || value.total_cmp(max) == Ordering::Greater {
                    *max = value.into_owned();
                }
            }
        }
        Ok(())
    }

    /// Folds in another accumulator for the same aggregate, which saw other
    /// rows. Not for DISTINCT aggregates: their values seen can't be merged.
    pub fn merge(&mut self, other: Accumulator) -> Result<()> {
        debug_assert!(self.seen.is_none() && other.seen.is_none());
        match (&mut self.state, other.state) {
            (State::Count(a), State::Count(b)) => *a += b,
            (State::Sum(a), State::Sum(b)) if !b.is_null() => {
                *a = if a.is_null() { b } else { arithmetic(BinaryOp::Add, a, &b)? };
            }
            (State::Avg(sum, n), State::Avg(other_sum, other_n)) => {
                *sum += other_sum;
                *n += other_n;
            }
            (State::Min(a), State::Min(b)) if !b.is_null() && (a.is_null() || b.total_cmp(a) == Ordering::Less) => {
                *a = b;
            }
            (State::Max(a), State::Max(b)) if !b.is_null() && (a.is_null() || b.total_cmp(a) == Ordering::Greater) => {
                *a = b;
            }
            _ => {}
        }
        Ok(())
    }

    pub fn finish(self) -> Value {
        match self.state {
            State::Count(n) => Value::Integer(n),
            State::Avg(_, 0) => Value::Null,
            State::Avg(sum, n) => Value::Real(sum / n as f64),
            State::Sum(v) | State::Min(v) | State::Max(v) => v,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_patterns() {
        assert!(like("hello", "h%o", false));
        assert!(like("hello", "%", false));
        assert!(like("hello", "_ell_", false));
        assert!(!like("hello", "h_o", false));
        assert!(like("abcabc", "%bc%bc", false));
        assert!(!like("Hello", "hello", false));
        assert!(like("Hello", "hello", true));
        assert!(like("", "%%", false));
        assert!(!like("", "_", false));
    }

    #[test]
    fn three_valued_logic() {
        let t = || Box::new(BExpr::Literal(Value::Boolean(true)));
        let f = || Box::new(BExpr::Literal(Value::Boolean(false)));
        let n = || Box::new(BExpr::Literal(Value::Null));
        let eval = |e: BExpr| e.eval(&[], &[]).unwrap();
        assert_eq!(eval(BExpr::Binary(BinaryOp::And, n(), f())), Value::Boolean(false));
        assert_eq!(eval(BExpr::Binary(BinaryOp::And, n(), t())), Value::Null);
        assert_eq!(eval(BExpr::Binary(BinaryOp::Or, n(), t())), Value::Boolean(true));
        assert_eq!(eval(BExpr::Not(n())), Value::Null);
        assert_eq!(eval(BExpr::InList(Box::new(BExpr::Literal(1.into())), vec![*n()], false)), Value::Null);
    }

    #[test]
    fn arithmetic_rules() {
        assert_eq!(arithmetic(BinaryOp::Div, &7.into(), &2.into()).unwrap(), Value::Integer(3));
        assert_eq!(arithmetic(BinaryOp::Div, &7.into(), &2.0.into()).unwrap(), Value::Real(3.5));
        assert!(arithmetic(BinaryOp::Div, &1.into(), &0.into()).is_err());
        assert!(arithmetic(BinaryOp::Add, &i64::MAX.into(), &1.into()).is_err());
        assert!(arithmetic(BinaryOp::Add, &"a".into(), &1.into()).is_err());
    }

    #[test]
    fn substr_counts_from_one() {
        let call = |args: Vec<Value>| Function::Substr.call(args).unwrap();
        assert_eq!(call(vec!["hello".into(), 2.into(), 3.into()]), Value::Text("ell".into()));
        assert_eq!(call(vec!["hello".into(), 4.into()]), Value::Text("lo".into()));
        assert_eq!(call(vec!["hello".into(), 0.into(), 2.into()]), Value::Text("h".into()));
        assert_eq!(call(vec!["hello".into(), 9.into()]), Value::Text("".into()));
    }
}
