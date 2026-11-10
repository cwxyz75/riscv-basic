use crate::error::{Error, Fault};
use crate::expr::{evaluate, parse_name, to_address, to_byte, Variables};
use crate::io::{Io, Line};
use crate::number::format_number;
use crate::program::{Program, MAX_LINE};
use crate::token::{detokenize, keyword, tokenize};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// Width of a `PRINT` comma zone.
const ZONE: usize = 14;

/// An active `FOR` loop: where `NEXT` jumps back to (just after the `FOR`
/// statement) and how to step.
#[derive(Debug)]
struct ForFrame {
    var: String,
    limit: f64,
    step: f64,
    line: Option<u16>,
    pos: usize,
}

/// What to do after a statement.
#[derive(Debug, PartialEq)]
enum Flow {
    /// Carry on with the next statement.
    Next,
    /// Stop the program (or the typed line).
    End,
    /// Continue at byte `pos` of `line` (`None` is the typed line).
    Jump { line: Option<u16>, pos: usize },
}

/// The interpreter state: program store, variables, loop stack, ...
///
/// Milestone status (M3): typed lines are tokenized and stored; `RUN`,
/// `GOTO`, `IF`, `FOR`/`NEXT`, `END`, `STOP`, `LET`, `PRINT`, `REM`, `LIST` and
/// `NEW` work, with numeric expressions. Everything else answers `?SYNTAX
/// ERROR` until the later milestones land. Behaviour is specified by `BASIC.md`.
#[derive(Debug, Default)]
pub struct Interpreter {
    program: Program,
    vars: Variables,
    loops: Vec<ForFrame>,
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
        // Loops typed at the prompt live and die within one line.
        self.loops.clear();
        if let Err(fault) = self.try_handle_line(io, line) {
            let location = fault.line.map_or(String::new(), |n| format!(" IN {n}"));
            self.fresh_line(io);
            self.emit(io, &format!("?{} ERROR{location}\n", fault.error.name()));
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

    fn try_handle_line<I: Io>(&mut self, io: &mut I, line: &str) -> Result<(), Fault> {
        let line = line.trim_start_matches([' ', '\t']);
        if line.is_empty() {
            return Ok(());
        }
        if line.starts_with(|c: char| c.is_ascii_digit()) {
            return Ok(self.enter_program_line(line)?);
        }
        let tokens = tokenize(line)?;
        match self.run_line(io, None, &tokens, 0)? {
            Flow::Jump {
                line: Some(target),
                pos,
            } => {
                if !self.program.contains(target) {
                    return Err(Error::UndefdStatement.into());
                }
                self.run_program(io, target, pos)
            }
            _ => Ok(()),
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

    /// Run the stored program starting at byte `pos` of line `line`, which
    /// must exist. Returns when the program ends or an error stops it.
    fn run_program<I: Io>(
        &mut self,
        io: &mut I,
        mut line: u16,
        mut pos: usize,
    ) -> Result<(), Fault> {
        loop {
            let Some(tokens) = self.program.get(line).map(<[u8]>::to_vec) else {
                return Err(Error::UndefdStatement.into());
            };
            let here = Some(line);
            let flow = self
                .run_line(io, here, &tokens, pos)
                .map_err(|error| Fault { error, line: here })?;
            match flow {
                Flow::Next => match self.program.next_after(line) {
                    Some(next) => {
                        line = next;
                        pos = 0;
                    }
                    None => return Ok(()),
                },
                Flow::End | Flow::Jump { line: None, .. } => return Ok(()),
                Flow::Jump {
                    line: Some(target),
                    pos: target_pos,
                } => {
                    if !self.program.contains(target) {
                        return Err(Fault {
                            error: Error::UndefdStatement,
                            line: here,
                        });
                    }
                    line = target;
                    pos = target_pos;
                }
            }
        }
    }

    /// Run the statements of one line of tokens from byte `pos` on. `line` is
    /// the program line number, or `None` for a typed line. A jump to a
    /// position within the same line is followed here; anything else is handed
    /// back to the caller.
    fn run_line<I: Io>(
        &mut self,
        io: &mut I,
        line: Option<u16>,
        tokens: &[u8],
        mut pos: usize,
    ) -> Result<Flow, Error> {
        loop {
            while matches!(tokens.get(pos), Some(b' ' | b'\t' | b':')) {
                pos += 1;
            }
            if pos >= tokens.len() {
                return Ok(Flow::Next);
            }
            match self.statement(io, line, tokens, &mut pos)? {
                Flow::Next => {}
                Flow::Jump {
                    line: target,
                    pos: target_pos,
                } if target == line => pos = target_pos,
                other => return Ok(other),
            }
        }
    }

    /// Execute the one statement starting at `tokens[*pos]` (not a space or
    /// colon), leaving `*pos` just after it.
    fn statement<I: Io>(
        &mut self,
        io: &mut I,
        line: Option<u16>,
        tokens: &[u8],
        pos: &mut usize,
    ) -> Result<Flow, Error> {
        let first = tokens[*pos];
        if first.is_ascii_alphabetic() {
            return self.stmt_let(io, tokens, pos);
        }
        *pos += 1;
        match keyword(first) {
            Some("LET") => self.stmt_let(io, tokens, pos),
            Some("PRINT") => self.stmt_print(io, tokens, pos),
            Some("REM") => {
                *pos = tokens.len();
                Ok(Flow::Next)
            }
            Some("GOTO") => {
                let target = parse_line_ref(tokens, pos)?;
                end_of_statement(tokens, pos)?;
                Ok(Flow::Jump {
                    line: Some(target),
                    pos: 0,
                })
            }
            Some("IF") => self.stmt_if(io, tokens, pos),
            Some("FOR") => self.stmt_for(io, line, tokens, pos),
            Some("NEXT") => self.stmt_next(tokens, pos),
            Some("END") => Ok(Flow::End),
            Some("STOP") => {
                self.fresh_line(io);
                let message = match line {
                    Some(n) => format!("BREAK IN {n}\n"),
                    None => String::from("BREAK\n"),
                };
                self.emit(io, &message);
                Ok(Flow::End)
            }
            Some("RUN") => self.stmt_run(tokens, pos),
            Some("POKE") => self.stmt_poke(io, tokens, pos),
            Some("LIST") => {
                let end = statement_end(tokens, *pos);
                let args = &tokens[*pos..end];
                *pos = end;
                self.stmt_list(io, args)?;
                Ok(Flow::Next)
            }
            Some("NEW") => {
                let end = statement_end(tokens, *pos);
                if !is_blank(&tokens[*pos..end]) {
                    return Err(Error::Syntax);
                }
                *pos = end;
                self.program.clear();
                Ok(Flow::End)
            }
            _ => Err(Error::Syntax),
        }
    }

    /// `[LET] name = expr`; `*pos` is just after `LET` or at the name.
    fn stmt_let<I: Io>(
        &mut self,
        io: &mut I,
        tokens: &[u8],
        pos: &mut usize,
    ) -> Result<Flow, Error> {
        let name = parse_name(tokens, pos)?.ok_or(Error::Syntax)?;
        skip_spaces(tokens, pos);
        if tokens.get(*pos) != Some(&b'=') {
            return Err(Error::Syntax);
        }
        let (value, end) = evaluate(tokens, *pos + 1, &self.vars, io)?;
        *pos = end;
        end_of_statement(tokens, pos)?;
        self.vars.insert(name, value);
        Ok(Flow::Next)
    }

    /// `PRINT [item [(;|,) [item]]...]`.
    fn stmt_print<I: Io>(
        &mut self,
        io: &mut I,
        src: &[u8],
        pos: &mut usize,
    ) -> Result<Flow, Error> {
        let mut newline = true;
        let mut after_item = false;
        loop {
            skip_spaces(src, pos);
            let Some(&byte) = src.get(*pos) else { break };
            match byte {
                b':' => break,
                b';' => {
                    *pos += 1;
                    newline = false;
                    after_item = false;
                }
                b',' => {
                    *pos += 1;
                    newline = false;
                    after_item = false;
                    let pad = ZONE - self.column % ZONE;
                    self.emit(io, &" ".repeat(pad));
                }
                _ if after_item => return Err(Error::Syntax),
                b'"' => {
                    // A string literal runs to the closing quote, or the end.
                    let start = *pos + 1;
                    let end = src[start..]
                        .iter()
                        .position(|&b| b == b'"')
                        .map_or(src.len(), |i| start + i);
                    let text: String = src[start..end].iter().map(|&b| char::from(b)).collect();
                    self.emit(io, &text);
                    *pos = (end + 1).min(src.len());
                    newline = true;
                    after_item = true;
                }
                _ => {
                    let (value, end) = evaluate(src, *pos, &self.vars, io)?;
                    self.emit(io, &format_number(value));
                    *pos = end;
                    newline = true;
                    after_item = true;
                }
            }
        }
        if newline {
            self.emit(io, "\n");
        }
        Ok(Flow::Next)
    }

    /// `IF expr THEN line | statements`, `IF expr GOTO line`.
    fn stmt_if<I: Io>(
        &mut self,
        io: &mut I,
        tokens: &[u8],
        pos: &mut usize,
    ) -> Result<Flow, Error> {
        let (condition, end) = evaluate(tokens, *pos, &self.vars, io)?;
        *pos = end;
        skip_spaces(tokens, pos);
        let word = tokens.get(*pos).and_then(|&b| keyword(b));
        if word != Some("THEN") && word != Some("GOTO") {
            return Err(Error::Syntax);
        }
        *pos += 1;
        if condition == 0.0 {
            // False: the rest of the line is skipped.
            *pos = tokens.len();
            return Ok(Flow::Next);
        }
        skip_spaces(tokens, pos);
        if tokens.get(*pos).is_some_and(u8::is_ascii_digit) || word == Some("GOTO") {
            let target = parse_line_ref(tokens, pos)?;
            end_of_statement(tokens, pos)?;
            return Ok(Flow::Jump {
                line: Some(target),
                pos: 0,
            });
        }
        // THEN followed by statements: just carry on with them.
        Ok(Flow::Next)
    }

    /// `FOR v = a TO b [STEP c]`.
    fn stmt_for<I: Io>(
        &mut self,
        io: &mut I,
        line: Option<u16>,
        tokens: &[u8],
        pos: &mut usize,
    ) -> Result<Flow, Error> {
        let var = parse_name(tokens, pos)?.ok_or(Error::Syntax)?;
        skip_spaces(tokens, pos);
        if tokens.get(*pos) != Some(&b'=') {
            return Err(Error::Syntax);
        }
        let (start, end) = evaluate(tokens, *pos + 1, &self.vars, io)?;
        *pos = end;
        expect_keyword(tokens, pos, "TO")?;
        let (limit, end) = evaluate(tokens, *pos, &self.vars, io)?;
        *pos = end;
        skip_spaces(tokens, pos);
        let mut step = 1.0;
        if tokens.get(*pos).and_then(|&b| keyword(b)) == Some("STEP") {
            *pos += 1;
            let (value, end) = evaluate(tokens, *pos, &self.vars, io)?;
            step = value;
            *pos = end;
        }
        end_of_statement(tokens, pos)?;

        self.vars.insert(var.clone(), start);
        // Restarting a loop discards it and every loop inside it.
        if let Some(index) = self.loops.iter().position(|f| f.var == var) {
            self.loops.truncate(index);
        }
        self.loops.push(ForFrame {
            var,
            limit,
            step,
            line,
            pos: *pos,
        });
        Ok(Flow::Next)
    }

    /// `NEXT [v[,v...]]`.
    fn stmt_next(&mut self, tokens: &[u8], pos: &mut usize) -> Result<Flow, Error> {
        let mut first = true;
        loop {
            skip_spaces(tokens, pos);
            let name = parse_name(tokens, pos)?;
            if name.is_none() && !first {
                break;
            }
            let index = match &name {
                None => self.loops.len().checked_sub(1),
                Some(var) => self.loops.iter().rposition(|f| &f.var == var),
            }
            .ok_or(Error::NextWithoutFor)?;
            // Loops inside the one being stepped are abandoned.
            self.loops.truncate(index + 1);
            let frame = &self.loops[index];
            let value = self.vars.get(&frame.var).copied().unwrap_or(0.0) + frame.step;
            if !value.is_finite() {
                return Err(Error::Overflow);
            }
            let again = if frame.step >= 0.0 {
                value <= frame.limit
            } else {
                value >= frame.limit
            };
            let var = frame.var.clone();
            let target = Flow::Jump {
                line: frame.line,
                pos: frame.pos,
            };
            self.vars.insert(var, value);
            if again {
                return Ok(target);
            }
            self.loops.pop();

            first = false;
            skip_spaces(tokens, pos);
            if tokens.get(*pos) == Some(&b',') {
                *pos += 1;
            } else {
                break;
            }
        }
        end_of_statement(tokens, pos)?;
        Ok(Flow::Next)
    }

    /// `POKE addr, value`.
    fn stmt_poke<I: Io>(
        &mut self,
        io: &mut I,
        tokens: &[u8],
        pos: &mut usize,
    ) -> Result<Flow, Error> {
        let (address, end) = evaluate(tokens, *pos, &self.vars, io)?;
        *pos = end;
        skip_spaces(tokens, pos);
        if tokens.get(*pos) != Some(&b',') {
            return Err(Error::Syntax);
        }
        let (value, end) = evaluate(tokens, *pos + 1, &self.vars, io)?;
        *pos = end;
        end_of_statement(tokens, pos)?;
        io.poke(to_address(address)?, to_byte(value)?);
        Ok(Flow::Next)
    }

    /// `RUN [line]`.
    fn stmt_run(&mut self, tokens: &[u8], pos: &mut usize) -> Result<Flow, Error> {
        skip_spaces(tokens, pos);
        let given = if tokens.get(*pos).is_some_and(u8::is_ascii_digit) {
            Some(parse_line_ref(tokens, pos)?)
        } else {
            None
        };
        end_of_statement(tokens, pos)?;
        if let Some(n) = given {
            if !self.program.contains(n) {
                return Err(Error::UndefdStatement);
            }
        }
        self.vars.clear();
        self.loops.clear();
        Ok(match given.or_else(|| self.program.first()) {
            Some(start) => Flow::Jump {
                line: Some(start),
                pos: 0,
            },
            None => Flow::Next,
        })
    }

    /// `LIST [from][-[to]]`; `args` is the tokenized text after the keyword.
    fn stmt_list<I: Io>(&mut self, io: &mut I, args: &[u8]) -> Result<(), Error> {
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
}

fn is_blank(bytes: &[u8]) -> bool {
    bytes.iter().all(|&b| b == b' ' || b == b'\t')
}

fn skip_spaces(tokens: &[u8], pos: &mut usize) {
    while matches!(tokens.get(*pos), Some(b' ' | b'\t')) {
        *pos += 1;
    }
}

/// A statement must be followed by a colon or the end of the line.
fn end_of_statement(tokens: &[u8], pos: &mut usize) -> Result<(), Error> {
    skip_spaces(tokens, pos);
    match tokens.get(*pos) {
        None | Some(b':') => Ok(()),
        Some(_) => Err(Error::Syntax),
    }
}

/// Index of the next `:` at or after `from`, or the end of the line.
fn statement_end(tokens: &[u8], from: usize) -> usize {
    tokens[from..]
        .iter()
        .position(|&b| b == b':')
        .map_or(tokens.len(), |i| from + i)
}

/// Consume the keyword `word` (after spaces) or fail with a syntax error.
fn expect_keyword(tokens: &[u8], pos: &mut usize, word: &str) -> Result<(), Error> {
    skip_spaces(tokens, pos);
    if tokens.get(*pos).and_then(|&b| keyword(b)) == Some(word) {
        *pos += 1;
        Ok(())
    } else {
        Err(Error::Syntax)
    }
}

/// A line number written in a statement (`GOTO 100`), after optional spaces.
fn parse_line_ref(tokens: &[u8], pos: &mut usize) -> Result<u16, Error> {
    skip_spaces(tokens, pos);
    let start = *pos;
    while tokens.get(*pos).is_some_and(u8::is_ascii_digit) {
        *pos += 1;
    }
    let digits: String = tokens[start..*pos].iter().map(|&b| char::from(b)).collect();
    parse_line_number(&digits)
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
