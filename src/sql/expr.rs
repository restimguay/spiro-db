//! Expression binding and evaluation.
//!
//! Binding turns a parsed expression into a [`BExpr`]: column names become
//! positions in the row and parameters become values, so unknown or ambiguous
//! names are reported before a single row is read.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

use super::ast::{BinaryOp, Expr, Select, UnaryOp, is_aggregate};
use super::catalog::TableSchema;
use super::datetime::{self, Kind, Moment};
use super::encoding::{encode_key, encode_keys};
use super::exec;
use super::functions::{self, Scalar};
use super::plan::{CteScope, SelectPlan, WINDOW_BASE, WINDOW_TABLE, plan_select};
use super::txn::Txn;
use super::value::{DataType, Value, sql_error};
use crate::error::Result;

/// A subquery's id and the outer values it ran with.
type Remembered = (usize, Vec<u8>);

/// What expressions are evaluated with besides the row: the statement's
/// parameters, and the transaction that subqueries read from.
pub struct Env<'a> {
    pub params: &'a [Value],
    pub txn: Option<&'a Txn>,
    /// While a recursive query runs its recursive part: the rows of its
    /// last round, which its self-reference reads.
    pub work: Option<(usize, Rc<Vec<Vec<Value>>>)>,
    /// Subquery results already worked out, by subquery and the values of
    /// the outer columns it reads: each runs once per distinct outer row.
    results: RefCell<HashMap<Remembered, Rc<Answer>>>,
    /// The statement's current time, read from the clock on first use, so
    /// every `now()` in a statement agrees.
    now: Cell<Option<Moment>>,
}

impl<'a> Env<'a> {
    pub fn new(params: &'a [Value], txn: Option<&'a Txn>) -> Self {
        Env { params, txn, work: None, results: RefCell::default(), now: Cell::new(None) }
    }

    fn now(&self) -> Moment {
        let now = self.now.get().unwrap_or_else(Moment::now);
        self.now.set(Some(now));
        now
    }
}

/// A subquery in an expression.
#[derive(Clone)]
pub struct Subquery {
    /// Tells this subquery's results apart from others' in an `Env`.
    pub id: usize,
    pub kind: SubqueryKind,
    pub plan: Arc<SelectPlan>,
    /// Expressions of the enclosing query whose values the subquery reads,
    /// as parameters numbered after the statement's own.
    pub outer: Vec<BExpr>,
    /// How many parameters the statement itself has.
    pub params: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SubqueryKind {
    /// `(SELECT x ...)`: one value, or NULL if there are no rows.
    Scalar,
    /// `EXISTS (SELECT ...)`
    Exists,
    /// `expr IN (SELECT x ...)`
    In(Box<BExpr>),
}

impl fmt::Debug for Subquery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Subquery#{}({:?}, outer {:?})", self.id, self.kind, self.outer)
    }
}

impl PartialEq for Subquery {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

/// What a subquery found.
enum Answer {
    Value(Value),
    /// For IN: the values found (by a key under which equal values meet) and
    /// whether any was NULL.
    Set {
        keys: HashSet<Vec<u8>>,
        null: bool,
    },
}

/// A key under which values that compare equal meet: 1 and 1.0 alike.
fn set_key(value: &Value) -> Vec<u8> {
    let mut key = Vec::new();
    match value {
        Value::Real(r) if r.fract() == 0.0 && *r >= i64::MIN as f64 && *r < i64::MAX as f64 => {
            encode_key(&mut key, &Value::Integer(*r as i64))
        }
        v => encode_key(&mut key, v),
    }
    key
}

/// Results kept per subquery for correlated ones; past this, they are not.
const MAX_REMEMBERED: usize = 100_000;

impl Subquery {
    fn answer(&self, row: &[Value], env: &Env) -> Result<Rc<Answer>> {
        let txn = env.txn.ok_or_else(|| sql_error("a subquery can't be used here"))?;
        let outer = self.outer.iter().map(|e| e.eval(row, env)).collect::<Result<Vec<_>>>()?;
        let mut key = Vec::new();
        encode_keys(&mut key, &outer);
        let key = (self.id, key);
        if let Some(answer) = env.results.borrow().get(&key) {
            return Ok(answer.clone());
        }
        let mut params = env.params[..self.params.min(env.params.len())].to_vec();
        params.extend(outer);
        let answer = match self.kind {
            SubqueryKind::Scalar => {
                let (mut first, mut more) = (None, false);
                exec::stream_plan(txn, &self.plan, &params, &mut |row| {
                    if first.is_some() {
                        more = true;
                        return Ok(false);
                    }
                    first = Some(row[0].clone());
                    Ok(true)
                })?;
                if more {
                    return Err(sql_error("a subquery used as a value returned more than one row"));
                }
                Answer::Value(first.unwrap_or(Value::Null))
            }
            SubqueryKind::Exists => {
                let mut any = false;
                exec::stream_plan(txn, &self.plan, &params, &mut |_| {
                    any = true;
                    Ok(false) // one row settles it
                })?;
                Answer::Value(Value::Boolean(any))
            }
            SubqueryKind::In(_) => {
                let (mut keys, mut null) = (HashSet::new(), false);
                exec::stream_plan(txn, &self.plan, &params, &mut |row| {
                    match &row[0] {
                        Value::Null => null = true,
                        v => {
                            keys.insert(set_key(v));
                        }
                    }
                    Ok(true)
                })?;
                Answer::Set { keys, null }
            }
        };
        let answer = Rc::new(answer);
        let mut results = env.results.borrow_mut();
        if self.outer.is_empty() || results.len() < MAX_REMEMBERED {
            results.insert(key, answer.clone());
        }
        Ok(answer)
    }

    fn eval(&self, row: &[Value], env: &Env) -> Result<Value> {
        let answer = self.answer(row, env)?;
        Ok(match (&self.kind, &*answer) {
            (SubqueryKind::In(e), Answer::Set { keys, null }) => {
                // As for IN with a list: no rows is FALSE, even for NULL;
                // otherwise NULL when unsure.
                if keys.is_empty() && !null {
                    return Ok(Value::Boolean(false));
                }
                let v = e.eval(row, env)?;
                if v.is_null() {
                    Value::Null
                } else if keys.contains(&set_key(&v)) {
                    Value::Boolean(true)
                } else if *null {
                    Value::Null
                } else {
                    Value::Boolean(false)
                }
            }
            (_, Answer::Value(v)) => v.clone(),
            (_, Answer::Set { .. }) => unreachable!("only IN collects a set"),
        })
    }
}

/// Numbers subqueries, so their results can be kept apart.
static NEXT_SUBQUERY: AtomicUsize = AtomicUsize::new(0);

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
    Subquery(Box<Subquery>),
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

    /// Renumbers every column the expression reads.
    pub fn map_columns(&mut self, f: &dyn Fn(usize) -> usize) {
        if let BExpr::Column(i) = self {
            *i = f(*i);
        }
        let mut each = |e: &mut BExpr| e.map_columns(f);
        match self {
            BExpr::Literal(_) | BExpr::Column(_) | BExpr::Param(_) => {}
            BExpr::Neg(e) | BExpr::Not(e) | BExpr::IsNull(e, _) | BExpr::Cast(e, _) => each(e),
            BExpr::Binary(_, a, b) | BExpr::Like(a, b, ..) => {
                each(a);
                each(b);
            }
            BExpr::InList(e, list, _) => {
                each(e);
                list.iter_mut().for_each(each);
            }
            BExpr::Between(a, b, c, _) => {
                each(a);
                each(b);
                each(c);
            }
            BExpr::Call(_, args) => args.iter_mut().for_each(each),
            BExpr::Subquery(s) => {
                s.outer.iter_mut().for_each(&mut each);
                if let SubqueryKind::In(e) = &mut s.kind {
                    each(e);
                }
            }
            BExpr::Case(operand, branches, otherwise) => {
                operand.iter_mut().for_each(|e| each(e));
                for (when, then) in branches {
                    each(when);
                    each(then);
                }
                otherwise.iter_mut().for_each(|e| each(e));
            }
        }
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
            BExpr::Subquery(s) => {
                s.outer.iter().for_each(|x| x.visit(f));
                if let SubqueryKind::In(e) = &s.kind {
                    e.visit(f);
                }
            }
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

    /// Whether a subquery appears anywhere in the expression.
    pub fn has_subquery(&self) -> bool {
        let mut found = false;
        self.visit(&mut |e| found |= matches!(e, BExpr::Subquery(_)));
        found
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
    pub fn matches(&self, row: &[Value], env: &Env) -> Result<bool> {
        Ok(self.eval(row, env)?.truth()? == Some(true))
    }

    /// Evaluates the expression against `row`, with `env` supplying the
    /// values of query parameters (and the transaction, for subqueries).
    pub fn eval(&self, row: &[Value], env: &Env) -> Result<Value> {
        Ok(match self {
            BExpr::Literal(v) => v.clone(),
            BExpr::Column(i) => row[*i].clone(),
            BExpr::Param(i) => param(env.params, *i)?.clone(),
            BExpr::Neg(e) => match &*operand(e, row, env)? {
                Value::Null => Value::Null,
                Value::Integer(i) => Value::Integer(i.checked_neg().ok_or_else(|| sql_error("integer overflow"))?),
                Value::Real(r) => Value::Real(-r),
                v => return Err(sql_error(format!("cannot negate {}", v.type_name()))),
            },
            BExpr::Not(e) => operand(e, row, env)?.truth()?.map_or(Value::Null, |b| Value::Boolean(!b)),
            BExpr::Binary(op, a, b) => binary(*op, a, b, row, env)?,
            BExpr::IsNull(e, negated) => Value::Boolean(operand(e, row, env)?.is_null() != *negated),
            BExpr::InList(e, list, negated) => {
                let v = operand(e, row, env)?;
                if v.is_null() {
                    return Ok(Value::Null);
                }
                let mut saw_null = false;
                for item in list {
                    match v.compare(&*operand(item, row, env)?)? {
                        Some(Ordering::Equal) => return Ok(Value::Boolean(!negated)),
                        None => saw_null = true,
                        _ => {}
                    }
                }
                if saw_null { Value::Null } else { Value::Boolean(*negated) }
            }
            BExpr::Between(e, low, high, negated) => {
                let v = operand(e, row, env)?;
                let above = v.compare(&*operand(low, row, env)?)?.map(|o| o != Ordering::Less);
                let below = v.compare(&*operand(high, row, env)?)?.map(|o| o != Ordering::Greater);
                match and3(above, below) {
                    None => Value::Null,
                    Some(b) => Value::Boolean(b != *negated),
                }
            }
            BExpr::Like(e, pattern, negated, case_insensitive) => {
                match (&*operand(e, row, env)?, &*operand(pattern, row, env)?) {
                    (Value::Null, _) | (_, Value::Null) => Value::Null,
                    (Value::Text(s), Value::Text(p)) => Value::Boolean(like(s, p, *case_insensitive) != *negated),
                    (a, b) => {
                        return Err(sql_error(format!("LIKE needs TEXT, got {} and {}", a.type_name(), b.type_name())));
                    }
                }
            }
            BExpr::Call(f, args) => f.call(args.iter().map(|a| a.eval(row, env)).collect::<Result<_>>()?, env)?,
            BExpr::Cast(e, ty) => operand(e, row, env)?.cast(*ty)?,
            BExpr::Case(case, branches, otherwise) => {
                let case = case.as_ref().map(|o| operand(o, row, env)).transpose()?;
                for (when, then) in branches {
                    let hit = match &case {
                        Some(v) => v.compare(&*operand(when, row, env)?)? == Some(Ordering::Equal),
                        None => when.matches(row, env)?,
                    };
                    if hit {
                        return then.eval(row, env);
                    }
                }
                otherwise.as_ref().map_or(Ok(Value::Null), |e| e.eval(row, env))?
            }
            BExpr::Subquery(s) => s.eval(row, env)?,
        })
    }
}

fn param(params: &[Value], i: usize) -> Result<&Value> {
    params.get(i).ok_or_else(|| sql_error(format!("no value for parameter ${}", i + 1)))
}

/// Evaluates an operand, borrowing instead of copying when it is a column,
/// literal or parameter. Filters compare values far more often than they
/// produce them, so this saves a copy (and, for text, an allocation) per test.
pub(super) fn operand<'a>(e: &'a BExpr, row: &'a [Value], env: &'a Env) -> Result<Cow<'a, Value>> {
    Ok(match e {
        BExpr::Column(i) => Cow::Borrowed(&row[*i]),
        BExpr::Literal(v) => Cow::Borrowed(v),
        BExpr::Param(i) => Cow::Borrowed(param(env.params, *i)?),
        e => Cow::Owned(e.eval(row, env)?),
    })
}

fn and3(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}

fn binary(op: BinaryOp, a: &BExpr, b: &BExpr, row: &[Value], env: &Env) -> Result<Value> {
    let to_value = |t: Option<bool>| t.map_or(Value::Null, Value::Boolean);
    match op {
        BinaryOp::And => {
            let left = operand(a, row, env)?.truth()?;
            if left == Some(false) {
                return Ok(Value::Boolean(false));
            }
            Ok(to_value(and3(left, operand(b, row, env)?.truth()?)))
        }
        BinaryOp::Or => {
            let left = operand(a, row, env)?.truth()?;
            if left == Some(true) {
                return Ok(Value::Boolean(true));
            }
            let right = operand(b, row, env)?.truth()?;
            Ok(to_value(match (left, right) {
                (_, Some(true)) => Some(true),
                (Some(false), Some(false)) => Some(false),
                _ => None,
            }))
        }
        op if op.is_comparison() => {
            let ordering = operand(a, row, env)?.compare(&*operand(b, row, env)?)?;
            Ok(to_value(ordering.map(|o| match op {
                BinaryOp::Eq => o == Ordering::Equal,
                BinaryOp::NotEq => o != Ordering::Equal,
                BinaryOp::Lt => o == Ordering::Less,
                BinaryOp::LtEq => o != Ordering::Greater,
                BinaryOp::Gt => o == Ordering::Greater,
                _ => o != Ordering::Less,
            })))
        }
        BinaryOp::Concat => match (&*operand(a, row, env)?, &*operand(b, row, env)?) {
            (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
            (x, y) => Ok(Value::Text(format!("{x}{y}"))),
        },
        op => arithmetic(op, &*operand(a, row, env)?, &*operand(b, row, env)?),
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
    Now,
    CurrentDate,
    CurrentTime,
    Date,
    Time,
    Datetime,
    Strftime,
    Julianday,
    Unixepoch,
    DatePart,
    DateTrunc,
    DateAdd,
    DateSub,
    Datediff,
    Interval,
    Scalar(Scalar),
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
            "trim" if args == 1 => (Function::Trim, true),
            "typeof" => (Function::Typeof, args == 1),
            "upper" => (Function::Upper, args == 1),
            "now" | "current_timestamp" | "localtimestamp" => (Function::Now, args == 0),
            "current_date" => (Function::CurrentDate, args == 0),
            "current_time" | "localtime" => (Function::CurrentTime, args == 0),
            "date" => (Function::Date, args >= 1),
            "time" => (Function::Time, args >= 1),
            "datetime" => (Function::Datetime, args >= 1),
            "strftime" => (Function::Strftime, args >= 2),
            "julianday" => (Function::Julianday, args >= 1),
            "unixepoch" => (Function::Unixepoch, args >= 1),
            "date_part" => (Function::DatePart, args == 2),
            "date_trunc" => (Function::DateTrunc, args == 2),
            "date_add" | "adddate" => (Function::DateAdd, args == 2),
            "date_sub" | "subdate" => (Function::DateSub, args == 2),
            "datediff" => (Function::Datediff, args == 2),
            "interval" => (Function::Interval, args == 1),
            _ => match functions::resolve(name, args) {
                Some((scalar, ok)) => (Function::Scalar(scalar), ok),
                None => return Err(sql_error(format!("no such function: {name}"))),
            },
        };
        if !arity_ok {
            return Err(sql_error(format!("wrong number of arguments to {name}()")));
        }
        Ok(function)
    }

    fn call(self, mut args: Vec<Value>, env: &Env) -> Result<Value> {
        let text = |v: &Value| v.to_string();
        let takes_nulls = match self {
            Function::Coalesce | Function::Nullif | Function::Typeof => true,
            Function::Scalar(scalar) => scalar.takes_nulls(),
            _ => false,
        };
        if !takes_nulls && args.iter().any(Value::is_null) {
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
            Function::Now => Value::Text(env.now().format(Kind::Timestamp)?),
            Function::CurrentDate => Value::Text(env.now().format(Kind::Date)?),
            Function::CurrentTime => Value::Text(env.now().format(Kind::Time)?),
            Function::Date => datetime::convert(&self.with_now(args, env), Kind::Date, "date")?,
            Function::Time => datetime::convert(&self.with_now(args, env), Kind::Time, "time")?,
            Function::Datetime => datetime::convert(&self.with_now(args, env), Kind::Timestamp, "datetime")?,
            Function::Strftime => datetime::strftime(&self.with_now(args, env))?,
            Function::Julianday => datetime::julianday(&self.with_now(args, env))?,
            Function::Unixepoch => datetime::unixepoch(&self.with_now(args, env))?,
            Function::DatePart => datetime::date_part(&args[0], &args[1])?,
            Function::DateTrunc => datetime::date_trunc(&args[0], &args[1])?,
            Function::DateAdd => datetime::date_add(&args[0], &args[1], 1)?,
            Function::DateSub => datetime::date_add(&args[0], &args[1], -1)?,
            Function::Datediff => datetime::datediff(&args[0], &args[1])?,
            Function::Interval => datetime::interval(&args[0])?,
            Function::Scalar(scalar) => scalar.call(args)?,
        })
    }

    /// The arguments with a time of `'now'` replaced by the statement's
    /// time, so it agrees with `now()`.
    fn with_now(self, mut args: Vec<Value>, env: &Env) -> Vec<Value> {
        let at = usize::from(self == Function::Strftime);
        if let Some(Value::Text(t)) = args.get(at)
            && t.trim().eq_ignore_ascii_case("now")
        {
            args[at] = Value::Text(env.now().format(Kind::Timestamp).unwrap_or_default());
        }
        args
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
    /// SQLite's `total`: a REAL sum, 0.0 over no rows.
    Total,
    /// `group_concat` and `string_agg`.
    Concat,
    BoolAnd,
    BoolOr,
    VarSamp,
    VarPop,
    StddevSamp,
    StddevPop,
}

impl AggFunc {
    pub fn of(name: &str) -> Option<AggFunc> {
        Some(match name {
            "count" => AggFunc::Count,
            "sum" => AggFunc::Sum,
            "avg" => AggFunc::Avg,
            "min" => AggFunc::Min,
            "max" => AggFunc::Max,
            "total" => AggFunc::Total,
            "group_concat" | "string_agg" => AggFunc::Concat,
            "bool_and" | "every" => AggFunc::BoolAnd,
            "bool_or" => AggFunc::BoolOr,
            "variance" | "var_samp" => AggFunc::VarSamp,
            "var_pop" => AggFunc::VarPop,
            "stddev" | "stddev_samp" => AggFunc::StddevSamp,
            "stddev_pop" => AggFunc::StddevPop,
            _ => return None,
        })
    }
}

/// An aggregate call. `arg` is `None` for `count(*)`.
#[derive(Clone, Debug)]
pub struct Aggregate {
    pub func: AggFunc,
    pub arg: Option<BExpr>,
    pub distinct: bool,
    /// `FILTER (WHERE ...)`: only rows it accepts are aggregated.
    pub filter: Option<BExpr>,
    /// What `group_concat` puts between values.
    pub separator: String,
}

impl Aggregate {
    /// Builds an aggregate call of `name`, binding its argument and filter
    /// with `bind`.
    pub fn build(
        name: &str,
        args: &[Expr],
        distinct: bool,
        star: bool,
        filter: Option<&Expr>,
        bind: &mut dyn FnMut(&Expr) -> Result<BExpr>,
    ) -> Result<Aggregate> {
        let func = AggFunc::of(name).ok_or_else(|| sql_error(format!("{name}() is not an aggregate")))?;
        let filter = filter.map(&mut *bind).transpose()?;
        let mut aggregate = Aggregate { func, arg: None, distinct, filter, separator: ",".into() };
        if star {
            return match func {
                AggFunc::Count if !distinct => Ok(aggregate),
                _ => Err(sql_error(format!("{name}(*) is not allowed; only count(*)"))),
            };
        }
        match (func, args) {
            (_, [arg]) => aggregate.arg = Some(bind(arg)?),
            (AggFunc::Concat, [arg, separator]) => {
                let Expr::Literal(Value::Text(separator)) = separator else {
                    return Err(sql_error(format!("{name}()'s separator must be a text literal")));
                };
                aggregate.separator = separator.clone();
                aggregate.arg = Some(bind(arg)?);
            }
            _ => return Err(sql_error(format!("wrong number of arguments to {name}()"))),
        }
        Ok(aggregate)
    }
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
#[derive(Clone, Copy)]
pub struct Binder<'a> {
    pub params: &'a [Value],
    /// The transaction, to plan subqueries against.
    pub txn: Option<&'a Txn>,
    /// Inside a subquery: the query around it, whose columns it may read.
    pub outer: Option<&'a Outer<'a>>,
    /// The common table expressions (`WITH`) in reach.
    pub ctes: Option<&'a CteScope<'a>>,
}

/// The query around a subquery being bound.
pub struct Outer<'a> {
    pub scope: &'a Scope,
    /// The outer query's columns the subquery reads, in the order of the
    /// parameters they become (after the statement's own).
    refs: RefCell<Vec<usize>>,
}

impl<'a> Binder<'a> {
    pub fn new(params: &'a [Value], txn: &'a Txn) -> Self {
        Binder { params, txn: Some(txn), outer: None, ctes: None }
    }

    /// Resolves a column: in `scope`, or failing that in the query around a
    /// subquery, where it becomes a parameter of the subquery.
    fn column(&self, scope: &Scope, table: Option<&str>, name: &str) -> Result<BExpr> {
        match scope.resolve(table, name) {
            Ok(i) => Ok(BExpr::Column(i)),
            Err(e) => {
                let Some(outer) = self.outer else { return Err(e) };
                let Ok(i) = outer.scope.resolve(table, name) else { return Err(e) };
                let mut refs = outer.refs.borrow_mut();
                let k = refs.iter().position(|&r| r == i).unwrap_or_else(|| {
                    refs.push(i);
                    refs.len() - 1
                });
                Ok(BExpr::Param(self.params.len() + k))
            }
        }
    }

    /// Binds `expr` against `scope`. `clause` names where it appears, for errors.
    pub fn bind(&self, expr: &Expr, scope: &Scope, clause: &str) -> Result<BExpr> {
        self.tree(expr, Some(scope), &mut |e| match e {
            Expr::Column { table, name } => Ok(Some(self.column(scope, table.as_deref(), name)?)),
            Expr::Function { name, .. } if is_aggregate(name) => {
                Err(sql_error(format!("aggregate function {name}() is not allowed in {clause}")))
            }
            _ => Ok(None),
        })
    }

    /// Evaluates an expression that may not refer to any column.
    pub fn constant(&self, expr: &Expr, clause: &str) -> Result<Value> {
        self.bind(expr, &Scope::default(), clause)?.eval(&[], &Env::new(self.params, self.txn))
    }

    /// Plans a subquery of a query whose rows look like `scope` (`None`: the
    /// subquery may not read them).
    fn subquery(&self, select: &Select, scope: Option<&Scope>, kind: SubqueryKind, one_column: bool) -> Result<BExpr> {
        let txn = self.txn.ok_or_else(|| sql_error("a subquery can't be used here"))?;
        let empty = Scope::default();
        let outer = Outer { scope: scope.unwrap_or(&empty), refs: RefCell::default() };
        let binder = Binder { txn: Some(txn), outer: Some(&outer), ..*self };
        let plan = plan_select(txn, select, &binder)?;
        if one_column && plan.columns.len() != 1 {
            return Err(sql_error(format!("a subquery here must return one column, not {}", plan.columns.len())));
        }
        let outer = outer.refs.into_inner().into_iter().map(BExpr::Column).collect();
        Ok(BExpr::Subquery(Box::new(Subquery {
            id: NEXT_SUBQUERY.fetch_add(1, AtomicOrdering::Relaxed),
            kind,
            plan: Arc::new(plan),
            outer,
            params: self.params.len(),
        })))
    }

    /// Binds an expression of an aggregate query, to be evaluated against the
    /// aggregated rows. Columns may only appear inside aggregates or as part
    /// of a GROUP BY expression.
    pub fn bind_grouped(&self, expr: &Expr, scope: &Scope, grouping: &mut Grouping) -> Result<BExpr> {
        let group_keys = grouping.keys.len();
        // Subqueries here see the aggregated rows, not the table's: they may
        // not read the outer query's columns.
        self.tree(expr, None, &mut |e| {
            if !e.contains_aggregate()
                && let Ok(bound) = self.bind(e, scope, "GROUP BY")
                && let Some(i) = grouping.keys.iter().position(|k| *k == bound)
            {
                return Ok(Some(BExpr::Column(i)));
            }
            match e {
                Expr::Function { name, args, distinct, star, filter } if is_aggregate(name) => {
                    let position = match grouping.aggregates.iter().position(|(seen, _)| seen == e) {
                        Some(j) => j,
                        None => {
                            let aggregate =
                                Aggregate::build(name, args, *distinct, *star, filter.as_deref(), &mut |e| {
                                    self.bind(e, scope, "an aggregate's argument")
                                })?;
                            grouping.aggregates.push((e.clone(), aggregate));
                            grouping.aggregates.len() - 1
                        }
                    };
                    Ok(Some(BExpr::Column(group_keys + position)))
                }
                // A window function's value, placed after the group's row later.
                Expr::Column { table: Some(table), name } if table == WINDOW_TABLE => {
                    Ok(Some(BExpr::Column(WINDOW_BASE + name.parse::<usize>().unwrap_or_default())))
                }
                Expr::Column { table, name } => {
                    scope.resolve(table.as_deref(), name)?; // report unknown columns first
                    Err(sql_error(format!("column {e} must appear in GROUP BY or be used in an aggregate function")))
                }
                _ => Ok(None),
            }
        })
    }

    /// Binds every node, giving `leaf` the first chance at each one.
    fn tree(
        &self,
        expr: &Expr,
        scope: Option<&Scope>,
        leaf: &mut dyn FnMut(&Expr) -> Result<Option<BExpr>>,
    ) -> Result<BExpr> {
        if let Some(bound) = leaf(expr)? {
            return Ok(bound);
        }
        match expr {
            Expr::Subquery(select) => return self.subquery(select, scope, SubqueryKind::Scalar, true),
            Expr::Exists { select, negated } => {
                let exists = self.subquery(select, scope, SubqueryKind::Exists, false)?;
                return Ok(if *negated { BExpr::Not(Box::new(exists)) } else { exists });
            }
            Expr::InSubquery { expr: left, select, negated } => {
                let left = self.tree(left, scope, leaf)?;
                let found = self.subquery(select, scope, SubqueryKind::In(Box::new(left)), true)?;
                return Ok(if *negated { BExpr::Not(Box::new(found)) } else { found });
            }
            _ => {}
        }
        let mut sub = |e: &Expr| self.tree(e, scope, leaf).map(Box::new);
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
            Expr::Function { name, args, distinct, star, filter } => {
                if filter.is_some() {
                    return Err(sql_error(format!("FILTER is only for aggregates, not {name}()")));
                }
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
            Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => unreachable!("bound above"),
            Expr::Window { name, .. } => {
                return Err(sql_error(format!(
                    "window function {name}() is allowed only in the select list and ORDER BY, and not inside another"
                )));
            }
        })
    }
}

/// A value given to `bool_and` or `bool_or`.
fn boolean(value: &Value, name: &str) -> Result<bool> {
    match value {
        Value::Boolean(b) => Ok(*b),
        v => Err(sql_error(format!("{name}() needs booleans, got {}", v.type_name()))),
    }
}

/// Running state for one aggregate in one group.
#[derive(Clone)]
pub struct Accumulator {
    state: State,
    /// Values already counted, for DISTINCT aggregates.
    seen: Option<HashSet<Vec<u8>>>,
}

#[derive(Clone)]
enum State {
    Count(i64),
    Sum(Value),
    Avg(f64, i64),
    Min(Value),
    Max(Value),
    Total(f64),
    /// The separator, and the text so far.
    Concat(String, Option<String>),
    BoolAnd(Option<bool>),
    BoolOr(Option<bool>),
    /// Welford's running count, mean and sum of squared deviations, which
    /// stay accurate where summing squares would cancel out.
    Variance {
        func: AggFunc,
        n: i64,
        mean: f64,
        m2: f64,
    },
}

impl Accumulator {
    pub fn new(aggregate: &Aggregate) -> Self {
        let state = match aggregate.func {
            AggFunc::Count => State::Count(0),
            AggFunc::Sum => State::Sum(Value::Null),
            AggFunc::Avg => State::Avg(0.0, 0),
            AggFunc::Min => State::Min(Value::Null),
            AggFunc::Max => State::Max(Value::Null),
            AggFunc::Total => State::Total(0.0),
            AggFunc::Concat => State::Concat(aggregate.separator.clone(), None),
            AggFunc::BoolAnd => State::BoolAnd(None),
            AggFunc::BoolOr => State::BoolOr(None),
            func => State::Variance { func, n: 0, mean: 0.0, m2: 0.0 },
        };
        Accumulator { state, seen: aggregate.distinct.then(HashSet::new) }
    }

    pub fn add(&mut self, aggregate: &Aggregate, row: &[Value], env: &Env) -> Result<()> {
        if let Some(filter) = &aggregate.filter
            && !filter.matches(row, env)?
        {
            return Ok(());
        }
        match &aggregate.arg {
            None => {
                self.add_row();
                Ok(())
            }
            // Borrowed: most values are only counted or summed, never kept.
            Some(arg) => self.add_value(operand(arg, row, env)?),
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
            State::Total(sum) => {
                numeric(&value, "total")?;
                *sum += value.as_f64().unwrap();
            }
            State::Concat(separator, text) => match text {
                Some(text) => {
                    text.push_str(separator);
                    text.push_str(&value.to_string());
                }
                None => *text = Some(value.to_string()),
            },
            State::BoolAnd(all) => {
                let b = boolean(&value, "bool_and")?;
                *all = Some(all.unwrap_or(true) && b);
            }
            State::BoolOr(any) => {
                let b = boolean(&value, "bool_or")?;
                *any = Some(any.unwrap_or(false) || b);
            }
            State::Variance { n, mean, m2, .. } => {
                numeric(&value, "variance")?;
                let x = value.as_f64().unwrap();
                *n += 1;
                let delta = x - *mean;
                *mean += delta / *n as f64;
                *m2 += delta * (x - *mean);
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
            (State::Total(a), State::Total(b)) => *a += b,
            (State::Concat(separator, a), State::Concat(_, Some(b))) => match a {
                Some(a) => {
                    a.push_str(separator);
                    a.push_str(&b);
                }
                None => *a = Some(b),
            },
            (State::BoolAnd(a), State::BoolAnd(Some(b))) => *a = Some(a.unwrap_or(true) && b),
            (State::BoolOr(a), State::BoolOr(Some(b))) => *a = Some(a.unwrap_or(false) || b),
            // Chan et al.'s formula for combining two runs of Welford's.
            (State::Variance { n, mean, m2, .. }, State::Variance { n: n2, mean: mean2, m2: m22, .. }) if n2 > 0 => {
                let total = *n + n2;
                let delta = mean2 - *mean;
                *m2 += m22 + delta * delta * (*n as f64) * (n2 as f64) / total as f64;
                *mean += delta * n2 as f64 / total as f64;
                *n = total;
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
            State::Total(sum) => Value::Real(sum),
            State::Concat(_, text) => text.map_or(Value::Null, Value::Text),
            State::BoolAnd(b) | State::BoolOr(b) => b.map_or(Value::Null, Value::Boolean),
            State::Variance { func, n, m2, .. } => {
                let sample = matches!(func, AggFunc::VarSamp | AggFunc::StddevSamp);
                let divisor = if sample { n - 1 } else { n };
                if divisor <= 0 {
                    return Value::Null;
                }
                let variance = m2 / divisor as f64;
                match func {
                    AggFunc::StddevSamp | AggFunc::StddevPop => Value::Real(variance.sqrt()),
                    _ => Value::Real(variance),
                }
            }
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
        let eval = |e: BExpr| e.eval(&[], &Env::new(&[], None)).unwrap();
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
        let call = |args: Vec<Value>| Function::Substr.call(args, &Env::new(&[], None)).unwrap();
        assert_eq!(call(vec!["hello".into(), 2.into(), 3.into()]), Value::Text("ell".into()));
        assert_eq!(call(vec!["hello".into(), 4.into()]), Value::Text("lo".into()));
        assert_eq!(call(vec!["hello".into(), 0.into(), 2.into()]), Value::Text("h".into()));
        assert_eq!(call(vec!["hello".into(), 9.into()]), Value::Text("".into()));
    }
}
