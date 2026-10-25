//! The syntax tree produced by the parser.

use std::cmp::Ordering;
use std::fmt;

use super::value::{DataType, Value};

#[derive(Clone, Debug, PartialEq)]
pub enum Statement {
    CreateTable(CreateTable),
    DropTable {
        name: String,
        if_exists: bool,
    },
    CreateIndex(CreateIndex),
    AlterTable {
        table: String,
        action: AlterAction,
    },
    DropIndex {
        name: String,
        if_exists: bool,
    },
    Insert(Insert),
    Select(Box<Select>),
    Update(Update),
    Delete(Delete),
    /// `CREATE [OR REPLACE] VIEW name [(columns)] AS query`
    CreateView {
        name: String,
        columns: Vec<String>,
        query: Box<Select>,
        /// The query as written, which is what is stored.
        sql: String,
        or_replace: bool,
        if_not_exists: bool,
    },
    DropView {
        name: String,
        if_exists: bool,
    },
    /// `BEGIN [READ ONLY | READ WRITE]`
    Begin {
        read_only: bool,
    },
    Commit,
    Rollback,
    Savepoint(String),
    /// `RELEASE [SAVEPOINT] name`
    Release(String),
    /// `ROLLBACK TO [SAVEPOINT] name`
    RollbackTo(String),
    Explain(Box<Statement>),
    /// `WITH name AS (statement), ... statement`, where some statement is an
    /// INSERT, UPDATE or DELETE (a WITH of queries alone is a `Select`).
    With {
        ctes: Vec<StatementCte>,
        body: Box<Statement>,
    },
    /// `TRUNCATE [TABLE] name, ... [RESTART IDENTITY]`
    Truncate {
        tables: Vec<String>,
        restart: bool,
    },
    /// `LISTEN channel`
    Listen(String),
    /// `UNLISTEN channel`, or `UNLISTEN *` (`None`)
    Unlisten(Option<String>),
    /// `NOTIFY channel [, payload]`
    Notify {
        channel: String,
        payload: Option<Expr>,
    },
}

impl Statement {
    /// Whether the statement only reads, so it needs no write lock.
    pub fn is_read_only(&self) -> bool {
        matches!(self, Statement::Select(_) | Statement::Explain(_))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CreateTable {
    pub name: String,
    pub if_not_exists: bool,
    pub columns: Vec<ColumnDef>,
    /// A table-level `PRIMARY KEY (a, b)`.
    pub primary_key: Vec<String>,
    /// Table-level `UNIQUE (a, b)` constraints.
    pub unique: Vec<Vec<String>>,
    /// `CHECK (expr)` constraints, the column-level ones included.
    pub checks: Vec<CheckDef>,
    /// `FOREIGN KEY (a) REFERENCES t (b)`, the column-level ones included.
    pub foreign_keys: Vec<ForeignKeyDef>,
    /// `CREATE TABLE name [(columns)] AS query`: the query whose rows fill
    /// it. `columns` then only name the query's columns.
    pub query: Option<Box<Select>>,
}

/// `[CONSTRAINT name] CHECK (expr)`: rows for which `expr` is false are refused.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckDef {
    pub name: Option<String>,
    pub expr: Expr,
}

/// `[CONSTRAINT name] FOREIGN KEY (columns) REFERENCES table [(columns)] [ON DELETE ...] [ON UPDATE ...]`
#[derive(Clone, Debug, PartialEq)]
pub struct ForeignKeyDef {
    pub name: Option<String>,
    pub columns: Vec<String>,
    pub table: String,
    /// Empty: the referenced table's primary key.
    pub references: Vec<String>,
    pub on_delete: FkAction,
    pub on_update: FkAction,
}

/// What happens to referencing rows when the row they reference is deleted
/// or its key changes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FkAction {
    /// Refuse, unless the statement leaves the referencing rows valid.
    #[default]
    NoAction,
    Restrict,
    Cascade,
    SetNull,
    SetDefault,
}

impl fmt::Display for FkAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            FkAction::NoAction => "NO ACTION",
            FkAction::Restrict => "RESTRICT",
            FkAction::Cascade => "CASCADE",
            FkAction::SetNull => "SET NULL",
            FkAction::SetDefault => "SET DEFAULT",
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub ty: DataType,
    pub primary_key: bool,
    pub not_null: bool,
    pub unique: bool,
    pub default: Option<Expr>,
    pub checks: Vec<CheckDef>,
    /// `REFERENCES table [(column)] ...`, with this column as the only one.
    pub references: Option<ForeignKeyDef>,
}

/// What `ALTER TABLE` changes.
#[derive(Clone, Debug, PartialEq)]
pub enum AlterAction {
    AddColumn(Box<ColumnDef>),
    DropColumn(String),
    RenameTable(String),
    RenameColumn {
        from: String,
        to: String,
    },
    AddConstraint(TableConstraint),
    DropConstraint {
        name: String,
        if_exists: bool,
    },
    /// `ALTER [COLUMN] c TYPE ty` (or `SET DATA TYPE ty`).
    SetType {
        column: String,
        ty: DataType,
    },
    /// `ALTER [COLUMN] c SET DEFAULT expr`, or `DROP DEFAULT` (`None`).
    SetDefault {
        column: String,
        default: Option<Expr>,
    },
    /// `ALTER [COLUMN] c SET NOT NULL` (true) or `DROP NOT NULL`.
    SetNotNull {
        column: String,
        not_null: bool,
    },
}

/// A constraint added with `ALTER TABLE ... ADD`.
#[derive(Clone, Debug, PartialEq)]
pub enum TableConstraint {
    Check(CheckDef),
    ForeignKey(ForeignKeyDef),
    Unique { name: Option<String>, columns: Vec<String> },
}

#[derive(Clone, Debug, PartialEq)]
pub struct CreateIndex {
    pub name: String,
    pub table: String,
    /// What it indexes: columns, or expressions such as `lower(email)`.
    pub parts: Vec<Expr>,
    /// `WHERE ...`: a partial index, of only the rows this accepts.
    pub predicate: Option<Expr>,
    pub unique: bool,
    pub if_not_exists: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Insert {
    pub table: String,
    pub columns: Option<Vec<String>>,
    pub source: InsertSource,
    pub on_conflict: Option<OnConflict>,
    pub returning: Vec<SelectItem>,
}

/// `ON CONFLICT [(columns)] DO ...`: what to do with a row whose key or
/// unique values are taken. No columns: any unique constraint.
#[derive(Clone, Debug, PartialEq)]
pub struct OnConflict {
    pub target: Vec<String>,
    pub action: ConflictAction,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ConflictAction {
    Nothing,
    /// `DO UPDATE SET ... [WHERE ...]`, where `excluded.column` is the
    /// value the insert proposed.
    Update {
        assignments: Vec<(String, Expr)>,
        filter: Option<Expr>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum InsertSource {
    Values(Vec<Vec<Expr>>),
    Select(Box<Select>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Update {
    pub table: String,
    pub assignments: Vec<(String, Expr)>,
    /// `FROM ...`: other tables the assignments and WHERE may read.
    pub from: Option<From>,
    pub filter: Option<Expr>,
    pub returning: Vec<SelectItem>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Delete {
    pub table: String,
    /// `USING ...`: other tables WHERE may read.
    pub using: Option<From>,
    pub filter: Option<Expr>,
    pub returning: Vec<SelectItem>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Select {
    pub distinct: bool,
    /// `DISTINCT ON (exprs)`: the first row of each set of these values.
    pub distinct_on: Vec<Expr>,
    pub items: Vec<SelectItem>,
    pub from: Option<From>,
    pub filter: Option<Expr>,
    pub group_by: Vec<Expr>,
    /// With ROLLUP, CUBE or GROUPING SETS: each set of `group_by`'s
    /// expressions to group by in turn.
    pub grouping_sets: Option<Vec<Vec<Expr>>>,
    pub having: Option<Expr>,
    pub order_by: Vec<OrderBy>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
    /// `UNION`, `INTERSECT` and `EXCEPT` with further queries, applied left
    /// to right. ORDER BY, LIMIT and OFFSET above then apply to the result.
    pub compound: Vec<(SetOp, bool, Select)>,
    /// `WITH [RECURSIVE] name AS (query), ...` before the query.
    pub with: Option<Box<With>>,
    /// `WINDOW name AS (...), ...`: windows that `OVER name` refers to.
    pub windows: Vec<(String, WindowSpec)>,
}

/// What `OVER (...)` describes: how rows are grouped and ordered for a
/// window function, and which of them it sees.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WindowSpec {
    /// A window named in the WINDOW clause that this one builds on.
    pub base: Option<String>,
    pub partition_by: Vec<Expr>,
    pub order_by: Vec<OrderBy>,
    pub frame: Option<Frame>,
}

/// `ROWS | RANGE BETWEEN start AND end`
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Frame {
    /// ROWS counts rows; RANGE also takes in the current row's peers.
    pub rows: bool,
    pub start: FrameBound,
    pub end: FrameBound,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FrameBound {
    UnboundedPreceding,
    Preceding(u64),
    CurrentRow,
    Following(u64),
    UnboundedFollowing,
}

impl fmt::Display for WindowSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        parts.extend(self.base.clone());
        if !self.partition_by.is_empty() {
            let exprs: Vec<String> = self.partition_by.iter().map(ToString::to_string).collect();
            parts.push(format!("PARTITION BY {}", exprs.join(", ")));
        }
        if !self.order_by.is_empty() {
            let exprs: Vec<String> = self.order_by.iter().map(ToString::to_string).collect();
            parts.push(format!("ORDER BY {}", exprs.join(", ")));
        }
        if let Some(frame) = &self.frame {
            let bound = |b: FrameBound| match b {
                FrameBound::UnboundedPreceding => "UNBOUNDED PRECEDING".to_string(),
                FrameBound::Preceding(n) => format!("{n} PRECEDING"),
                FrameBound::CurrentRow => "CURRENT ROW".to_string(),
                FrameBound::Following(n) => format!("{n} FOLLOWING"),
                FrameBound::UnboundedFollowing => "UNBOUNDED FOLLOWING".to_string(),
            };
            let unit = if frame.rows { "ROWS" } else { "RANGE" };
            parts.push(format!("{unit} BETWEEN {} AND {}", bound(frame.start), bound(frame.end)));
        }
        write!(f, "({})", parts.join(" "))
    }
}

/// A common table expression that may write: `name [(columns)] AS (INSERT
/// ... RETURNING ...)`, or a query.
#[derive(Clone, Debug, PartialEq)]
pub struct StatementCte {
    pub name: String,
    pub columns: Vec<String>,
    pub statement: Statement,
}

/// The common table expressions of a `WITH` clause.
#[derive(Clone, Debug, PartialEq)]
pub struct With {
    pub recursive: bool,
    pub ctes: Vec<Cte>,
}

/// `name [(columns)] AS (query)`
#[derive(Clone, Debug, PartialEq)]
pub struct Cte {
    pub name: String,
    pub columns: Vec<String>,
    pub query: Select,
}

impl Select {
    /// Calls `f` with every table this query names, in any FROM, join,
    /// compound part or subquery within it.
    pub fn visit_tables(&self, f: &mut dyn FnMut(&TableRef)) {
        if let Some(with) = &self.with {
            with.ctes.iter().for_each(|cte| cte.query.visit_tables(f));
        }
        let mut exprs: Vec<&Expr> = Vec::new();
        if let Some(from) = &self.from {
            for table in std::iter::once(&from.base).chain(from.joins.iter().map(|j| &j.table)) {
                f(table);
                if let Some(query) = &table.subquery {
                    query.visit_tables(f);
                }
            }
            exprs.extend(from.joins.iter().filter_map(|j| j.on.as_ref()));
        }
        for item in &self.items {
            if let SelectItem::Expr { expr, .. } = item {
                exprs.push(expr);
            }
        }
        exprs.extend(self.filter.iter().chain(&self.group_by).chain(&self.having));
        exprs.extend(self.order_by.iter().map(|o| &o.expr));
        for expr in exprs {
            expr.walk(&mut |e| {
                if let Expr::Subquery(select) | Expr::Exists { select, .. } | Expr::InSubquery { select, .. } = e {
                    select.visit_tables(f);
                }
            });
        }
        self.compound.iter().for_each(|(_, _, part)| part.visit_tables(f));
    }

    /// `SELECT * FROM (query)`: the query as a single operand, keeping its
    /// own ORDER BY, LIMIT and compound parts to itself.
    pub fn wrapping(query: Select) -> Select {
        Select {
            distinct: false,
            distinct_on: Vec::new(),
            grouping_sets: None,
            items: vec![SelectItem::Wildcard],
            from: Some(From {
                base: TableRef {
                    name: "subquery".into(),
                    alias: None,
                    columns: Vec::new(),
                    subquery: Some(Box::new(query)),
                },
                joins: Vec::new(),
            }),
            filter: None,
            group_by: Vec::new(),
            having: None,
            order_by: Vec::new(),
            limit: None,
            offset: None,
            compound: Vec::new(),
            with: None,
            windows: Vec::new(),
        }
    }
}

/// How a compound query combines its parts; the flag beside it means `ALL`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetOp {
    Union,
    Intersect,
    Except,
}

impl fmt::Display for SetOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SetOp::Union => "UNION",
            SetOp::Intersect => "INTERSECT",
            SetOp::Except => "EXCEPT",
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum SelectItem {
    /// `*`
    Wildcard,
    /// `table.*`
    TableWildcard(String),
    Expr {
        expr: Expr,
        alias: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct From {
    pub base: TableRef,
    pub joins: Vec<Join>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TableRef {
    pub name: String,
    pub alias: Option<String>,
    /// Names for its columns: `AS name (a, b)`. Empty: their own names.
    pub columns: Vec<String>,
    /// A query in place of a table: `FROM (SELECT ...) AS name`.
    pub subquery: Option<Box<Select>>,
}

impl TableRef {
    /// The name the table goes by in the query.
    pub fn binding(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Join {
    pub kind: JoinKind,
    pub table: TableRef,
    pub on: Option<Expr>,
    /// `USING (columns)`
    pub using: Vec<String>,
    /// `NATURAL`: USING every column name both sides have.
    pub natural: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OrderBy {
    pub expr: Expr,
    pub desc: bool,
    /// `NULLS FIRST` (true) or `NULLS LAST` (false), if given.
    pub nulls_first: Option<bool>,
}

impl OrderBy {
    pub fn sort(&self) -> Sort {
        // NULLs sort below every value unless told otherwise, as in SQLite.
        Sort { desc: self.desc, nulls_first: self.nulls_first.unwrap_or(!self.desc) }
    }
}

impl fmt::Display for OrderBy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.expr, if self.desc { " DESC" } else { "" })?;
        match self.nulls_first {
            Some(true) => f.write_str(" NULLS FIRST"),
            Some(false) => f.write_str(" NULLS LAST"),
            None => Ok(()),
        }
    }
}

/// How an ORDER BY key sorts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sort {
    pub desc: bool,
    pub nulls_first: bool,
}

impl Sort {
    pub fn compare(self, a: &Value, b: &Value) -> Ordering {
        match (a.is_null(), b.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) if self.nulls_first => Ordering::Less,
            (true, false) => Ordering::Greater,
            (false, true) if self.nulls_first => Ordering::Greater,
            (false, true) => Ordering::Less,
            _ if self.desc => b.total_cmp(a),
            _ => a.total_cmp(b),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Literal(Value),
    Column {
        table: Option<String>,
        name: String,
    },
    /// A query parameter, numbered from 0.
    Param(usize),
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    Binary {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    InList {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    Between {
        expr: Box<Expr>,
        low: Box<Expr>,
        high: Box<Expr>,
        negated: bool,
    },
    Like {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        negated: bool,
        case_insensitive: bool,
    },
    /// A function call. `star` marks `count(*)`.
    Function {
        name: String,
        args: Vec<Expr>,
        distinct: bool,
        star: bool,
        /// An aggregate's `FILTER (WHERE ...)`.
        filter: Option<Box<Expr>>,
        /// An aggregate's `ORDER BY` (`string_agg(x, ',' ORDER BY y)`), or the
        /// ordering of `WITHIN GROUP (ORDER BY ...)`.
        order_by: Vec<OrderBy>,
    },
    /// `name(args) OVER (...)`: a window function.
    Window {
        name: String,
        args: Vec<Expr>,
        star: bool,
        filter: Option<Box<Expr>>,
        over: Box<WindowSpec>,
    },
    Cast {
        expr: Box<Expr>,
        ty: DataType,
    },
    Case {
        operand: Option<Box<Expr>>,
        branches: Vec<(Expr, Expr)>,
        otherwise: Option<Box<Expr>>,
    },
    /// `(SELECT ...)` as a value.
    Subquery(Box<Select>),
    /// `(a, b, ...)`: a row value, which may be compared or tested with IN.
    Row(Vec<Expr>),
    /// `DEFAULT` in INSERT's VALUES: the column's default.
    Default,
    /// `[NOT] EXISTS (SELECT ...)`
    Exists {
        select: Box<Select>,
        negated: bool,
    },
    /// `expr [NOT] IN (SELECT ...)`
    InSubquery {
        expr: Box<Expr>,
        select: Box<Select>,
        negated: bool,
    },
}

impl Expr {
    /// Visits this expression and every subexpression.
    pub fn walk(&self, f: &mut dyn FnMut(&Expr)) {
        f(self);
        match self {
            // A subquery's own expressions belong to it, not to this query.
            Expr::Literal(_)
            | Expr::Column { .. }
            | Expr::Param(_)
            | Expr::Subquery(_)
            | Expr::Exists { .. }
            | Expr::Default => {}
            Expr::Unary { expr, .. }
            | Expr::IsNull { expr, .. }
            | Expr::Cast { expr, .. }
            | Expr::InSubquery { expr, .. } => expr.walk(f),
            Expr::Binary { left, right, .. } => {
                left.walk(f);
                right.walk(f);
            }
            Expr::InList { expr, list, .. } => {
                expr.walk(f);
                list.iter().for_each(|e| e.walk(f));
            }
            Expr::Between { expr, low, high, .. } => {
                expr.walk(f);
                low.walk(f);
                high.walk(f);
            }
            Expr::Like { expr, pattern, .. } => {
                expr.walk(f);
                pattern.walk(f);
            }
            Expr::Row(items) => items.iter().for_each(|e| e.walk(f)),
            Expr::Function { args, filter, order_by, .. } => {
                args.iter().for_each(|e| e.walk(f));
                filter.iter().for_each(|e| e.walk(f));
                order_by.iter().for_each(|o| o.expr.walk(f));
            }
            Expr::Window { args, filter, over, .. } => {
                args.iter().for_each(|e| e.walk(f));
                filter.iter().for_each(|e| e.walk(f));
                over.partition_by.iter().for_each(|e| e.walk(f));
                over.order_by.iter().for_each(|o| o.expr.walk(f));
            }
            Expr::Case { operand, branches, otherwise } => {
                operand.iter().for_each(|e| e.walk(f));
                for (when, then) in branches {
                    when.walk(f);
                    then.walk(f);
                }
                otherwise.iter().for_each(|e| e.walk(f));
            }
        }
    }

    /// Rewrites every node bottom-up with `f`.
    pub fn rewrite(&mut self, f: &mut dyn FnMut(&mut Expr)) {
        self.for_each_child(&mut |child| child.rewrite(f));
        f(self);
    }

    /// Calls `f` with each direct child expression (not those inside subqueries).
    pub fn for_each_child(&mut self, f: &mut dyn FnMut(&mut Expr)) {
        match self {
            Expr::Literal(_)
            | Expr::Column { .. }
            | Expr::Param(_)
            | Expr::Subquery(_)
            | Expr::Exists { .. }
            | Expr::Default => {}
            Expr::Unary { expr, .. }
            | Expr::IsNull { expr, .. }
            | Expr::Cast { expr, .. }
            | Expr::InSubquery { expr, .. } => f(expr),
            Expr::Binary { left, right, .. } => {
                f(left);
                f(right);
            }
            Expr::InList { expr, list, .. } => {
                f(expr);
                list.iter_mut().for_each(&mut *f);
            }
            Expr::Between { expr, low, high, .. } => {
                f(expr);
                f(low);
                f(high);
            }
            Expr::Like { expr, pattern, .. } => {
                f(expr);
                f(pattern);
            }
            Expr::Row(items) => items.iter_mut().for_each(&mut *f),
            Expr::Function { args, filter, order_by, .. } => {
                args.iter_mut().for_each(&mut *f);
                filter.iter_mut().for_each(|e| f(e));
                order_by.iter_mut().for_each(|o| f(&mut o.expr));
            }
            Expr::Window { args, filter, over, .. } => {
                args.iter_mut().for_each(&mut *f);
                filter.iter_mut().for_each(|e| f(e));
                over.partition_by.iter_mut().for_each(&mut *f);
                over.order_by.iter_mut().for_each(|o| f(&mut o.expr));
            }
            Expr::Case { operand, branches, otherwise } => {
                operand.iter_mut().for_each(|e| f(e));
                for (when, then) in branches {
                    f(when);
                    f(then);
                }
                otherwise.iter_mut().for_each(|e| f(e));
            }
        }
    }

    pub fn contains_aggregate(&self) -> bool {
        let mut found = false;
        self.walk(&mut |e| found |= matches!(e, Expr::Function { name, .. } if is_aggregate(name)));
        found
    }
}

pub fn is_aggregate(name: &str) -> bool {
    matches!(
        name,
        "count"
            | "sum"
            | "avg"
            | "min"
            | "max"
            | "total"
            | "group_concat"
            | "string_agg"
            | "bool_and"
            | "every"
            | "bool_or"
            | "variance"
            | "var_samp"
            | "var_pop"
            | "stddev"
            | "stddev_samp"
            | "stddev_pop"
            | "json_group_array"
            | "json_agg"
            | "json_group_object"
            | "json_object_agg"
            | "array_agg"
            | "median"
            | "percentile_cont"
            | "percentile_disc"
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    Not,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    Or,
    And,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Concat,
}

impl BinaryOp {
    pub fn is_comparison(self) -> bool {
        matches!(self, BinaryOp::Eq | BinaryOp::NotEq | BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq)
    }

    /// The operator with its operands swapped: `a < b` is `b > a`.
    pub fn flipped(self) -> BinaryOp {
        match self {
            BinaryOp::Lt => BinaryOp::Gt,
            BinaryOp::LtEq => BinaryOp::GtEq,
            BinaryOp::Gt => BinaryOp::Lt,
            BinaryOp::GtEq => BinaryOp::LtEq,
            op => op,
        }
    }
}

impl fmt::Display for BinaryOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            BinaryOp::Or => "OR",
            BinaryOp::And => "AND",
            BinaryOp::Eq => "=",
            BinaryOp::NotEq => "!=",
            BinaryOp::Lt => "<",
            BinaryOp::LtEq => "<=",
            BinaryOp::Gt => ">",
            BinaryOp::GtEq => ">=",
            BinaryOp::Add => "+",
            BinaryOp::Sub => "-",
            BinaryOp::Mul => "*",
            BinaryOp::Div => "/",
            BinaryOp::Rem => "%",
            BinaryOp::Concat => "||",
        })
    }
}

/// Renders expressions back to SQL; used to name result columns.
impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Parenthesise compound operands so the rendering parses back the same way.
        let operand = |e: &Expr| match e {
            Expr::Binary { .. } | Expr::Between { .. } | Expr::Like { .. } | Expr::InList { .. } => format!("({e})"),
            e => e.to_string(),
        };
        let not = |negated: &bool| if *negated { "NOT " } else { "" };
        match self {
            Expr::Literal(v) => f.write_str(&v.literal()),
            Expr::Column { table: Some(t), name } => write!(f, "{t}.{name}"),
            Expr::Column { table: None, name } => f.write_str(name),
            Expr::Param(i) => write!(f, "${}", i + 1),
            Expr::Unary { op: UnaryOp::Neg, expr } => write!(f, "-{}", operand(expr)),
            Expr::Unary { op: UnaryOp::Not, expr } => write!(f, "NOT {}", operand(expr)),
            Expr::Binary { op, left, right } => write!(f, "{} {op} {}", operand(left), operand(right)),
            Expr::IsNull { expr, negated } => write!(f, "{} IS {}NULL", operand(expr), not(negated)),
            Expr::InList { expr, list, negated } => {
                let items: Vec<String> = list.iter().map(ToString::to_string).collect();
                write!(f, "{} {}IN ({})", operand(expr), not(negated), items.join(", "))
            }
            Expr::Between { expr, low, high, negated } => {
                write!(f, "{} {}BETWEEN {} AND {}", operand(expr), not(negated), operand(low), operand(high))
            }
            Expr::Like { expr, pattern, negated, case_insensitive } => {
                let op = if *case_insensitive { "ILIKE" } else { "LIKE" };
                write!(f, "{} {}{op} {}", operand(expr), not(negated), operand(pattern))
            }
            Expr::Function { name, star: true, filter, .. } => {
                let filter = filter.as_ref().map(|e| format!(" FILTER (WHERE {e})")).unwrap_or_default();
                write!(f, "{name}(*){filter}")
            }
            // Written without parentheses, as SQL spells them.
            Expr::Function { name, args, .. }
                if args.is_empty()
                    && matches!(
                        name.as_str(),
                        "current_timestamp" | "current_date" | "current_time" | "localtimestamp" | "localtime"
                    ) =>
            {
                f.write_str(&name.to_ascii_uppercase())
            }
            Expr::Window { name, args, star, filter, over } => {
                let args: Vec<String> = args.iter().map(ToString::to_string).collect();
                let args = if *star { "*".to_string() } else { args.join(", ") };
                let filter = filter.as_ref().map(|e| format!(" FILTER (WHERE {e})")).unwrap_or_default();
                write!(f, "{name}({args}){filter} OVER {over}")
            }
            Expr::Function { name, args, .. } if name == "interval" && args.len() == 1 => {
                write!(f, "INTERVAL {}", args[0])
            }
            Expr::Function { name, args, distinct, filter, order_by, .. } => {
                let args: Vec<String> = args.iter().map(ToString::to_string).collect();
                let filter = filter.as_ref().map(|e| format!(" FILTER (WHERE {e})")).unwrap_or_default();
                let order = if order_by.is_empty() {
                    String::new()
                } else {
                    let keys: Vec<String> = order_by.iter().map(ToString::to_string).collect();
                    format!(" ORDER BY {}", keys.join(", "))
                };
                write!(f, "{name}({}{}{order}){filter}", if *distinct { "DISTINCT " } else { "" }, args.join(", "))
            }
            Expr::Cast { expr, ty } => write!(f, "CAST({expr} AS {ty})"),
            Expr::Row(items) => {
                let items: Vec<String> = items.iter().map(ToString::to_string).collect();
                write!(f, "({})", items.join(", "))
            }
            Expr::Default => f.write_str("DEFAULT"),
            Expr::Case { operand: op, branches, otherwise } => {
                f.write_str("CASE")?;
                if let Some(op) = op {
                    write!(f, " {op}")?;
                }
                for (when, then) in branches {
                    write!(f, " WHEN {when} THEN {then}")?;
                }
                if let Some(e) = otherwise {
                    write!(f, " ELSE {e}")?;
                }
                f.write_str(" END")
            }
            Expr::Subquery(_) => f.write_str("(SELECT ...)"),
            Expr::Exists { negated, .. } => write!(f, "{}EXISTS (SELECT ...)", not(negated)),
            Expr::InSubquery { expr, negated, .. } => write!(f, "{} {}IN (SELECT ...)", operand(expr), not(negated)),
        }
    }
}
