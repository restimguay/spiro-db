//! A small JSON reader and writer, for the network protocol.

use std::fmt::{self, Write as _};

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Array(Vec<Json>),
    /// Keys stay in the order they were written.
    Object(Vec<(String, Json)>),
}

impl Json {
    pub fn parse(text: &str) -> Result<Json, String> {
        let mut p = Parser { bytes: text.as_bytes(), pos: 0 };
        let value = p.value(0)?;
        p.skip_space();
        if p.pos != p.bytes.len() {
            return Err(format!("unexpected text after JSON value at byte {}", p.pos));
        }
        Ok(value)
    }

    pub fn object(fields: Vec<(&str, Json)>) -> Json {
        Json::Object(fields.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Json::Int(i) => u64::try_from(*i).ok(),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(items) => Some(items),
            _ => None,
        }
    }
}

impl fmt::Display for Json {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Json::Null => f.write_str("null"),
            Json::Bool(b) => write!(f, "{b}"),
            Json::Int(i) => write!(f, "{i}"),
            // Keep a decimal point on whole numbers, so they read back as floats.
            Json::Float(x) if x.is_finite() && x.fract() == 0.0 && x.abs() < 1e16 => write!(f, "{x:.1}"),
            Json::Float(x) if x.is_finite() => write!(f, "{x}"),
            Json::Float(_) => f.write_str("null"), // JSON has no NaN or infinity
            Json::Str(s) => write_string(f, s),
            Json::Array(items) => {
                f.write_char('[')?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_char(',')?;
                    }
                    write!(f, "{item}")?;
                }
                f.write_char(']')
            }
            Json::Object(fields) => {
                f.write_char('{')?;
                for (i, (k, v)) in fields.iter().enumerate() {
                    if i > 0 {
                        f.write_char(',')?;
                    }
                    write_string(f, k)?;
                    write!(f, ":{v}")?;
                }
                f.write_char('}')
            }
        }
    }
}

fn write_string(f: &mut fmt::Formatter<'_>, s: &str) -> fmt::Result {
    f.write_char('"')?;
    for c in s.chars() {
        match c {
            '"' => f.write_str("\\\"")?,
            '\\' => f.write_str("\\\\")?,
            '\n' => f.write_str("\\n")?,
            '\r' => f.write_str("\\r")?,
            '\t' => f.write_str("\\t")?,
            c if (c as u32) < 0x20 => write!(f, "\\u{:04x}", c as u32)?,
            c => f.write_char(c)?,
        }
    }
    f.write_char('"')
}

/// Deeper nesting than this is refused rather than risking the stack.
const MAX_DEPTH: usize = 128;

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn skip_space(&mut self) {
        while self.bytes.get(self.pos).is_some_and(|b| b.is_ascii_whitespace()) {
            self.pos += 1;
        }
    }

    fn error(&self, what: &str) -> String {
        format!("invalid JSON at byte {}: {what}", self.pos)
    }

    fn expect(&mut self, literal: &str, value: Json) -> Result<Json, String> {
        if self.bytes[self.pos..].starts_with(literal.as_bytes()) {
            self.pos += literal.len();
            Ok(value)
        } else {
            Err(self.error("unknown literal"))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, String> {
        if depth > MAX_DEPTH {
            return Err(self.error("nested too deeply"));
        }
        self.skip_space();
        match self.bytes.get(self.pos) {
            None => Err(self.error("unexpected end")),
            Some(b'n') => self.expect("null", Json::Null),
            Some(b't') => self.expect("true", Json::Bool(true)),
            Some(b'f') => self.expect("false", Json::Bool(false)),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b'[') => {
                self.pos += 1;
                let mut items = Vec::new();
                self.skip_space();
                if self.bytes.get(self.pos) == Some(&b']') {
                    self.pos += 1;
                    return Ok(Json::Array(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    self.skip_space();
                    match self.bytes.get(self.pos) {
                        Some(b',') => self.pos += 1,
                        Some(b']') => {
                            self.pos += 1;
                            return Ok(Json::Array(items));
                        }
                        _ => return Err(self.error("expected ',' or ']'")),
                    }
                }
            }
            Some(b'{') => {
                self.pos += 1;
                let mut fields = Vec::new();
                self.skip_space();
                if self.bytes.get(self.pos) == Some(&b'}') {
                    self.pos += 1;
                    return Ok(Json::Object(fields));
                }
                loop {
                    self.skip_space();
                    if self.bytes.get(self.pos) != Some(&b'"') {
                        return Err(self.error("expected a key"));
                    }
                    let key = self.string()?;
                    self.skip_space();
                    if self.bytes.get(self.pos) != Some(&b':') {
                        return Err(self.error("expected ':'"));
                    }
                    self.pos += 1;
                    fields.push((key, self.value(depth + 1)?));
                    self.skip_space();
                    match self.bytes.get(self.pos) {
                        Some(b',') => self.pos += 1,
                        Some(b'}') => {
                            self.pos += 1;
                            return Ok(Json::Object(fields));
                        }
                        _ => return Err(self.error("expected ',' or '}'")),
                    }
                }
            }
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.error("unexpected character")),
        }
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        let mut float = false;
        if self.bytes.get(self.pos) == Some(&b'-') {
            self.pos += 1;
        }
        while let Some(&b) = self.bytes.get(self.pos) {
            match b {
                b'0'..=b'9' => {}
                b'.' | b'e' | b'E' | b'+' | b'-' => float = true,
                _ => break,
            }
            self.pos += 1;
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos]).unwrap();
        if !float && let Ok(i) = text.parse::<i64>() {
            return Ok(Json::Int(i));
        }
        text.parse::<f64>().map(Json::Float).map_err(|_| self.error("bad number"))
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let digits = self.bytes.get(self.pos..self.pos + 4).ok_or_else(|| self.error("short \\u escape"))?;
        let value = std::str::from_utf8(digits)
            .ok()
            .and_then(|d| u32::from_str_radix(d, 16).ok())
            .ok_or_else(|| self.error("bad \\u escape"))?;
        self.pos += 4;
        Ok(value)
    }

    fn string(&mut self) -> Result<String, String> {
        self.pos += 1; // opening quote
        let mut out = String::new();
        loop {
            let start = self.pos;
            while let Some(&b) = self.bytes.get(self.pos) {
                if b == b'"' || b == b'\\' || b < 0x20 {
                    break;
                }
                self.pos += 1;
            }
            out.push_str(std::str::from_utf8(&self.bytes[start..self.pos]).map_err(|_| self.error("bad UTF-8"))?);
            match self.bytes.get(self.pos) {
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    let escape = *self.bytes.get(self.pos).ok_or_else(|| self.error("unfinished escape"))?;
                    self.pos += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let mut code = self.hex4()?;
                            // A surrogate pair spells one character above U+FFFF.
                            if (0xd800..0xdc00).contains(&code) && self.bytes[self.pos..].starts_with(b"\\u") {
                                self.pos += 2;
                                let low = self.hex4()?;
                                code = 0x10000 + ((code - 0xd800) << 10) + (low.wrapping_sub(0xdc00) & 0x3ff);
                            }
                            out.push(char::from_u32(code).ok_or_else(|| self.error("bad character code"))?);
                        }
                        _ => return Err(self.error("unknown escape")),
                    }
                }
                _ => return Err(self.error("unterminated string")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let text = r#"{"sql":"SELECT \"x\"\n","params":[1,-2.5,true,null,"é😀"],"nested":{"a":[]}}"#;
        let value = Json::parse(text).unwrap();
        assert_eq!(value.get("sql").and_then(Json::as_str), Some("SELECT \"x\"\n"));
        assert_eq!(value.get("params").and_then(Json::as_array).unwrap()[1], Json::Float(-2.5));
        assert_eq!(Json::parse(&value.to_string()).unwrap(), value);
        assert_eq!(Json::parse(r#""😀""#).unwrap(), Json::Str("😀".into()));
        assert_eq!(Json::Float(3.0).to_string(), "3.0");
        assert_eq!(Json::parse("3.0").unwrap(), Json::Float(3.0));
        assert_eq!(Json::parse("9223372036854775807").unwrap(), Json::Int(i64::MAX));
    }

    #[test]
    fn rejects_garbage() {
        for bad in ["", "{", "[1,]", "{\"a\" 1}", "tru", "\"abc", "1 2", &"[".repeat(200)] {
            assert!(Json::parse(bad).is_err(), "{bad}");
        }
    }
}
