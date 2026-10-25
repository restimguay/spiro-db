//! Exact decimal numbers: a 128-bit integer scaled by a power of ten, so
//! up to 38 significant digits with no binary rounding (0.1 + 0.2 = 0.3).

use std::cmp::Ordering;
use std::fmt;

use super::value::sql_error;
use crate::error::Result;

/// The most digits after the decimal point, and in all.
pub const MAX_DIGITS: u8 = 38;

/// `mantissa / 10^scale`. The scale is kept as given, so 19.90 stays 19.90,
/// but equal values compare equal whatever their scales.
#[derive(Clone, Copy, Debug)]
pub struct Decimal {
    mantissa: i128,
    scale: u8,
}

fn pow10(n: u8) -> i128 {
    10i128.pow(n as u32)
}

fn overflow() -> crate::error::Error {
    sql_error("numeric value out of range")
}

impl Decimal {
    pub fn new(mantissa: i128, scale: u8) -> Decimal {
        Decimal { mantissa, scale: scale.min(MAX_DIGITS) }
    }

    pub fn from_i64(i: i64) -> Decimal {
        Decimal::new(i as i128, 0)
    }

    /// The exact decimal a float prints as (its shortest round-trip form),
    /// so 0.1 becomes 0.1 rather than 0.1000000000000000055....
    pub fn from_f64(x: f64) -> Option<Decimal> {
        if !x.is_finite() {
            return None;
        }
        Decimal::parse(&format!("{x}"))
    }

    pub fn scale(self) -> u8 {
        self.scale
    }

    /// Parses `[+-]digits[.digits][e[+-]digits]`.
    pub fn parse(text: &str) -> Option<Decimal> {
        let text = text.trim();
        let (body, exponent) = match text.find(['e', 'E']) {
            Some(at) => (&text[..at], text[at + 1..].parse::<i32>().ok()?),
            None => (text, 0),
        };
        let (negative, body) = match body.as_bytes().first()? {
            b'-' => (true, &body[1..]),
            b'+' => (false, &body[1..]),
            _ => (false, body),
        };
        let (whole, fraction) = body.split_once('.').unwrap_or((body, ""));
        if whole.is_empty() && fraction.is_empty() {
            return None;
        }
        let digits = whole.bytes().chain(fraction.bytes());
        let mut mantissa: i128 = 0;
        for d in digits {
            if !d.is_ascii_digit() {
                return None;
            }
            mantissa = mantissa.checked_mul(10)?.checked_add((d - b'0') as i128)?;
        }
        let mut scale = fraction.len() as i32 - exponent;
        if scale < 0 {
            mantissa = mantissa.checked_mul(10i128.checked_pow(scale.unsigned_abs())?)?;
            scale = 0;
        }
        let mut decimal = Decimal { mantissa: if negative { -mantissa } else { mantissa }, scale: 0 };
        if scale > MAX_DIGITS as i32 {
            decimal = Decimal { mantissa: decimal.mantissa, scale: scale.min(255) as u8 }.rescale(MAX_DIGITS)?;
        } else {
            decimal.scale = scale as u8;
        }
        Some(decimal)
    }

    pub fn to_f64(self) -> f64 {
        self.to_string().parse().unwrap_or(f64::NAN)
    }

    /// The value if it is a whole number that fits in an i64.
    pub fn to_i64(self) -> Option<i64> {
        let whole = self.mantissa.checked_div(pow10(self.scale))?;
        (whole * pow10(self.scale) == self.mantissa).then(|| i64::try_from(whole).ok()).flatten()
    }

    /// The same value with trailing zeros after the point dropped.
    pub fn normalized(self) -> Decimal {
        let mut d = self;
        while d.scale > 0 && d.mantissa % 10 == 0 {
            d.mantissa /= 10;
            d.scale -= 1;
        }
        d
    }

    /// The value with `scale` digits after the point, rounding half away
    /// from zero; `None` if it no longer fits.
    pub fn rescale(self, scale: u8) -> Option<Decimal> {
        match scale.cmp(&self.scale) {
            Ordering::Equal => Some(self),
            Ordering::Greater => {
                let factor = 10i128.checked_pow((scale - self.scale) as u32)?;
                Some(Decimal { mantissa: self.mantissa.checked_mul(factor)?, scale })
            }
            Ordering::Less => {
                let shift = (self.scale - scale) as u32;
                let Some(factor) = 10i128.checked_pow(shift) else {
                    return Some(Decimal { mantissa: 0, scale });
                };
                let (quotient, remainder) = (self.mantissa / factor, self.mantissa % factor);
                let round = (remainder.unsigned_abs() * 2 >= factor.unsigned_abs()) as i128;
                Some(Decimal { mantissa: quotient + round * self.mantissa.signum(), scale })
            }
        }
    }

    /// Rounds to `digits` after the point (negative: before it).
    pub fn round(self, digits: i32) -> Result<Decimal> {
        if digits >= self.scale as i32 {
            return Ok(self);
        }
        if digits >= 0 {
            return self.rescale(digits as u8).ok_or_else(overflow);
        }
        let shift = digits.unsigned_abs().min(MAX_DIGITS as u32 + 1) as u8;
        let factor = Decimal::new(10i128.checked_pow(shift as u32).ok_or_else(overflow)?, 0);
        let scaled = self.div_to(factor, self.scale)?.rescale(0).ok_or_else(overflow)?;
        scaled.checked_mul(factor)
    }

    pub fn trunc(self, digits: i32) -> Decimal {
        let digits = digits.clamp(0, MAX_DIGITS as i32) as u8;
        if digits >= self.scale {
            return self;
        }
        let factor = pow10(self.scale - digits);
        Decimal { mantissa: self.mantissa / factor, scale: digits }
    }

    /// The number of digits before the decimal point.
    pub fn whole_digits(self) -> u32 {
        let whole = (self.mantissa / pow10(self.scale)).unsigned_abs();
        if whole == 0 { 0 } else { whole.ilog10() + 1 }
    }

    pub fn is_zero(self) -> bool {
        self.mantissa == 0
    }

    pub fn signum(self) -> i32 {
        self.mantissa.signum() as i32
    }

    pub fn abs(self) -> Decimal {
        Decimal { mantissa: self.mantissa.abs(), scale: self.scale }
    }

    pub fn negate(self) -> Decimal {
        Decimal { mantissa: -self.mantissa, scale: self.scale }
    }

    /// Both values at the larger of their scales.
    fn aligned(self, other: Decimal) -> Option<(i128, i128, u8)> {
        let scale = self.scale.max(other.scale);
        Some((self.rescale(scale)?.mantissa, other.rescale(scale)?.mantissa, scale))
    }

    pub fn checked_add(self, other: Decimal) -> Result<Decimal> {
        let (a, b, scale) = self.aligned(other).ok_or_else(overflow)?;
        Ok(Decimal { mantissa: a.checked_add(b).ok_or_else(overflow)?, scale })
    }

    pub fn checked_sub(self, other: Decimal) -> Result<Decimal> {
        self.checked_add(other.negate())
    }

    pub fn checked_mul(self, other: Decimal) -> Result<Decimal> {
        let mut mantissa = self.mantissa.checked_mul(other.mantissa).ok_or_else(overflow)?;
        let mut scale = self.scale as u32 + other.scale as u32;
        if scale > MAX_DIGITS as u32 {
            let d = Decimal { mantissa, scale: 255.min(scale) as u8 }.rescale(MAX_DIGITS).ok_or_else(overflow)?;
            (mantissa, scale) = (d.mantissa, MAX_DIGITS as u32);
        }
        Ok(Decimal { mantissa, scale: scale as u8 })
    }

    /// Division, to six more digits than either operand has, at most 38.
    pub fn checked_div(self, other: Decimal) -> Result<Decimal> {
        let scale = (self.scale.max(other.scale) + 6).min(MAX_DIGITS);
        self.div_to(other, scale)
    }

    /// Division rounded to `scale` digits after the point, or as many as fit.
    fn div_to(self, other: Decimal, scale: u8) -> Result<Decimal> {
        if other.mantissa == 0 {
            return Err(sql_error("division by zero"));
        }
        // a/b at scale s is a.m * 10^(s - a.s + b.s) / b.m, rounded.
        let mut scale = scale;
        loop {
            let shift = scale as i32 - self.scale as i32 + other.scale as i32;
            let numerator = if shift >= 0 {
                10i128.checked_pow(shift as u32).and_then(|f| self.mantissa.checked_mul(f))
            } else {
                Some(self.mantissa / pow10(shift.unsigned_abs().min(MAX_DIGITS as u32) as u8))
            };
            if let Some(numerator) = numerator {
                let (quotient, remainder) = (numerator / other.mantissa, numerator % other.mantissa);
                let round = (remainder.unsigned_abs() * 2 >= other.mantissa.unsigned_abs()) as i128;
                let sign = if (numerator < 0) != (other.mantissa < 0) { -1 } else { 1 };
                return Ok(Decimal { mantissa: quotient + round * sign, scale });
            }
            if scale == 0 {
                return Err(overflow());
            }
            scale -= 1;
        }
    }

    pub fn checked_rem(self, other: Decimal) -> Result<Decimal> {
        let (a, b, scale) = self.aligned(other).ok_or_else(overflow)?;
        if b == 0 {
            return Err(sql_error("division by zero"));
        }
        Ok(Decimal { mantissa: a % b, scale })
    }

    /// Appends an order-preserving key encoding: a sign byte, then for
    /// non-zero values the exponent and the significant digits, so equal
    /// values encode alike whatever their scale.
    pub fn encode_key(self, buf: &mut Vec<u8>) {
        let d = self.normalized();
        if d.mantissa == 0 {
            buf.push(0x02);
            return;
        }
        let digits = d.mantissa.unsigned_abs().to_string();
        // value = 0.digits * 10^exponent
        let exponent = digits.len() as i32 - d.scale as i32;
        if d.mantissa > 0 {
            buf.push(0x03);
            buf.push((exponent + 128) as u8);
            buf.extend(digits.bytes());
            buf.push(0x00);
        } else {
            // Larger magnitudes sort first: invert the exponent and digits.
            buf.push(0x01);
            buf.push((128 - exponent) as u8);
            buf.extend(digits.bytes().map(|b| b'9' - b + b'0'));
            buf.push(0xff);
        }
    }

    /// Reads a value written by `encode_key`, advancing past it.
    pub fn decode_key(input: &mut &[u8]) -> Option<Decimal> {
        let (&sign, rest) = input.split_first()?;
        if sign == 0x02 {
            *input = rest;
            return Some(Decimal::new(0, 0));
        }
        let (&exponent, rest) = rest.split_first()?;
        let end = if sign == 0x03 { 0x00 } else { 0xff };
        let len = rest.iter().position(|&b| b == end)?;
        let digits: String =
            rest[..len].iter().map(|&b| if sign == 0x03 { b } else { b'9' - b + b'0' } as char).collect();
        *input = &rest[len + 1..];
        let exponent = if sign == 0x03 { exponent as i32 - 128 } else { 128 - exponent as i32 };
        let mantissa: i128 = digits.parse().ok()?;
        let scale = digits.len() as i32 - exponent;
        let d = if scale >= 0 {
            Decimal::new(mantissa, scale as u8)
        } else {
            Decimal::new(mantissa.checked_mul(10i128.checked_pow(scale.unsigned_abs())?)?, 0)
        };
        Some(if sign == 0x03 { d } else { d.negate() })
    }

    /// The row encoding: the scale, then the mantissa.
    pub fn to_bytes(self) -> [u8; 17] {
        let mut out = [0; 17];
        out[0] = self.scale;
        out[1..].copy_from_slice(&self.mantissa.to_le_bytes());
        out
    }

    pub fn from_bytes(bytes: &[u8; 17]) -> Decimal {
        Decimal::new(i128::from_le_bytes(bytes[1..].try_into().unwrap()), bytes[0])
    }
}

impl Ord for Decimal {
    fn cmp(&self, other: &Decimal) -> Ordering {
        match self.aligned(*other) {
            Some((a, b, _)) => a.cmp(&b),
            // Scaling up overflowed: the side with the smaller scale is the
            // one that grew, so it has the larger magnitude.
            None => {
                let grew = if self.scale < other.scale { self } else { other };
                let magnitude = if grew.mantissa < 0 { Ordering::Less } else { Ordering::Greater };
                if std::ptr::eq(grew, self) { magnitude } else { magnitude.reverse() }
            }
        }
    }
}

impl PartialEq for Decimal {
    fn eq(&self, other: &Decimal) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Decimal {}

impl PartialOrd for Decimal {
    fn partial_cmp(&self, other: &Decimal) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl std::str::FromStr for Decimal {
    type Err = crate::error::Error;

    fn from_str(s: &str) -> Result<Decimal> {
        Decimal::parse(s).ok_or_else(|| sql_error(format!("not a decimal number: {s}")))
    }
}

impl fmt::Display for Decimal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let digits = self.mantissa.unsigned_abs().to_string();
        let sign = if self.mantissa < 0 { "-" } else { "" };
        let scale = self.scale as usize;
        if scale == 0 {
            return write!(f, "{sign}{digits}");
        }
        let padded = format!("{digits:0>width$}", width = scale + 1);
        let (whole, fraction) = padded.split_at(padded.len() - scale);
        write!(f, "{sign}{whole}.{fraction}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        Decimal::parse(s).unwrap()
    }

    #[test]
    fn parse_and_print() {
        assert_eq!(d("19.90").to_string(), "19.90");
        assert_eq!(d("-0.05").to_string(), "-0.05");
        assert_eq!(d("1.5e3").to_string(), "1500");
        assert_eq!(d("12e-3").to_string(), "0.012");
        assert_eq!(d(".5").to_string(), "0.5");
        assert_eq!(Decimal::from_f64(0.1).unwrap().to_string(), "0.1");
        assert!(Decimal::parse("1.2.3").is_none());
        assert!(Decimal::parse("").is_none());
    }

    #[test]
    fn arithmetic_is_exact() {
        assert_eq!(d("0.1").checked_add(d("0.2")).unwrap(), d("0.3"));
        assert_eq!(d("19.99").checked_mul(d("3")).unwrap().to_string(), "59.97");
        assert_eq!(d("10").checked_div(d("4")).unwrap().to_string(), "2.500000");
        assert_eq!(d("1").checked_div(d("3")).unwrap().to_string(), "0.333333");
        assert_eq!(d("-2.5").rescale(0).unwrap().to_string(), "-3");
        assert_eq!(d("2.345").round(2).unwrap().to_string(), "2.35");
        assert_eq!(d("1234.5").round(-2).unwrap().to_string(), "1200");
        assert_eq!(d("7.5").checked_rem(d("2")).unwrap().to_string(), "1.5");
        assert!(d("1").checked_div(d("0")).is_err());
    }

    #[test]
    fn order_and_keys() {
        let values = ["-1000", "-2.5", "-2.25", "-0.001", "0", "0.00", "0.001", "1", "1.0", "2.25", "2.5", "1000"];
        for a in values {
            for b in values {
                let (mut ka, mut kb) = (Vec::new(), Vec::new());
                d(a).encode_key(&mut ka);
                d(b).encode_key(&mut kb);
                assert_eq!(ka.cmp(&kb), d(a).cmp(&d(b)), "{a} vs {b}");
                let mut input = ka.as_slice();
                assert_eq!(Decimal::decode_key(&mut input).unwrap().cmp(&d(a)), Ordering::Equal);
                assert!(input.is_empty());
            }
        }
        let huge = Decimal::new(i128::MAX / 10, 0);
        assert_eq!(huge.cmp(&d("0.5")), Ordering::Greater);
        assert_eq!(huge.negate().cmp(&d("0.5")), Ordering::Less);
    }
}
