//! A recursive-descent SQL parser, with precedence climbing for expressions.

use std::fmt;

use super::ast::*;
use super::lexer::{Located, Token, syntax_error, tokenize};
use super::value::{DataType, Value};
use crate::error::{Error, Result};

/// Words that can't be used as unquoted identifiers, because they would make
/// an alias or a column name ambiguous with the clause that follows.
const RESERVED: &[&str] = &[
    "ALL",
    "AND",
    "AS",
    "ASC",
    "BETWEEN",
    "BY",
    "CASE",
    "CAST",
    "CREATE",
    "CROSS",
    "DELETE",
    "DESC",
    "DISTINCT",
    "DROP",
    "ELSE",
    "END",
    "EXCEPT",
    "EXISTS",
    "FALSE",
    "FROM",
    "FULL",
    "GROUP",
    "HAVING",
    "ILIKE",
    "IN",
    "INNER",
    "INSERT",
    "INTERSECT",
    "IS",
    "JOIN",
    "LEFT",
    "LIKE",
    "LIMIT",
    "NOT",
    "NULL",
    "OFFSET",
    "ON",
    "OR",
    "ORDER",
    "OUTER",
    "RETURNING",
    "RIGHT",
    "SELECT",
    "SET",
    "THEN",
    "TRUE",
    "UNION",
    "UPDATE",
    "USING",
    "VALUES",
    "WHEN",
    "WHERE",
    "WINDOW",
    "WITH",
];

/// Whether `e` is an `INTERVAL '...'` literal.
fn is_interval(e: &Expr) -> bool {
    matches!(e, Expr::Function { name, .. } if name == "interval")
}

fn is_reserved(word: &str) -> bool {
    RESERVED.iter().any(|r| r.eq_ignore_ascii_case(word))
}

// Binding power of each operator class, loosest first.
const P_OR: u8 = 1;
const P_AND: u8 = 2;
const P_NOT: u8 = 3;
const P_CMP: u8 = 4;
const P_ADD: u8 = 5;
const P_MUL: u8 = 6;
const P_UNARY: u8 = 7;

/// Parses every statement in `sql`, each with the number of parameters it takes.
pub fn parse_statements(sql: &str) -> Result<Vec<(Statement, usize)>> {
    let mut p = Parser { source: sql, tokens: tokenize(sql)?, pos: 0, positional: 0, params: 0 };
    let mut out = Vec::new();
    loop {
        while p.eat(&Token::Semicolon) {}
        if *p.peek() == Token::Eof {
            return Ok(out);
        }
        p.positional = 0;
        p.params = 0;
        let stmt = p.statement()?;
        out.push((stmt, p.params));
        if !p.eat(&Token::Semicolon) && *p.peek() != Token::Eof {
            return Err(p.unexpected("; or end of input"));
        }
    }
}

/// Parses exactly one statement.
pub fn parse_statement(sql: &str) -> Result<(Statement, usize)> {
    let mut statements = parse_statements(sql)?;
    match statements.len() {
        1 => Ok(statements.pop().unwrap()),
        0 => Err(Error::Syntax("syntax error: empty statement".into())),
        n => Err(Error::Sql(format!("expected one statement, found {n}; use execute_script to run several"))),
    }
}

/// Parses a query on its own, as stored for a view.
pub fn parse_query(sql: &str) -> Result<Select> {
    let mut p = Parser { source: sql, tokens: tokenize(sql)?, pos: 0, positional: 0, params: 0 };
    let query = p.query()?;
    if *p.peek() != Token::Eof {
        return Err(p.unexpected("end of query"));
    }
    Ok(query)
}

/// Parses an expression on its own, as stored for a CHECK constraint.
pub fn parse_expression(sql: &str) -> Result<Expr> {
    let mut p = Parser { source: sql, tokens: tokenize(sql)?, pos: 0, positional: 0, params: 0 };
    let expr = p.expr()?;
    if *p.peek() != Token::Eof {
        return Err(p.unexpected("end of expression"));
    }
    Ok(expr)
}

struct Parser<'a> {
    /// The SQL being parsed, for keeping a view's query as written.
    source: &'a str,
    tokens: Vec<Located>,
    pos: usize,
    /// `?` parameters seen so far in this statement.
    positional: usize,
    /// Parameters this statement takes: the highest number used.
    params: usize,
}

impl Parser<'_> {
    fn peek(&self) -> &Token {
        &self.tokens[self.pos].token
    }

    fn peek_nth(&self, n: usize) -> &Token {
        &self.tokens[(self.pos + n).min(self.tokens.len() - 1)].token
    }

    fn advance(&mut self) -> Token {
        let token = self.tokens[self.pos].token.clone();
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
        token
    }

    fn error(&self, msg: impl fmt::Display) -> Error {
        let at = &self.tokens[self.pos];
        syntax_error(at.line, at.column, msg)
    }

    fn unexpected(&self, expected: &str) -> Error {
        self.error(format!("expected {expected}, found {}", self.peek().describe()))
    }

    fn is_kw_at(&self, n: usize, kw: &str) -> bool {
        matches!(self.peek_nth(n), Token::Word(w) if w.eq_ignore_ascii_case(kw))
    }

    fn is_kw(&self, kw: &str) -> bool {
        self.is_kw_at(0, kw)
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        let found = self.is_kw(kw);
        if found {
            self.advance();
        }
        found
    }

    fn expect_kw(&mut self, kw: &str) -> Result<()> {
        if self.eat_kw(kw) { Ok(()) } else { Err(self.unexpected(kw)) }
    }

    fn eat(&mut self, token: &Token) -> bool {
        let found = self.peek() == token;
        if found {
            self.advance();
        }
        found
    }

    fn expect(&mut self, token: Token) -> Result<()> {
        if self.eat(&token) { Ok(()) } else { Err(self.unexpected(&format!("'{}'", token.describe()))) }
    }

    /// An identifier: unquoted ones are case-insensitive, so they're lowercased.
    fn ident(&mut self) -> Result<String> {
        match self.peek() {
            Token::Word(w) if !is_reserved(w) => {
                let name = w.to_lowercase();
                self.advance();
                Ok(name)
            }
            Token::QuotedIdent(w) => {
                let name = w.clone();
                self.advance();
                Ok(name)
            }
            _ => Err(self.unexpected("an identifier")),
        }
    }

    fn ident_list(&mut self) -> Result<Vec<String>> {
        self.expect(Token::LParen)?;
        let mut names = vec![self.ident()?];
        while self.eat(&Token::Comma) {
            names.push(self.ident()?);
        }
        self.expect(Token::RParen)?;
        Ok(names)
    }

    fn statement(&mut self) -> Result<Statement> {
        if *self.peek() == Token::LParen {
            return Ok(Statement::Select(Box::new(self.query()?)));
        }
        let Token::Word(word) = self.peek() else { return Err(self.unexpected("a statement")) };
        match word.to_ascii_uppercase().as_str() {
            "SELECT" | "WITH" => Ok(Statement::Select(Box::new(self.query()?))),
            "INSERT" => self.insert(),
            "UPDATE" => self.update(),
            "DELETE" => self.delete(),
            "CREATE" => self.create(),
            "ALTER" => self.alter(),
            "DROP" => self.drop(),
            "BEGIN" => {
                self.advance();
                self.eat_kw("TRANSACTION");
                Ok(Statement::Begin)
            }
            "START" => {
                self.advance();
                self.expect_kw("TRANSACTION")?;
                Ok(Statement::Begin)
            }
            "COMMIT" | "END" => {
                self.advance();
                self.eat_kw("TRANSACTION");
                Ok(Statement::Commit)
            }
            "ROLLBACK" => {
                self.advance();
                if !self.eat_kw("TRANSACTION") {
                    self.eat_kw("WORK");
                }
                if self.eat_kw("TO") {
                    self.eat_kw("SAVEPOINT");
                    return Ok(Statement::RollbackTo(self.ident()?));
                }
                Ok(Statement::Rollback)
            }
            "SAVEPOINT" => {
                self.advance();
                Ok(Statement::Savepoint(self.ident()?))
            }
            "RELEASE" => {
                self.advance();
                self.eat_kw("SAVEPOINT");
                Ok(Statement::Release(self.ident()?))
            }
            "EXPLAIN" => {
                self.advance();
                Ok(Statement::Explain(Box::new(self.statement()?)))
            }
            "LISTEN" => {
                self.advance();
                Ok(Statement::Listen(self.ident()?))
            }
            "UNLISTEN" => {
                self.advance();
                if self.eat(&Token::Star) {
                    return Ok(Statement::Unlisten(None));
                }
                Ok(Statement::Unlisten(Some(self.ident()?)))
            }
            "NOTIFY" => {
                self.advance();
                let channel = self.ident()?;
                let payload = if self.eat(&Token::Comma) { Some(self.expr()?) } else { None };
                Ok(Statement::Notify { channel, payload })
            }
            _ => Err(self.unexpected("a statement")),
        }
    }

    /// A whole query: SELECTs combined with UNION, INTERSECT and EXCEPT,
    /// then ORDER BY, LIMIT and OFFSET for the result. INTERSECT binds more
    /// tightly than the other two, as in standard SQL.
    fn query(&mut self) -> Result<Select> {
        if self.eat_kw("WITH") {
            let recursive = self.eat_kw("RECURSIVE");
            let mut ctes = Vec::new();
            loop {
                let name = self.ident()?;
                let columns = if *self.peek() == Token::LParen { self.ident_list()? } else { Vec::new() };
                self.expect_kw("AS")?;
                self.expect(Token::LParen)?;
                let query = self.query()?;
                self.expect(Token::RParen)?;
                ctes.push(Cte { name, columns, query });
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
            let mut query = self.query()?;
            if query.with.is_some() {
                query = Select::wrapping(query);
            }
            query.with = Some(Box::new(With { recursive, ctes }));
            return Ok(query);
        }
        let mut query = self.operand()?;
        loop {
            let op = if self.eat_kw("UNION") {
                SetOp::Union
            } else if self.eat_kw("INTERSECT") {
                SetOp::Intersect
            } else if self.eat_kw("EXCEPT") {
                SetOp::Except
            } else {
                break;
            };
            let all = self.eat_kw("ALL");
            if !all {
                self.eat_kw("DISTINCT");
            }
            let right = if op == SetOp::Intersect { self.operand()? } else { self.intersect_group()? };
            query.compound.push((op, all, right));
        }
        if self.eat_kw("ORDER") {
            self.expect_kw("BY")?;
            query.order_by = self.order_items()?;
        }
        query.limit = if self.eat_kw("LIMIT") { Some(self.expr()?) } else { None };
        query.offset = if self.eat_kw("OFFSET") { Some(self.expr()?) } else { None };
        Ok(query)
    }

    /// One part of a compound query: a SELECT, or a query in parentheses.
    fn operand(&mut self) -> Result<Select> {
        if *self.peek() == Token::LParen
            && (self.is_kw_at(1, "SELECT") || self.is_kw_at(1, "WITH") || *self.peek_nth(1) == Token::LParen)
        {
            self.advance();
            let inner = self.query()?;
            self.expect(Token::RParen)?;
            // Its own ORDER BY, LIMIT or parts stay its own.
            let alone = inner.order_by.is_empty() && inner.limit.is_none() && inner.offset.is_none();
            return Ok(if alone && inner.compound.is_empty() { inner } else { Select::wrapping(inner) });
        }
        self.select()
    }

    /// The right side of UNION or EXCEPT: an operand with any INTERSECTs
    /// that follow it, since those bind first.
    fn intersect_group(&mut self) -> Result<Select> {
        let mut group = self.operand()?;
        if !self.is_kw("INTERSECT") {
            return Ok(group);
        }
        if !group.compound.is_empty() {
            group = Select::wrapping(group);
        }
        while self.eat_kw("INTERSECT") {
            let all = self.eat_kw("ALL");
            if !all {
                self.eat_kw("DISTINCT");
            }
            group.compound.push((SetOp::Intersect, all, self.operand()?));
        }
        Ok(Select::wrapping(group))
    }

    /// A single SELECT, up to HAVING.
    fn select(&mut self) -> Result<Select> {
        self.expect_kw("SELECT")?;
        let distinct = self.eat_kw("DISTINCT");
        if !distinct {
            self.eat_kw("ALL");
        }
        let mut items = vec![self.select_item()?];
        while self.eat(&Token::Comma) {
            items.push(self.select_item()?);
        }
        let from = if self.eat_kw("FROM") { Some(self.from()?) } else { None };
        let filter = if self.eat_kw("WHERE") { Some(self.expr()?) } else { None };
        let mut group_by = Vec::new();
        if self.eat_kw("GROUP") {
            self.expect_kw("BY")?;
            group_by = self.expr_list()?;
        }
        let having = if self.eat_kw("HAVING") { Some(self.expr()?) } else { None };
        let mut windows = Vec::new();
        if self.eat_kw("WINDOW") {
            loop {
                let name = self.ident()?;
                self.expect_kw("AS")?;
                windows.push((name, self.window_spec()?));
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
        }
        Ok(Select {
            distinct,
            items,
            from,
            filter,
            group_by,
            having,
            order_by: Vec::new(),
            limit: None,
            offset: None,
            compound: Vec::new(),
            with: None,
            windows,
        })
    }

    fn select_item(&mut self) -> Result<SelectItem> {
        if self.eat(&Token::Star) {
            return Ok(SelectItem::Wildcard);
        }
        if matches!(self.peek(), Token::Word(_) | Token::QuotedIdent(_))
            && *self.peek_nth(1) == Token::Dot
            && *self.peek_nth(2) == Token::Star
        {
            let table = self.ident()?;
            self.advance();
            self.advance();
            return Ok(SelectItem::TableWildcard(table));
        }
        let expr = self.expr()?;
        let alias = self.alias()?;
        Ok(SelectItem::Expr { expr, alias })
    }

    fn alias(&mut self) -> Result<Option<String>> {
        if self.eat_kw("AS") {
            return Ok(Some(self.ident()?));
        }
        match self.peek() {
            Token::Word(w) if !is_reserved(w) => Ok(Some(self.ident()?)),
            Token::QuotedIdent(_) => Ok(Some(self.ident()?)),
            _ => Ok(None),
        }
    }

    fn table_ref(&mut self) -> Result<TableRef> {
        if self.eat(&Token::LParen) {
            // `(SELECT ...) AS name`: a query read like a table.
            let query = self.query()?;
            self.expect(Token::RParen)?;
            let alias = self.alias()?;
            let columns =
                if alias.is_some() && *self.peek() == Token::LParen { self.ident_list()? } else { Vec::new() };
            return Ok(TableRef { name: "subquery".into(), alias, columns, subquery: Some(Box::new(query)) });
        }
        let name = self.ident()?;
        let alias = self.alias()?;
        Ok(TableRef { name, alias, columns: Vec::new(), subquery: None })
    }

    fn from(&mut self) -> Result<From> {
        let base = self.table_ref()?;
        let mut joins = Vec::new();
        loop {
            if self.eat(&Token::Comma) {
                joins.push(Join { kind: JoinKind::Inner, table: self.table_ref()?, on: None });
                continue;
            }
            if self.is_kw("CROSS") {
                self.advance();
                self.expect_kw("JOIN")?;
                joins.push(Join { kind: JoinKind::Inner, table: self.table_ref()?, on: None });
                continue;
            }
            let kind = if self.eat_kw("JOIN") {
                JoinKind::Inner
            } else if self.eat_kw("INNER") {
                self.expect_kw("JOIN")?;
                JoinKind::Inner
            } else if self.eat_kw("LEFT") {
                self.eat_kw("OUTER");
                self.expect_kw("JOIN")?;
                JoinKind::Left
            } else if self.is_kw("RIGHT") || self.is_kw("FULL") {
                return Err(self.error("RIGHT and FULL joins are not supported yet"));
            } else {
                break;
            };
            let table = self.table_ref()?;
            self.expect_kw("ON")?;
            joins.push(Join { kind, table, on: Some(self.expr()?) });
        }
        Ok(From { base, joins })
    }

    fn insert(&mut self) -> Result<Statement> {
        self.expect_kw("INSERT")?;
        // SQLite's INSERT OR IGNORE: ON CONFLICT DO NOTHING.
        let ignore = self.eat_kw("OR");
        if ignore {
            self.expect_kw("IGNORE")?;
        }
        self.expect_kw("INTO")?;
        let table = self.ident()?;
        let columns = if *self.peek() == Token::LParen { Some(self.ident_list()?) } else { None };
        let source = if self.eat_kw("VALUES") {
            let mut rows = Vec::new();
            loop {
                self.expect(Token::LParen)?;
                rows.push(self.expr_list()?);
                self.expect(Token::RParen)?;
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
            InsertSource::Values(rows)
        } else if self.is_kw("SELECT") || self.is_kw("WITH") || *self.peek() == Token::LParen {
            InsertSource::Select(Box::new(self.query()?))
        } else {
            return Err(self.unexpected("VALUES or SELECT"));
        };
        let mut on_conflict = ignore.then(|| OnConflict { target: Vec::new(), action: ConflictAction::Nothing });
        if self.is_kw("ON") && self.is_kw_at(1, "CONFLICT") {
            self.advance();
            self.advance();
            let target = if *self.peek() == Token::LParen { self.ident_list()? } else { Vec::new() };
            self.expect_kw("DO")?;
            let action = if self.eat_kw("NOTHING") {
                ConflictAction::Nothing
            } else {
                self.expect_kw("UPDATE")?;
                self.expect_kw("SET")?;
                let assignments = self.assignments()?;
                let filter = if self.eat_kw("WHERE") { Some(self.expr()?) } else { None };
                ConflictAction::Update { assignments, filter }
            };
            on_conflict = Some(OnConflict { target, action });
        } else if self.is_kw("ON") && self.is_kw_at(1, "DUPLICATE") {
            // MySQL's spelling, where VALUES(column) is the proposed value.
            self.advance();
            self.advance();
            self.expect_kw("KEY")?;
            self.expect_kw("UPDATE")?;
            let mut assignments = self.assignments()?;
            for (_, e) in &mut assignments {
                e.rewrite(&mut |e| {
                    if let Expr::Function { name, args, .. } = e
                        && name == "values"
                        && let [Expr::Column { name: column, .. }] = args.as_slice()
                    {
                        *e = Expr::Column { table: Some("excluded".into()), name: column.clone() };
                    }
                });
            }
            on_conflict =
                Some(OnConflict { target: Vec::new(), action: ConflictAction::Update { assignments, filter: None } });
        }
        let returning = self.returning()?;
        Ok(Statement::Insert(Insert { table, columns, source, on_conflict, returning }))
    }

    /// `column = expr, ...` after SET.
    fn assignments(&mut self) -> Result<Vec<(String, Expr)>> {
        let mut assignments = Vec::new();
        loop {
            let column = self.ident()?;
            self.expect(Token::Eq)?;
            assignments.push((column, self.expr()?));
            if !self.eat(&Token::Comma) {
                return Ok(assignments);
            }
        }
    }

    /// `RETURNING item, ...`, if present.
    fn returning(&mut self) -> Result<Vec<SelectItem>> {
        let mut items = Vec::new();
        if self.eat_kw("RETURNING") {
            items.push(self.select_item()?);
            while self.eat(&Token::Comma) {
                items.push(self.select_item()?);
            }
        }
        Ok(items)
    }

    fn update(&mut self) -> Result<Statement> {
        self.expect_kw("UPDATE")?;
        let table = self.ident()?;
        self.expect_kw("SET")?;
        let assignments = self.assignments()?;
        let filter = if self.eat_kw("WHERE") { Some(self.expr()?) } else { None };
        let returning = self.returning()?;
        Ok(Statement::Update(Update { table, assignments, filter, returning }))
    }

    fn delete(&mut self) -> Result<Statement> {
        self.expect_kw("DELETE")?;
        self.expect_kw("FROM")?;
        let table = self.ident()?;
        let filter = if self.eat_kw("WHERE") { Some(self.expr()?) } else { None };
        let returning = self.returning()?;
        Ok(Statement::Delete(Delete { table, filter, returning }))
    }

    fn if_not_exists(&mut self) -> Result<bool> {
        if !self.eat_kw("IF") {
            return Ok(false);
        }
        self.expect_kw("NOT")?;
        self.expect_kw("EXISTS")?;
        Ok(true)
    }

    fn if_exists(&mut self) -> Result<bool> {
        if !self.eat_kw("IF") {
            return Ok(false);
        }
        self.expect_kw("EXISTS")?;
        Ok(true)
    }

    fn create(&mut self) -> Result<Statement> {
        self.expect_kw("CREATE")?;
        if self.eat_kw("TABLE") {
            return self.create_table();
        }
        let or_replace = self.eat_kw("OR");
        if or_replace {
            self.expect_kw("REPLACE")?;
        }
        if self.eat_kw("VIEW") {
            let if_not_exists = self.if_not_exists()?;
            let name = self.ident()?;
            let columns = if *self.peek() == Token::LParen { self.ident_list()? } else { Vec::new() };
            self.expect_kw("AS")?;
            let start = self.tokens[self.pos].offset;
            let query = Box::new(self.query()?);
            let end = self.tokens[self.pos].offset;
            let sql: String = self.source.chars().skip(start).take(end - start).collect();
            let sql = sql.trim().to_string();
            return Ok(Statement::CreateView { name, columns, query, sql, or_replace, if_not_exists });
        }
        if or_replace {
            return Err(self.unexpected("VIEW"));
        }
        let unique = self.eat_kw("UNIQUE");
        if !self.eat_kw("INDEX") {
            return Err(self.unexpected(if unique { "INDEX" } else { "TABLE or INDEX" }));
        }
        let if_not_exists = self.if_not_exists()?;
        let name = self.ident()?;
        self.expect_kw("ON")?;
        let table = self.ident()?;
        let columns = self.ident_list()?;
        Ok(Statement::CreateIndex(CreateIndex { name, table, columns, unique, if_not_exists }))
    }

    fn create_table(&mut self) -> Result<Statement> {
        let if_not_exists = self.if_not_exists()?;
        let name = self.ident()?;
        self.expect(Token::LParen)?;
        let (mut columns, mut primary_key, mut unique) = (Vec::new(), Vec::new(), Vec::new());
        let (mut checks, mut foreign_keys) = (Vec::new(), Vec::new());
        loop {
            let constraint = if self.eat_kw("CONSTRAINT") { Some(self.ident()?) } else { None };
            if self.is_kw("CHECK") {
                checks.push(self.check(constraint)?);
            } else if self.eat_kw("FOREIGN") {
                self.expect_kw("KEY")?;
                let columns = self.ident_list()?;
                foreign_keys.push(self.references(constraint, columns)?);
            } else if self.is_kw("PRIMARY") {
                self.advance();
                self.expect_kw("KEY")?;
                if !primary_key.is_empty() {
                    return Err(self.error("multiple PRIMARY KEY clauses"));
                }
                primary_key = self.ident_list()?;
            } else if self.is_kw("UNIQUE") && *self.peek_nth(1) == Token::LParen {
                self.advance();
                unique.push(self.ident_list()?);
            } else if constraint.is_some() {
                return Err(self.unexpected("CHECK, FOREIGN KEY, PRIMARY KEY or UNIQUE"));
            } else {
                let mut column = self.column_def()?;
                checks.append(&mut column.checks);
                foreign_keys.extend(column.references.take());
                columns.push(column);
            }
            if !self.eat(&Token::Comma) {
                break;
            }
        }
        self.expect(Token::RParen)?;
        Ok(Statement::CreateTable(CreateTable {
            name,
            if_not_exists,
            columns,
            primary_key,
            unique,
            checks,
            foreign_keys,
        }))
    }

    fn alter(&mut self) -> Result<Statement> {
        self.expect_kw("ALTER")?;
        self.expect_kw("TABLE")?;
        let table = self.ident()?;
        let action = if self.eat_kw("ADD") {
            self.eat_kw("COLUMN");
            AlterAction::AddColumn(self.column_def()?)
        } else if self.eat_kw("DROP") {
            self.eat_kw("COLUMN");
            AlterAction::DropColumn(self.ident()?)
        } else if self.eat_kw("RENAME") {
            if self.eat_kw("TO") {
                AlterAction::RenameTable(self.ident()?)
            } else {
                self.eat_kw("COLUMN");
                let from = self.ident()?;
                self.expect_kw("TO")?;
                AlterAction::RenameColumn { from, to: self.ident()? }
            }
        } else {
            return Err(self.unexpected("ADD, DROP or RENAME"));
        };
        Ok(Statement::AlterTable { table, action })
    }

    fn column_def(&mut self) -> Result<ColumnDef> {
        let name = self.ident()?;
        let ty = self.data_type()?;
        let mut column = ColumnDef {
            name,
            ty,
            primary_key: false,
            not_null: false,
            unique: false,
            default: None,
            checks: Vec::new(),
            references: None,
        };
        loop {
            let constraint = if self.eat_kw("CONSTRAINT") { Some(self.ident()?) } else { None };
            if self.is_kw("CHECK") {
                column.checks.push(self.check(constraint)?);
                continue;
            }
            if self.is_kw("REFERENCES") {
                let columns = vec![column.name.clone()];
                column.references = Some(self.references(constraint, columns)?);
                continue;
            }
            if let Some(name) = constraint {
                return Err(self.error(format!("constraint {name} must be CHECK or REFERENCES here")));
            }
            if self.eat_kw("PRIMARY") {
                self.expect_kw("KEY")?;
                column.primary_key = true;
                self.eat_kw("AUTOINCREMENT"); // integer primary keys auto-assign anyway
            } else if self.eat_kw("NOT") {
                self.expect_kw("NULL")?;
                column.not_null = true;
            } else if self.eat_kw("NULL") {
            } else if self.eat_kw("UNIQUE") {
                column.unique = true;
            } else if self.eat_kw("DEFAULT") {
                column.default = Some(self.expr_bp(P_CMP + 1)?);
            } else {
                return Ok(column);
            }
        }
    }

    /// `CHECK (expr)`
    fn check(&mut self, name: Option<String>) -> Result<CheckDef> {
        self.expect_kw("CHECK")?;
        self.expect(Token::LParen)?;
        let expr = self.expr()?;
        self.expect(Token::RParen)?;
        Ok(CheckDef { name, expr })
    }

    /// `REFERENCES table [(columns)] [ON DELETE action] [ON UPDATE action]`
    fn references(&mut self, name: Option<String>, columns: Vec<String>) -> Result<ForeignKeyDef> {
        self.expect_kw("REFERENCES")?;
        let table = self.ident()?;
        let references = if *self.peek() == Token::LParen { self.ident_list()? } else { Vec::new() };
        let mut fk = ForeignKeyDef {
            name,
            columns,
            table,
            references,
            on_delete: FkAction::NoAction,
            on_update: FkAction::NoAction,
        };
        while self.is_kw("ON") && (self.is_kw_at(1, "DELETE") || self.is_kw_at(1, "UPDATE")) {
            self.advance();
            let deleting = self.eat_kw("DELETE");
            if !deleting {
                self.expect_kw("UPDATE")?;
            }
            let action = if self.eat_kw("CASCADE") {
                FkAction::Cascade
            } else if self.eat_kw("RESTRICT") {
                FkAction::Restrict
            } else if self.eat_kw("NO") {
                self.expect_kw("ACTION")?;
                FkAction::NoAction
            } else {
                self.expect_kw("SET")?;
                if self.eat_kw("NULL") {
                    FkAction::SetNull
                } else {
                    self.expect_kw("DEFAULT")?;
                    FkAction::SetDefault
                }
            };
            if deleting {
                fk.on_delete = action;
            } else {
                fk.on_update = action;
            }
        }
        if self.is_kw("DEFERRABLE") || self.is_kw("MATCH") {
            return Err(self.error(format!("{} is not supported", self.peek().describe())));
        }
        Ok(fk)
    }

    fn data_type(&mut self) -> Result<DataType> {
        let Token::Word(word) = self.peek() else { return Err(self.unexpected("a type name")) };
        let ty = match word.to_ascii_uppercase().as_str() {
            "INTEGER" | "INT" | "BIGINT" | "SMALLINT" | "TINYINT" | "INT2" | "INT4" | "INT8" | "SERIAL"
            | "BIGSERIAL" => DataType::Integer,
            "REAL" | "FLOAT" | "FLOAT4" | "FLOAT8" | "DOUBLE" | "NUMERIC" | "DECIMAL" => DataType::Real,
            "TEXT" | "VARCHAR" | "CHAR" | "CHARACTER" | "STRING" | "CLOB" => DataType::Text,
            "BOOLEAN" | "BOOL" => DataType::Boolean,
            "DATE" => DataType::Date,
            "TIME" => DataType::Time,
            "TIMESTAMP" | "DATETIME" | "TIMESTAMPTZ" => DataType::Timestamp,
            _ => return Err(self.error(format!("unknown type {word}"))),
        };
        self.advance();
        // TIMESTAMP WITH[OUT] TIME ZONE: either way, stored in UTC.
        if ty.is_temporal() && (self.is_kw("WITH") || self.is_kw("WITHOUT")) && self.is_kw_at(1, "TIME") {
            self.advance();
            self.advance();
            self.expect_kw("ZONE")?;
        }
        self.eat_kw("PRECISION"); // DOUBLE PRECISION
        self.eat_kw("VARYING"); // CHARACTER VARYING
        // Length and precision modifiers, as in VARCHAR(255), are accepted and ignored.
        if self.eat(&Token::LParen) {
            loop {
                match self.advance() {
                    Token::Number(_) => {}
                    _ => return Err(self.unexpected("a number")),
                }
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
            self.expect(Token::RParen)?;
        }
        Ok(ty)
    }

    fn drop(&mut self) -> Result<Statement> {
        self.expect_kw("DROP")?;
        if self.eat_kw("TABLE") {
            let if_exists = self.if_exists()?;
            Ok(Statement::DropTable { if_exists, name: self.ident()? })
        } else if self.eat_kw("INDEX") {
            let if_exists = self.if_exists()?;
            Ok(Statement::DropIndex { if_exists, name: self.ident()? })
        } else if self.eat_kw("VIEW") {
            let if_exists = self.if_exists()?;
            Ok(Statement::DropView { if_exists, name: self.ident()? })
        } else {
            Err(self.unexpected("TABLE, INDEX or VIEW"))
        }
    }

    fn expr_list(&mut self) -> Result<Vec<Expr>> {
        let mut exprs = vec![self.expr()?];
        while self.eat(&Token::Comma) {
            exprs.push(self.expr()?);
        }
        Ok(exprs)
    }

    fn expr(&mut self) -> Result<Expr> {
        self.expr_bp(0)
    }

    /// Parses an expression whose operators all bind at least as tightly as `min`.
    fn expr_bp(&mut self, min: u8) -> Result<Expr> {
        let mut lhs = self.prefix()?;
        loop {
            if self.is_kw("IS") {
                if P_CMP < min {
                    break;
                }
                self.advance();
                let negated = self.eat_kw("NOT");
                self.expect_kw("NULL")?;
                lhs = Expr::IsNull { expr: Box::new(lhs), negated };
                continue;
            }
            let negated = self.is_kw("NOT") && ["IN", "LIKE", "ILIKE", "BETWEEN"].iter().any(|kw| self.is_kw_at(1, kw));
            if negated || ["IN", "LIKE", "ILIKE", "BETWEEN"].iter().any(|kw| self.is_kw(kw)) {
                if P_CMP < min {
                    break;
                }
                if negated {
                    self.advance();
                }
                let expr = Box::new(lhs);
                lhs = if self.eat_kw("IN") {
                    self.expect(Token::LParen)?;
                    if self.is_kw("SELECT") || self.is_kw("WITH") {
                        let select = Box::new(self.query()?);
                        self.expect(Token::RParen)?;
                        lhs = Expr::InSubquery { expr, select, negated };
                        continue;
                    }
                    let list = self.expr_list()?;
                    self.expect(Token::RParen)?;
                    Expr::InList { expr, list, negated }
                } else if self.eat_kw("BETWEEN") {
                    let low = Box::new(self.expr_bp(P_CMP + 1)?);
                    self.expect_kw("AND")?;
                    let high = Box::new(self.expr_bp(P_CMP + 1)?);
                    Expr::Between { expr, low, high, negated }
                } else {
                    let case_insensitive = self.eat_kw("ILIKE");
                    if !case_insensitive {
                        self.expect_kw("LIKE")?;
                    }
                    let pattern = Box::new(self.expr_bp(P_CMP + 1)?);
                    Expr::Like { expr, pattern, negated, case_insensitive }
                };
                continue;
            }
            let (op, prec) = match self.peek() {
                Token::Word(w) if w.eq_ignore_ascii_case("OR") => (BinaryOp::Or, P_OR),
                Token::Word(w) if w.eq_ignore_ascii_case("AND") => (BinaryOp::And, P_AND),
                Token::Eq => (BinaryOp::Eq, P_CMP),
                Token::NotEq => (BinaryOp::NotEq, P_CMP),
                Token::Lt => (BinaryOp::Lt, P_CMP),
                Token::LtEq => (BinaryOp::LtEq, P_CMP),
                Token::Gt => (BinaryOp::Gt, P_CMP),
                Token::GtEq => (BinaryOp::GtEq, P_CMP),
                Token::Plus => (BinaryOp::Add, P_ADD),
                Token::Minus => (BinaryOp::Sub, P_ADD),
                Token::Concat => (BinaryOp::Concat, P_ADD),
                Token::Star => (BinaryOp::Mul, P_MUL),
                Token::Slash => (BinaryOp::Div, P_MUL),
                Token::Percent => (BinaryOp::Rem, P_MUL),
                _ => break,
            };
            if prec < min {
                break;
            }
            self.advance();
            let rhs = self.expr_bp(prec + 1)?;
            lhs = match (op, lhs, rhs) {
                // time ± INTERVAL '...' shifts the time.
                (BinaryOp::Add | BinaryOp::Sub, time, interval) | (BinaryOp::Add, interval, time)
                    if is_interval(&interval) =>
                {
                    let name = if op == BinaryOp::Add { "date_add" } else { "date_sub" };
                    Expr::Function {
                        name: name.into(),
                        args: vec![time, interval],
                        distinct: false,
                        star: false,
                        filter: None,
                    }
                }
                (op, lhs, rhs) => Expr::Binary { op, left: Box::new(lhs), right: Box::new(rhs) },
            };
        }
        Ok(lhs)
    }

    fn prefix(&mut self) -> Result<Expr> {
        match self.peek().clone() {
            Token::Number(n) => {
                self.advance();
                self.number(&n)
            }
            Token::String(s) => {
                self.advance();
                Ok(Expr::Literal(Value::Text(s)))
            }
            Token::Param(number) => {
                self.advance();
                let index = match number {
                    Some(n) => n - 1,
                    None => {
                        self.positional += 1;
                        self.positional - 1
                    }
                };
                self.params = self.params.max(index + 1);
                Ok(Expr::Param(index))
            }
            Token::Minus => {
                self.advance();
                // Fold the sign into numeric literals, so i64::MIN can be written.
                if let Token::Number(n) = self.peek().clone() {
                    self.advance();
                    return self.number(&format!("-{n}"));
                }
                Ok(Expr::Unary { op: UnaryOp::Neg, expr: Box::new(self.expr_bp(P_UNARY)?) })
            }
            Token::Plus => {
                self.advance();
                self.expr_bp(P_UNARY)
            }
            Token::LParen => {
                self.advance();
                if self.is_kw("SELECT") || self.is_kw("WITH") {
                    let select = self.query()?;
                    self.expect(Token::RParen)?;
                    return Ok(Expr::Subquery(Box::new(select)));
                }
                let expr = self.expr()?;
                self.expect(Token::RParen)?;
                Ok(expr)
            }
            Token::Word(word) => match word.to_ascii_uppercase().as_str() {
                "NULL" => {
                    self.advance();
                    Ok(Expr::Literal(Value::Null))
                }
                "TRUE" | "FALSE" => {
                    self.advance();
                    Ok(Expr::Literal(Value::Boolean(word.eq_ignore_ascii_case("TRUE"))))
                }
                "NOT" => {
                    self.advance();
                    Ok(Expr::Unary { op: UnaryOp::Not, expr: Box::new(self.expr_bp(P_NOT)?) })
                }
                "CASE" => self.case(),
                "CAST" => {
                    self.advance();
                    self.expect(Token::LParen)?;
                    let expr = Box::new(self.expr()?);
                    self.expect_kw("AS")?;
                    let ty = self.data_type()?;
                    self.expect(Token::RParen)?;
                    Ok(Expr::Cast { expr, ty })
                }
                "EXISTS" => {
                    self.advance();
                    self.expect(Token::LParen)?;
                    let select = Box::new(self.query()?);
                    self.expect(Token::RParen)?;
                    Ok(Expr::Exists { select, negated: false })
                }
                // Typed literals: DATE '2024-03-15', TIMESTAMP '...', TIME '...'.
                "DATE" | "TIME" | "TIMESTAMP" if matches!(self.peek_nth(1), Token::String(_)) => {
                    let ty = self.data_type()?;
                    let Token::String(s) = self.advance() else { unreachable!() };
                    Ok(Expr::Cast { expr: Box::new(Expr::Literal(Value::Text(s))), ty })
                }
                // INTERVAL '1 day', INTERVAL '3' HOUR or INTERVAL 3 HOUR.
                "INTERVAL" if matches!(self.peek_nth(1), Token::String(_) | Token::Number(_)) => {
                    self.advance();
                    let mut text = match self.advance() {
                        Token::String(s) | Token::Number(s) => s,
                        _ => unreachable!(),
                    };
                    if let Token::Word(unit) = self.peek().clone()
                        && !is_reserved(&unit)
                        && !unit.eq_ignore_ascii_case("AS")
                    {
                        self.advance();
                        text = format!("{text} {}", unit.to_ascii_lowercase());
                    }
                    let literal = Expr::Literal(Value::Text(text));
                    Ok(Expr::Function {
                        name: "interval".into(),
                        args: vec![literal],
                        distinct: false,
                        star: false,
                        filter: None,
                    })
                }
                "CURRENT_TIMESTAMP" | "CURRENT_DATE" | "CURRENT_TIME" | "LOCALTIMESTAMP" | "LOCALTIME"
                    if *self.peek_nth(1) != Token::LParen =>
                {
                    self.advance();
                    Ok(Expr::Function {
                        name: word.to_ascii_lowercase(),
                        args: Vec::new(),
                        distinct: false,
                        star: false,
                        filter: None,
                    })
                }
                // EXTRACT(field FROM time) is date_part('field', time).
                "EXTRACT" if *self.peek_nth(1) == Token::LParen => {
                    self.advance();
                    self.advance();
                    let field = match self.advance() {
                        Token::Word(w) | Token::String(w) => w.to_ascii_lowercase(),
                        _ => return Err(self.unexpected("a date field")),
                    };
                    self.expect_kw("FROM")?;
                    let time = self.expr()?;
                    self.expect(Token::RParen)?;
                    let args = vec![Expr::Literal(Value::Text(field)), time];
                    Ok(Expr::Function { name: "date_part".into(), args, distinct: false, star: false, filter: None })
                }
                // MySQL's VALUES(column) in ON DUPLICATE KEY UPDATE.
                "VALUES" if *self.peek_nth(1) == Token::LParen => {
                    self.advance();
                    self.advance();
                    let column = self.column()?;
                    self.expect(Token::RParen)?;
                    Ok(Expr::Function {
                        name: "values".into(),
                        args: vec![column],
                        distinct: false,
                        star: false,
                        filter: None,
                    })
                }
                _ if *self.peek_nth(1) == Token::LParen && !is_reserved(&word) => self.function(),
                _ => self.column(),
            },
            Token::QuotedIdent(_) => self.column(),
            _ => Err(self.unexpected("an expression")),
        }
    }

    fn number(&self, text: &str) -> Result<Expr> {
        let value = if text.contains(['.', 'e', 'E']) {
            Value::Real(text.parse().map_err(|_| self.error(format!("invalid number {text}")))?)
        } else {
            Value::Integer(text.parse().map_err(|_| self.error(format!("integer {text} is out of range")))?)
        };
        Ok(Expr::Literal(value))
    }

    fn column(&mut self) -> Result<Expr> {
        let first = self.ident()?;
        if self.eat(&Token::Dot) {
            return Ok(Expr::Column { table: Some(first), name: self.ident()? });
        }
        Ok(Expr::Column { table: None, name: first })
    }

    fn function(&mut self) -> Result<Expr> {
        let name = self.ident()?;
        self.expect(Token::LParen)?;
        let call = |name: &str, args: Vec<Expr>| Expr::Function {
            name: name.into(),
            args,
            distinct: false,
            star: false,
            filter: None,
        };
        // Standard SQL's keyword forms: POSITION(sub IN s),
        // SUBSTRING(s FROM start FOR length), TRIM([LEADING | TRAILING | BOTH] [chars] FROM s).
        let mut first = None;
        match name.to_ascii_lowercase().as_str() {
            "position" => {
                let sub = self.expr_bp(P_CMP + 1)?;
                self.expect_kw("IN")?;
                let s = self.expr()?;
                self.expect(Token::RParen)?;
                return Ok(call("strpos", vec![s, sub]));
            }
            "substring" | "substr" if *self.peek() != Token::RParen => {
                let s = self.expr()?;
                if self.is_kw("FROM") || self.is_kw("FOR") {
                    let start = if self.eat_kw("FROM") { self.expr()? } else { Expr::Literal(Value::Integer(1)) };
                    let mut args = vec![s, start];
                    if self.eat_kw("FOR") {
                        args.push(self.expr()?);
                    }
                    self.expect(Token::RParen)?;
                    return Ok(call("substr", args));
                }
                first = Some(s);
            }
            "trim" if *self.peek() != Token::RParen => {
                let side = ["LEADING", "TRAILING", "BOTH"].into_iter().find(|kw| self.eat_kw(kw));
                let function = match side {
                    Some("LEADING") => "ltrim",
                    Some("TRAILING") => "rtrim",
                    _ => "trim",
                };
                let (chars, s) = if side.is_some() && self.eat_kw("FROM") {
                    (None, self.expr()?)
                } else {
                    let e = self.expr()?;
                    if self.eat_kw("FROM") {
                        (Some(e), self.expr()?)
                    } else if side.is_some() {
                        return Err(self.unexpected("FROM"));
                    } else {
                        first = Some(e);
                        (None, Expr::Literal(Value::Null))
                    }
                };
                if first.is_none() {
                    self.expect(Token::RParen)?;
                    return Ok(call(function, std::iter::once(s).chain(chars).collect()));
                }
            }
            _ => {}
        }
        let (args, distinct, star) = if first.is_none() && self.eat(&Token::Star) {
            (Vec::new(), false, true)
        } else {
            let distinct = first.is_none() && self.eat_kw("DISTINCT");
            let mut args: Vec<Expr> = first.into_iter().collect();
            if args.is_empty() {
                if *self.peek() != Token::RParen {
                    args = self.expr_list()?;
                }
            } else {
                while self.eat(&Token::Comma) {
                    args.push(self.expr()?);
                }
            }
            (args, distinct, false)
        };
        self.expect(Token::RParen)?;
        let filter = if self.is_kw("FILTER") && *self.peek_nth(1) == Token::LParen {
            self.advance();
            self.advance();
            self.expect_kw("WHERE")?;
            let filter = self.expr()?;
            self.expect(Token::RParen)?;
            Some(Box::new(filter))
        } else {
            None
        };
        if self.eat_kw("OVER") {
            if distinct {
                return Err(self.error("DISTINCT is not supported in window functions"));
            }
            let over = if *self.peek() == Token::LParen {
                self.window_spec()?
            } else {
                WindowSpec { base: Some(self.ident()?), ..WindowSpec::default() }
            };
            return Ok(Expr::Window { name, args, star, filter, over: Box::new(over) });
        }
        Ok(Expr::Function { name, args, distinct, star, filter })
    }

    /// `expr [ASC | DESC], ...` after ORDER BY.
    fn order_items(&mut self) -> Result<Vec<OrderBy>> {
        let mut items = Vec::new();
        loop {
            let expr = self.expr()?;
            let desc = self.eat_kw("DESC");
            if !desc {
                self.eat_kw("ASC");
            }
            let nulls_first = if self.eat_kw("NULLS") {
                if self.eat_kw("FIRST") {
                    Some(true)
                } else {
                    self.expect_kw("LAST")?;
                    Some(false)
                }
            } else {
                None
            };
            items.push(OrderBy { expr, desc, nulls_first });
            if !self.eat(&Token::Comma) {
                return Ok(items);
            }
        }
    }

    /// `([name] [PARTITION BY ...] [ORDER BY ...] [frame])`
    fn window_spec(&mut self) -> Result<WindowSpec> {
        self.expect(Token::LParen)?;
        let mut spec = WindowSpec::default();
        if let Token::Word(w) = self.peek()
            && !["PARTITION", "ORDER", "ROWS", "RANGE"].iter().any(|k| w.eq_ignore_ascii_case(k))
        {
            spec.base = Some(self.ident()?);
        }
        if self.eat_kw("PARTITION") {
            self.expect_kw("BY")?;
            spec.partition_by = self.expr_list()?;
        }
        if self.eat_kw("ORDER") {
            self.expect_kw("BY")?;
            spec.order_by = self.order_items()?;
        }
        let rows = self.is_kw("ROWS");
        if rows || self.is_kw("RANGE") {
            self.advance();
            let (start, end) = if self.eat_kw("BETWEEN") {
                let start = self.frame_bound()?;
                self.expect_kw("AND")?;
                (start, self.frame_bound()?)
            } else {
                (self.frame_bound()?, FrameBound::CurrentRow)
            };
            if start == FrameBound::UnboundedFollowing || end == FrameBound::UnboundedPreceding || start > end {
                return Err(self.error("the window frame ends before it starts"));
            }
            if !rows
                && (matches!(start, FrameBound::Preceding(_) | FrameBound::Following(_))
                    || matches!(end, FrameBound::Preceding(_) | FrameBound::Following(_)))
            {
                return Err(self.error("RANGE with an offset is not supported; use ROWS"));
            }
            spec.frame = Some(Frame { rows, start, end });
        }
        self.expect(Token::RParen)?;
        Ok(spec)
    }

    fn frame_bound(&mut self) -> Result<FrameBound> {
        if self.eat_kw("UNBOUNDED") {
            return if self.eat_kw("PRECEDING") {
                Ok(FrameBound::UnboundedPreceding)
            } else {
                self.expect_kw("FOLLOWING")?;
                Ok(FrameBound::UnboundedFollowing)
            };
        }
        if self.eat_kw("CURRENT") {
            self.expect_kw("ROW")?;
            return Ok(FrameBound::CurrentRow);
        }
        let Token::Number(n) = self.peek().clone() else { return Err(self.unexpected("a frame bound")) };
        let n: u64 = n.parse().map_err(|_| self.error(format!("frame offset must be a whole number, not {n}")))?;
        self.advance();
        if self.eat_kw("PRECEDING") {
            Ok(FrameBound::Preceding(n))
        } else {
            self.expect_kw("FOLLOWING")?;
            Ok(FrameBound::Following(n))
        }
    }

    fn case(&mut self) -> Result<Expr> {
        self.expect_kw("CASE")?;
        let operand = if self.is_kw("WHEN") { None } else { Some(Box::new(self.expr()?)) };
        let mut branches = Vec::new();
        while self.eat_kw("WHEN") {
            let when = self.expr()?;
            self.expect_kw("THEN")?;
            branches.push((when, self.expr()?));
        }
        if branches.is_empty() {
            return Err(self.unexpected("WHEN"));
        }
        let otherwise = if self.eat_kw("ELSE") { Some(Box::new(self.expr()?)) } else { None };
        self.expect_kw("END")?;
        Ok(Expr::Case { operand, branches, otherwise })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expr(sql: &str) -> String {
        match parse_statement(&format!("SELECT {sql}")).unwrap().0 {
            Statement::Select(s) => match &s.items[0] {
                SelectItem::Expr { expr, .. } => expr.to_string(),
                _ => unreachable!(),
            },
            _ => unreachable!(),
        }
    }

    #[test]
    fn operator_precedence() {
        assert_eq!(expr("1 + 2 * 3"), "1 + (2 * 3)");
        assert_eq!(expr("a = 1 OR b = 2 AND NOT c"), "(a = 1) OR ((b = 2) AND NOT c)");
        assert_eq!(expr("x NOT BETWEEN 1 AND 2 AND y IS NOT NULL"), "(x NOT BETWEEN 1 AND 2) AND y IS NOT NULL");
        assert_eq!(expr("-a * b"), "-a * b");
        assert_eq!(expr("a || 'x' LIKE 'y%'"), "(a || 'x') LIKE 'y%'");
        assert_eq!(expr("-9223372036854775808"), "-9223372036854775808");
    }

    #[test]
    fn statements() {
        let (stmt, params) = parse_statement(
            "SELECT DISTINCT u.name AS n, count(*) FROM users u LEFT JOIN orders o ON o.user_id = u.id \
             WHERE u.age > ? AND o.total < $3 GROUP BY u.name HAVING count(*) > 1 ORDER BY 2 DESC LIMIT 10 OFFSET 5",
        )
        .unwrap();
        assert_eq!(params, 3);
        let Statement::Select(s) = stmt else { panic!() };
        assert!(s.distinct);
        assert_eq!(s.from.as_ref().unwrap().joins[0].kind, JoinKind::Left);
        assert!(s.order_by[0].desc);

        let (stmt, _) = parse_statement(
            "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY, name VARCHAR(20) NOT NULL UNIQUE, \
             score DOUBLE PRECISION DEFAULT 0.5, UNIQUE (name, score))",
        )
        .unwrap();
        let Statement::CreateTable(ct) = stmt else { panic!() };
        assert!(ct.if_not_exists && ct.columns[0].primary_key && ct.columns[1].unique);
        assert_eq!(ct.columns[2].ty, DataType::Real);
        assert_eq!(ct.unique, vec![vec!["name".to_string(), "score".to_string()]]);

        assert_eq!(parse_statements("BEGIN; COMMIT; ; ROLLBACK").unwrap().len(), 3);
        assert_eq!(parse_statement("LISTEN Orders").unwrap().0, Statement::Listen("orders".into()));
        assert_eq!(parse_statement("UNLISTEN *").unwrap().0, Statement::Unlisten(None));
        let (notify, params) = parse_statement("NOTIFY orders, $1").unwrap();
        assert_eq!(
            (notify, params),
            (Statement::Notify { channel: "orders".into(), payload: Some(Expr::Param(0)) }, 1)
        );
    }

    #[test]
    fn syntax_errors_point_at_the_problem() {
        let err = parse_statement("SELECT name FROM WHERE x = 1").unwrap_err().to_string();
        assert!(err.contains("column 18") && err.contains("expected an identifier, found WHERE"), "{err}");
        assert!(parse_statement("SELECT (1 + 2").unwrap_err().to_string().contains("expected ')'"));
        assert!(parse_statement("SELECT * FROM t WHERE id IN (SELECT 1").is_err());
        assert!(parse_statement("SELECT 1 UNION").is_err());
    }

    #[test]
    fn compound_queries_group_intersect_first() {
        let Statement::Select(q) = parse_statement("SELECT 1 UNION SELECT 2 INTERSECT SELECT 3 ORDER BY 1").unwrap().0
        else {
            panic!()
        };
        // a UNION (b INTERSECT c), ordered as a whole.
        assert_eq!(q.compound.len(), 1);
        assert_eq!(q.order_by.len(), 1);
        let (op, all, right) = &q.compound[0];
        assert_eq!((*op, *all), (SetOp::Union, false));
        let inner = right.from.as_ref().unwrap().base.subquery.as_ref().unwrap();
        assert_eq!(inner.compound[0].0, SetOp::Intersect);
    }
}
