//! SQL values and types.

use std::cmp::Ordering;
use std::fmt;

use crate::error::{Error, Result};

pub(crate) fn sql_error(msg: impl Into<String>) -> Error {
    Error::Sql(msg.into())
}

/// A column type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataType {
    Integer,
    Real,
    Text,
    Boolean,
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DataType::Integer => "INTEGER",
            DataType::Real => "REAL",
            DataType::Text => "TEXT",
            DataType::Boolean => "BOOLEAN",
        })
    }
}

/// A SQL value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Boolean(bool),
}

impl Value {
    pub fn data_type(&self) -> Option<DataType> {
        match self {
            Value::Null => None,
            Value::Integer(_) => Some(DataType::Integer),
            Value::Real(_) => Some(DataType::Real),
            Value::Text(_) => Some(DataType::Text),
            Value::Boolean(_) => Some(DataType::Boolean),
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Integer(i) => Some(*i),
            _ => None,
        }
    }

    /// The value as a float, for integers and reals.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Integer(i) => Some(*i as f64),
            Value::Real(r) => Some(*r),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Boolean(b) => Some(*b),
            _ => None,
        }
    }

    pub(crate) fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "NULL",
            Value::Integer(_) => "INTEGER",
            Value::Real(_) => "REAL",
            Value::Text(_) => "TEXT",
            Value::Boolean(_) => "BOOLEAN",
        }
    }

    /// The value as it would appear in SQL source: text is quoted.
    pub(crate) fn literal(&self) -> String {
        match self {
            Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
            v => v.to_string(),
        }
    }

    /// SQL comparison. `None` when either side is NULL, i.e. the result is unknown.
    pub(crate) fn compare(&self, other: &Value) -> Result<Option<Ordering>> {
        use Value::*;
        Ok(Some(match (self, other) {
            (Null, _) | (_, Null) => return Ok(None),
            (Integer(a), Integer(b)) => a.cmp(b),
            (Integer(_) | Real(_), Integer(_) | Real(_)) => cmp_f64(self.as_f64().unwrap(), other.as_f64().unwrap()),
            (Text(a), Text(b)) => a.cmp(b),
            (Boolean(a), Boolean(b)) => a.cmp(b),
            _ => return Err(sql_error(format!("cannot compare {} with {}", self.type_name(), other.type_name()))),
        }))
    }

    /// A total order for sorting, grouping and DISTINCT: NULL first, then by
    /// value, with values of incomparable types ordered by type.
    pub(crate) fn total_cmp(&self, other: &Value) -> Ordering {
        match self.compare(other) {
            Ok(Some(ordering)) => ordering,
            _ => self.type_rank().cmp(&other.type_rank()),
        }
    }

    fn type_rank(&self) -> u8 {
        match self {
            Value::Null => 0,
            Value::Boolean(_) => 1,
            Value::Integer(_) | Value::Real(_) => 2,
            Value::Text(_) => 3,
        }
    }

    /// Interprets the value as a condition: `None` means unknown (NULL).
    pub(crate) fn truth(&self) -> Result<Option<bool>> {
        match self {
            Value::Null => Ok(None),
            Value::Boolean(b) => Ok(Some(*b)),
            Value::Integer(i) => Ok(Some(*i != 0)),
            v => Err(sql_error(format!("expected a BOOLEAN condition, got {}", v.type_name()))),
        }
    }

    /// Converts the value for storage in a column of type `ty`, allowing only
    /// lossless conversions. Hands the value back if it doesn't fit.
    pub(crate) fn assign_to(self, ty: DataType) -> Result<Value, Value> {
        match (self, ty) {
            (Value::Null, _) => Ok(Value::Null),
            (Value::Integer(i), DataType::Real) => Ok(Value::Real(i as f64)),
            (Value::Real(r), DataType::Integer) if r.fract() == 0.0 && r >= i64::MIN as f64 && r < i64::MAX as f64 => {
                Ok(Value::Integer(r as i64))
            }
            (v, ty) if v.data_type() == Some(ty) => Ok(v),
            (v, _) => Err(v),
        }
    }

    /// An explicit `CAST(value AS ty)`.
    pub(crate) fn cast(&self, ty: DataType) -> Result<Value> {
        let fail = || sql_error(format!("cannot cast {} to {ty}", self.literal()));
        Ok(match (self, ty) {
            (Value::Null, _) => Value::Null,
            (Value::Integer(_), DataType::Integer) => self.clone(),
            (Value::Real(r), DataType::Integer) => {
                if !r.is_finite() || *r < i64::MIN as f64 || *r >= i64::MAX as f64 {
                    return Err(fail());
                }
                Value::Integer(r.trunc() as i64)
            }
            (Value::Text(s), DataType::Integer) => match s.trim().parse::<i64>() {
                Ok(i) => Value::Integer(i),
                Err(_) => Value::Real(s.trim().parse::<f64>().map_err(|_| fail())?).cast(ty)?,
            },
            (Value::Boolean(b), DataType::Integer) => Value::Integer(i64::from(*b)),
            (Value::Integer(i), DataType::Real) => Value::Real(*i as f64),
            (Value::Real(_), DataType::Real) => self.clone(),
            (Value::Text(s), DataType::Real) => Value::Real(s.trim().parse().map_err(|_| fail())?),
            (Value::Boolean(b), DataType::Real) => Value::Real(if *b { 1.0 } else { 0.0 }),
            (v, DataType::Text) => Value::Text(v.to_string()),
            (Value::Boolean(_), DataType::Boolean) => self.clone(),
            (Value::Integer(i), DataType::Boolean) => Value::Boolean(*i != 0),
            (Value::Real(r), DataType::Boolean) => Value::Boolean(*r != 0.0),
            (Value::Text(s), DataType::Boolean) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "t" | "yes" | "y" | "on" | "1" => Value::Boolean(true),
                "false" | "f" | "no" | "n" | "off" | "0" => Value::Boolean(false),
                _ => return Err(fail()),
            },
        })
    }

    /// The value as JSON. Integers beyond ±2^53 lose precision when JavaScript
    /// parses them, as with any JSON number.
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        self.write_json(&mut out);
        out
    }

    pub(crate) fn write_json(&self, out: &mut String) {
        match self {
            Value::Null => out.push_str("null"),
            Value::Integer(i) => out.push_str(&i.to_string()),
            Value::Real(r) if r.is_finite() => out.push_str(&r.to_string()),
            Value::Real(_) => out.push_str("null"), // JSON has no NaN or infinity
            Value::Text(s) => write_json_string(out, s),
            Value::Boolean(b) => out.push_str(if *b { "true" } else { "false" }),
        }
    }
}

pub(crate) fn write_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

pub(crate) fn cmp_f64(a: f64, b: f64) -> Ordering {
    // NaN sorts above every other number.
    a.partial_cmp(&b).unwrap_or_else(|| a.is_nan().cmp(&b.is_nan()))
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("NULL"),
            Value::Integer(i) => write!(f, "{i}"),
            // Keep a decimal point on whole reals so they read as reals.
            Value::Real(r) if r.is_finite() && r.fract() == 0.0 && r.abs() < 1e16 => write!(f, "{r:.1}"),
            Value::Real(r) => write!(f, "{r}"),
            Value::Text(s) => f.write_str(s),
            Value::Boolean(b) => write!(f, "{b}"),
        }
    }
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Integer(v)
    }
}

impl From<i32> for Value {
    fn from(v: i32) -> Self {
        Value::Integer(v.into())
    }
}

impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Real(v)
    }
}

impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Boolean(v)
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Text(v.to_string())
    }
}

impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Text(v)
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Self {
        v.map_or(Value::Null, Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comparison_follows_sql_rules() {
        assert_eq!(Value::Integer(1).compare(&Value::Real(1.5)).unwrap(), Some(Ordering::Less));
        assert_eq!(Value::Null.compare(&Value::Integer(1)).unwrap(), None);
        assert!(Value::Text("a".into()).compare(&Value::Integer(1)).is_err());
        assert_eq!(Value::Null.total_cmp(&Value::Integer(-5)), Ordering::Less);
    }

    #[test]
    fn assignment_is_lossless_only() {
        assert_eq!(Value::Integer(3).assign_to(DataType::Real), Ok(Value::Real(3.0)));
        assert_eq!(Value::Real(3.0).assign_to(DataType::Integer), Ok(Value::Integer(3)));
        assert_eq!(Value::Real(3.5).assign_to(DataType::Integer), Err(Value::Real(3.5)));
        assert!(Value::Text("3".into()).assign_to(DataType::Integer).is_err());
    }

    #[test]
    fn casts() {
        assert_eq!(Value::Text(" 42 ".into()).cast(DataType::Integer).unwrap(), Value::Integer(42));
        assert_eq!(Value::Real(-2.7).cast(DataType::Integer).unwrap(), Value::Integer(-2));
        assert_eq!(Value::Integer(7).cast(DataType::Text).unwrap(), Value::Text("7".into()));
        assert!(Value::Text("abc".into()).cast(DataType::Integer).is_err());
    }

    #[test]
    fn json() {
        assert_eq!(Value::Text("a\"b\n".into()).to_json(), r#""a\"b\n""#);
        assert_eq!(Value::Real(1.5).to_json(), "1.5");
        assert_eq!(Value::Null.to_json(), "null");
        assert_eq!(Value::Boolean(true).to_json(), "true");
    }
}
