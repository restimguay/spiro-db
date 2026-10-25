//! A sorted map from byte-string keys to byte-string values (or deletion
//! markers), built for the memtable.
//!
//! The standard `BTreeMap` searches each node from the left. Keys that
//! arrive in increasing order, the common case for tables with growing
//! primary keys, always land at the right edge, so every insert scans every
//! node on the way down. This B+tree differs in four ways:
//!
//! - nodes are searched by bisection, and hold up to 64 entries;
//! - it remembers the last few leaves it inserted into (its *fingers*), and
//!   a key that belongs in one of them skips the search from the root. There
//!   are several because writes interleave: a table's row keys and its index
//!   keys grow in different parts of the tree;
//! - leaves are linked, so ordered iteration walks from leaf to leaf;
//! - each leaf keeps its values back to back in one buffer, rather than one
//!   allocation per value: storing a value allocates nothing most of the
//!   time, and a scan reads a leaf's values from contiguous memory.
//!
//! Entries are never removed (the memtable records deletions as values), so
//! nodes only ever split. Nodes live in a vector and refer to each other by
//! index.

use std::ops::Bound;

use crate::coding::{Key, key_cmp};

/// Most entries a node holds before it splits.
const CAPACITY: usize = 64;
const NONE: u32 = u32::MAX;
const FINGERS: usize = 4;

/// Where a value lies in its leaf's buffer; a deletion has no bytes.
#[derive(Clone, Copy)]
struct Span {
    start: u32,
    len: u32,
}

const DELETED: u32 = u32::MAX;

impl Span {
    fn len(self) -> usize {
        if self.len == DELETED { 0 } else { self.len as usize }
    }
}

enum Node {
    Leaf {
        keys: Vec<Key>,
        spans: Vec<Span>,
        /// The values, back to back. Replaced values stay until the leaf
        /// next compacts; `garbage` counts their bytes.
        data: Vec<u8>,
        garbage: usize,
        /// The leaf with the next keys, or `NONE`.
        next: u32,
    },
    /// `keys[i]` is the smallest key under `children[i + 1]`.
    Inner { keys: Vec<Key>, children: Vec<u32> },
}

fn value(data: &[u8], span: Span) -> Option<&[u8]> {
    (span.len != DELETED).then(|| &data[span.start as usize..span.start as usize + span.len as usize])
}

/// Appends `value` to a leaf buffer and says where it went.
fn store(data: &mut Vec<u8>, value: Option<&[u8]>) -> Span {
    match value {
        None => Span { start: 0, len: DELETED },
        Some(v) => {
            let start = data.len() as u32;
            data.extend_from_slice(v);
            Span { start, len: v.len() as u32 }
        }
    }
}

/// A fresh buffer holding just the values `spans` point to, re-pointed.
fn compacted(data: &[u8], spans: &mut [Span]) -> Vec<u8> {
    let mut out = Vec::with_capacity(spans.iter().map(|s| s.len()).sum());
    for span in spans {
        *span = store(&mut out, value(data, *span));
    }
    out
}

pub struct KeyMap {
    nodes: Vec<Node>,
    root: u32,
    first_leaf: u32,
    /// Leaves recently inserted into, and which to replace next.
    fingers: [u32; FINGERS],
    next_finger: usize,
    len: usize,
}

impl Default for KeyMap {
    fn default() -> Self {
        let leaf = Node::Leaf { keys: Vec::new(), spans: Vec::new(), data: Vec::new(), garbage: 0, next: NONE };
        KeyMap { nodes: vec![leaf], root: 0, first_leaf: 0, fingers: [0; FINGERS], next_finger: 0, len: 0 }
    }
}

/// The position of `key` in sorted `keys`: `Ok` if present, else where it goes.
fn search(keys: &[Key], key: &[u8]) -> Result<usize, usize> {
    keys.binary_search_by(|k| key_cmp(k.as_slice(), key))
}

impl KeyMap {
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn leaf_keys(&self, leaf: u32) -> &[Key] {
        match &self.nodes[leaf as usize] {
            Node::Leaf { keys, .. } => keys,
            Node::Inner { .. } => unreachable!("not a leaf"),
        }
    }

    /// Whether `key` belongs in `leaf`: at or after its first key (or it is
    /// the first leaf), and before the next leaf's first key.
    fn belongs_in(&self, leaf: u32, key: &[u8]) -> bool {
        let Node::Leaf { keys, next, .. } = &self.nodes[leaf as usize] else { return false };
        let after_start = leaf == self.first_leaf || keys.first().is_some_and(|k| key_cmp(k.as_slice(), key).is_le());
        let before_end = *next == NONE || key_cmp(key, self.leaf_keys(*next)[0].as_slice()).is_lt();
        after_start && before_end
    }

    /// The leaf where `key` is or would be, and whether a finger found it.
    fn find_leaf(&self, key: &[u8]) -> (u32, bool) {
        if let Some(&leaf) = self.fingers.iter().find(|&&leaf| self.belongs_in(leaf, key)) {
            return (leaf, true);
        }
        let mut node = self.root;
        loop {
            match &self.nodes[node as usize] {
                Node::Leaf { .. } => return (node, false),
                Node::Inner { keys, children } => {
                    let i = keys.partition_point(|k| key_cmp(k.as_slice(), key).is_le());
                    node = children[i];
                }
            }
        }
    }

    /// `None` if the key isn't here; `Some(None)` if it is a deletion.
    pub fn get(&self, key: &[u8]) -> Option<Option<&[u8]>> {
        let Node::Leaf { keys, spans, data, .. } = &self.nodes[self.find_leaf(key).0 as usize] else { unreachable!() };
        search(keys, key).ok().map(|i| value(data, spans[i]))
    }

    /// Inserts or replaces a value (`None` records a deletion). Returns
    /// `None` for a new key, or the length of the value replaced (`Some(None)`
    /// if that was a deletion).
    pub fn insert(&mut self, key: &[u8], new: Option<&[u8]>) -> Option<Option<usize>> {
        let (leaf, fingered) = self.find_leaf(key);
        if !fingered {
            self.fingers[self.next_finger] = leaf;
            self.next_finger = (self.next_finger + 1) % FINGERS;
        }
        let Node::Leaf { keys, spans, data, garbage, .. } = &mut self.nodes[leaf as usize] else { unreachable!() };
        match search(keys, key) {
            Ok(i) => {
                let old = spans[i];
                *garbage += old.len();
                spans[i] = store(data, new);
                if *garbage > 4096 && *garbage * 2 > data.len() {
                    *data = compacted(data, spans);
                    *garbage = 0;
                }
                return Some((old.len != DELETED).then(|| old.len()));
            }
            Err(i) => {
                keys.insert(i, Key::new(key));
                let span = store(data, new);
                spans.insert(i, span);
            }
        }
        self.len += 1;
        if keys.len() > CAPACITY {
            self.split(key);
        }
        None
    }

    /// Splits the overfull leaf holding `key`, and any ancestors that
    /// overflow in turn.
    fn split(&mut self, key: &[u8]) {
        // The path from the root to the leaf.
        let mut path = vec![self.root];
        while let Node::Inner { keys, children } = &self.nodes[*path.last().unwrap() as usize] {
            path.push(children[keys.partition_point(|k| key_cmp(k.as_slice(), key).is_le())]);
        }
        let mut carry: Option<(Key, u32)> = None; // separator and new right sibling
        while let Some(node) = path.pop() {
            if let Some((separator, right)) = carry.take() {
                let Node::Inner { keys, children } = &mut self.nodes[node as usize] else { unreachable!() };
                let i = keys.partition_point(|k| key_cmp(k.as_slice(), separator.as_slice()).is_le());
                keys.insert(i, separator);
                children.insert(i + 1, right);
            }
            let new_index = self.nodes.len() as u32;
            let sibling = match &mut self.nodes[node as usize] {
                Node::Leaf { keys, spans, data, garbage, next } if keys.len() > CAPACITY => {
                    let half = keys.len() / 2;
                    let right_keys = keys.split_off(half);
                    let mut right_spans = spans.split_off(half);
                    let right_data = compacted(data, &mut right_spans);
                    *data = compacted(data, spans);
                    *garbage = 0;
                    let separator = right_keys[0].clone();
                    let right_next = std::mem::replace(next, new_index);
                    Some((
                        separator,
                        Node::Leaf {
                            keys: right_keys,
                            spans: right_spans,
                            data: right_data,
                            garbage: 0,
                            next: right_next,
                        },
                    ))
                }
                Node::Inner { keys, children } if keys.len() > CAPACITY => {
                    let half = keys.len() / 2;
                    let mut right_keys = keys.split_off(half);
                    let separator = right_keys.remove(0);
                    let right_children = children.split_off(half + 1);
                    Some((separator, Node::Inner { keys: right_keys, children: right_children }))
                }
                _ => None,
            };
            let Some((separator, right)) = sibling else { return };
            self.nodes.push(right);
            carry = Some((separator, new_index));
        }
        // The root itself split: grow the tree by a level.
        let (separator, right) = carry.expect("a split reached the root");
        self.nodes.push(Node::Inner { keys: vec![separator], children: vec![self.root, right] });
        self.root = self.nodes.len() as u32 - 1;
    }

    /// Entries in key order, starting at `from`.
    pub fn range_from(&self, from: Bound<&[u8]>) -> Iter<'_> {
        let (leaf, pos) = match from {
            Bound::Unbounded => (self.first_leaf, 0),
            Bound::Included(key) | Bound::Excluded(key) => {
                let leaf = self.find_leaf(key).0;
                let pos = match search(self.leaf_keys(leaf), key) {
                    Ok(i) if matches!(from, Bound::Excluded(_)) => i + 1,
                    Ok(i) | Err(i) => i,
                };
                (leaf, pos)
            }
        };
        Iter { map: self, leaf, pos }
    }

    pub fn iter(&self) -> Iter<'_> {
        self.range_from(Bound::Unbounded)
    }
}

pub struct Iter<'a> {
    map: &'a KeyMap,
    leaf: u32,
    pos: usize,
}

impl<'a> Iterator for Iter<'a> {
    type Item = (&'a [u8], Option<&'a [u8]>);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.leaf == NONE {
                return None;
            }
            let Node::Leaf { keys, spans, data, next, .. } = &self.map.nodes[self.leaf as usize] else {
                unreachable!()
            };
            if self.pos < keys.len() {
                self.pos += 1;
                return Some((keys[self.pos - 1].as_slice(), value(data, spans[self.pos - 1])));
            }
            (self.leaf, self.pos) = (*next, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn matches_btreemap_for_any_insertion_order() {
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut rand = |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n
        };
        for order in 0..3 {
            let (mut ours, mut model) = (KeyMap::default(), BTreeMap::<Vec<u8>, Option<Vec<u8>>>::new());
            for i in 0..20_000u64 {
                let k = match order {
                    0 => i,           // increasing: the fingers' case
                    1 => 20_000 - i,  // decreasing
                    _ => rand(5_000), // random, with many overwrites
                };
                // Mixed lengths, some longer than a key stores inline.
                let key = format!("{k:06}{}", "x".repeat((k % 40) as usize)).into_bytes();
                // Values of varying sizes, and deletions.
                let value = (rand(7) != 0).then(|| format!("{i}-{}", "v".repeat(rand(200) as usize)).into_bytes());
                let old = model.insert(key.clone(), value.clone());
                assert_eq!(ours.insert(&key, value.as_deref()), old.map(|v| v.map(|v| v.len())));
            }
            assert_eq!(ours.len(), model.len());
            let all: Vec<_> = ours.iter().map(|(k, v)| (k.to_vec(), v.map(<[u8]>::to_vec))).collect();
            assert_eq!(all, model.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>());
            for _ in 0..2_000 {
                let probe = format!("{:06}", rand(21_000)).into_bytes();
                assert_eq!(ours.get(&probe), model.get(&probe).map(|v| v.as_deref()));
                for bound in [Bound::Included(probe.as_slice()), Bound::Excluded(probe.as_slice())] {
                    let got: Vec<_> =
                        ours.range_from(bound).take(5).map(|(k, v)| (k.to_vec(), v.map(<[u8]>::to_vec))).collect();
                    let want: Vec<_> = model
                        .range::<[u8], _>((bound, Bound::Unbounded))
                        .take(5)
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    assert_eq!(got, want);
                }
            }
        }
    }
}
