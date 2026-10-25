//! Scalar functions for text, numbers and conditions, beyond the handful
//! the expression evaluator handles itself.

use std::cell::Cell;
use std::cmp::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use super::ast::BinaryOp;
use super::expr::arithmetic;
use super::value::{Value, sql_error};
use crate::error::Result;

/// The longest text a function may build, so `repeat('x', 1e12)` fails
/// instead of exhausting memory.
const MAX_TEXT: usize = 1 << 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scalar {
    Ltrim,
    Rtrim,
    Btrim,
    Strpos,
    Concat,
    ConcatWs,
    Left,
    Right,
    Lpad,
    Rpad,
    Repeat,
    Reverse,
    Initcap,
    Ascii,
    Chr,
    Hex,
    ToHex,
    OctetLength,
    SplitPart,
    StartsWith,
    Iif,
    Greatest,
    Least,
    Sign,
    Ceil,
    Floor,
    Trunc,
    Sqrt,
    Exp,
    Ln,
    Log,
    Log10,
    Log2,
    Power,
    Mod,
    Pi,
    Random,
    Degrees,
    Radians,
    Sin,
    Cos,
    Tan,
    Asin,
    Acos,
    Atan,
    Atan2,
}

/// The function called `name`, if it is one of these, and whether it
/// takes `args` arguments.
pub fn resolve(name: &str, args: usize) -> Option<(Scalar, bool)> {
    use Scalar::*;
    let one = args == 1;
    Some(match name {
        "ltrim" => (Ltrim, args == 1 || args == 2),
        "rtrim" => (Rtrim, args == 1 || args == 2),
        "btrim" | "trim" => (Btrim, args == 1 || args == 2),
        "strpos" | "instr" => (Strpos, args == 2),
        "concat" => (Concat, args >= 1),
        "concat_ws" => (ConcatWs, args >= 2),
        "left" => (Left, args == 2),
        "right" => (Right, args == 2),
        "lpad" => (Lpad, args == 2 || args == 3),
        "rpad" => (Rpad, args == 2 || args == 3),
        "repeat" => (Repeat, args == 2),
        "reverse" => (Reverse, one),
        "initcap" => (Initcap, one),
        "ascii" | "unicode" => (Ascii, one),
        "chr" | "char" => (Chr, args >= 1),
        "hex" => (Hex, one),
        "to_hex" => (ToHex, one),
        "octet_length" => (OctetLength, one),
        "split_part" => (SplitPart, args == 3),
        "starts_with" => (StartsWith, args == 2),
        "iif" | "if" => (Iif, args == 3),
        "greatest" => (Greatest, args >= 1),
        "least" => (Least, args >= 1),
        "sign" => (Sign, one),
        "ceil" | "ceiling" => (Ceil, one),
        "floor" => (Floor, one),
        "trunc" | "truncate" => (Trunc, args == 1 || args == 2),
        "sqrt" => (Sqrt, one),
        "exp" => (Exp, one),
        "ln" => (Ln, one),
        "log" => (Log, args == 1 || args == 2),
        "log10" => (Log10, one),
        "log2" => (Log2, one),
        "power" | "pow" => (Power, args == 2),
        "mod" => (Mod, args == 2),
        "pi" => (Pi, args == 0),
        "random" => (Random, args == 0),
        "degrees" => (Degrees, one),
        "radians" => (Radians, one),
        "sin" => (Sin, one),
        "cos" => (Cos, one),
        "tan" => (Tan, one),
        "asin" => (Asin, one),
        "acos" => (Acos, one),
        "atan" => (Atan, one),
        "atan2" => (Atan2, args == 2),
        _ => return None,
    })
}

impl Scalar {
    /// Whether the function looks at NULL arguments itself, rather than
    /// returning NULL when any argument is NULL.
    pub fn takes_nulls(self) -> bool {
        matches!(self, Scalar::Concat | Scalar::ConcatWs | Scalar::Iif | Scalar::Greatest | Scalar::Least)
    }

    pub fn call(self, args: Vec<Value>) -> Result<Value> {
        use Scalar::*;
        let name = Name(self);
        let text = |i: usize| args[i].to_string();
        let int = |i: usize| match &args[i] {
            Value::Integer(n) => Ok(*n),
            v => Err(sql_error(format!("{name}() needs an integer, got {}", v.type_name()))),
        };
        let real = |i: usize| {
            args[i].as_f64().ok_or_else(|| sql_error(format!("{name}() needs a number, got {}", args[i].type_name())))
        };
        let math = |x: f64| {
            if x.is_finite() { Ok(Value::Real(x)) } else { Err(sql_error(format!("{name}(): argument out of range"))) }
        };
        Ok(match self {
            Ltrim | Rtrim | Btrim => {
                let s = text(0);
                let chars: Option<Vec<char>> = args.get(1).map(|c| c.to_string().chars().collect());
                let strip = |c: char| chars.as_ref().map_or(c.is_whitespace(), |set| set.contains(&c));
                Value::Text(
                    match self {
                        Ltrim => s.trim_start_matches(strip),
                        Rtrim => s.trim_end_matches(strip),
                        _ => s.trim_matches(strip),
                    }
                    .to_string(),
                )
            }
            Strpos => {
                let (s, sub) = (text(0), text(1));
                Value::Integer(s.find(&sub).map_or(0, |at| s[..at].chars().count() as i64 + 1))
            }
            Concat => Value::Text(args.iter().filter(|v| !v.is_null()).map(Value::to_string).collect()),
            ConcatWs => {
                if args[0].is_null() {
                    return Ok(Value::Null);
                }
                let parts: Vec<String> = args[1..].iter().filter(|v| !v.is_null()).map(Value::to_string).collect();
                Value::Text(parts.join(&text(0)))
            }
            Left | Right => {
                let chars: Vec<char> = text(0).chars().collect();
                let n = int(1)?;
                // A negative count leaves off that many from the other end.
                let keep = if n >= 0 {
                    (n as usize).min(chars.len())
                } else {
                    chars.len().saturating_sub(n.unsigned_abs() as usize)
                };
                let range = if self == Left { 0..keep } else { chars.len() - keep..chars.len() };
                Value::Text(chars[range].iter().collect())
            }
            Lpad | Rpad => {
                let chars: Vec<char> = text(0).chars().collect();
                let width = usize::try_from(int(1)?.max(0)).unwrap_or(usize::MAX);
                if width > MAX_TEXT {
                    return Err(sql_error(format!("{name}() would make text longer than {MAX_TEXT} characters")));
                }
                let fill: Vec<char> = args.get(2).map_or(vec![' '], |f| f.to_string().chars().collect());
                if chars.len() >= width || fill.is_empty() {
                    return Ok(Value::Text(chars[..width.min(chars.len())].iter().collect()));
                }
                let padding: String = fill.iter().cycle().take(width - chars.len()).collect();
                let s: String = chars.iter().collect();
                Value::Text(if self == Lpad { padding + &s } else { s + &padding })
            }
            Repeat => {
                let (s, n) = (text(0), int(1)?.max(0) as usize);
                if s.len().saturating_mul(n) > MAX_TEXT {
                    return Err(sql_error(format!("repeat() would make text longer than {MAX_TEXT} bytes")));
                }
                Value::Text(s.repeat(n))
            }
            Reverse => Value::Text(text(0).chars().rev().collect()),
            Initcap => {
                let mut out = String::new();
                let mut start = true;
                for c in text(0).chars() {
                    if c.is_alphanumeric() {
                        if start {
                            out.extend(c.to_uppercase());
                        } else {
                            out.extend(c.to_lowercase());
                        }
                        start = false;
                    } else {
                        out.push(c);
                        start = true;
                    }
                }
                Value::Text(out)
            }
            Ascii => match text(0).chars().next() {
                Some(c) => Value::Integer(c as i64),
                None => Value::Null,
            },
            Chr => {
                let mut out = String::new();
                for i in 0..args.len() {
                    let code = int(i)?;
                    let c = u32::try_from(code).ok().and_then(char::from_u32);
                    out.push(c.ok_or_else(|| sql_error(format!("chr(): {code} is not a character code")))?);
                }
                Value::Text(out)
            }
            Hex => Value::Text(text(0).bytes().map(|b| format!("{b:02X}")).collect()),
            ToHex => Value::Text(format!("{:x}", int(0)?)),
            OctetLength => Value::Integer(text(0).len() as i64),
            SplitPart => {
                let (s, delimiter, n) = (text(0), text(1), int(2)?);
                let parts: Vec<&str> =
                    if delimiter.is_empty() { vec![&s] } else { s.split(delimiter.as_str()).collect() };
                // Counting from 1, or from the end when negative.
                let at = match n {
                    0 => return Err(sql_error("split_part(): the field number can't be 0")),
                    n if n > 0 => usize::try_from(n - 1).ok(),
                    n => parts.len().checked_sub(n.unsigned_abs() as usize),
                };
                Value::Text(at.and_then(|i| parts.get(i)).map_or(String::new(), |p| p.to_string()))
            }
            StartsWith => Value::Boolean(text(0).starts_with(&text(1))),
            Iif => {
                let mut args = args;
                let pick = if args[0].truth()? == Some(true) { 1 } else { 2 };
                args.swap_remove(pick)
            }
            Greatest | Least => {
                // NULLs are passed over, as in PostgreSQL.
                let wanted = if self == Greatest { Ordering::Greater } else { Ordering::Less };
                let mut best: Option<Value> = None;
                for v in args.into_iter().filter(|v| !v.is_null()) {
                    if best.as_ref().is_none_or(|b| v.total_cmp(b) == wanted) {
                        best = Some(v);
                    }
                }
                best.unwrap_or(Value::Null)
            }
            Sign => match &args[0] {
                Value::Integer(n) => Value::Integer(n.signum()),
                _ => {
                    let x = real(0)?;
                    Value::Real(if x == 0.0 { 0.0 } else { x.signum() })
                }
            },
            Ceil | Floor => match &args[0] {
                Value::Integer(n) => Value::Integer(*n),
                _ => Value::Real(if self == Ceil { real(0)?.ceil() } else { real(0)?.floor() }),
            },
            Trunc => {
                let digits = if args.len() > 1 { int(1)? } else { 0 };
                match &args[0] {
                    Value::Integer(n) if digits >= 0 => Value::Integer(*n),
                    _ => {
                        let scale = 10f64.powi(digits.clamp(-300, 300) as i32);
                        Value::Real((real(0)? * scale).trunc() / scale)
                    }
                }
            }
            Sqrt => {
                let x = real(0)?;
                if x < 0.0 {
                    return Err(sql_error("sqrt() of a negative number"));
                }
                Value::Real(x.sqrt())
            }
            Exp => math(real(0)?.exp())?,
            Ln | Log10 | Log2 | Log => {
                let (base, x) = match (self, args.len()) {
                    (Log, 2) => (Some(real(0)?), real(1)?),
                    (Ln, _) => (None, real(0)?),
                    (Log2, _) => (Some(2.0), real(0)?),
                    _ => (Some(10.0), real(0)?),
                };
                if x <= 0.0 || base.is_some_and(|b| b <= 0.0 || b == 1.0) {
                    return Err(sql_error(format!("{name}() of a number that has no logarithm")));
                }
                math(match base {
                    None => x.ln(),
                    Some(10.0) => x.log10(),
                    Some(2.0) => x.log2(),
                    Some(b) => x.ln() / b.ln(),
                })?
            }
            Power => math(real(0)?.powf(real(1)?))?,
            Mod => arithmetic(BinaryOp::Rem, &args[0], &args[1])?,
            Pi => Value::Real(std::f64::consts::PI),
            Random => Value::Real(random()),
            Degrees => math(real(0)?.to_degrees())?,
            Radians => math(real(0)?.to_radians())?,
            Sin => math(real(0)?.sin())?,
            Cos => math(real(0)?.cos())?,
            Tan => math(real(0)?.tan())?,
            Asin => math(real(0)?.asin())?,
            Acos => math(real(0)?.acos())?,
            Atan => math(real(0)?.atan())?,
            Atan2 => math(real(0)?.atan2(real(1)?))?,
        })
    }
}

/// A function's name for messages, written only when one is.
#[derive(Clone, Copy)]
struct Name(Scalar);

impl std::fmt::Display for Name {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&format!("{:?}", self.0).to_lowercase())
    }
}

thread_local! {
    static RANDOM: Cell<u64> = Cell::new({
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
        let place = &nanos as *const u64 as u64;
        (nanos ^ place.rotate_left(32)) | 1
    });
}

/// A pseudo-random number in [0, 1), from xorshift64*.
fn random() -> f64 {
    RANDOM.with(|state| {
        let mut x = state.get();
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        state.set(x);
        (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, args: Vec<Value>) -> Value {
        let (f, ok) = resolve(name, args.len()).unwrap();
        assert!(ok, "{name} with {} arguments", args.len());
        f.call(args).unwrap()
    }

    #[test]
    fn text() {
        assert_eq!(call("ltrim", vec!["xxhixx".into(), "x".into()]), "hixx".into());
        assert_eq!(call("strpos", vec!["héllo".into(), "llo".into()]), 3.into());
        assert_eq!(call("left", vec!["abcdef".into(), (-2).into()]), "abcd".into());
        assert_eq!(call("right", vec!["abcdef".into(), 2.into()]), "ef".into());
        assert_eq!(call("lpad", vec!["7".into(), 3.into(), "0".into()]), "007".into());
        assert_eq!(call("rpad", vec!["abcd".into(), 2.into()]), "ab".into());
        assert_eq!(call("initcap", vec!["hello wORLD-x".into()]), "Hello World-X".into());
        assert_eq!(call("split_part", vec!["a,b,c".into(), ",".into(), (-1).into()]), "c".into());
        assert_eq!(call("concat_ws", vec!["-".into(), "a".into(), Value::Null, 1.into()]), "a-1".into());
        assert_eq!(call("chr", vec![72.into(), 105.into()]), "Hi".into());
        assert_eq!(call("hex", vec!["Hi".into()]), "4869".into());
    }

    #[test]
    fn numbers() {
        assert_eq!(call("greatest", vec![1.into(), Value::Null, 3.5.into()]), 3.5.into());
        assert_eq!(call("least", vec![Value::Null, Value::Null]), Value::Null);
        assert_eq!(call("trunc", vec![(-2.75).into(), 1.into()]), (-2.7).into());
        assert_eq!(call("log", vec![2.into(), 8.into()]), 3.0.into());
        assert_eq!(call("sign", vec![(-4).into()]), (-1).into());
        assert_eq!(call("mod", vec![7.into(), 3.into()]), 1.into());
        assert!(resolve("sqrt", 1).unwrap().0.call(vec![(-1).into()]).is_err());
        assert!(resolve("ln", 1).unwrap().0.call(vec![0.into()]).is_err());
        let r = call("random", vec![]).as_f64().unwrap();
        assert!((0.0..1.0).contains(&r));
    }
}
