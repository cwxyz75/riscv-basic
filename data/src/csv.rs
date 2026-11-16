//! CSV (RFC 4180) and TSV with a defined escape scheme (SHELL-SPEC section 6). Both read UTF-8 text into a list of rows
//! (lists of strings) or, with a header, a list of maps. Strict: every row has the same number of fields; a blank line is
//! a record of one empty field (so in a one-column table it is a row, otherwise it is a ragged-row error); in CSV a quote
//! inside an unquoted field and text after a closing quote are errors, and a CR must be part of CRLF. Fields are always
//! strings (no type inference). The record terminator is LF or CRLF; the last terminator is optional.
//!
//! TSV: fields are separated by a tab and records by LF (or CRLF); inside a field the escapes `\\`, `\t`, `\n` and `\r`
//! stand for backslash, tab, line feed and carriage return; any other escape, a trailing backslash and a bare CR are errors.

use crate::{Limits, Map, Value};
use alloc::string::String;
use alloc::vec::Vec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableErrorKind {
    TooLarge,
    TooManyValues,
    InvalidUtf8,
    UnterminatedQuote,
    /// A `"` inside an unquoted field.
    QuoteInField,
    /// Text between a closing quote and the next delimiter or end of record.
    TextAfterQuote,
    /// A CR that is not part of CRLF.
    BareCr,
    /// A row with a different number of fields than the first (or the header).
    Ragged,
    DuplicateHeader,
    /// A TSV escape other than `\\ \t \n \r`.
    BadEscape,
    /// (emitting) the value is not a list of rows, or the rows are not all lists, or all maps with the same keys.
    NotATable,
    /// (emitting) a cell that is not a string, number or bool.
    BadCell,
}

impl TableErrorKind {
    pub fn message(self) -> &'static str {
        match self {
            TableErrorKind::TooLarge => "input too large",
            TableErrorKind::TooManyValues => "too many fields",
            TableErrorKind::InvalidUtf8 => "invalid UTF-8",
            TableErrorKind::UnterminatedQuote => "unterminated quote",
            TableErrorKind::QuoteInField => "quote inside an unquoted field",
            TableErrorKind::TextAfterQuote => "text after a closing quote",
            TableErrorKind::BareCr => "carriage return without line feed",
            TableErrorKind::Ragged => "row has a different number of fields",
            TableErrorKind::DuplicateHeader => "duplicate column name",
            TableErrorKind::BadEscape => "bad escape",
            TableErrorKind::NotATable => "not a table (a list of rows)",
            TableErrorKind::BadCell => "a cell must be a string, number or bool",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableError {
    pub kind: TableErrorKind,
    pub offset: usize,
    pub line: usize,
    pub col: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct TableOptions {
    /// The first record names the columns: rows come back as maps, and a list of maps is written with a header.
    pub header: bool,
    /// CSV only: the field delimiter (a byte other than quote, CR and LF).
    pub delimiter: u8,
    /// Emitting: end records with CRLF instead of LF.
    pub crlf: bool,
    pub limits: Limits,
}

impl Default for TableOptions {
    fn default() -> TableOptions {
        TableOptions {
            header: false,
            delimiter: b',',
            crlf: false,
            limits: Limits::DEFAULT,
        }
    }
}

struct Ctx<'a> {
    s: &'a [u8],
}

impl Ctx<'_> {
    fn err<T>(&self, kind: TableErrorKind, at: usize) -> Result<T, TableError> {
        let at = at.min(self.s.len());
        let before = &self.s[..at];
        Err(TableError {
            kind,
            offset: at,
            line: 1 + before.iter().filter(|&&b| b == b'\n').count(),
            col: 1 + before.iter().rev().take_while(|&&b| b != b'\n').count(),
        })
    }
}

type Rows = Vec<Vec<String>>;

fn text(s: &[u8]) -> String {
    // (the whole input was validated as UTF-8 and the cuts are at ASCII delimiters, so this never replaces anything)
    String::from_utf8_lossy(s).into_owned()
}

fn csv_records(c: &Ctx, delim: u8, max_fields: usize) -> Result<Rows, TableError> {
    let s = c.s;
    let mut rows: Rows = Vec::new();
    let mut fields_total = 0usize;
    let mut i = 0;
    while i < s.len() {
        let mut row: Vec<String> = Vec::new();
        loop {
            fields_total += 1;
            if fields_total > max_fields {
                return c.err(TableErrorKind::TooManyValues, i);
            }
            if s.get(i) == Some(&b'"') {
                let open = i;
                i += 1;
                let mut f: Vec<u8> = Vec::new();
                loop {
                    match s.get(i) {
                        None => return c.err(TableErrorKind::UnterminatedQuote, open),
                        Some(b'"') if s.get(i + 1) == Some(&b'"') => {
                            f.push(b'"');
                            i += 2;
                        }
                        Some(b'"') => {
                            i += 1;
                            break;
                        }
                        Some(&b) => {
                            f.push(b);
                            i += 1;
                        }
                    }
                }
                row.push(text(&f));
                match s.get(i) {
                    None | Some(b'\n') => {}
                    Some(b'\r') if s.get(i + 1) == Some(&b'\n') => {}
                    Some(&b) if b == delim => {}
                    Some(_) => return c.err(TableErrorKind::TextAfterQuote, i),
                }
            } else {
                let start = i;
                while let Some(&b) = s.get(i) {
                    if b == delim || b == b'\n' || b == b'\r' {
                        break;
                    }
                    if b == b'"' {
                        return c.err(TableErrorKind::QuoteInField, i);
                    }
                    i += 1;
                }
                row.push(text(&s[start..i]));
                if s.get(i) == Some(&b'\r') && s.get(i + 1) != Some(&b'\n') {
                    return c.err(TableErrorKind::BareCr, i);
                }
            }
            match s.get(i) {
                Some(&b) if b == delim => i += 1, // another field follows
                Some(b'\r') => {
                    i += 2;
                    break;
                }
                Some(b'\n') => {
                    i += 1;
                    break;
                }
                _ => break, // the end of the input
            }
        }
        rows.push(row);
    }
    Ok(rows)
}

fn tsv_records(c: &Ctx, max_fields: usize) -> Result<Rows, TableError> {
    let s = c.s;
    let mut rows: Rows = Vec::new();
    let mut fields_total = 0usize;
    let mut i = 0;
    while i < s.len() {
        let mut row: Vec<String> = Vec::new();
        loop {
            fields_total += 1;
            if fields_total > max_fields {
                return c.err(TableErrorKind::TooManyValues, i);
            }
            let mut f: Vec<u8> = Vec::new();
            loop {
                match s.get(i) {
                    None | Some(b'\t') | Some(b'\n') => break,
                    Some(b'\r') => {
                        if s.get(i + 1) == Some(&b'\n') {
                            break;
                        }
                        return c.err(TableErrorKind::BareCr, i);
                    }
                    Some(b'\\') => {
                        let e = match s.get(i + 1) {
                            Some(b'\\') => b'\\',
                            Some(b't') => b'\t',
                            Some(b'n') => b'\n',
                            Some(b'r') => b'\r',
                            _ => return c.err(TableErrorKind::BadEscape, i),
                        };
                        f.push(e);
                        i += 2;
                    }
                    Some(&b) => {
                        f.push(b);
                        i += 1;
                    }
                }
            }
            row.push(text(&f));
            match s.get(i) {
                Some(b'\t') => i += 1,
                Some(b'\r') => {
                    i += 2;
                    break;
                }
                Some(b'\n') => {
                    i += 1;
                    break;
                }
                _ => break,
            }
        }
        rows.push(row);
    }
    Ok(rows)
}

fn build(rows: Rows, header: bool) -> Result<Value, TableError> {
    let want = rows.first().map_or(0, Vec::len);
    if let Some(bad) = rows.iter().position(|r| r.len() != want) {
        // (the offset is not tracked per row: `line` is the record number)
        return Err(TableError {
            kind: TableErrorKind::Ragged,
            offset: 0,
            line: bad + 1,
            col: 1,
        });
    }
    if !header {
        return Ok(Value::List(
            rows.into_iter()
                .map(|r| Value::List(r.into_iter().map(Value::Str).collect()))
                .collect(),
        ));
    }
    let mut it = rows.into_iter();
    let Some(names) = it.next() else {
        return Ok(Value::List(Vec::new()));
    };
    let mut out = Vec::new();
    for r in it {
        let mut m = Map::new();
        for (k, v) in names.iter().zip(r) {
            if m.insert(k.clone(), Value::Str(v)).is_err() {
                return Err(TableError {
                    kind: TableErrorKind::DuplicateHeader,
                    offset: 0,
                    line: 1,
                    col: 1,
                });
            }
        }
        out.push(Value::Map(m));
    }
    // a header alone has no rows to carry a duplicate: check it anyway
    let mut seen = Map::new();
    for k in &names {
        if seen.insert(k.clone(), Value::Null).is_err() {
            return Err(TableError {
                kind: TableErrorKind::DuplicateHeader,
                offset: 0,
                line: 1,
                col: 1,
            });
        }
    }
    Ok(Value::List(out))
}

fn parse(input: &[u8], o: &TableOptions, tsv: bool) -> Result<Value, TableError> {
    let c = Ctx { s: input };
    if input.len() > o.limits.max_bytes {
        return c.err(TableErrorKind::TooLarge, 0);
    }
    if let Err(e) = core::str::from_utf8(input) {
        return c.err(TableErrorKind::InvalidUtf8, e.valid_up_to());
    }
    let rows = if tsv {
        tsv_records(&c, o.limits.max_nodes)?
    } else {
        csv_records(&c, o.delimiter, o.limits.max_nodes)?
    };
    build(rows, o.header)
}

/// Parse CSV.
pub fn from_csv(input: &[u8], o: &TableOptions) -> Result<Value, TableError> {
    parse(input, o, false)
}

/// Parse TSV.
pub fn from_tsv(input: &[u8], o: &TableOptions) -> Result<Value, TableError> {
    parse(input, o, true)
}

fn cell(v: &Value) -> Result<String, TableErrorKind> {
    match v {
        Value::Str(s) => Ok(s.clone()),
        Value::Num(n) => Ok(n.format()),
        Value::Bool(b) => Ok(String::from(if *b { "true" } else { "false" })),
        _ => Err(TableErrorKind::BadCell),
    }
}

/// The rows of a table value as cell text: lists of scalars, or (with `header`) maps with the same keys, header first.
fn cells(v: &Value, header: bool) -> Result<Vec<Vec<String>>, TableErrorKind> {
    let Value::List(rows) = v else {
        return Err(TableErrorKind::NotATable);
    };
    let mut out: Vec<Vec<String>> = Vec::new();
    if header {
        let Some(Value::Map(first)) = rows.first() else {
            return Ok(out);
        };
        let names: Vec<&str> = first.keys().collect();
        out.push(names.iter().map(|k| String::from(*k)).collect());
        for r in rows {
            let Value::Map(m) = r else {
                return Err(TableErrorKind::NotATable);
            };
            if m.len() != names.len() {
                return Err(TableErrorKind::NotATable);
            }
            let mut row = Vec::new();
            for k in &names {
                row.push(cell(m.get(k).ok_or(TableErrorKind::NotATable)?)?);
            }
            out.push(row);
        }
    } else {
        for r in rows {
            let Value::List(items) = r else {
                return Err(TableErrorKind::NotATable);
            };
            out.push(items.iter().map(cell).collect::<Result<_, _>>()?);
        }
        let want = out.first().map_or(0, Vec::len);
        if out.iter().any(|r| r.len() != want) {
            return Err(TableErrorKind::NotATable);
        }
    }
    Ok(out)
}

/// Write a table as CSV: a list of rows (lists of strings, numbers or bools), or with `header` a list of maps.
pub fn to_csv(v: &Value, o: &TableOptions) -> Result<String, TableErrorKind> {
    let d = o.delimiter as char;
    let mut out = String::new();
    for row in cells(v, o.header)? {
        let one = row.len() == 1;
        for (i, f) in row.iter().enumerate() {
            if i > 0 {
                out.push(d);
            }
            let quote = f.contains([d, '"', '\r', '\n']) || (one && f.is_empty());
            if quote {
                out.push('"');
                out.push_str(&f.replace('"', "\"\""));
                out.push('"');
            } else {
                out.push_str(f);
            }
        }
        out.push_str(if o.crlf { "\r\n" } else { "\n" });
    }
    Ok(out)
}

/// Write a table as TSV (escaping backslash, tab, line feed and carriage return).
pub fn to_tsv(v: &Value, o: &TableOptions) -> Result<String, TableErrorKind> {
    let mut out = String::new();
    for row in cells(v, o.header)? {
        for (i, f) in row.iter().enumerate() {
            if i > 0 {
                out.push('\t');
            }
            for ch in f.chars() {
                match ch {
                    '\\' => out.push_str("\\\\"),
                    '\t' => out.push_str("\\t"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    c => out.push(c),
                }
            }
        }
        out.push_str(if o.crlf { "\r\n" } else { "\n" });
    }
    Ok(out)
}
