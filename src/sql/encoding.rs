//! How SQL values become bytes in the key-value store.
//!
//! Keys use an order-preserving encoding: comparing two encoded keys byte by
//! byte gives the same answer as comparing the values they hold, so the
//! engine's sorted storage doubles as a primary-key index and secondary
//! indexes. Row contents use a compact encoding that needn't preserve order.

use crate::coding::{Reader, put_bytes, put_varint};
use crate::error::{Result, corruption};

use super::value::Value;

const KEY_NULL: u8 = 0x00;
const KEY_FALSE: u8 = 0x10;
const KEY_TRUE: u8 = 0x11;
const KEY_INTEGER: u8 = 0x20;
const KEY_REAL: u8 = 0x30;
const KEY_TEXT: u8 = 0x40;

/// Appends `value` in the order-preserving key encoding. NULL sorts first.
pub fn encode_key(buf: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Null => buf.push(KEY_NULL),
        Value::Boolean(b) => buf.push(if *b { KEY_TRUE } else { KEY_FALSE }),
        Value::Integer(i) => {
            // Flipping the sign bit makes two's complement sort as unsigned bytes.
            buf.push(KEY_INTEGER);
            buf.extend_from_slice(&((*i as u64) ^ (1 << 63)).to_be_bytes());
        }
        Value::Real(r) => {
            // Positive floats: set the sign bit. Negative: invert everything.
            let r = if *r == 0.0 { 0.0 } else { *r }; // -0.0 and 0.0 are equal
            let bits = r.to_bits();
            let bits = if bits >> 63 == 1 { !bits } else { bits ^ (1 << 63) };
            buf.push(KEY_REAL);
            buf.extend_from_slice(&bits.to_be_bytes());
        }
        Value::Text(s) => {
            // Escape 0x00 as 00 FF and terminate with 00 00, so a string sorts
            // before any longer string it is a prefix of.
            buf.push(KEY_TEXT);
            for &b in s.as_bytes() {
                buf.push(b);
                if b == 0 {
                    buf.push(0xff);
                }
            }
            buf.extend_from_slice(&[0, 0]);
        }
    }
}

pub fn encode_keys(buf: &mut Vec<u8>, values: &[Value]) {
    values.iter().for_each(|v| encode_key(buf, v));
}

/// Decodes one value from the front of `input`, advancing past it.
pub fn decode_key(input: &mut &[u8]) -> Result<Value> {
    let (&tag, rest) = input.split_first().ok_or_else(|| corruption("truncated key"))?;
    *input = rest;
    let fixed8 = |input: &mut &[u8]| -> Result<u64> {
        let (head, rest) = input.split_at_checked(8).ok_or_else(|| corruption("truncated key"))?;
        *input = rest;
        Ok(u64::from_be_bytes(head.try_into().unwrap()))
    };
    Ok(match tag {
        KEY_NULL => Value::Null,
        KEY_FALSE => Value::Boolean(false),
        KEY_TRUE => Value::Boolean(true),
        KEY_INTEGER => Value::Integer((fixed8(input)? ^ (1 << 63)) as i64),
        KEY_REAL => {
            let bits = fixed8(input)?;
            Value::Real(f64::from_bits(if bits >> 63 == 1 { bits ^ (1 << 63) } else { !bits }))
        }
        KEY_TEXT => {
            let mut bytes = Vec::new();
            loop {
                let data: &[u8] = input;
                match data {
                    [0, 0, rest @ ..] => {
                        *input = rest;
                        break;
                    }
                    [0, 0xff, rest @ ..] => {
                        bytes.push(0);
                        *input = rest;
                    }
                    [b, rest @ ..] if *b != 0 => {
                        bytes.push(*b);
                        *input = rest;
                    }
                    _ => return Err(corruption("bad text in key")),
                }
            }
            Value::Text(String::from_utf8(bytes).map_err(|_| corruption("key text is not UTF-8"))?)
        }
        tag => return Err(corruption(format!("unknown key tag {tag:#x}"))),
    })
}

/// Advances past one key-encoded value without decoding it.
pub fn skip_key(input: &mut &[u8]) -> Result<()> {
    let truncated = || corruption("truncated key");
    let (&tag, rest) = input.split_first().ok_or_else(truncated)?;
    *input = match tag {
        KEY_NULL | KEY_FALSE | KEY_TRUE => rest,
        KEY_INTEGER | KEY_REAL => rest.get(8..).ok_or_else(truncated)?,
        KEY_TEXT => {
            // Find the 00 00 terminator, stepping over escaped 00 FF pairs.
            let mut pos = 0;
            loop {
                pos += rest[pos..].iter().position(|&b| b == 0).ok_or_else(truncated)?;
                match rest.get(pos + 1) {
                    Some(0) => break &rest[pos + 2..],
                    Some(0xff) => pos += 2,
                    _ => return Err(corruption("bad text in key")),
                }
            }
        }
        tag => return Err(corruption(format!("unknown key tag {tag:#x}"))),
    };
    Ok(())
}

const ROW_NULL: u8 = 0;
const ROW_FALSE: u8 = 1;
const ROW_TRUE: u8 = 2;
const ROW_INTEGER: u8 = 3;
const ROW_REAL: u8 = 4;
const ROW_TEXT: u8 = 5;

pub fn encode_value(buf: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Null => buf.push(ROW_NULL),
        Value::Boolean(b) => buf.push(if *b { ROW_TRUE } else { ROW_FALSE }),
        Value::Integer(i) => {
            buf.push(ROW_INTEGER);
            put_varint(buf, ((i << 1) ^ (i >> 63)) as u64); // zigzag: small magnitudes stay small
        }
        Value::Real(r) => {
            buf.push(ROW_REAL);
            buf.extend_from_slice(&r.to_le_bytes());
        }
        Value::Text(s) => {
            buf.push(ROW_TEXT);
            put_bytes(buf, s.as_bytes());
        }
    }
}

pub fn decode_value(r: &mut Reader) -> Result<Value> {
    Ok(match r.u8()? {
        ROW_NULL => Value::Null,
        ROW_FALSE => Value::Boolean(false),
        ROW_TRUE => Value::Boolean(true),
        ROW_INTEGER => {
            let z = r.varint()?;
            Value::Integer((z >> 1) as i64 ^ -((z & 1) as i64))
        }
        ROW_REAL => Value::Real(f64::from_le_bytes(r.bytes(8)?.try_into().unwrap())),
        ROW_TEXT => {
            Value::Text(String::from_utf8(r.len_prefixed()?.to_vec()).map_err(|_| corruption("text is not UTF-8"))?)
        }
        tag => return Err(corruption(format!("unknown value tag {tag}"))),
    })
}

pub fn encode_row(row: &[Value]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(8 * row.len());
    put_varint(&mut buf, row.len() as u64);
    row.iter().for_each(|v| encode_value(&mut buf, v));
    buf
}

/// Decodes a row, padding it with NULLs to `width` columns.
#[cfg(test)]
pub fn decode_row(data: &[u8], width: usize) -> Result<Vec<Value>> {
    let mut row = Vec::with_capacity(width);
    decode_row_into(data, width, None, &mut row)?;
    Ok(row)
}

/// Decodes a row into `out`, reusing it across rows. Only the columns that
/// `needed` marks are written; the rest are skipped over (text isn't even
/// copied) and stay NULL. A scan decodes every row into the same buffer, so
/// columns it never needs are never touched.
pub fn decode_row_into(data: &[u8], width: usize, needed: Option<&[bool]>, out: &mut Vec<Value>) -> Result<()> {
    if out.len() != width {
        out.clear();
        out.resize(width, Value::Null);
    }
    let mut r = Reader::new(data);
    // Nothing past the last needed column has to be read at all.
    let stop = needed.map_or(width, |needed| needed.iter().rposition(|&n| n).map_or(0, |last| last + 1));
    let stored = r.varint()? as usize;
    for (i, slot) in out.iter_mut().enumerate().take(stop) {
        let wanted = needed.is_none_or(|needed| needed[i]);
        if i >= stored {
            // Written before this column existed.
            if wanted {
                *slot = Value::Null;
            }
        } else if wanted {
            *slot = decode_value(&mut r)?;
        } else {
            skip_value(&mut r)?;
        }
    }
    Ok(())
}

fn skip_value(r: &mut Reader) -> Result<()> {
    match r.u8()? {
        ROW_NULL | ROW_FALSE | ROW_TRUE => {}
        ROW_INTEGER => {
            r.varint()?;
        }
        ROW_REAL => {
            r.bytes(8)?;
        }
        ROW_TEXT => {
            r.len_prefixed()?;
        }
        tag => return Err(corruption(format!("unknown value tag {tag}"))),
    }
    Ok(())
}

/// The smallest key greater than every key starting with `prefix`, or `None`
/// if there is none (the prefix is all 0xFF).
pub fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last < 0xff {
            end.push(last + 1);
            return Some(end);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_values() -> Vec<Value> {
        let mut values = vec![Value::Null, Value::Boolean(false), Value::Boolean(true)];
        for i in [i64::MIN, -1_000_000, -1, 0, 1, 255, 256, 1_000_000, i64::MAX] {
            values.push(Value::Integer(i));
        }
        for r in [f64::NEG_INFINITY, -1e300, -2.5, -1e-300, 0.0, 1e-300, 2.5, 1e300, f64::INFINITY] {
            values.push(Value::Real(r));
        }
        for s in ["", "\0", "\0\0", "a", "a\0", "a\0b", "ab", "b", "ü"] {
            values.push(Value::Text(s.into()));
        }
        values
    }

    #[test]
    fn key_encoding_preserves_order() {
        let values = sample_values();
        for a in &values {
            for b in &values {
                if a.data_type() != b.data_type() && !a.is_null() && !b.is_null() {
                    continue; // only same-typed values share a column
                }
                let (mut ka, mut kb) = (Vec::new(), Vec::new());
                encode_key(&mut ka, a);
                encode_key(&mut kb, b);
                assert_eq!(ka.cmp(&kb), a.total_cmp(b), "{a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn composite_keys_order_by_first_value_first() {
        let key = |a: &str, b: i64| {
            let mut k = Vec::new();
            encode_keys(&mut k, &[Value::Text(a.into()), Value::Integer(b)]);
            k
        };
        assert!(key("a", 999) < key("a\0", -999));
        assert!(key("a", 1) < key("a", 2));
        assert!(key("a", 999) < key("ab", 0));
    }

    #[test]
    fn keys_and_rows_round_trip() {
        let values = sample_values();
        let mut buf = Vec::new();
        encode_keys(&mut buf, &values);
        let mut input = buf.as_slice();
        for v in &values {
            assert_eq!(&decode_key(&mut input).unwrap(), v);
        }
        assert!(input.is_empty());
        let mut input = buf.as_slice();
        values.iter().for_each(|_| skip_key(&mut input).unwrap());
        assert!(input.is_empty());
        assert_eq!(decode_row(&encode_row(&values), values.len()).unwrap(), values);
        assert_eq!(decode_row(&encode_row(&[Value::Integer(1)]), 3).unwrap().len(), 3);

        let needed: Vec<bool> = (0..values.len()).map(|i| i % 3 == 0).collect();
        let mut out = vec![Value::Text("stale".into())];
        decode_row_into(&encode_row(&values), values.len(), Some(&needed), &mut out).unwrap();
        for (i, (got, want)) in out.iter().zip(&values).enumerate() {
            assert_eq!(got, if needed[i] { want } else { &Value::Null }, "column {i}");
        }
        // Reusing the buffer for a shorter row leaves no stale values behind.
        decode_row_into(&encode_row(&values[..2]), values.len(), Some(&needed), &mut out).unwrap();
        assert_eq!(out[0], values[0]);
        assert!(out[2..].iter().all(Value::is_null));
    }

    #[test]
    fn prefix_end_skips_trailing_ff() {
        assert_eq!(prefix_end(&[1, 2]), Some(vec![1, 3]));
        assert_eq!(prefix_end(&[1, 0xff]), Some(vec![2]));
        assert_eq!(prefix_end(&[0xff]), None);
    }
}
