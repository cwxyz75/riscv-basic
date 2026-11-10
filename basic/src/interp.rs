use crate::error::Error;
use crate::expr::{evaluate, parse_name, Variables};
use crate::io::{Io, Line};
use crate::number::format_number;
use crate::program::{Program, MAX_LINE};
use crate::token::{detokenize, token, tokenize};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// Width of a `PRINT` comma zone.
const ZONE: usize = 14;

/// The interpreter state: program store, variables, stacks, ...
///
/// Milestone status (M3, in progress): typed lines are tokenized and stored;
/// `LIST`, `NEW`, `LET` and `PRINT` work in immediate mode, with numeric
/// expressions. Everything else answers `?SYNTAX ERROR` until the later M3
/// items land. Behaviour is specified by `BASIC.md`.
#[derive(Debug, Default)]
pub struct Interpreter {
    program: Program,
    vars: Variables,
    /// Output column of the console cursor, for `,` zones and fresh lines.
    column: usize,
}

impl Interpreter {
    pub fn new() -> Self {
        Self::default()
    }

    /// The top-level loop: print `READY.`, read a line, handle it, repeat.
    /// Returns only when the console reports end of input.
    pub fn run_session<I: Io>(&mut self, io: &mut I) {
        loop {
            self.fresh_line(io);
            self.emit(io, "READY.\n");
            match io.read_line() {
                Line::Text(text) => self.handle_line(io, &text),
                Line::Break => {}
                Line::Eof => return,
            }
        }
    }

    /// Handle one line typed at the prompt: either a program line to store
    /// (starts with a line number) or a statement/command to run immediately.
    /// Errors are reported to the user, never returned or panicked.
    pub fn handle_line<I: Io>(&mut self, io: &mut I, line: &str) {
        // The user just pressed Enter, so the cursor is at the start of a line.
        self.column = 0;
        if let Err(error) = self.try_handle_line(io, line) {
            self.fresh_line(io);
            self.emit(io, &format!("?{} ERROR\n", error.name()));
        }
    }

    /// Write to the console, tracking the cursor column.
    fn emit<I: Io>(&mut self, io: &mut I, text: &str) {
        io.write_str(text);
        for c in text.chars() {
            if c == '\n' {
                self.column = 0;
            } else {
                self.column += 1;
            }
        }
    }

    /// Start a new line if the cursor is not already at the start of one.
    fn fresh_line<I: Io>(&mut self, io: &mut I) {
        if self.column != 0 {
            self.emit(io, "\n");
        }
    }

    fn try_handle_line<I: Io>(&mut self, io: &mut I, line: &str) -> Result<(), Error> {
        let line = line.trim_start_matches([' ', '\t']);
        if line.is_empty() {
            return Ok(());
        }
        if line.starts_with(|c: char| c.is_ascii_digit()) {
            return self.enter_program_line(line);
        }
        let tokens = tokenize(line)?;
        self.execute(io, &tokens)
    }

    /// Run one statement or command given as tokens.
    fn execute<I: Io>(&mut self, io: &mut I, tokens: &[u8]) -> Result<(), Error> {
        match tokens.split_first() {
            Some((&first, rest)) if first == token("LIST") => self.list(io, rest),
            Some((&first, rest)) if first == token("NEW") && is_blank(rest) => {
                self.program.clear();
                Ok(())
            }
            Some((&first, rest)) if first == token("PRINT") => self.print(io, rest),
            Some((&first, rest)) if first == token("LET") => self.assign(rest),
            Some((first, _)) if first.is_ascii_alphabetic() => self.assign(tokens),
            _ => Err(Error::Syntax),
        }
    }

    /// `line` starts with a digit: store it, replace an existing line, or
    /// (number alone) delete.
    fn enter_program_line(&mut self, line: &str) -> Result<(), Error> {
        let digits_end = line
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(line.len());
        let number = parse_line_number(&line[..digits_end])?;
        let body = line[digits_end..].trim_start_matches([' ', '\t']);
        if body.is_empty() {
            self.program.delete(number);
        } else {
            self.program.set(number, tokenize(body)?);
        }
        Ok(())
    }

    /// `LIST [from][-[to]]`; `args` is the tokenized text after the keyword.
    fn list<I: Io>(&mut self, io: &mut I, args: &[u8]) -> Result<(), Error> {
        let (from, to) = parse_range(&detokenize(args))?;
        let lines: Vec<String> = self
            .program
            .range(from, to)
            .map(|(number, tokens)| format!("{number} {}\n", detokenize(tokens)))
            .collect();
        for line in lines {
            self.emit(io, &line);
        }
        Ok(())
    }

    /// `[LET] name = expr`; `src` is everything after `LET` (or the whole
    /// statement when `LET` was omitted).
    fn assign(&mut self, src: &[u8]) -> Result<(), Error> {
        let mut pos = 0;
        let name = parse_name(src, &mut pos)?.ok_or(Error::Syntax)?;
        while src.get(pos) == Some(&b' ') {
            pos += 1;
        }
        if src.get(pos) != Some(&b'=') {
            return Err(Error::Syntax);
        }
        let (value, end) = evaluate(src, pos + 1, &self.vars)?;
        if !is_blank(&src[end..]) {
            return Err(Error::Syntax);
        }
        self.vars.insert(name, value);
        Ok(())
    }

    /// `PRINT [item [(;|,) [item]]...]`.
    fn print<I: Io>(&mut self, io: &mut I, src: &[u8]) -> Result<(), Error> {
        let mut pos = 0;
        let mut newline = true;
        let mut after_item = false;
        loop {
            while src.get(pos) == Some(&b' ') {
                pos += 1;
            }
            let Some(&byte) = src.get(pos) else { break };
            match byte {
                b';' => {
                    pos += 1;
                    newline = false;
                    after_item = false;
                }
                b',' => {
                    pos += 1;
                    newline = false;
                    after_item = false;
                    let pad = ZONE - self.column % ZONE;
                    self.emit(io, &" ".repeat(pad));
                }
                _ if after_item => return Err(Error::Syntax),
                b'"' => {
                    // A string literal runs to the closing quote, or the end.
                    let start = pos + 1;
                    let end = src[start..]
                        .iter()
                        .position(|&b| b == b'"')
                        .map_or(src.len(), |i| start + i);
                    let text: String = src[start..end].iter().map(|&b| char::from(b)).collect();
                    self.emit(io, &text);
                    pos = (end + 1).min(src.len());
                    newline = true;
                    after_item = true;
                }
                _ => {
                    let (value, end) = evaluate(src, pos, &self.vars)?;
                    self.emit(io, &format_number(value));
                    pos = end;
                    newline = true;
                    after_item = true;
                }
            }
        }
        if newline {
            self.emit(io, "\n");
        }
        Ok(())
    }
}

fn is_blank(bytes: &[u8]) -> bool {
    bytes.iter().all(|&b| b == b' ' || b == b'\t')
}

/// A line number: digits only (the caller guarantees that), at most 63999.
fn parse_line_number(digits: &str) -> Result<u16, Error> {
    let number: u32 = digits.parse().map_err(|_| Error::Syntax)?;
    if number > u32::from(MAX_LINE) {
        return Err(Error::Syntax);
    }
    u16::try_from(number).map_err(|_| Error::Syntax)
}

/// One bound of a `LIST` range: empty means "no bound given".
fn parse_bound(text: &str) -> Result<Option<u16>, Error> {
    let text = text.trim_matches([' ', '\t']);
    if text.is_empty() {
        return Ok(None);
    }
    if !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::Syntax);
    }
    parse_line_number(text).map(Some)
}

/// The argument of `LIST`: nothing (all), `n`, `a-b`, `a-` or `-b`.
fn parse_range(text: &str) -> Result<(u16, u16), Error> {
    let parts: Vec<&str> = text.split('-').collect();
    match parts.as_slice() {
        [single] => Ok(match parse_bound(single)? {
            Some(n) => (n, n),
            None => (0, MAX_LINE),
        }),
        [from, to] => Ok((
            parse_bound(from)?.unwrap_or(0),
            parse_bound(to)?.unwrap_or(MAX_LINE),
        )),
        _ => Err(Error::Syntax),
    }
}
