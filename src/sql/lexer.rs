//! Splits SQL text into tokens.

use crate::error::{Error, Result};

#[derive(Clone, Debug, PartialEq)]
pub enum Token {
    /// A keyword or unquoted identifier, as written.
    Word(String),
    /// A "double-quoted" identifier: never a keyword, case preserved.
    QuotedIdent(String),
    Number(String),
    String(String),
    /// `X'0aff'`: bytes written in hexadecimal.
    Blob(Vec<u8>),
    /// `?` (positional, `None`) or `$N`.
    Param(Option<usize>),
    LParen,
    RParen,
    Comma,
    Semicolon,
    Dot,
    Star,
    Plus,
    Minus,
    Slash,
    Percent,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Concat,
    /// `->`: a JSON member or element, as JSON.
    Arrow,
    /// `->>`: a JSON member or element, as a value.
    LongArrow,
    /// `~`, `~*`, `!~` and `!~*`: regular expression matches, the starred
    /// ones ignoring case, the `!` ones negated.
    Match {
        negated: bool,
        case_insensitive: bool,
    },
    Eof,
}

impl Token {
    pub fn describe(&self) -> String {
        match self {
            Token::Word(w) => w.clone(),
            Token::QuotedIdent(w) => format!("\"{w}\""),
            Token::Number(n) => n.clone(),
            Token::String(s) => format!("'{s}'"),
            Token::Blob(b) => format!("X'{}'", b.iter().map(|b| format!("{b:02x}")).collect::<String>()),
            Token::Param(None) => "?".into(),
            Token::Param(Some(n)) => format!("${n}"),
            Token::Eof => "end of input".into(),
            punct => punct.symbol().into(),
        }
    }

    fn symbol(&self) -> &'static str {
        match self {
            Token::LParen => "(",
            Token::RParen => ")",
            Token::Comma => ",",
            Token::Semicolon => ";",
            Token::Dot => ".",
            Token::Star => "*",
            Token::Plus => "+",
            Token::Minus => "-",
            Token::Slash => "/",
            Token::Percent => "%",
            Token::Eq => "=",
            Token::NotEq => "!=",
            Token::Lt => "<",
            Token::LtEq => "<=",
            Token::Gt => ">",
            Token::GtEq => ">=",
            Token::Concat => "||",
            Token::Arrow => "->",
            Token::LongArrow => "->>",
            Token::Match { negated: false, case_insensitive: false } => "~",
            Token::Match { negated: false, case_insensitive: true } => "~*",
            Token::Match { negated: true, case_insensitive: false } => "!~",
            Token::Match { negated: true, case_insensitive: true } => "!~*",
            _ => "",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Located {
    pub token: Token,
    pub line: usize,
    pub column: usize,
    /// Where the token starts in the SQL, in characters.
    pub offset: usize,
}

pub fn syntax_error(line: usize, column: usize, msg: impl std::fmt::Display) -> Error {
    Error::Syntax(format!("syntax error at line {line}, column {column}: {msg}"))
}

struct Lexer {
    chars: Vec<char>,
    pos: usize,
    line: usize,
    column: usize,
}

impl Lexer {
    fn peek(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.pos + ahead).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek(0)?;
        self.pos += 1;
        if c == '\n' {
            self.line += 1;
            self.column = 1;
        } else {
            self.column += 1;
        }
        Some(c)
    }

    fn take_while(&mut self, f: impl Fn(char) -> bool) -> String {
        let mut s = String::new();
        while let Some(c) = self.peek(0).filter(|&c| f(c)) {
            s.push(c);
            self.bump();
        }
        s
    }

    /// Reads a string delimited by `quote`, where a doubled quote stands for itself.
    fn quoted(&mut self, quote: char) -> Option<String> {
        self.bump();
        let mut s = String::new();
        loop {
            match self.bump()? {
                c if c == quote && self.peek(0) == Some(quote) => {
                    self.bump();
                    s.push(quote);
                }
                c if c == quote => return Some(s),
                c => s.push(c),
            }
        }
    }
}

pub fn tokenize(sql: &str) -> Result<Vec<Located>> {
    let mut lx = Lexer { chars: sql.chars().collect(), pos: 0, line: 1, column: 1 };
    let mut tokens = Vec::new();
    while let Some(c) = lx.peek(0) {
        let (line, column, offset) = (lx.line, lx.column, lx.pos);
        let token = match c {
            c if c.is_whitespace() => {
                lx.bump();
                continue;
            }
            '-' if lx.peek(1) == Some('-') => {
                lx.take_while(|c| c != '\n');
                continue;
            }
            '/' if lx.peek(1) == Some('*') => {
                lx.bump();
                lx.bump();
                while !(lx.peek(0) == Some('*') && lx.peek(1) == Some('/')) {
                    lx.bump().ok_or_else(|| syntax_error(line, column, "unterminated comment"))?;
                }
                lx.bump();
                lx.bump();
                continue;
            }
            'x' | 'X' if lx.peek(1) == Some('\'') => {
                lx.bump();
                let hex = lx.quoted('\'').ok_or_else(|| syntax_error(line, column, "unterminated blob"))?;
                let bad = || syntax_error(line, column, format!("X'{hex}' is not an even number of hex digits"));
                if hex.len() % 2 != 0 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(bad());
                }
                let bytes = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16));
                Token::Blob(bytes.collect::<std::result::Result<_, _>>().map_err(|_| bad())?)
            }
            c if c.is_alphabetic() || c == '_' => Token::Word(lx.take_while(|c| c.is_alphanumeric() || c == '_')),
            c if c.is_ascii_digit() || (c == '.' && lx.peek(1).is_some_and(|d| d.is_ascii_digit())) => {
                let mut n = lx.take_while(|c| c.is_ascii_digit());
                if lx.peek(0) == Some('.') {
                    lx.bump();
                    n.push('.');
                    n.push_str(&lx.take_while(|c| c.is_ascii_digit()));
                }
                if matches!(lx.peek(0), Some('e' | 'E')) {
                    let sign = matches!(lx.peek(1), Some('+' | '-'));
                    if lx.peek(1 + usize::from(sign)).is_some_and(|d| d.is_ascii_digit()) {
                        n.push(lx.bump().unwrap());
                        if sign {
                            n.push(lx.bump().unwrap());
                        }
                        n.push_str(&lx.take_while(|c| c.is_ascii_digit()));
                    }
                }
                Token::Number(n)
            }
            '\'' => Token::String(lx.quoted('\'').ok_or_else(|| syntax_error(line, column, "unterminated string"))?),
            '"' => {
                Token::QuotedIdent(lx.quoted('"').ok_or_else(|| syntax_error(line, column, "unterminated identifier"))?)
            }
            '?' => {
                lx.bump();
                Token::Param(None)
            }
            '$' => {
                lx.bump();
                let digits = lx.take_while(|c| c.is_ascii_digit());
                match digits.parse::<usize>() {
                    Ok(n) if n >= 1 => Token::Param(Some(n)),
                    _ => return Err(syntax_error(line, column, "expected a parameter number after $")),
                }
            }
            _ => {
                lx.bump();
                let next = lx.peek(0);
                let two = |lx: &mut Lexer, t: Token| {
                    lx.bump();
                    t
                };
                match (c, next) {
                    ('(', _) => Token::LParen,
                    (')', _) => Token::RParen,
                    (',', _) => Token::Comma,
                    (';', _) => Token::Semicolon,
                    ('.', _) => Token::Dot,
                    ('*', _) => Token::Star,
                    ('+', _) => Token::Plus,
                    ('-', Some('>')) => {
                        lx.bump();
                        if lx.peek(0) == Some('>') {
                            lx.bump();
                            Token::LongArrow
                        } else {
                            Token::Arrow
                        }
                    }
                    ('-', _) => Token::Minus,
                    ('/', _) => Token::Slash,
                    ('%', _) => Token::Percent,
                    ('=', Some('=')) => two(&mut lx, Token::Eq),
                    ('=', _) => Token::Eq,
                    ('!', Some('=')) => two(&mut lx, Token::NotEq),
                    ('~', _) | ('!', Some('~')) => {
                        let negated = c == '!';
                        if negated {
                            lx.bump();
                        }
                        let case_insensitive = lx.peek(0) == Some('*');
                        if case_insensitive {
                            lx.bump();
                        }
                        Token::Match { negated, case_insensitive }
                    }
                    ('<', Some('=')) => two(&mut lx, Token::LtEq),
                    ('<', Some('>')) => two(&mut lx, Token::NotEq),
                    ('<', _) => Token::Lt,
                    ('>', Some('=')) => two(&mut lx, Token::GtEq),
                    ('>', _) => Token::Gt,
                    ('|', Some('|')) => two(&mut lx, Token::Concat),
                    _ => return Err(syntax_error(line, column, format!("unexpected character '{c}'"))),
                }
            }
        };
        tokens.push(Located { token, line, column, offset });
    }
    tokens.push(Located { token: Token::Eof, line: lx.line, column: lx.column, offset: lx.pos });
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(sql: &str) -> Vec<Token> {
        tokenize(sql).unwrap().into_iter().map(|t| t.token).collect()
    }

    #[test]
    fn basic_tokens() {
        use Token::*;
        assert_eq!(
            tokens("SELECT a, \"B c\" FROM t WHERE x >= 1.5e3 AND y <> 'it''s' -- comment\n;"),
            vec![
                Word("SELECT".into()),
                Word("a".into()),
                Comma,
                QuotedIdent("B c".into()),
                Word("FROM".into()),
                Word("t".into()),
                Word("WHERE".into()),
                Word("x".into()),
                GtEq,
                Number("1.5e3".into()),
                Word("AND".into()),
                Word("y".into()),
                NotEq,
                String("it's".into()),
                Semicolon,
                Eof
            ]
        );
        assert_eq!(tokens("? $2 /* x */ ||"), vec![Param(None), Param(Some(2)), Concat, Eof]);
    }

    #[test]
    fn errors_have_positions() {
        let err = tokenize("SELECT\n  'oops").unwrap_err().to_string();
        assert!(err.contains("line 2, column 3"), "{err}");
        assert!(tokenize("SELECT #").is_err());
    }
}
