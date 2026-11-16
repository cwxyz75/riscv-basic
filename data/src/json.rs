//! JSON (RFC 8259) in and out. Strict: the only whitespace is space, tab, LF and CR; no comments, no trailing commas, no
//! BOM; strings must be valid UTF-8 and `\u` escapes must pair into scalar values (a lone surrogate is an error, since a
//! string is UTF-8); a duplicate key in an object is an error; numbers are exact (see `number`). Errors carry the byte
//! offset, line and column.

use crate::number::{NumError, Number};
use crate::{Limits, Map, Value};
use alloc::string::String;
use alloc::vec::Vec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Input larger than `Limits::max_bytes`.
    TooLarge,
    /// Nesting deeper than `Limits::max_depth`.
    TooDeep,
    /// More values than `Limits::max_nodes`.
    TooManyValues,
    /// A number with more digits or scale than a decimal holds.
    NumberTooPrecise,
    DuplicateKey,
    /// Input ended inside a value.
    UnexpectedEnd,
    /// A byte that cannot start or continue what is being read.
    Unexpected,
    BadNumber,
    BadEscape,
    /// A `\u` escape that is a surrogate without its pair.
    LoneSurrogate,
    /// A raw control character inside a string.
    ControlInString,
    /// A string that is not valid UTF-8.
    InvalidUtf8,
    /// Something after the value.
    TrailingData,
}

impl ErrorKind {
    pub fn message(self) -> &'static str {
        match self {
            ErrorKind::TooLarge => "input too large",
            ErrorKind::TooDeep => "nested too deeply",
            ErrorKind::TooManyValues => "too many values",
            ErrorKind::NumberTooPrecise => "number has too many digits",
            ErrorKind::DuplicateKey => "duplicate key",
            ErrorKind::UnexpectedEnd => "unexpected end of input",
            ErrorKind::Unexpected => "unexpected character",
            ErrorKind::BadNumber => "malformed number",
            ErrorKind::BadEscape => "bad escape",
            ErrorKind::LoneSurrogate => "unpaired surrogate escape",
            ErrorKind::ControlInString => "control character in string",
            ErrorKind::InvalidUtf8 => "invalid UTF-8",
            ErrorKind::TrailingData => "data after the value",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error {
    pub kind: ErrorKind,
    /// Byte offset into the input.
    pub offset: usize,
    /// 1-based.
    pub line: usize,
    /// 1-based, in bytes.
    pub col: usize,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{} at line {} column {}",
            self.kind.message(),
            self.line,
            self.col
        )
    }
}

struct P<'a> {
    s: &'a [u8],
    i: usize,
    lim: Limits,
    nodes: usize,
}

impl P<'_> {
    fn err<T>(&self, kind: ErrorKind, at: usize) -> Result<T, Error> {
        let at = at.min(self.s.len());
        let before = &self.s[..at];
        let line = 1 + before.iter().filter(|&&b| b == b'\n').count();
        let col = 1 + before.iter().rev().take_while(|&&b| b != b'\n').count();
        Err(Error {
            kind,
            offset: at,
            line,
            col,
        })
    }

    fn ws(&mut self) {
        while matches!(self.s.get(self.i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn node(&mut self) -> Result<(), Error> {
        self.nodes += 1;
        if self.nodes > self.lim.max_nodes {
            return self.err(ErrorKind::TooManyValues, self.i);
        }
        Ok(())
    }

    fn value(&mut self, depth: usize) -> Result<Value, Error> {
        self.ws();
        self.node()?;
        let Some(&c) = self.s.get(self.i) else {
            return self.err(ErrorKind::UnexpectedEnd, self.i);
        };
        match c {
            b'{' => self.object(depth),
            b'[' => self.array(depth),
            b'"' => Ok(Value::Str(self.string()?)),
            b't' => self.word(b"true", Value::Bool(true)),
            b'f' => self.word(b"false", Value::Bool(false)),
            b'n' => self.word(b"null", Value::Null),
            b'-' | b'0'..=b'9' => self.number(),
            _ => self.err(ErrorKind::Unexpected, self.i),
        }
    }

    fn word(&mut self, w: &[u8], v: Value) -> Result<Value, Error> {
        for (k, &b) in w.iter().enumerate() {
            match self.s.get(self.i + k) {
                Some(&x) if x == b => {}
                Some(_) => return self.err(ErrorKind::Unexpected, self.i + k),
                None => return self.err(ErrorKind::UnexpectedEnd, self.i + k),
            }
        }
        self.i += w.len();
        Ok(v)
    }

    fn number(&mut self) -> Result<Value, Error> {
        let start = self.i;
        // the token is every byte a number may contain; `Number::parse` decides whether it is one
        while matches!(
            self.s.get(self.i),
            Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
        ) {
            self.i += 1;
        }
        match Number::parse(&self.s[start..self.i]) {
            Ok(n) => Ok(Value::Num(n)),
            Err(NumError::TooPrecise) => self.err(ErrorKind::NumberTooPrecise, start),
            Err(NumError::Syntax) => self.err(ErrorKind::BadNumber, start),
        }
    }

    fn hex4(&mut self) -> Result<u32, Error> {
        let mut v = 0u32;
        for _ in 0..4 {
            let Some(&c) = self.s.get(self.i) else {
                return self.err(ErrorKind::UnexpectedEnd, self.i);
            };
            let d = match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                b'A'..=b'F' => c - b'A' + 10,
                _ => return self.err(ErrorKind::BadEscape, self.i),
            };
            v = v * 16 + u32::from(d);
            self.i += 1;
        }
        Ok(v)
    }

    fn string(&mut self) -> Result<String, Error> {
        self.i += 1; // the opening quote
        let mut out = String::new();
        loop {
            // a run of plain bytes
            let start = self.i;
            while let Some(&c) = self.s.get(self.i) {
                if c == b'"' || c == b'\\' || c < 0x20 {
                    break;
                }
                self.i += 1;
            }
            if self.i > start {
                match core::str::from_utf8(&self.s[start..self.i]) {
                    Ok(t) => out.push_str(t),
                    Err(e) => return self.err(ErrorKind::InvalidUtf8, start + e.valid_up_to()),
                }
            }
            let Some(&c) = self.s.get(self.i) else {
                return self.err(ErrorKind::UnexpectedEnd, self.i);
            };
            match c {
                b'"' => {
                    self.i += 1;
                    return Ok(out);
                }
                b'\\' => {
                    let esc_at = self.i;
                    self.i += 1;
                    let Some(&e) = self.s.get(self.i) else {
                        return self.err(ErrorKind::UnexpectedEnd, self.i);
                    };
                    self.i += 1;
                    let ch = match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let hi = self.hex4()?;
                            match hi {
                                0xD800..=0xDBFF => {
                                    if self.s.get(self.i) == Some(&b'\\')
                                        && self.s.get(self.i + 1) == Some(&b'u')
                                    {
                                        self.i += 2;
                                        let lo = self.hex4()?;
                                        if !(0xDC00..=0xDFFF).contains(&lo) {
                                            return self.err(ErrorKind::LoneSurrogate, esc_at);
                                        }
                                        let cp = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                                        char::from_u32(cp).unwrap_or('\u{fffd}')
                                    } else {
                                        return self.err(ErrorKind::LoneSurrogate, esc_at);
                                    }
                                }
                                0xDC00..=0xDFFF => {
                                    return self.err(ErrorKind::LoneSurrogate, esc_at)
                                }
                                _ => char::from_u32(hi).unwrap_or('\u{fffd}'),
                            }
                        }
                        _ => return self.err(ErrorKind::BadEscape, self.i - 1),
                    };
                    out.push(ch);
                }
                _ => return self.err(ErrorKind::ControlInString, self.i),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Value, Error> {
        if depth >= self.lim.max_depth {
            return self.err(ErrorKind::TooDeep, self.i);
        }
        self.i += 1;
        let mut items = Vec::new();
        self.ws();
        if self.s.get(self.i) == Some(&b']') {
            self.i += 1;
            return Ok(Value::List(items));
        }
        loop {
            items.push(self.value(depth + 1)?);
            self.ws();
            match self.s.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Value::List(items));
                }
                Some(_) => return self.err(ErrorKind::Unexpected, self.i),
                None => return self.err(ErrorKind::UnexpectedEnd, self.i),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Value, Error> {
        if depth >= self.lim.max_depth {
            return self.err(ErrorKind::TooDeep, self.i);
        }
        self.i += 1;
        let mut map = Map::new();
        self.ws();
        if self.s.get(self.i) == Some(&b'}') {
            self.i += 1;
            return Ok(Value::Map(map));
        }
        loop {
            self.ws();
            match self.s.get(self.i) {
                Some(b'"') => {}
                Some(_) => return self.err(ErrorKind::Unexpected, self.i),
                None => return self.err(ErrorKind::UnexpectedEnd, self.i),
            }
            let key_at = self.i;
            let key = self.string()?;
            self.ws();
            match self.s.get(self.i) {
                Some(b':') => self.i += 1,
                Some(_) => return self.err(ErrorKind::Unexpected, self.i),
                None => return self.err(ErrorKind::UnexpectedEnd, self.i),
            }
            let v = self.value(depth + 1)?;
            if map.insert(key, v).is_err() {
                return self.err(ErrorKind::DuplicateKey, key_at);
            }
            self.ws();
            match self.s.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Value::Map(map));
                }
                Some(_) => return self.err(ErrorKind::Unexpected, self.i),
                None => return self.err(ErrorKind::UnexpectedEnd, self.i),
            }
        }
    }
}

/// Parse one JSON document (any value at the top level) from bytes.
pub fn from_json(input: &[u8], lim: &Limits) -> Result<Value, Error> {
    let mut p = P {
        s: input,
        i: 0,
        lim: *lim,
        nodes: 0,
    };
    if input.len() > lim.max_bytes {
        return p.err(ErrorKind::TooLarge, 0);
    }
    let v = p.value(0)?;
    p.ws();
    if p.i != input.len() {
        return p.err(ErrorKind::TrailingData, p.i);
    }
    Ok(v)
}

fn push_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                use core::fmt::Write;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn emit(
    v: &Value,
    out: &mut String,
    indent: Option<usize>,
    level: usize,
    max_depth: usize,
) -> Result<(), ErrorKind> {
    let nl = |out: &mut String, level: usize| {
        if let Some(n) = indent {
            out.push('\n');
            for _ in 0..level * n {
                out.push(' ');
            }
        }
    };
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Num(n) => out.push_str(&n.format()),
        Value::Str(s) => push_string(out, s),
        Value::List(l) => {
            if level >= max_depth {
                return Err(ErrorKind::TooDeep);
            }
            out.push('[');
            for (i, x) in l.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                nl(out, level + 1);
                emit(x, out, indent, level + 1, max_depth)?;
            }
            if !l.is_empty() {
                nl(out, level);
            }
            out.push(']');
        }
        Value::Map(m) => {
            if level >= max_depth {
                return Err(ErrorKind::TooDeep);
            }
            out.push('{');
            for (i, (k, x)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                nl(out, level + 1);
                push_string(out, k);
                out.push(':');
                if indent.is_some() {
                    out.push(' ');
                }
                emit(x, out, indent, level + 1, max_depth)?;
            }
            if !m.is_empty() {
                nl(out, level);
            }
            out.push('}');
        }
    }
    Ok(())
}

/// Compact JSON text of a value. `Err(TooDeep)` if the value nests deeper than `lim.max_depth`.
pub fn to_json(v: &Value, lim: &Limits) -> Result<String, ErrorKind> {
    let mut out = String::new();
    emit(v, &mut out, None, 0, lim.max_depth)?;
    Ok(out)
}

/// Indented JSON text (`indent` spaces per level).
pub fn to_json_pretty(v: &Value, indent: usize, lim: &Limits) -> Result<String, ErrorKind> {
    let mut out = String::new();
    emit(v, &mut out, Some(indent), 0, lim.max_depth)?;
    Ok(out)
}
