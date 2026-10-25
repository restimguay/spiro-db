//! Regular expressions, for `regexp_like`, `regexp_replace`, `~` and the
//! rest: a parser for the usual syntax and a Pike VM to run it, which tries
//! every alternative in step rather than backtracking, so matching takes
//! time linear in the text whatever the pattern.
//!
//! Supported: literals, `.`, `[...]` classes (ranges, negation, `\d` and
//! friends, POSIX `[:alpha:]` names), `\d \w \s` and their negations, `\b`
//! and `\B`, `^` and `$`, groups `(...)` and `(?:...)`, `|`, the quantifiers
//! `* + ? {n} {n,} {n,m}` and their lazy forms, and `(?i)` at the start.
//! Matches are leftmost-first, as in Perl and PostgreSQL.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use super::value::sql_error;
use crate::error::Result;

/// The most instructions a pattern may compile to, which bounds `{n,m}`.
const MAX_PROGRAM: usize = 100_000;

#[derive(Clone, Debug)]
enum ClassItem {
    Range(char, char),
    Digit(bool),
    Word(bool),
    Space(bool),
    Posix(&'static str, bool),
}

#[derive(Clone, Debug)]
struct Class {
    items: Vec<ClassItem>,
    negated: bool,
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

impl ClassItem {
    fn matches(&self, c: char) -> bool {
        match *self {
            ClassItem::Range(lo, hi) => lo <= c && c <= hi,
            ClassItem::Digit(negated) => c.is_ascii_digit() != negated,
            ClassItem::Word(negated) => is_word(c) != negated,
            ClassItem::Space(negated) => c.is_whitespace() != negated,
            ClassItem::Posix(name, negated) => {
                let yes = match name {
                    "alpha" => c.is_alphabetic(),
                    "digit" => c.is_ascii_digit(),
                    "alnum" => c.is_alphanumeric(),
                    "upper" => c.is_uppercase(),
                    "lower" => c.is_lowercase(),
                    "space" => c.is_whitespace(),
                    "punct" => c.is_ascii_punctuation(),
                    "xdigit" => c.is_ascii_hexdigit(),
                    "blank" => c == ' ' || c == '\t',
                    "cntrl" => c.is_control(),
                    _ => false,
                };
                yes != negated
            }
        }
    }
}

impl Class {
    fn matches(&self, c: char, fold: bool) -> bool {
        let hit = |c: char| self.items.iter().any(|item| item.matches(c));
        let found = hit(c) || (fold && (c.to_lowercase().any(hit) || c.to_uppercase().any(hit)));
        found != self.negated
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Look {
    Start,
    End,
    WordBoundary,
    NotWordBoundary,
}

#[derive(Clone, Debug)]
enum Node {
    Empty,
    Char(char),
    Any,
    Class(Rc<Class>),
    Look(Look),
    /// A group, capturing as group `n` if numbered.
    Group(Box<Node>, Option<usize>),
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    Repeat {
        node: Box<Node>,
        min: u32,
        max: Option<u32>,
        greedy: bool,
    },
}

struct Parser<'a> {
    chars: Vec<char>,
    pos: usize,
    groups: usize,
    pattern: &'a str,
}

impl Parser<'_> {
    fn error(&self, what: &str) -> crate::error::Error {
        sql_error(format!("invalid regular expression {:?}: {what}", self.pattern))
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn eat(&mut self, c: char) -> bool {
        let found = self.peek() == Some(c);
        self.pos += found as usize;
        found
    }

    fn alternation(&mut self) -> Result<Node> {
        let mut branches = vec![self.concat()?];
        while self.eat('|') {
            branches.push(self.concat()?);
        }
        Ok(if branches.len() == 1 { branches.pop().unwrap() } else { Node::Alt(branches) })
    }

    fn concat(&mut self) -> Result<Node> {
        let mut items = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            let atom = self.atom()?;
            items.push(self.quantified(atom)?);
        }
        Ok(match items.len() {
            0 => Node::Empty,
            1 => items.pop().unwrap(),
            _ => Node::Concat(items),
        })
    }

    fn number(&mut self) -> Option<u32> {
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
        }
        self.chars[start..self.pos].iter().collect::<String>().parse().ok()
    }

    fn quantified(&mut self, atom: Node) -> Result<Node> {
        let mut node = atom;
        loop {
            let (min, max) = match self.peek() {
                Some('*') => (0, None),
                Some('+') => (1, None),
                Some('?') => (0, Some(1)),
                Some('{') => {
                    // {n}, {n,} or {n,m}; anything else is a literal brace.
                    let save = self.pos;
                    self.pos += 1;
                    let Some(min) = self.number() else {
                        self.pos = save;
                        return Ok(node);
                    };
                    let max = if self.eat(',') { self.number() } else { Some(min) };
                    if !self.eat('}') {
                        self.pos = save;
                        return Ok(node);
                    }
                    self.pos -= 1; // stepped over below
                    if max.is_some_and(|max| max < min) {
                        return Err(self.error("a {n,m} with m below n"));
                    }
                    (min, max)
                }
                _ => return Ok(node),
            };
            self.pos += 1;
            if matches!(node, Node::Look(_) | Node::Empty) {
                return Err(self.error("nothing to repeat"));
            }
            let greedy = !self.eat('?');
            node = Node::Repeat { node: Box::new(node), min, max, greedy };
        }
    }

    fn atom(&mut self) -> Result<Node> {
        let c = self.peek().expect("atom at end");
        self.pos += 1;
        Ok(match c {
            '.' => Node::Any,
            '^' => Node::Look(Look::Start),
            '$' => Node::Look(Look::End),
            '(' => {
                let capture = if self.eat('?') {
                    if !self.eat(':') {
                        return Err(self.error("only (?:...) groups and a leading (?i) are supported"));
                    }
                    None
                } else {
                    self.groups += 1;
                    Some(self.groups)
                };
                let inner = self.alternation()?;
                if !self.eat(')') {
                    return Err(self.error("unclosed group"));
                }
                Node::Group(Box::new(inner), capture)
            }
            ')' => return Err(self.error("unmatched )")),
            '[' => Node::Class(Rc::new(self.class()?)),
            '\\' => self.escape(false)?,
            '*' | '+' | '?' => return Err(self.error("nothing to repeat")),
            c => Node::Char(c),
        })
    }

    /// After a backslash: a class shorthand, an assertion, or a literal.
    fn escape(&mut self, in_class: bool) -> Result<Node> {
        let c = self.peek().ok_or_else(|| self.error("trailing backslash"))?;
        self.pos += 1;
        let class = |item| Node::Class(Rc::new(Class { items: vec![item], negated: false }));
        Ok(match c {
            'd' => class(ClassItem::Digit(false)),
            'D' => class(ClassItem::Digit(true)),
            'w' => class(ClassItem::Word(false)),
            'W' => class(ClassItem::Word(true)),
            's' => class(ClassItem::Space(false)),
            'S' => class(ClassItem::Space(true)),
            'b' if !in_class => Node::Look(Look::WordBoundary),
            'B' if !in_class => Node::Look(Look::NotWordBoundary),
            'A' if !in_class => Node::Look(Look::Start),
            'z' | 'Z' if !in_class => Node::Look(Look::End),
            'n' => Node::Char('\n'),
            't' => Node::Char('\t'),
            'r' => Node::Char('\r'),
            '0'..='9' => return Err(self.error("backreferences are not supported")),
            c if c.is_alphanumeric() => return Err(self.error(&format!("unknown escape \\{c}"))),
            c => Node::Char(c),
        })
    }

    fn class(&mut self) -> Result<Class> {
        let negated = self.eat('^');
        let mut items = Vec::new();
        let mut first = true;
        loop {
            let c = self.peek().ok_or_else(|| self.error("unclosed ["))?;
            if c == ']' && !first {
                self.pos += 1;
                break;
            }
            first = false;
            // [:name:]
            if c == '[' && self.chars.get(self.pos + 1) == Some(&':') {
                let rest: String = self.chars[self.pos + 2..].iter().collect();
                let end = rest.find(":]").ok_or_else(|| self.error("unclosed [:name:]"))?;
                let name = &rest[..end];
                let known = ["alpha", "digit", "alnum", "upper", "lower", "space", "punct", "xdigit", "blank", "cntrl"];
                let name = known.into_iter().find(|k| *k == name).ok_or_else(|| self.error("unknown [:class:]"))?;
                items.push(ClassItem::Posix(name, false));
                self.pos += 2 + end + 2;
                continue;
            }
            self.pos += 1;
            let low = if c == '\\' {
                match self.escape(true)? {
                    Node::Char(c) => c,
                    Node::Class(class) => {
                        items.extend(class.items.iter().cloned());
                        continue;
                    }
                    _ => unreachable!("only characters and classes inside []"),
                }
            } else {
                c
            };
            if self.peek() == Some('-') && self.chars.get(self.pos + 1).is_some_and(|&n| n != ']') {
                self.pos += 1;
                let mut high = self.peek().unwrap();
                self.pos += 1;
                if high == '\\' {
                    match self.escape(true)? {
                        Node::Char(c) => high = c,
                        _ => return Err(self.error("a range must end in a character")),
                    }
                }
                if high < low {
                    return Err(self.error("a range out of order"));
                }
                items.push(ClassItem::Range(low, high));
            } else {
                items.push(ClassItem::Range(low, low));
            }
        }
        Ok(Class { items, negated })
    }
}

#[derive(Clone, Debug)]
enum Inst {
    Char(char),
    Any,
    Class(Rc<Class>),
    Look(Look),
    Save(usize),
    /// Try the first target, then the second (lower priority).
    Split(usize, usize),
    Jmp(usize),
    Match,
}

struct Compiler {
    prog: Vec<Inst>,
}

impl Compiler {
    fn emit(&mut self, inst: Inst) -> Result<usize> {
        if self.prog.len() >= MAX_PROGRAM {
            return Err(sql_error("regular expression is too large"));
        }
        self.prog.push(inst);
        Ok(self.prog.len() - 1)
    }

    fn node(&mut self, node: &Node) -> Result<()> {
        match node {
            Node::Empty => {}
            Node::Char(c) => {
                self.emit(Inst::Char(*c))?;
            }
            Node::Any => {
                self.emit(Inst::Any)?;
            }
            Node::Class(class) => {
                self.emit(Inst::Class(class.clone()))?;
            }
            Node::Look(look) => {
                self.emit(Inst::Look(*look))?;
            }
            Node::Group(inner, capture) => {
                if let Some(n) = capture {
                    self.emit(Inst::Save(2 * n))?;
                }
                self.node(inner)?;
                if let Some(n) = capture {
                    self.emit(Inst::Save(2 * n + 1))?;
                }
            }
            Node::Concat(items) => items.iter().try_for_each(|n| self.node(n))?,
            Node::Alt(branches) => {
                let mut jumps = Vec::new();
                for (i, branch) in branches.iter().enumerate() {
                    if i + 1 < branches.len() {
                        let split = self.emit(Inst::Split(0, 0))?;
                        self.node(branch)?;
                        jumps.push(self.emit(Inst::Jmp(0))?);
                        let next = self.prog.len();
                        self.prog[split] = Inst::Split(split + 1, next);
                    } else {
                        self.node(branch)?;
                    }
                }
                let end = self.prog.len();
                for j in jumps {
                    self.prog[j] = Inst::Jmp(end);
                }
            }
            Node::Repeat { node, min, max, greedy } => {
                for _ in 0..*min {
                    self.node(node)?;
                }
                let split = |body: usize, out: usize| {
                    if *greedy { Inst::Split(body, out) } else { Inst::Split(out, body) }
                };
                match max {
                    None => {
                        // L: split body, out; body; jmp L
                        let start = self.emit(Inst::Split(0, 0))?;
                        self.node(node)?;
                        self.emit(Inst::Jmp(start))?;
                        let out = self.prog.len();
                        self.prog[start] = split(start + 1, out);
                    }
                    Some(max) => {
                        // Each optional copy: split body, end; body.
                        let mut splits = Vec::new();
                        for _ in *min..*max {
                            splits.push(self.emit(Inst::Split(0, 0))?);
                            self.node(node)?;
                        }
                        let out = self.prog.len();
                        for s in splits {
                            self.prog[s] = split(s + 1, out);
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// A compiled regular expression.
pub struct Regex {
    prog: Vec<Inst>,
    /// Capture slots: two per group, group 0 being the whole match.
    slots: usize,
    fold: bool,
}

/// Where a match and its groups lie, as character positions.
pub type Captures = Vec<Option<usize>>;

impl Regex {
    pub fn new(pattern: &str, case_insensitive: bool) -> Result<Regex> {
        let (pattern_body, fold) = match pattern.strip_prefix("(?i)") {
            Some(rest) => (rest, true),
            None => (pattern, case_insensitive),
        };
        let mut parser = Parser { chars: pattern_body.chars().collect(), pos: 0, groups: 0, pattern };
        let tree = parser.alternation()?;
        if parser.pos < parser.chars.len() {
            return Err(parser.error("unmatched )"));
        }
        let mut compiler = Compiler { prog: Vec::new() };
        compiler.emit(Inst::Save(0))?;
        compiler.node(&tree)?;
        compiler.emit(Inst::Save(1))?;
        compiler.emit(Inst::Match)?;
        Ok(Regex { prog: compiler.prog, slots: 2 * (parser.groups + 1), fold })
    }

    /// The first match starting at or after `start`, with its groups.
    pub fn find_at(&self, text: &[char], start: usize) -> Option<Captures> {
        let n = self.prog.len();
        let mut current: Vec<(usize, Captures)> = Vec::new();
        let mut next: Vec<(usize, Captures)> = Vec::new();
        let mut seen = vec![usize::MAX; n];
        let mut matched: Option<Captures> = None;
        let mut pos = start;
        loop {
            // A new attempt starts here, behind every attempt already running.
            if matched.is_none() {
                self.add(&mut current, &mut seen, pos, 0, vec![None; self.slots], text);
            }
            // Nothing running and nothing more to start: done.
            if current.is_empty() && matched.is_some() {
                break;
            }
            let c = text.get(pos).copied();
            for (pc, caps) in current.drain(..) {
                let advance = match &self.prog[pc] {
                    Inst::Match => {
                        matched = Some(caps);
                        // Attempts behind this one have lower priority.
                        break;
                    }
                    Inst::Char(want) => c.is_some_and(|c| self.same(c, *want)),
                    Inst::Any => c.is_some(),
                    Inst::Class(class) => c.is_some_and(|c| class.matches(c, self.fold)),
                    _ => false,
                };
                if advance {
                    self.add(&mut next, &mut seen, pos + 1, pc + 1, caps, text);
                }
            }
            if pos >= text.len() {
                break;
            }
            pos += 1;
            std::mem::swap(&mut current, &mut next);
            next.clear();
        }
        matched
    }

    fn same(&self, c: char, want: char) -> bool {
        c == want || (self.fold && c.to_lowercase().eq(want.to_lowercase()))
    }

    /// Adds the thread at `pc`, following jumps, splits, saves and
    /// assertions, in priority order. `seen` marks what this step added.
    fn add(
        &self,
        list: &mut Vec<(usize, Captures)>,
        seen: &mut [usize],
        pos: usize,
        pc: usize,
        mut caps: Captures,
        text: &[char],
    ) {
        if seen[pc] == pos {
            return;
        }
        seen[pc] = pos;
        match &self.prog[pc] {
            Inst::Jmp(to) => self.add(list, seen, pos, *to, caps, text),
            Inst::Split(a, b) => {
                self.add(list, seen, pos, *a, caps.clone(), text);
                self.add(list, seen, pos, *b, caps, text);
            }
            Inst::Save(slot) => {
                caps[*slot] = Some(pos);
                self.add(list, seen, pos, pc + 1, caps, text);
            }
            Inst::Look(look) => {
                let before = pos.checked_sub(1).and_then(|p| text.get(p)).is_some_and(|&c| is_word(c));
                let after = text.get(pos).is_some_and(|&c| is_word(c));
                let holds = match look {
                    Look::Start => pos == 0,
                    Look::End => pos == text.len(),
                    Look::WordBoundary => before != after,
                    Look::NotWordBoundary => before == after,
                };
                if holds {
                    self.add(list, seen, pos, pc + 1, caps, text);
                }
            }
            _ => list.push((pc, caps)),
        }
    }

    #[cfg(test)]
    pub fn is_match(&self, text: &str) -> bool {
        let chars: Vec<char> = text.chars().collect();
        self.find_at(&chars, 0).is_some()
    }
}

thread_local! {
    /// Recently used patterns, compiled, so a query doesn't recompile its
    /// pattern for every row.
    static CACHE: RefCell<HashMap<(String, bool), Rc<Regex>>> = RefCell::new(HashMap::new());
}

/// `pattern` compiled, from the cache when it was used lately.
pub fn compiled(pattern: &str, case_insensitive: bool) -> Result<Rc<Regex>> {
    let key = (pattern.to_string(), case_insensitive);
    if let Some(regex) = CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return Ok(regex);
    }
    let regex = Rc::new(Regex::new(pattern, case_insensitive)?);
    CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        if cache.len() >= 64 {
            cache.clear();
        }
        cache.insert(key, regex.clone());
    });
    Ok(regex)
}

/// The text of `replacement` for one match: `\1`..`\9` are groups, `\&` (or
/// `\0`) the whole match, and `\\` a backslash.
pub fn expand(replacement: &str, text: &[char], caps: &Captures, out: &mut String) {
    let group = |n: usize, out: &mut String| {
        if let (Some(Some(s)), Some(Some(e))) = (caps.get(2 * n), caps.get(2 * n + 1)) {
            out.extend(&text[*s..*e]);
        }
    };
    let mut chars = replacement.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some(d @ '0'..='9') => group(d as usize - '0' as usize, out),
            Some('&') => group(0, out),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
}

/// SQLite's GLOB: `*` matches any run of characters, `?` exactly one, and
/// `[...]` one from a set (`[^...]` one not in it). Case matters.
pub fn glob(text: &str, pattern: &str) -> bool {
    let (t, p): (Vec<char>, Vec<char>) = (text.chars().collect(), pattern.chars().collect());
    fn class_end(p: &[char], at: usize) -> Option<usize> {
        // `at` is just past '['; a ']' right away is a member.
        let mut i = at;
        if p.get(i) == Some(&'^') {
            i += 1;
        }
        if p.get(i) == Some(&']') {
            i += 1;
        }
        while i < p.len() && p[i] != ']' {
            i += 1;
        }
        (i < p.len()).then_some(i)
    }
    fn in_class(set: &[char], c: char) -> bool {
        let (negated, set) = match set.first() {
            Some('^') => (true, &set[1..]),
            _ => (false, set),
        };
        let mut i = 0;
        let mut found = false;
        while i < set.len() {
            if i + 2 < set.len() && set[i + 1] == '-' {
                found |= set[i] <= c && c <= set[i + 2];
                i += 3;
            } else {
                found |= set[i] == c;
                i += 1;
            }
        }
        found != negated
    }
    fn matches(t: &[char], p: &[char]) -> bool {
        let (mut ti, mut pi) = (0, 0);
        let mut star: Option<(usize, usize)> = None;
        while ti < t.len() {
            let step = match p.get(pi) {
                Some('*') => {
                    star = Some((pi, ti));
                    pi += 1;
                    continue;
                }
                Some('?') => Some(pi + 1),
                Some('[') => class_end(p, pi + 1).filter(|&end| in_class(&p[pi + 1..end], t[ti])).map(|end| end + 1),
                Some(&c) if c == t[ti] => Some(pi + 1),
                _ => None,
            };
            match (step, star) {
                (Some(next), _) => {
                    pi = next;
                    ti += 1;
                }
                (None, Some((s, from))) => {
                    pi = s + 1;
                    ti = from + 1;
                    star = Some((s, from + 1));
                }
                (None, None) => return false,
            }
        }
        p[pi..].iter().all(|&c| c == '*')
    }
    matches(&t, &p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(pattern: &str, text: &str) -> Option<String> {
        let chars: Vec<char> = text.chars().collect();
        let caps = Regex::new(pattern, false).unwrap().find_at(&chars, 0)?;
        Some(chars[caps[0].unwrap()..caps[1].unwrap()].iter().collect())
    }

    #[test]
    fn matching() {
        assert_eq!(find("b+", "aabbbc").as_deref(), Some("bbb"));
        assert_eq!(find("b+?", "aabbbc").as_deref(), Some("b"));
        assert_eq!(find("^a.c$", "abc").as_deref(), Some("abc"));
        assert_eq!(find("^a.c$", "abcd"), None);
        assert_eq!(find("cat|category", "category").as_deref(), Some("cat"));
        assert_eq!(find("(\\d{3})-(\\d{4})", "call 555-1234 now").as_deref(), Some("555-1234"));
        assert_eq!(find("[[:alpha:]]+", "12 abc 3").as_deref(), Some("abc"));
        assert_eq!(find("\\bis\\b", "this is it").as_deref(), Some("is"));
        assert_eq!(find("[^a-c]+", "abcdef").as_deref(), Some("def"));
        assert_eq!(find("x{2,3}", "xxxxx").as_deref(), Some("xxx"));
        assert_eq!(find("a{2}b?", "aaab").as_deref(), Some("aa"));
        assert_eq!(find("(?:ab)*c", "ababc").as_deref(), Some("ababc"));
        assert_eq!(find("é.", "café!").as_deref(), Some("é!"));
        assert!(Regex::new("(?i)HELLO", false).unwrap().is_match("say hello"));
        assert!(Regex::new("[a-z]+", true).unwrap().is_match("ABC"));
        // Linear time: this would take ages to backtrack.
        let long = "a".repeat(5000);
        assert!(!Regex::new("(a*)*b", false).unwrap().is_match(&long));
        for bad in ["(", ")", "[a", "*a", "a{3,1}", "\\1", "\\q"] {
            assert!(Regex::new(bad, false).is_err(), "{bad}");
        }
    }

    #[test]
    fn replacement_and_globs() {
        let text: Vec<char> = "john smith".chars().collect();
        let caps = Regex::new("(\\w+) (\\w+)", false).unwrap().find_at(&text, 0).unwrap();
        let mut out = String::new();
        expand("\\2, \\1 (\\&)", &text, &caps, &mut out);
        assert_eq!(out, "smith, john (john smith)");
        assert!(glob("hello.txt", "*.txt"));
        assert!(glob("a1", "[a-c][0-9]"));
        assert!(!glob("d1", "[a-c][0-9]"));
        assert!(glob("x", "[^a-c]"));
        assert!(!glob("Hello", "hello"));
        assert!(glob("abc", "a?c"));
    }
}
