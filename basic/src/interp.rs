use crate::error::Error;
use crate::io::{Io, Line};
use crate::program::{Program, MAX_LINE};
use crate::token::{detokenize, token, tokenize};
use alloc::format;

/// The interpreter state: program store, variables, stacks, ...
///
/// Milestone status (M3, in progress): typed lines are tokenized and stored,
/// and `LIST` and `NEW` work. Everything else answers `?SYNTAX ERROR` until
/// the later M3 items land. Behaviour is specified by `BASIC.md`.
#[derive(Debug, Default)]
pub struct Interpreter {
    program: Program,
}

impl Interpreter {
    pub fn new() -> Self {
        Self::default()
    }

    /// The top-level loop: print `READY.`, read a line, handle it, repeat.
    /// Returns only when the console reports end of input.
    pub fn run_session<I: Io>(&mut self, io: &mut I) {
        loop {
            io.write_str("READY.\n");
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
        if let Err(error) = self.try_handle_line(io, line) {
            io.write_str(&format!("?{} ERROR\n", error.name()));
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
        match tokens.split_first() {
            Some((&first, rest)) if first == token("LIST") => self.list(io, rest),
            Some((&first, rest)) if first == token("NEW") && is_blank(rest) => {
                self.program.clear();
                Ok(())
            }
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
    fn list<I: Io>(&self, io: &mut I, args: &[u8]) -> Result<(), Error> {
        let (from, to) = parse_range(&detokenize(args))?;
        for (number, tokens) in self.program.range(from, to) {
            io.write_str(&format!("{number} {}\n", detokenize(tokens)));
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
    let parts: alloc::vec::Vec<&str> = text.split('-').collect();
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
