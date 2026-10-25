//! Dates and times. Values of the `DATE`, `TIME` and `TIMESTAMP` types are
//! stored as ISO 8601 text in UTC: `2024-03-15`, `13:45:00`,
//! `2024-03-15 13:45:00.25`. Those forms sort as text in time order, so
//! comparisons, `ORDER BY`, indexes and column sections all work on them
//! unchanged.

use std::time::{SystemTime, UNIX_EPOCH};

use super::value::{DataType, Value, sql_error};
use crate::error::Result;

const MICROS_PER_SECOND: i64 = 1_000_000;
const MICROS_PER_DAY: i64 = 86_400 * MICROS_PER_SECOND;
/// The Julian day number of 1970-01-01 at midnight.
const UNIX_JULIAN_DAY: f64 = 2_440_587.5;

/// What a piece of text names: a day, a time of day, or both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Date,
    Time,
    Timestamp,
}

impl Kind {
    /// How a column of type `ty` holds a time: TEXT holds a timestamp.
    pub fn of(ty: DataType) -> Kind {
        match ty {
            DataType::Date => Kind::Date,
            DataType::Time => Kind::Time,
            _ => Kind::Timestamp,
        }
    }
}

/// A point in time, in microseconds since 1970-01-01 00:00:00 UTC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Moment(pub i64);

impl Moment {
    pub fn now() -> Moment {
        let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        Moment(since.as_micros() as i64)
    }

    fn of(days: i64, micros: i64) -> Moment {
        Moment(days * MICROS_PER_DAY + micros)
    }

    fn days(self) -> i64 {
        self.0.div_euclid(MICROS_PER_DAY)
    }

    /// Microseconds since midnight.
    fn time(self) -> i64 {
        self.0.rem_euclid(MICROS_PER_DAY)
    }

    fn civil(self) -> (i64, u32, u32) {
        civil_from_days(self.days())
    }

    fn with_date(self, year: i64, month: u32, day: u32) -> Moment {
        Moment::of(days_from_civil(year, month, day), self.time())
    }

    /// Day of the week, 0 for Sunday.
    fn weekday(self) -> i64 {
        (self.days() + 4).rem_euclid(7)
    }

    fn add_months(self, months: i64) -> Moment {
        let (year, month, day) = self.civil();
        let total = year * 12 + (month as i64 - 1) + months;
        let (year, month) = (total.div_euclid(12), total.rem_euclid(12) as u32 + 1);
        // The 31st plus a month is the last day of a shorter month, as in PostgreSQL.
        self.with_date(year, month, day.min(days_in_month(year, month)))
    }

    pub fn format(self, kind: Kind) -> Result<String> {
        let (year, month, day) = self.civil();
        if !(0..=9999).contains(&year) && kind != Kind::Time {
            return Err(sql_error(format!("date out of range: year {year}")));
        }
        let date = format!("{year:04}-{month:02}-{day:02}");
        let micros = self.time();
        let (seconds, fraction) = (micros / MICROS_PER_SECOND, micros % MICROS_PER_SECOND);
        let mut time = format!("{:02}:{:02}:{:02}", seconds / 3600, seconds / 60 % 60, seconds % 60);
        if fraction != 0 {
            let digits = format!("{fraction:06}");
            time.push('.');
            time.push_str(digits.trim_end_matches('0'));
        }
        Ok(match kind {
            Kind::Date => date,
            Kind::Time => time,
            Kind::Timestamp => format!("{date} {time}"),
        })
    }
}

fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let month = month as i64;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { yoe + era * 400 + 1 } else { yoe + era * 400 }, month, day)
}

/// A cursor over the text being parsed.
struct Scanner<'a> {
    s: &'a [u8],
    pos: usize,
}

impl Scanner<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.pos).copied()
    }

    fn eat(&mut self, b: u8) -> bool {
        let found = self.peek() == Some(b);
        self.pos += found as usize;
        found
    }

    /// Exactly `n` digits.
    fn digits(&mut self, n: usize) -> Option<i64> {
        let digits = self.s.get(self.pos..self.pos + n)?;
        if !digits.iter().all(u8::is_ascii_digit) {
            return None;
        }
        self.pos += n;
        Some(digits.iter().fold(0, |acc, d| acc * 10 + (d - b'0') as i64))
    }

    fn done(&self) -> bool {
        self.pos == self.s.len()
    }
}

/// Parses `YYYY-MM-DD`, `HH:MM[:SS[.ffffff]]` or both (separated by a space
/// or `T`), with an optional `Z` or `±HH[:MM]` offset, which is converted to
/// UTC. `now` is the current time.
pub fn parse(text: &str) -> Option<(Moment, Kind)> {
    let text = text.trim();
    if text.eq_ignore_ascii_case("now") {
        return Some((Moment::now(), Kind::Timestamp));
    }
    let mut s = Scanner { s: text.as_bytes(), pos: 0 };
    let mut days = None;
    if s.s.get(4) == Some(&b'-') {
        let year = s.digits(4)?;
        s.eat(b'-');
        let month = s.digits(2)? as u32;
        if !s.eat(b'-') {
            return None;
        }
        let day = s.digits(2)? as u32;
        if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
            return None;
        }
        days = Some(days_from_civil(year, month, day));
        if s.done() {
            return Some((Moment::of(days?, 0), Kind::Date));
        }
        if !(s.eat(b' ') || s.eat(b'T') || s.eat(b't')) {
            return None;
        }
    }
    let hour = s.digits(2)?;
    if !s.eat(b':') {
        return None;
    }
    let minute = s.digits(2)?;
    let mut micros = 0;
    if s.eat(b':') {
        micros = s.digits(2)? * MICROS_PER_SECOND;
        if s.eat(b'.') {
            let mut scale = MICROS_PER_SECOND / 10;
            let start = s.pos;
            while let Some(d) = s.peek().filter(u8::is_ascii_digit) {
                micros += (d - b'0') as i64 * scale;
                scale /= 10;
                s.pos += 1;
            }
            if s.pos == start {
                return None;
            }
        }
    }
    if hour > 23 || minute > 59 || micros >= 60 * MICROS_PER_SECOND {
        return None;
    }
    micros += (hour * 60 + minute) * 60 * MICROS_PER_SECOND;
    s.eat(b' ');
    if s.eat(b'Z') || s.eat(b'z') {
    } else if text[s.pos..].eq_ignore_ascii_case("utc") {
        s.pos = s.s.len();
    } else if let Some(sign) = s.peek().filter(|&c| c == b'+' || c == b'-') {
        s.pos += 1;
        let hours = s.digits(2)?;
        s.eat(b':');
        let minutes = if s.done() { 0 } else { s.digits(2)? };
        let offset = (hours * 60 + minutes) * 60 * MICROS_PER_SECOND;
        micros -= if sign == b'+' { offset } else { -offset };
    }
    if !s.done() {
        return None;
    }
    Some(match days {
        Some(days) => (Moment::of(days, micros), Kind::Timestamp),
        None => (Moment(micros.rem_euclid(MICROS_PER_DAY)), Kind::Time),
    })
}

/// The canonical text of `text` as a value of `ty` (DATE, TIME or
/// TIMESTAMP), if it is one. A timestamp converts to its date or its time.
pub fn normalize(text: &str, ty: DataType) -> Option<String> {
    let (moment, kind) = parse(text)?;
    let kind = match (ty, kind) {
        (DataType::Date, Kind::Date | Kind::Timestamp) => Kind::Date,
        (DataType::Time, Kind::Time | Kind::Timestamp) => Kind::Time,
        (DataType::Timestamp, Kind::Date | Kind::Timestamp) => Kind::Timestamp,
        _ => return None,
    };
    moment.format(kind).ok()
}

/// A value read as a point in time: text in any form `parse` accepts, or a
/// number of seconds since 1970 when `unix` is set (otherwise a Julian day
/// number, as in SQLite).
fn moment_of(value: &Value, unix: bool, function: &str) -> Result<(Moment, Kind)> {
    let seconds = match value {
        Value::Text(text) => {
            return parse(text).ok_or_else(|| sql_error(format!("{function}(): not a date or time: '{text}'")));
        }
        Value::Integer(i) if unix => *i as f64,
        Value::Real(r) if unix => *r,
        Value::Integer(i) => (*i as f64 - UNIX_JULIAN_DAY) * 86_400.0,
        Value::Real(r) => (r - UNIX_JULIAN_DAY) * 86_400.0,
        v => return Err(sql_error(format!("{function}() needs a date or time, got {}", v.type_name()))),
    };
    if !seconds.is_finite() || seconds.abs() > 1e14 {
        return Err(sql_error(format!("{function}(): time out of range")));
    }
    Ok((Moment((seconds * MICROS_PER_SECOND as f64).round() as i64), Kind::Timestamp))
}

/// A span of time: calendar months plus microseconds, as in `1 month 2 days`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Interval {
    months: i64,
    micros: i64,
    /// Whether it has a time-of-day part, so adding it to a date gives a timestamp.
    timed: bool,
}

impl Interval {
    /// Parses `[+|-]N unit ...` (units from microsecond to year, singular
    /// or plural) and `[+|-]HH:MM[:SS]` parts.
    pub fn parse(text: &str) -> Option<Interval> {
        let mut interval = Interval::default();
        let mut words = text.split_whitespace().peekable();
        words.peek()?;
        while let Some(word) = words.next() {
            let (negative, body) = match word.as_bytes()[0] {
                b'-' => (true, &word[1..]),
                b'+' => (false, &word[1..]),
                _ => (false, word),
            };
            let sign = if negative { -1.0 } else { 1.0 };
            if body.contains(':') {
                let (Moment(micros), Kind::Time) = parse(body)? else { return None };
                interval.micros += if negative { -micros } else { micros };
                interval.timed = true;
                continue;
            }
            let amount: f64 = body.parse().ok().filter(|a: &f64| a.is_finite())?;
            let unit = words.next()?.to_ascii_lowercase();
            let unit = unit.strip_suffix('s').unwrap_or(&unit);
            let micros = match unit {
                "year" | "month" => {
                    if amount.fract() != 0.0 {
                        return None;
                    }
                    let months = (sign * amount) as i64 * if unit == "year" { 12 } else { 1 };
                    interval.months += months;
                    continue;
                }
                "week" => 7 * MICROS_PER_DAY,
                "day" => MICROS_PER_DAY,
                "hour" => 3600 * MICROS_PER_SECOND,
                "minute" => 60 * MICROS_PER_SECOND,
                "second" => MICROS_PER_SECOND,
                "millisecond" => 1000,
                "microsecond" => 1,
                _ => return None,
            };
            let micros = sign * amount * micros as f64;
            if micros.abs() > 1e18 {
                return None;
            }
            interval.micros += micros.round() as i64;
            interval.timed |= micros as i64 % MICROS_PER_DAY != 0;
        }
        Some(interval)
    }

    fn add_to(self, moment: Moment, sign: i64) -> Moment {
        Moment(moment.add_months(sign * self.months).0 + sign * self.micros)
    }
}

/// Applies one of SQLite's modifiers to a point in time.
fn modify(moment: Moment, modifier: &str) -> Result<Moment> {
    let m = modifier.trim().to_ascii_lowercase();
    let (year, month, _) = moment.civil();
    Ok(match m.as_str() {
        "start of day" => Moment::of(moment.days(), 0),
        "start of month" => Moment::of(days_from_civil(year, month, 1), 0),
        "start of year" => Moment::of(days_from_civil(year, 1, 1), 0),
        "utc" => moment,
        "localtime" => return Err(sql_error("localtime is not supported: SpiroDB keeps times in UTC")),
        _ => {
            if let Some(n) = m.strip_prefix("weekday ") {
                let target: i64 = n.trim().parse().ok().filter(|n| (0..7).contains(n)).ok_or_else(|| bad(modifier))?;
                Moment(moment.0 + (target - moment.weekday()).rem_euclid(7) * MICROS_PER_DAY)
            } else {
                Interval::parse(&m).ok_or_else(|| bad(modifier))?.add_to(moment, 1)
            }
        }
    })
}

fn bad(modifier: &str) -> crate::error::Error {
    sql_error(format!("unknown date modifier: '{modifier}'"))
}

/// SQLite's `date`, `time` and `datetime`: the first argument, moved by
/// any modifiers, as `kind`. `'unixepoch'` right after a number reads it as
/// seconds since 1970.
pub fn convert(args: &[Value], kind: Kind, function: &str) -> Result<Value> {
    let moment = modified(args, function)?.0;
    Ok(Value::Text(moment.format(kind)?))
}

fn modified(args: &[Value], function: &str) -> Result<(Moment, Kind)> {
    let mut modifiers = args[1..].iter().map(|m| match m {
        Value::Text(m) => Ok(m.as_str()),
        v => Err(sql_error(format!("{function}() modifiers must be text, got {}", v.type_name()))),
    });
    let unix = matches!(args.get(1), Some(Value::Text(m)) if m.eq_ignore_ascii_case("unixepoch"));
    if unix {
        modifiers.next();
    }
    let (mut moment, kind) = moment_of(&args[0], unix, function)?;
    for modifier in modifiers {
        moment = modify(moment, modifier?)?;
    }
    Ok((moment, kind))
}

/// `strftime(format, time, modifiers...)`.
pub fn strftime(args: &[Value]) -> Result<Value> {
    let Value::Text(format) = &args[0] else {
        return Err(sql_error("strftime() format must be text"));
    };
    let moment = modified(&args[1..], "strftime")?.0;
    let (year, month, day) = moment.civil();
    let micros = moment.time();
    let (hour, minute, second) = (micros / 3_600_000_000, micros / 60_000_000 % 60, micros / 1_000_000 % 60);
    let mut out = String::new();
    let mut chars = format.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let spec = chars.next().ok_or_else(|| sql_error("strftime(): format ends with %"))?;
        let piece = match spec {
            'Y' => format!("{year:04}"),
            'y' => format!("{:02}", year.rem_euclid(100)),
            'm' => format!("{month:02}"),
            'd' => format!("{day:02}"),
            'e' => format!("{day:2}"),
            'H' => format!("{hour:02}"),
            'k' => format!("{hour:2}"),
            'I' => format!("{:02}", (hour + 11) % 12 + 1),
            'l' => format!("{:2}", (hour + 11) % 12 + 1),
            'p' => (if hour < 12 { "AM" } else { "PM" }).to_string(),
            'M' => format!("{minute:02}"),
            'S' => format!("{second:02}"),
            'f' => format!("{second:02}.{:03}", micros / 1000 % 1000),
            'F' => format!("{year:04}-{month:02}-{day:02}"),
            'T' => format!("{hour:02}:{minute:02}:{second:02}"),
            'j' => format!("{:03}", day_of_year(moment)),
            'w' => moment.weekday().to_string(),
            'u' => ((moment.weekday() + 6) % 7 + 1).to_string(),
            'W' => format!("{:02}", (day_of_year(moment) + 6 - (moment.weekday() + 6) % 7) / 7),
            's' => moment.0.div_euclid(MICROS_PER_SECOND).to_string(),
            'J' => julian_day(moment).to_string(),
            '%' => "%".to_string(),
            c => return Err(sql_error(format!("strftime(): unknown format %{c}"))),
        };
        out.push_str(&piece);
    }
    Ok(Value::Text(out))
}

fn day_of_year(moment: Moment) -> i64 {
    let (year, _, _) = moment.civil();
    moment.days() - days_from_civil(year, 1, 1) + 1
}

/// The ISO 8601 week number and the year it belongs to.
fn iso_week(moment: Moment) -> (i64, i64) {
    // The week belongs to the year of its Thursday.
    let thursday = Moment(moment.0 + (3 - (moment.weekday() + 6) % 7) * MICROS_PER_DAY);
    let (year, _, _) = thursday.civil();
    (year, (day_of_year(thursday) - 1) / 7 + 1)
}

fn julian_day(moment: Moment) -> f64 {
    moment.0 as f64 / MICROS_PER_DAY as f64 + UNIX_JULIAN_DAY
}

pub fn julianday(args: &[Value]) -> Result<Value> {
    Ok(Value::Real(julian_day(modified(args, "julianday")?.0)))
}

pub fn unixepoch(args: &[Value]) -> Result<Value> {
    Ok(Value::Integer(modified(args, "unixepoch")?.0.0.div_euclid(MICROS_PER_SECOND)))
}

/// A whole number as an INTEGER, anything else as a REAL.
fn number(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9e15 { Value::Integer(n as i64) } else { Value::Real(n) }
}

/// `date_part(field, time)`, also written `EXTRACT(field FROM time)`.
pub fn date_part(field: &Value, value: &Value) -> Result<Value> {
    let (moment, _) = moment_of(value, false, "date_part")?;
    let (year, month, day) = moment.civil();
    let micros = moment.time();
    let field = field.to_string().to_ascii_lowercase();
    Ok(match field.trim_end_matches('s') {
        "year" => Value::Integer(year),
        "isoyear" => Value::Integer(iso_week(moment).0),
        "quarter" => Value::Integer((month as i64 - 1) / 3 + 1),
        "month" => Value::Integer(month as i64),
        "week" => Value::Integer(iso_week(moment).1),
        "day" => Value::Integer(day as i64),
        "dow" => Value::Integer(moment.weekday()),
        "isodow" => Value::Integer((moment.weekday() + 6) % 7 + 1),
        "doy" => Value::Integer(day_of_year(moment)),
        "hour" => Value::Integer(micros / 3_600_000_000),
        "minute" => Value::Integer(micros / 60_000_000 % 60),
        "second" => number((micros % 60_000_000) as f64 / 1e6),
        "millisecond" => number((micros % 60_000_000) as f64 / 1e3),
        "microsecond" => Value::Integer(micros % 60_000_000),
        "epoch" => number(moment.0 as f64 / 1e6),
        _ => return Err(sql_error(format!("unknown date field: {field}"))),
    })
}

/// `date_trunc(field, time)`: the time rounded down to the start of its
/// year, quarter, month, week (from Monday), day, hour, minute or second.
pub fn date_trunc(field: &Value, value: &Value) -> Result<Value> {
    let (moment, kind) = moment_of(value, false, "date_trunc")?;
    let (year, month, _) = moment.civil();
    let unit = |micros: i64| Moment(moment.0 - moment.0.rem_euclid(micros));
    let field = field.to_string().to_ascii_lowercase();
    let truncated = match field.trim_end_matches('s') {
        "year" => Moment::of(days_from_civil(year, 1, 1), 0),
        "quarter" => Moment::of(days_from_civil(year, (month - 1) / 3 * 3 + 1, 1), 0),
        "month" => Moment::of(days_from_civil(year, month, 1), 0),
        "week" => Moment::of(moment.days() - (moment.weekday() + 6) % 7, 0),
        "day" => unit(MICROS_PER_DAY),
        "hour" => unit(3600 * MICROS_PER_SECOND),
        "minute" => unit(60 * MICROS_PER_SECOND),
        "second" => unit(MICROS_PER_SECOND),
        "millisecond" => unit(1000),
        _ => return Err(sql_error(format!("unknown date field: {field}"))),
    };
    let kind = if kind == Kind::Time { Kind::Time } else { Kind::Timestamp };
    Ok(Value::Text(truncated.format(kind)?))
}

/// `INTERVAL '...'`: the interval's text, checked.
pub fn interval(value: &Value) -> Result<Value> {
    let text = value.to_string();
    Interval::parse(&text).ok_or_else(|| sql_error(format!("invalid interval: '{text}'")))?;
    Ok(Value::Text(text))
}

/// `date_add(time, interval)` (also `time + INTERVAL '...'`) and, with a
/// negative `sign`, `date_sub`. A date stays a date unless the interval
/// has a time part; a time of day wraps around midnight.
pub fn date_add(value: &Value, interval: &Value, sign: i64) -> Result<Value> {
    let function = if sign > 0 { "date_add" } else { "date_sub" };
    let (moment, kind) = moment_of(value, false, function)?;
    let text = interval.to_string();
    let interval = Interval::parse(&text).ok_or_else(|| sql_error(format!("invalid interval: '{text}'")))?;
    let kind = match kind {
        Kind::Date if interval.timed => Kind::Timestamp,
        kind => kind,
    };
    Ok(Value::Text(interval.add_to(moment, sign).format(kind)?))
}

/// `datediff(a, b)`: whole days from `b` to `a`, as in MySQL.
pub fn datediff(a: &Value, b: &Value) -> Result<Value> {
    let (a, _) = moment_of(a, false, "datediff")?;
    let (b, _) = moment_of(b, false, "datediff")?;
    Ok(Value::Integer(a.days() - b.days()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Value {
        Value::Text(s.into())
    }

    #[test]
    fn civil_round_trips() {
        for days in -800_000..800_000 {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
    }

    #[test]
    fn parses_and_normalizes() {
        assert_eq!(normalize("2024-02-29", DataType::Date).as_deref(), Some("2024-02-29"));
        assert_eq!(normalize("2023-02-29", DataType::Date), None);
        assert_eq!(
            normalize("2024-03-15T13:45:00.250Z", DataType::Timestamp).as_deref(),
            Some("2024-03-15 13:45:00.25")
        );
        assert_eq!(normalize("2024-03-15 23:30+02:00", DataType::Timestamp).as_deref(), Some("2024-03-15 21:30:00"));
        assert_eq!(normalize("2024-03-15 01:00:00-0300", DataType::Timestamp).as_deref(), Some("2024-03-15 04:00:00"));
        assert_eq!(normalize("2024-03-15", DataType::Timestamp).as_deref(), Some("2024-03-15 00:00:00"));
        assert_eq!(normalize("2024-03-15 13:45:10", DataType::Date).as_deref(), Some("2024-03-15"));
        assert_eq!(normalize("2024-03-15 13:45:10", DataType::Time).as_deref(), Some("13:45:10"));
        assert_eq!(normalize("7:00", DataType::Time), None);
        assert_eq!(normalize("07:00", DataType::Time).as_deref(), Some("07:00:00"));
        assert_eq!(normalize("12:00", DataType::Date), None);
        assert_eq!(normalize("24:00", DataType::Time), None);
        assert_eq!(normalize("2024-13-01", DataType::Date), None);
        assert_eq!(normalize("hello", DataType::Date), None);
    }

    #[test]
    fn modifiers_and_intervals() {
        let at = |args: &[&str]| -> String {
            let args: Vec<Value> = args.iter().map(|a| text(a)).collect();
            convert(&args, Kind::Timestamp, "datetime").unwrap().to_string()
        };
        assert_eq!(at(&["2024-01-31", "+1 month"]), "2024-02-29 00:00:00");
        assert_eq!(at(&["2024-03-15 10:00", "start of month", "+1 day", "-2 hours"]), "2024-03-01 22:00:00");
        assert_eq!(at(&["2024-03-15", "weekday 0"]), "2024-03-17 00:00:00");
        assert_eq!(at(&["2024-03-15 10:20:30", "start of year"]), "2024-01-01 00:00:00");
        let unix = convert(&[Value::Integer(86_400), text("unixepoch")], Kind::Timestamp, "datetime").unwrap();
        assert_eq!(unix, text("1970-01-02 00:00:00"));
        let julian = convert(&[Value::Real(2_460_000.5)], Kind::Date, "date").unwrap();
        assert_eq!(julian, text("2023-02-25"));

        assert_eq!(date_add(&text("2024-03-15"), &text("1 day"), 1).unwrap(), text("2024-03-16"));
        assert_eq!(date_add(&text("2024-03-15"), &text("36 hours"), 1).unwrap(), text("2024-03-16 12:00:00"));
        assert_eq!(date_add(&text("2024-03-15"), &text("1 year 2 months"), -1).unwrap(), text("2023-01-15"));
        assert_eq!(date_add(&text("23:30:00"), &text("01:00"), 1).unwrap(), text("00:30:00"));
        assert!(Interval::parse("1 fortnight").is_none());
        assert!(Interval::parse("").is_none());
    }

    #[test]
    fn fields() {
        let at = text("2024-12-30 13:45:30.5");
        let part = |f: &str| date_part(&text(f), &at).unwrap();
        assert_eq!(part("year"), Value::Integer(2024));
        assert_eq!(part("isoyear"), Value::Integer(2025));
        assert_eq!(part("week"), Value::Integer(1));
        assert_eq!(part("quarter"), Value::Integer(4));
        assert_eq!(part("dow"), Value::Integer(1));
        assert_eq!(part("doy"), Value::Integer(365));
        assert_eq!(part("second"), Value::Real(30.5));
        assert_eq!(part("hour"), Value::Integer(13));
        assert_eq!(date_trunc(&text("week"), &at).unwrap(), text("2024-12-30 00:00:00"));
        assert_eq!(date_trunc(&text("quarter"), &at).unwrap(), text("2024-10-01 00:00:00"));
        assert_eq!(date_trunc(&text("minute"), &at).unwrap(), text("2024-12-30 13:45:00"));
        let format = strftime(&[text("%Y/%m/%d %H:%M:%f %j %w %u %s"), text("1970-01-02 03:04:05.678")]).unwrap();
        assert_eq!(format, text("1970/01/02 03:04:05.678 002 5 5 97445"));
        assert_eq!(datediff(&text("2024-03-01"), &text("2024-02-01 23:00")).unwrap(), Value::Integer(29));
    }
}
