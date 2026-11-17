//! POSIX regular expressions: basic (BRE) and extended (ERE) syntax, `[...]` sets with classes, ranges and negation, groups with
//! captures, alternation, `* + ? {m,n}`, back-references, anchors, and the GNU extensions BusyBox and ported software use
//! (`\w \W \s \S \b \< \>` and, in BRE, `\| \+ \?`). Matching is a backtracking search for the **leftmost, longest** match, bounded by a
//! step budget so a hostile pattern cannot run for ever. Bytes, not characters (the C locale).
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

#[derive(Debug, Clone, Copy, Default)]
pub struct Flags {
    pub extended: bool,
    pub icase: bool,
    pub newline: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    BadPattern,
    BadBrace,
    BadRepeat,
    UnmatchedBracket,
    UnmatchedParen,
    UnmatchedBrace,
    BadBackref,
    BadRange,
    BadClass,
    TrailingBackslash,
    TooBig,
}

#[derive(Debug, Clone)]
enum Node {
    Char(u8),
    Any,
    Set(Box<[bool; 256]>),
    Bol,
    Eol,
    WordB,
    WordStart,
    WordEnd,
    Group(Box<Node>, usize),
    Alt(Vec<Node>),
    Cat(Vec<Node>),
    Repeat(Box<Node>, u32, Option<u32>),
    Backref(usize),
}

/// The extent of the whole match and of each group.
pub type Captures = Vec<Option<(usize, usize)>>;

#[derive(Debug, Clone)]
pub struct Regex {
    root: Node,
    groups: usize,
    flags: Flags,
}

const MAX_DEPTH: usize = 64;
const MAX_REPEAT: u32 = 1000;
const STEP_BUDGET: usize = 2_000_000;

struct Parser<'a> {
    p: &'a [u8],
    i: usize,
    flags: Flags,
    groups: usize,
    depth: usize,
}

fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

fn class(name: &[u8]) -> Option<fn(u8) -> bool> {
    Some(match name {
        b"alpha" => |c| c.is_ascii_alphabetic(),
        b"digit" => |c| c.is_ascii_digit(),
        b"alnum" => |c| c.is_ascii_alphanumeric(),
        b"upper" => |c| c.is_ascii_uppercase(),
        b"lower" => |c| c.is_ascii_lowercase(),
        b"space" => |c| c.is_ascii_whitespace() || c == 0x0b,
        b"blank" => |c| c == b' ' || c == b'\t',
        b"punct" => |c| c.is_ascii_punctuation(),
        b"print" => |c| (0x20..0x7f).contains(&c),
        b"graph" => |c| (0x21..0x7f).contains(&c),
        b"cntrl" => |c| c < 0x20 || c == 0x7f,
        b"xdigit" => |c| c.is_ascii_hexdigit(),
        _ => return None,
    })
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.p.get(self.i).copied()
    }
    fn at(&self, s: &[u8]) -> bool {
        self.p[self.i..].starts_with(s)
    }

    fn alternation(&mut self) -> Result<Node, Error> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(Error::TooBig);
        }
        let mut alts = vec![self.concat()?];
        loop {
            if self.flags.extended && self.peek() == Some(b'|') {
                self.i += 1;
            } else if !self.flags.extended && self.at(b"\\|") {
                self.i += 2;
            } else {
                break;
            }
            alts.push(self.concat()?);
        }
        self.depth -= 1;
        Ok(if alts.len() == 1 {
            alts.pop().unwrap()
        } else {
            Node::Alt(alts)
        })
    }

    fn at_branch_end(&self) -> bool {
        match self.peek() {
            None => true,
            Some(b'|') if self.flags.extended => true,
            Some(b')') if self.flags.extended && self.depth > 1 => true,
            Some(b'\\') if !self.flags.extended => {
                self.at(b"\\|") || (self.at(b"\\)") && self.depth > 1)
            }
            _ => false,
        }
    }

    fn concat(&mut self) -> Result<Node, Error> {
        let mut items: Vec<Node> = Vec::new();
        let start = self.i;
        while !self.at_branch_end() {
            let first = self.i == start;
            let atom = self.atom(first)?;
            let atom = self.repeats(atom)?;
            items.push(atom);
        }
        Ok(Node::Cat(items))
    }

    fn repeats(&mut self, mut atom: Node) -> Result<Node, Error> {
        loop {
            let (min, max) = if self.peek() == Some(b'*') {
                self.i += 1;
                (0, None)
            } else if self.flags.extended && self.peek() == Some(b'+') {
                self.i += 1;
                (1, None)
            } else if self.flags.extended && self.peek() == Some(b'?') {
                self.i += 1;
                (0, Some(1))
            } else if !self.flags.extended && self.at(b"\\+") {
                self.i += 2;
                (1, None)
            } else if !self.flags.extended && self.at(b"\\?") {
                self.i += 2;
                (0, Some(1))
            } else if (self.flags.extended && self.peek() == Some(b'{'))
                || (!self.flags.extended && self.at(b"\\{"))
            {
                self.i += if self.flags.extended { 1 } else { 2 };
                self.interval()?
            } else {
                return Ok(atom);
            };
            atom = Node::Repeat(Box::new(atom), min, max);
        }
    }

    fn number(&mut self) -> Option<u32> {
        let s = self.i;
        let mut n: u32 = 0;
        while let Some(c) = self.peek().filter(u8::is_ascii_digit) {
            n = n.saturating_mul(10).saturating_add(u32::from(c - b'0'));
            self.i += 1;
        }
        (self.i > s).then_some(n)
    }

    fn interval(&mut self) -> Result<(u32, Option<u32>), Error> {
        let min = self.number().ok_or(Error::BadBrace)?;
        let max = if self.peek() == Some(b',') {
            self.i += 1;
            self.number()
        } else {
            Some(min)
        };
        let close: &[u8] = if self.flags.extended { b"}" } else { b"\\}" };
        if !self.at(close) {
            return Err(Error::UnmatchedBrace);
        }
        self.i += close.len();
        if min > MAX_REPEAT || max.is_some_and(|m| m > MAX_REPEAT || m < min) {
            return Err(Error::BadBrace);
        }
        Ok((min, max))
    }

    fn atom(&mut self, first: bool) -> Result<Node, Error> {
        let c = self.peek().ok_or(Error::BadPattern)?;
        let ere = self.flags.extended;
        match c {
            b'.' => {
                self.i += 1;
                Ok(Node::Any)
            }
            b'[' => self.bracket(),
            b'^' if ere || first => {
                self.i += 1;
                Ok(Node::Bol)
            }
            b'$' if ere || self.i + 1 == self.p.len() || self.at(b"$\\)") || self.at(b"$\\|") => {
                self.i += 1;
                Ok(Node::Eol)
            }
            b'*' if first || (!ere && false) => {
                if ere {
                    return Err(Error::BadRepeat);
                }
                self.i += 1;
                Ok(Node::Char(b'*'))
            }
            b'(' if ere => {
                self.i += 1;
                self.group()
            }
            b'+' | b'?' if ere && first => Err(Error::BadRepeat),
            b'{' if ere && first => Err(Error::BadRepeat),
            b'\\' => {
                self.i += 1;
                let e = self.peek().ok_or(Error::TrailingBackslash)?;
                self.i += 1;
                match e {
                    b'(' if !ere => self.group(),
                    b')' if !ere => Err(Error::UnmatchedParen),
                    b'{' if !ere => Err(Error::BadRepeat),
                    b'1'..=b'9' => {
                        let n = usize::from(e - b'0');
                        if n > self.groups {
                            return Err(Error::BadBackref);
                        }
                        Ok(Node::Backref(n))
                    }
                    b'w' | b'W' => Ok(Node::Set(self.make_set(|c| is_word(c) == (e == b'w')))),
                    b's' | b'S' => {
                        Ok(Node::Set(self.make_set(|c| {
                            (c.is_ascii_whitespace() || c == 0x0b) == (e == b's')
                        })))
                    }
                    b'b' => Ok(Node::WordB),
                    b'<' => Ok(Node::WordStart),
                    b'>' => Ok(Node::WordEnd),
                    other => Ok(Node::Char(other)),
                }
            }
            b')' if ere && self.depth <= 1 => {
                self.i += 1;
                Ok(Node::Char(b')'))
            }
            other => {
                self.i += 1;
                Ok(Node::Char(other))
            }
        }
    }

    fn make_set(&self, f: impl Fn(u8) -> bool) -> Box<[bool; 256]> {
        let mut s = Box::new([false; 256]);
        for c in 0..=255u8 {
            s[usize::from(c)] = f(c);
        }
        s
    }

    fn group(&mut self) -> Result<Node, Error> {
        self.groups += 1;
        let idx = self.groups;
        let inner = self.alternation()?;
        let close: &[u8] = if self.flags.extended { b")" } else { b"\\)" };
        if !self.at(close) {
            return Err(Error::UnmatchedParen);
        }
        self.i += close.len();
        Ok(Node::Group(Box::new(inner), idx))
    }

    fn bracket(&mut self) -> Result<Node, Error> {
        self.i += 1; // [
        let mut set = Box::new([false; 256]);
        let negate = self.peek() == Some(b'^');
        if negate {
            self.i += 1;
        }
        let mut first = true;
        loop {
            let c = self.peek().ok_or(Error::UnmatchedBracket)?;
            if c == b']' && !first {
                self.i += 1;
                break;
            }
            first = false;
            if c == b'[' && self.p.get(self.i + 1) == Some(&b':') {
                let rest = &self.p[self.i + 2..];
                let end = rest
                    .windows(2)
                    .position(|w| w == b":]")
                    .ok_or(Error::BadClass)?;
                let f = class(&rest[..end]).ok_or(Error::BadClass)?;
                for ch in 0..=255u8 {
                    if f(ch) {
                        set[usize::from(ch)] = true;
                    }
                }
                self.i += 2 + end + 2;
                continue;
            }
            self.i += 1;
            if self.peek() == Some(b'-') && self.p.get(self.i + 1).is_some_and(|&x| x != b']') {
                let hi = self.p[self.i + 1];
                if hi < c {
                    return Err(Error::BadRange);
                }
                for ch in c..=hi {
                    set[usize::from(ch)] = true;
                }
                self.i += 2;
            } else {
                set[usize::from(c)] = true;
            }
        }
        if self.flags.icase {
            for ch in b'a'..=b'z' {
                let both = set[usize::from(ch)] || set[usize::from(ch.to_ascii_uppercase())];
                set[usize::from(ch)] = both;
                set[usize::from(ch.to_ascii_uppercase())] = both;
            }
        }
        if negate {
            for v in set.iter_mut() {
                *v = !*v;
            }
            if self.flags.newline {
                set[usize::from(b'\n')] = false;
            }
        }
        Ok(Node::Set(set))
    }
}

pub fn compile(pattern: &[u8], flags: Flags) -> Result<Regex, Error> {
    let mut p = Parser {
        p: pattern,
        i: 0,
        flags,
        groups: 0,
        depth: 0,
    };
    let root = p.alternation()?;
    if p.i != pattern.len() {
        return Err(Error::UnmatchedParen);
    }
    Ok(Regex {
        root,
        groups: p.groups,
        flags,
    })
}

struct Matcher<'a> {
    text: &'a [u8],
    flags: Flags,
    notbol: bool,
    noteol: bool,
    steps: usize,
    caps: Vec<Option<(usize, usize)>>,
}

type Cont<'c> = &'c mut dyn FnMut(&mut Matcher<'_>, usize) -> bool;

impl Matcher<'_> {
    fn eq(&self, a: u8, b: u8) -> bool {
        a == b || (self.flags.icase && a.eq_ignore_ascii_case(&b))
    }

    fn word_at(&self, i: usize) -> bool {
        self.text.get(i).is_some_and(|&c| is_word(c))
    }

    fn m(&mut self, n: &Node, pos: usize, k: Cont) -> bool {
        self.steps += 1;
        if self.steps > STEP_BUDGET {
            return false;
        }
        match n {
            Node::Char(c) => {
                pos < self.text.len() && self.eq(self.text[pos], *c) && k(self, pos + 1)
            }
            Node::Any => {
                pos < self.text.len()
                    && !(self.flags.newline && self.text[pos] == b'\n')
                    && k(self, pos + 1)
            }
            Node::Set(s) => {
                pos < self.text.len() && s[usize::from(self.text[pos])] && k(self, pos + 1)
            }
            Node::Bol => {
                (if pos == 0 {
                    !self.notbol
                } else {
                    self.flags.newline && self.text[pos - 1] == b'\n'
                }) && k(self, pos)
            }
            Node::Eol => {
                (if pos == self.text.len() {
                    !self.noteol
                } else {
                    self.flags.newline && self.text[pos] == b'\n'
                }) && k(self, pos)
            }
            Node::WordB => (pos > 0 && self.word_at(pos - 1)) != self.word_at(pos) && k(self, pos),
            Node::WordStart => {
                !(pos > 0 && self.word_at(pos - 1)) && self.word_at(pos) && k(self, pos)
            }
            Node::WordEnd => {
                (pos > 0 && self.word_at(pos - 1)) && !self.word_at(pos) && k(self, pos)
            }
            Node::Group(inner, idx) => {
                let idx = *idx;
                let old = self.caps[idx];
                let r = self.m(inner, pos, &mut |mm: &mut Matcher, end| {
                    let before = mm.caps[idx];
                    mm.caps[idx] = Some((pos, end));
                    if k(mm, end) {
                        return true;
                    }
                    mm.caps[idx] = before;
                    false
                });
                if !r {
                    self.caps[idx] = old;
                }
                r
            }
            Node::Alt(alts) => {
                for a in alts {
                    if self.m(a, pos, k) {
                        return true;
                    }
                }
                false
            }
            Node::Cat(items) => self.cat(items, pos, k),
            Node::Repeat(inner, min, max) => self.rep(inner, *min, *max, 0, pos, usize::MAX, k),
            Node::Backref(i) => match self.caps[*i] {
                Some((s, e)) => {
                    let len = e - s;
                    if pos + len > self.text.len() {
                        return false;
                    }
                    for j in 0..len {
                        if !self.eq(self.text[s + j], self.text[pos + j]) {
                            return false;
                        }
                    }
                    k(self, pos + len)
                }
                None => false,
            },
        }
    }

    fn cat(&mut self, items: &[Node], pos: usize, k: Cont) -> bool {
        match items.split_first() {
            None => k(self, pos),
            Some((first, rest)) => {
                self.m(first, pos, &mut |mm: &mut Matcher, p| mm.cat(rest, p, k))
            }
        }
    }

    /// `count` repetitions done so far; `last` is where the previous one started (an empty repetition ends the loop).
    #[allow(clippy::too_many_arguments)]
    fn rep(
        &mut self,
        inner: &Node,
        min: u32,
        max: Option<u32>,
        count: u32,
        pos: usize,
        last: usize,
        k: Cont,
    ) -> bool {
        // greedy: try one more first, then stop
        if max.is_none_or(|m| count < m) && !(count >= min && last == pos) {
            let more = self.m(inner, pos, &mut |mm: &mut Matcher, p| {
                mm.rep(inner, min, max, count + 1, p, pos, k)
            });
            if more {
                return true;
            }
        }
        count >= min && k(self, pos)
    }
}

impl Regex {
    /// Number of parenthesised groups.
    pub fn groups(&self) -> usize {
        self.groups
    }

    /// The leftmost, longest match in `text`: `(start, end)` of the whole match and of each group (`None` for a group that did not
    /// take part). `notbol` and `noteol` say that the start and end of `text` are not a line start and end.
    pub fn exec(&self, text: &[u8], notbol: bool, noteol: bool) -> Option<Captures> {
        for start in 0..=text.len() {
            let mut m = Matcher {
                text,
                flags: self.flags,
                notbol,
                noteol,
                steps: 0,
                caps: vec![None; self.groups + 1],
            };
            let mut best: Option<(usize, Captures)> = None;
            m.m(&self.root, start, &mut |mm: &mut Matcher, end| {
                if best.as_ref().is_none_or(|(e, _)| end > *e) {
                    best = Some((end, mm.caps.clone()));
                }
                // keep looking for a longer one unless this reaches the end of the text
                end == mm.text.len()
            });
            if let Some((end, mut caps)) = best {
                caps[0] = Some((start, end));
                return Some(caps);
            }
        }
        None
    }
}
