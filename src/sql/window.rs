//! Window functions: values computed over related rows (a partition, in an
//! order, within a frame) and appended to each row.

use std::cmp::Ordering;

use super::ast::FrameBound;
use super::encoding::encode_key;
use super::expr::{Accumulator, Env};
use super::plan::{WindowFunc, WindowKind};
use super::value::{Value, sql_error};
use crate::error::Result;

/// Appends the value of each window function to every row.
pub fn compute(windows: &[WindowFunc], rows: &mut [Vec<Value>], env: &Env) -> Result<()> {
    let mut values = Vec::with_capacity(windows.len());
    for w in windows {
        values.push(compute_one(w, rows, env)?);
    }
    for (i, row) in rows.iter_mut().enumerate() {
        row.extend(values.iter_mut().map(|v| std::mem::replace(&mut v[i], Value::Null)));
    }
    Ok(())
}

/// One window function's value for each row, in the rows' order.
fn compute_one(w: &WindowFunc, rows: &[Vec<Value>], env: &Env) -> Result<Vec<Value>> {
    // Each row's partition (as encoded bytes) and ORDER BY values.
    let mut keys = Vec::with_capacity(rows.len());
    for row in rows {
        let mut partition = Vec::new();
        for e in &w.partition {
            encode_key(&mut partition, &e.eval(row, env)?);
        }
        let order = w.order.iter().map(|(e, _)| e.eval(row, env)).collect::<Result<Vec<_>>>()?;
        keys.push((partition, order));
    }
    let compare = |a: &[Value], b: &[Value]| {
        for ((x, y), (_, sort)) in a.iter().zip(b).zip(&w.order) {
            let ordering = sort.compare(x, y);
            if ordering.is_ne() {
                return ordering;
            }
        }
        Ordering::Equal
    };
    // A stable sort, so rows that tie keep the order they came in.
    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by(|&a, &b| keys[a].0.cmp(&keys[b].0).then_with(|| compare(&keys[a].1, &keys[b].1)));

    let mut out = vec![Value::Null; rows.len()];
    let mut start = 0;
    while start < order.len() {
        let end = start + order[start..].iter().take_while(|&&i| keys[i].0 == keys[order[start]].0).count();
        let part = &order[start..end];
        // The peers of each position: the run of rows with equal ORDER BY values.
        let mut peers = vec![(0, 0); part.len()];
        let mut first = 0;
        for i in 1..=part.len() {
            if i == part.len() || compare(&keys[part[i]].1, &keys[part[first]].1).is_ne() {
                peers[first..i].fill((first, i));
                first = i;
            }
        }
        partition(w, part, &peers, rows, env, &mut out)?;
        start = end;
    }
    Ok(out)
}

/// Computes the values for one partition. `part` lists its rows in window
/// order; `peers[i]` is the range of positions sharing position i's ORDER BY
/// values.
fn partition(
    w: &WindowFunc,
    part: &[usize],
    peers: &[(usize, usize)],
    rows: &[Vec<Value>],
    env: &Env,
    out: &mut [Value],
) -> Result<()> {
    let m = part.len();
    let row = |i: usize| &rows[part[i]];
    let arg = |k: usize, i: usize| w.args[k].eval(row(i), env);
    match &w.kind {
        WindowKind::RowNumber => (0..m).for_each(|i| out[part[i]] = Value::Integer(i as i64 + 1)),
        WindowKind::Rank => (0..m).for_each(|i| out[part[i]] = Value::Integer(peers[i].0 as i64 + 1)),
        WindowKind::DenseRank => {
            let mut rank = 0;
            for i in 0..m {
                rank += i64::from(peers[i].0 == i);
                out[part[i]] = Value::Integer(rank);
            }
        }
        WindowKind::PercentRank => {
            for i in 0..m {
                let rank = if m == 1 { 0.0 } else { peers[i].0 as f64 / (m - 1) as f64 };
                out[part[i]] = Value::Real(rank);
            }
        }
        WindowKind::CumeDist => (0..m).for_each(|i| out[part[i]] = Value::Real(peers[i].1 as f64 / m as f64)),
        WindowKind::Ntile => {
            let buckets = match arg(0, 0)? {
                Value::Integer(n) if n > 0 => n as usize,
                v => return Err(sql_error(format!("ntile() needs a positive integer, got {}", v.literal()))),
            };
            // The first m % buckets buckets take one row more than the rest.
            let (size, extra) = (m / buckets, m % buckets);
            for i in 0..m {
                let bucket =
                    if i < extra * (size + 1) { i / (size + 1) } else { extra + (i - extra * (size + 1)) / size };
                out[part[i]] = Value::Integer(bucket as i64 + 1);
            }
        }
        WindowKind::Lag | WindowKind::Lead => {
            for i in 0..m {
                let offset = match w.args.get(1).map(|_| arg(1, i)).transpose()? {
                    None => 1,
                    Some(Value::Integer(n)) if n >= 0 => n as usize,
                    Some(v) => {
                        return Err(sql_error(format!(
                            "lag() and lead() need an offset of 0 or more, got {}",
                            v.literal()
                        )));
                    }
                };
                let target =
                    if matches!(w.kind, WindowKind::Lag) { i.checked_sub(offset) } else { i.checked_add(offset) };
                out[part[i]] = match target.filter(|&t| t < m) {
                    Some(t) => arg(0, t)?,
                    None if w.args.len() > 2 => arg(2, i)?,
                    None => Value::Null,
                };
            }
        }
        WindowKind::FirstValue | WindowKind::LastValue | WindowKind::NthValue => {
            for i in 0..m {
                let (lo, hi) = frame(w, i, m, peers[i]);
                let at = match w.kind {
                    WindowKind::FirstValue => Some(lo),
                    WindowKind::LastValue => hi.checked_sub(1),
                    _ => match arg(1, i)? {
                        Value::Integer(n) if n > 0 => lo.checked_add(n as usize - 1),
                        v => {
                            return Err(sql_error(format!(
                                "nth_value() needs a positive integer, got {}",
                                v.literal()
                            )));
                        }
                    },
                };
                out[part[i]] = match at.filter(|&at| lo <= at && at < hi) {
                    Some(at) => arg(0, at)?,
                    None => Value::Null,
                };
            }
        }
        WindowKind::Aggregate(aggregate) => {
            if w.frame.start == FrameBound::UnboundedPreceding {
                // The frame only grows: keep one running total.
                let (mut acc, mut taken, mut last) = (Accumulator::new(aggregate), 0, None);
                for i in 0..m {
                    let (_, hi) = frame(w, i, m, peers[i]);
                    if last.is_none() || hi > taken {
                        while taken < hi {
                            acc.add(aggregate, row(taken), env)?;
                            taken += 1;
                        }
                        last = Some(acc.clone().finish()?);
                    }
                    out[part[i]] = last.clone().unwrap_or(Value::Null);
                }
            } else {
                for i in 0..m {
                    let (lo, hi) = frame(w, i, m, peers[i]);
                    let mut acc = Accumulator::new(aggregate);
                    for j in lo..hi {
                        acc.add(aggregate, row(j), env)?;
                    }
                    out[part[i]] = acc.finish()?;
                }
            }
        }
    }
    Ok(())
}

/// The frame of position `i` of a partition of `m` rows, as a range of
/// positions (empty when it ends before it starts).
fn frame(w: &WindowFunc, i: usize, m: usize, (peers_start, peers_end): (usize, usize)) -> (usize, usize) {
    let frame = w.frame;
    let lo = match frame.start {
        FrameBound::UnboundedPreceding => 0,
        FrameBound::Preceding(n) => i.saturating_sub(n as usize),
        FrameBound::CurrentRow if frame.rows => i,
        FrameBound::CurrentRow => peers_start,
        FrameBound::Following(n) => i.saturating_add(n as usize).min(m),
        FrameBound::UnboundedFollowing => m,
    };
    let hi = match frame.end {
        FrameBound::UnboundedPreceding => 0,
        FrameBound::Preceding(n) => (i + 1).saturating_sub(n as usize),
        FrameBound::CurrentRow if frame.rows => i + 1,
        FrameBound::CurrentRow => peers_end,
        FrameBound::Following(n) => i.saturating_add(n as usize + 1).min(m),
        FrameBound::UnboundedFollowing => m,
    };
    (lo, hi.max(lo))
}
