//! JSON functions, over JSON kept as text (as SQLite does). Paths follow
//! SQLite: `$`, `$.key`, `$."odd key"`, `$[0]`, and `$[#-1]` for the last
//! element of an array.

use crate::error::Result;
use crate::json::Json;

use super::value::{Value, sql_error};

/// Marks text that holds JSON produced by a JSON function, so that a JSON
/// builder given it nests it as JSON rather than as a string. Only ever seen
/// by those builders; see `embedded`.
const EMBEDDED: char = '\u{1}';

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JsonFn {
    Json,
    Valid,
    Type,
    Extract,
    /// The `->` operator: the member or element, as JSON.
    Get,
    /// The `->>` operator: the member or element, as an SQL value.
    GetValue,
    Array,
    Object,
    ArrayLength,
    Set,
    Insert,
    Replace,
    Remove,
    /// Marks an argument of a builder as JSON; see `EMBEDDED`.
    Embed,
    /// `[key, value]` for `json_group_object`, which aggregates one value.
    Pair,
}

pub fn resolve(name: &str, args: usize) -> Option<(JsonFn, bool)> {
    use JsonFn::*;
    Some(match name {
        "json" => (Json, args == 1),
        "json_valid" => (Valid, args == 1),
        "json_type" => (Type, args == 1 || args == 2),
        "json_extract" => (Extract, args >= 2),
        "json_get" => (Get, args == 2),
        "json_get_value" => (GetValue, args == 2),
        "json_array" => (Array, true),
        "json_object" => (Object, args.is_multiple_of(2)),
        "json_array_length" => (ArrayLength, args == 1 || args == 2),
        "json_set" => (Set, args >= 3 && args % 2 == 1),
        "json_insert" => (Insert, args >= 3 && args % 2 == 1),
        "json_replace" => (Replace, args >= 3 && args % 2 == 1),
        "json_remove" => (Remove, args >= 2),
        _ => return None,
    })
}

/// Whether a function of this name returns JSON text, so a builder should
/// nest its result as JSON.
pub fn produces_json(name: &str) -> bool {
    matches!(
        name,
        "json"
            | "json_get"
            | "json_array"
            | "json_object"
            | "json_set"
            | "json_insert"
            | "json_replace"
            | "json_remove"
            | "json_group_array"
            | "json_agg"
            | "json_group_object"
            | "json_object_agg"
    )
}

/// Whether a function of this name builds JSON from SQL values.
pub fn builds_json(name: &str) -> bool {
    matches!(
        name,
        "json_array"
            | "json_object"
            | "json_set"
            | "json_insert"
            | "json_replace"
            | "json_group_array"
            | "json_agg"
            | "json_group_object"
            | "json_object_agg"
    )
}

impl JsonFn {
    /// Whether NULL arguments mean something to the function, rather than
    /// making the result NULL.
    pub fn takes_nulls(self) -> bool {
        use JsonFn::*;
        matches!(self, Array | Object | Set | Insert | Replace | Embed | Pair | Valid)
    }

    pub fn call(self, args: Vec<Value>) -> Result<Value> {
        use JsonFn::*;
        Ok(match self {
            Json => Value::Text(parse(&args[0])?.to_string()),
            Valid => match &args[0] {
                Value::Null => Value::Null,
                Value::Text(s) => Value::Boolean(crate::json::Json::parse(s).is_ok()),
                _ => Value::Boolean(false),
            },
            Type => {
                let json = parse(&args[0])?;
                let found = match args.get(1) {
                    Some(path) => lookup(&json, &steps(path)?),
                    None => Some(&json),
                };
                found.map_or(Value::Null, |j| Value::Text(type_name(j).into()))
            }
            Extract => {
                let json = parse(&args[0])?;
                if args.len() == 2 {
                    return Ok(lookup(&json, &steps(&args[1])?).map_or(Value::Null, to_value));
                }
                let mut found = Vec::new();
                for path in &args[1..] {
                    found.push(lookup(&json, &steps(path)?).cloned().unwrap_or(crate::json::Json::Null));
                }
                Value::Text(crate::json::Json::Array(found).to_string())
            }
            Get | GetValue => {
                let json = parse(&args[0])?;
                let step = match &args[1] {
                    Value::Integer(i) => Step::Index(*i),
                    Value::Text(s) if s.starts_with('$') => {
                        return Ok(lookup(&json, &steps(&args[1])?).map_or(Value::Null, |j| shown(self, j)));
                    }
                    v => Step::Key(v.to_string()),
                };
                lookup(&json, &[step]).map_or(Value::Null, |j| shown(self, j))
            }
            Array => Value::Text(crate::json::Json::Array(args.iter().map(to_json).collect()).to_string()),
            Object => {
                let mut fields = Vec::new();
                for pair in args.chunks(2) {
                    let Value::Text(key) = &pair[0] else {
                        return Err(sql_error("json_object() keys must be text"));
                    };
                    fields.push((key.clone(), to_json(&pair[1])));
                }
                Value::Text(crate::json::Json::Object(fields).to_string())
            }
            ArrayLength => {
                let json = parse(&args[0])?;
                let found = match args.get(1) {
                    Some(path) => lookup(&json, &steps(path)?),
                    None => Some(&json),
                };
                match found {
                    Some(crate::json::Json::Array(items)) => Value::Integer(items.len() as i64),
                    Some(_) => Value::Integer(0),
                    None => Value::Null,
                }
            }
            Set | Insert | Replace => {
                if args[0].is_null() {
                    return Ok(Value::Null);
                }
                let mut json = parse(&args[0])?;
                for pair in args[1..].chunks(2) {
                    let path = steps(&pair[0])?;
                    edit(&mut json, &path, to_json(&pair[1]), self)?;
                }
                Value::Text(json.to_string())
            }
            Remove => {
                let mut json = parse(&args[0])?;
                for path in &args[1..] {
                    remove(&mut json, &steps(path)?)?;
                }
                Value::Text(json.to_string())
            }
            Embed => match &args[0] {
                Value::Text(s) => Value::Text(format!("{EMBEDDED}{s}")),
                v => v.clone(),
            },
            Pair => Value::Text(crate::json::Json::Array(vec![to_json(&args[0]), to_json(&args[1])]).to_string()),
        })
    }
}

/// A JSON argument, parsed.
fn parse(value: &Value) -> Result<Json> {
    let text = match value {
        Value::Text(s) => s.strip_prefix(EMBEDDED).unwrap_or(s),
        v => return Err(sql_error(format!("expected JSON text, got {}", v.type_name()))),
    };
    Json::parse(text).map_err(|e| sql_error(format!("malformed JSON: {e}")))
}

/// An SQL value as JSON: text is a string, unless a JSON function made it.
pub fn to_json(value: &Value) -> Json {
    match value {
        Value::Null => Json::Null,
        Value::Integer(i) => Json::Int(*i),
        Value::Real(r) => Json::Float(*r),
        Value::Boolean(b) => Json::Bool(*b),
        Value::Decimal(d) => d.to_i64().map_or(Json::Float(d.to_f64()), Json::Int),
        Value::Text(s) => match s.strip_prefix(EMBEDDED) {
            Some(json) => Json::parse(json).unwrap_or_else(|_| Json::Str(json.to_string())),
            None => Json::Str(s.clone()),
        },
        Value::Blob(_) => Json::Str(value.to_string()),
    }
}

/// A JSON value as SQL: scalars as themselves, arrays and objects as text.
pub fn to_value(json: &Json) -> Value {
    match json {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Boolean(*b),
        Json::Int(i) => Value::Integer(*i),
        Json::Float(x) => Value::Real(*x),
        Json::Str(s) => Value::Text(s.clone()),
        Json::Array(_) | Json::Object(_) => Value::Text(json.to_string()),
    }
}

fn shown(f: JsonFn, json: &Json) -> Value {
    if f == JsonFn::Get { Value::Text(json.to_string()) } else { to_value(json) }
}

fn type_name(json: &Json) -> &'static str {
    match json {
        Json::Null => "null",
        Json::Bool(true) => "true",
        Json::Bool(false) => "false",
        Json::Int(_) => "integer",
        Json::Float(_) => "real",
        Json::Str(_) => "text",
        Json::Array(_) => "array",
        Json::Object(_) => "object",
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Step {
    Key(String),
    /// Negative counts from the end: -1 is the last element.
    Index(i64),
}

/// Parses a path such as `$.a[2]."b c"[#-1]`.
fn steps(path: &Value) -> Result<Vec<Step>> {
    let Value::Text(text) = path else { return Err(sql_error("a JSON path must be text")) };
    let bad = || sql_error(format!("bad JSON path: {text}"));
    let mut rest = text.strip_prefix('$').ok_or_else(bad)?;
    let mut steps = Vec::new();
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('.') {
            if let Some(quoted) = after.strip_prefix('"') {
                let end = quoted.find('"').ok_or_else(bad)?;
                steps.push(Step::Key(quoted[..end].to_string()));
                rest = &quoted[end + 1..];
            } else {
                let end = after.find(['.', '[']).unwrap_or(after.len());
                if end == 0 {
                    return Err(bad());
                }
                steps.push(Step::Key(after[..end].to_string()));
                rest = &after[end..];
            }
        } else if let Some(after) = rest.strip_prefix('[') {
            let end = after.find(']').ok_or_else(bad)?;
            let inside = after[..end].trim();
            let index = match inside.strip_prefix('#') {
                Some("") => i64::MAX, // one past the end, for appending
                Some(back) => back.trim().parse::<i64>().map_err(|_| bad())?,
                None => inside.parse::<i64>().ok().filter(|i| *i >= 0).ok_or_else(bad)?,
            };
            steps.push(Step::Index(index));
            rest = &after[end + 1..];
        } else {
            return Err(bad());
        }
    }
    Ok(steps)
}

/// The position an index names in an array of `len` elements.
fn position(index: i64, len: usize) -> Option<usize> {
    if index < 0 { len.checked_sub(index.unsigned_abs() as usize) } else { usize::try_from(index).ok() }
}

fn lookup<'a>(json: &'a Json, steps: &[Step]) -> Option<&'a Json> {
    let mut at = json;
    for step in steps {
        at = match (step, at) {
            (Step::Key(key), Json::Object(fields)) => &fields.iter().find(|(k, _)| k == key)?.1,
            (Step::Index(i), Json::Array(items)) => items.get(position(*i, items.len())?)?,
            _ => return None,
        };
    }
    Some(at)
}

/// json_set (create or overwrite), json_insert (create only) or
/// json_replace (overwrite only) of `value` at `path`.
fn edit(json: &mut Json, path: &[Step], value: Json, f: JsonFn) -> Result<()> {
    let Some((last, parents)) = path.split_last() else {
        if f != JsonFn::Insert {
            *json = value;
        }
        return Ok(());
    };
    let mut at = json;
    for step in parents {
        at = match (step, at) {
            (Step::Key(key), Json::Object(fields)) => match fields.iter().position(|(k, _)| k == key) {
                Some(i) => &mut fields[i].1,
                None => return Ok(()),
            },
            (Step::Index(i), Json::Array(items)) => match position(*i, items.len()).filter(|&p| p < items.len()) {
                Some(p) => &mut items[p],
                None => return Ok(()),
            },
            _ => return Ok(()),
        };
    }
    match (last, at) {
        (Step::Key(key), Json::Object(fields)) => match fields.iter().position(|(k, _)| k == key) {
            Some(i) if f != JsonFn::Insert => fields[i].1 = value,
            None if f != JsonFn::Replace => fields.push((key.clone(), value)),
            _ => {}
        },
        (Step::Index(i), Json::Array(items)) => match position(*i, items.len()) {
            Some(p) if p < items.len() && f != JsonFn::Insert => items[p] = value,
            Some(p) if p >= items.len() && f != JsonFn::Replace => items.push(value),
            _ => {}
        },
        _ => {}
    }
    Ok(())
}

fn remove(json: &mut Json, path: &[Step]) -> Result<()> {
    let Some((last, parents)) = path.split_last() else {
        return Err(sql_error("json_remove() can't remove the whole document"));
    };
    let mut at = json;
    for step in parents {
        at = match (step, at) {
            (Step::Key(key), Json::Object(fields)) => match fields.iter().position(|(k, _)| k == key) {
                Some(i) => &mut fields[i].1,
                None => return Ok(()),
            },
            (Step::Index(i), Json::Array(items)) => match position(*i, items.len()).filter(|&p| p < items.len()) {
                Some(p) => &mut items[p],
                None => return Ok(()),
            },
            _ => return Ok(()),
        };
    }
    match (last, at) {
        (Step::Key(key), Json::Object(fields)) => fields.retain(|(k, _)| k != key),
        (Step::Index(i), Json::Array(items)) => {
            if let Some(p) = position(*i, items.len()).filter(|&p| p < items.len()) {
                items.remove(p);
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(f: JsonFn, args: Vec<Value>) -> Value {
        f.call(args).unwrap()
    }

    #[test]
    fn paths_and_edits() {
        let doc: Value = r#"{"a": {"b": [1, 2, {"c": "x"}]}, "odd key": true}"#.into();
        assert_eq!(call(JsonFn::Extract, vec![doc.clone(), "$.a.b[2].c".into()]), "x".into());
        assert_eq!(call(JsonFn::Extract, vec![doc.clone(), "$.a.b[#-1].c".into()]), "x".into());
        assert_eq!(call(JsonFn::Extract, vec![doc.clone(), "$.\"odd key\"".into()]), true.into());
        assert_eq!(call(JsonFn::Extract, vec![doc.clone(), "$.a.b".into()]), "[1,2,{\"c\":\"x\"}]".into());
        assert_eq!(call(JsonFn::Extract, vec![doc.clone(), "$.nope".into()]), Value::Null);
        assert_eq!(call(JsonFn::Type, vec![doc.clone(), "$.a".into()]), "object".into());
        let set = call(JsonFn::Set, vec![doc.clone(), "$.a.b[#]".into(), 3.into(), "$.n".into(), Value::Null]);
        assert_eq!(call(JsonFn::Extract, vec![set.clone(), "$.a.b[3]".into()]), 3.into());
        assert_eq!(call(JsonFn::Type, vec![set, "$.n".into()]), "null".into());
        let replaced = call(JsonFn::Replace, vec![doc.clone(), "$.zz".into(), 1.into()]);
        assert_eq!(call(JsonFn::Extract, vec![replaced, "$.zz".into()]), Value::Null);
        let removed = call(JsonFn::Remove, vec![doc, "$.a.b[0]".into()]);
        assert_eq!(call(JsonFn::ArrayLength, vec![removed, "$.a.b".into()]), 2.into());
        assert!(JsonFn::Extract.call(vec!["{".into(), "$".into()]).is_err());
        assert!(JsonFn::Extract.call(vec!["{}".into(), "a.b".into()]).is_err());
    }
}
