//! The syntax tree produced by the parser.

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
    DropIndex {
        name: String,
        if_exists: bool,
    },
    Insert(Insert),
    Select(Box<Select>),
    Update(Update),
    Delete(Delete),
    Begin,
    Commit,
    Rollback,
    Explain(Box<Statement>),
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
}

#[derive(Clone, Debug, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub ty: DataType,
    pub primary_key: bool,
    pub not_null: bool,
    pub unique: bool,
    pub default: Option<Expr>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CreateIndex {
    pub name: String,
    pub table: String,
    pub columns: Vec<String>,
    pub unique: bool,
    pub if_not_exists: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Insert {
    pub table: String,
    pub columns: Option<Vec<String>>,
    pub source: InsertSource,
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
    pub filter: Option<Expr>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Delete {
    pub table: String,
    pub filter: Option<Expr>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Select {
    pub distinct: bool,
    pub items: Vec<SelectItem>,
    pub from: Option<From>,
    pub filter: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
    pub order_by: Vec<OrderBy>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OrderBy {
    pub expr: Expr,
    pub desc: bool,
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
}

impl Expr {
    /// Visits this expression and every subexpression.
    pub fn walk(&self, f: &mut dyn FnMut(&Expr)) {
        f(self);
        match self {
            Expr::Literal(_) | Expr::Column { .. } | Expr::Param(_) => {}
            Expr::Unary { expr, .. } | Expr::IsNull { expr, .. } | Expr::Cast { expr, .. } => expr.walk(f),
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
            Expr::Function { args, .. } => args.iter().for_each(|e| e.walk(f)),
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

    pub fn contains_aggregate(&self) -> bool {
        let mut found = false;
        self.walk(&mut |e| found |= matches!(e, Expr::Function { name, .. } if is_aggregate(name)));
        found
    }
}

pub fn is_aggregate(name: &str) -> bool {
    matches!(name, "count" | "sum" | "avg" | "min" | "max")
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
            Expr::Function { name, star: true, .. } => write!(f, "{name}(*)"),
            Expr::Function { name, args, distinct, .. } => {
                let args: Vec<String> = args.iter().map(ToString::to_string).collect();
                write!(f, "{name}({}{})", if *distinct { "DISTINCT " } else { "" }, args.join(", "))
            }
            Expr::Cast { expr, ty } => write!(f, "CAST({expr} AS {ty})"),
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
        }
    }
}
