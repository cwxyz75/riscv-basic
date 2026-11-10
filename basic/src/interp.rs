use crate::data::{next_item, split_input, DataItem, DataPtr};
use crate::error::{Error, Fault};
use crate::expr::{
    evaluate, evaluate_number, evaluate_subscripts, parse_name, strict_number, to_address, to_byte,
    to_count,
};
use crate::io::{Io, Line};
use crate::memory::Memory;
use crate::number::format_number;
use crate::program::{Program, MAX_LINE};
use crate::token::{detokenize, keyword, tokenize};
use crate::value::{check_length, Value};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// Width of a `PRINT` comma zone.
const ZONE: usize = 14;

/// How many `GOSUB`s may be pending at once; deeper is `?OUT OF MEMORY`.
const MAX_GOSUB_DEPTH: usize = 1000;

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

/// Something a statement can store a value in: a variable, or an array element
/// (with its subscripts already evaluated).
#[derive(Debug)]
struct Target {
    name: String,
    subs: Option<Vec<f64>>,
}

/// Turn a `DATA` or `INPUT` item into a value of the type `name` needs. A
/// quoted item for a number is a type mismatch; a bare item that is not a
/// complete number is a syntax error.
fn convert_item(name: &str, item: &DataItem) -> Result<Value, Error> {
    if name.ends_with('$') {
        let text = match item {
            DataItem::Quoted(s) | DataItem::Bare(s) => s.clone(),
        };
        return Ok(Value::Str(check_length(text)?));
    }
    match item {
        DataItem::Quoted(_) => Err(Error::TypeMismatch),
        DataItem::Bare(s) => strict_number(s)?.map(Value::Num).ok_or(Error::Syntax),
    }
}

/// Where a `RETURN` goes back to: the statement after the `GOSUB`.
#[derive(Debug)]
struct ReturnPoint {
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
    mem: Memory,
    loops: Vec<ForFrame>,
    gosubs: Vec<ReturnPoint>,
    /// Where `READ` continues in the program's `DATA`.
    data: Option<DataPtr>,
    /// Output column of the console cursor, for `,` zones and fresh lines.
    column: usize,
}

impl Interpreter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bytes of the program area still free (the `BYTES FREE` figure).
    pub fn bytes_free(&self) -> usize {
        self.program.bytes_free()
    }

    /// Print the start-up banner: the title and the free program memory.
    pub fn banner<I: Io>(&mut self, io: &mut I) {
        let text = format!(
            "RISCV BASIC V0.1\n{} BYTES FREE\n\n",
            self.program.bytes_free()
        );
        self.emit(io, &text);
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
        // Loops and subroutine calls typed at the prompt live and die within one line.
        self.loops.clear();
        self.gosubs.clear();
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
            self.program.set(number, tokenize(body)?)?;
        }
        self.mem.set_program_free(self.program.bytes_free());
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
            // Ctrl-C is checked before every statement.
            if io.break_pressed() {
                self.stop(io, line);
                return Ok(Flow::End);
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
            Some("GOSUB") => {
                let target = parse_line_ref(tokens, pos)?;
                end_of_statement(tokens, pos)?;
                self.call(line, *pos, target)
            }
            Some("RETURN") => {
                end_of_statement(tokens, pos)?;
                match self.gosubs.pop() {
                    None => Err(Error::ReturnWithoutGosub),
                    // A GOSUB typed at the prompt has nothing to return to.
                    Some(ReturnPoint { line: None, .. }) => Ok(Flow::End),
                    Some(point) => Ok(Flow::Jump {
                        line: point.line,
                        pos: point.pos,
                    }),
                }
            }
            Some("ON") => self.stmt_on(io, line, tokens, pos),
            Some("FOR") => self.stmt_for(io, line, tokens, pos),
            Some("NEXT") => self.stmt_next(tokens, pos),
            Some("END") => Ok(Flow::End),
            Some("STOP") => {
                self.stop(io, line);
                Ok(Flow::End)
            }
            Some("DATA") => {
                *pos = data_statement_end(tokens, *pos);
                Ok(Flow::Next)
            }
            Some("READ") => self.stmt_read(io, tokens, pos),
            Some("RESTORE") => {
                end_of_statement(tokens, pos)?;
                self.data = None;
                Ok(Flow::Next)
            }
            Some("INPUT") => self.stmt_input(io, line, tokens, pos),
            Some("RUN") => self.stmt_run(tokens, pos),
            Some("CLS") => {
                end_of_statement(tokens, pos)?;
                io.clear_screen();
                self.column = 0;
                Ok(Flow::Next)
            }
            Some("COLOR") => self.stmt_color(io, tokens, pos),
            Some("LOCATE") => self.stmt_locate(io, tokens, pos),
            Some("POKE") => self.stmt_poke(io, tokens, pos),
            Some("DIM") => self.stmt_dim(io, tokens, pos),
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
                self.mem.set_program_free(self.program.bytes_free());
                self.data = None;
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
        let target = self.parse_target(io, tokens, pos)?;
        skip_spaces(tokens, pos);
        if tokens.get(*pos) != Some(&b'=') {
            return Err(Error::Syntax);
        }
        let (value, end) = evaluate(tokens, *pos + 1, &mut self.mem, io)?;
        *pos = end;
        end_of_statement(tokens, pos)?;
        self.store(&target, value)?;
        Ok(Flow::Next)
    }

    /// A variable or array element to assign to: `name` or `name(subscripts)`.
    fn parse_target<I: Io>(
        &mut self,
        io: &mut I,
        tokens: &[u8],
        pos: &mut usize,
    ) -> Result<Target, Error> {
        let name = parse_name(tokens, pos)?.ok_or(Error::Syntax)?;
        let subs = if tokens.get(*pos) == Some(&b'(') {
            let (subs, end) = evaluate_subscripts(tokens, *pos, &mut self.mem, io)?;
            *pos = end;
            Some(subs)
        } else {
            None
        };
        Ok(Target { name, subs })
    }

    fn store(&mut self, target: &Target, value: Value) -> Result<(), Error> {
        match &target.subs {
            Some(subs) => self.mem.set_element(&target.name, subs, value),
            None => self.mem.assign(target.name.clone(), value),
        }
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
                _ if keyword(byte).is_some_and(|k| k == "TAB" || k == "SPC") => {
                    // TAB(n) / SPC(n): pad with spaces; no separator needed after.
                    let is_tab = keyword(byte) == Some("TAB");
                    *pos += 1;
                    let (n, end) = evaluate_number(src, *pos, &mut self.mem, io)?;
                    let n = to_count(n)?;
                    if n > 255 {
                        return Err(Error::IllegalQuantity);
                    }
                    let pad = if is_tab {
                        n.saturating_sub(self.column)
                    } else {
                        n
                    };
                    self.emit(io, &" ".repeat(pad));
                    *pos = end;
                    newline = true;
                    after_item = false;
                }
                _ if after_item => return Err(Error::Syntax),
                _ => {
                    let (value, end) = evaluate(src, *pos, &mut self.mem, io)?;
                    match value {
                        Value::Num(n) => self.emit(io, &format_number(n)),
                        Value::Str(text) => self.emit(io, &text),
                    }
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
        let (condition, end) = evaluate_number(tokens, *pos, &mut self.mem, io)?;
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
        if var.ends_with('$') {
            return Err(Error::TypeMismatch);
        }
        skip_spaces(tokens, pos);
        if tokens.get(*pos) != Some(&b'=') {
            return Err(Error::Syntax);
        }
        let (start, end) = evaluate_number(tokens, *pos + 1, &mut self.mem, io)?;
        *pos = end;
        expect_keyword(tokens, pos, "TO")?;
        let (limit, end) = evaluate_number(tokens, *pos, &mut self.mem, io)?;
        *pos = end;
        skip_spaces(tokens, pos);
        let mut step = 1.0;
        if tokens.get(*pos).and_then(|&b| keyword(b)) == Some("STEP") {
            *pos += 1;
            let (value, end) = evaluate_number(tokens, *pos, &mut self.mem, io)?;
            step = value;
            *pos = end;
        }
        end_of_statement(tokens, pos)?;

        self.mem.set_number(var.clone(), start);
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
            let value = self.mem.number(&frame.var) + frame.step;
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
            self.mem.set_number(var, value);
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

    /// Push a return point (line `line`, byte `pos`) and jump to `target`.
    fn call(&mut self, line: Option<u16>, pos: usize, target: u16) -> Result<Flow, Error> {
        if self.gosubs.len() >= MAX_GOSUB_DEPTH {
            return Err(Error::OutOfMemory);
        }
        self.gosubs.push(ReturnPoint { line, pos });
        Ok(Flow::Jump {
            line: Some(target),
            pos: 0,
        })
    }

    /// `ON expr GOTO l1,l2,...` / `ON expr GOSUB l1,l2,...`.
    fn stmt_on<I: Io>(
        &mut self,
        io: &mut I,
        line: Option<u16>,
        tokens: &[u8],
        pos: &mut usize,
    ) -> Result<Flow, Error> {
        let (selector, end) = evaluate_number(tokens, *pos, &mut self.mem, io)?;
        *pos = end;
        skip_spaces(tokens, pos);
        let word = tokens.get(*pos).and_then(|&b| keyword(b));
        if word != Some("GOTO") && word != Some("GOSUB") {
            return Err(Error::Syntax);
        }
        *pos += 1;
        let mut targets = Vec::new();
        loop {
            targets.push(parse_line_ref(tokens, pos)?);
            skip_spaces(tokens, pos);
            if tokens.get(*pos) == Some(&b',') {
                *pos += 1;
            } else {
                break;
            }
        }
        end_of_statement(tokens, pos)?;
        // Truncate toward zero; anything outside 1..=count falls through.
        let choice = libm::trunc(selector) as i64;
        let Some(&target) = usize::try_from(choice)
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| targets.get(i))
        else {
            return Ok(Flow::Next);
        };
        if word == Some("GOSUB") {
            self.call(line, *pos, target)
        } else {
            Ok(Flow::Jump {
                line: Some(target),
                pos: 0,
            })
        }
    }

    /// `DIM name(bound[,bound...])[, name(...)...]`.
    fn stmt_dim<I: Io>(
        &mut self,
        io: &mut I,
        tokens: &[u8],
        pos: &mut usize,
    ) -> Result<Flow, Error> {
        loop {
            let name = parse_name(tokens, pos)?.ok_or(Error::Syntax)?;
            if tokens.get(*pos) != Some(&b'(') {
                return Err(Error::Syntax);
            }
            let (bounds, end) = evaluate_subscripts(tokens, *pos, &mut self.mem, io)?;
            *pos = end;
            self.mem.dim(name, &bounds)?;
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

    /// `COLOR fg[, bg]`.
    fn stmt_color<I: Io>(
        &mut self,
        io: &mut I,
        tokens: &[u8],
        pos: &mut usize,
    ) -> Result<Flow, Error> {
        let (fg, end) = evaluate_number(tokens, *pos, &mut self.mem, io)?;
        *pos = end;
        let mut bg = None;
        skip_spaces(tokens, pos);
        if tokens.get(*pos) == Some(&b',') {
            let (value, end) = evaluate_number(tokens, *pos + 1, &mut self.mem, io)?;
            *pos = end;
            bg = Some(colour_number(value)?);
        }
        end_of_statement(tokens, pos)?;
        io.set_color(colour_number(fg)?, bg);
        Ok(Flow::Next)
    }

    /// `LOCATE row, col`.
    fn stmt_locate<I: Io>(
        &mut self,
        io: &mut I,
        tokens: &[u8],
        pos: &mut usize,
    ) -> Result<Flow, Error> {
        let (row, end) = evaluate_number(tokens, *pos, &mut self.mem, io)?;
        *pos = end;
        skip_spaces(tokens, pos);
        if tokens.get(*pos) != Some(&b',') {
            return Err(Error::Syntax);
        }
        let (col, end) = evaluate_number(tokens, *pos + 1, &mut self.mem, io)?;
        *pos = end;
        end_of_statement(tokens, pos)?;
        let (row, col) = (screen_position(row)?, screen_position(col)?);
        io.move_cursor(row, col);
        self.column = usize::from(col) - 1;
        Ok(Flow::Next)
    }

    /// `POKE addr, value`.
    fn stmt_poke<I: Io>(
        &mut self,
        io: &mut I,
        tokens: &[u8],
        pos: &mut usize,
    ) -> Result<Flow, Error> {
        let (address, end) = evaluate_number(tokens, *pos, &mut self.mem, io)?;
        *pos = end;
        skip_spaces(tokens, pos);
        if tokens.get(*pos) != Some(&b',') {
            return Err(Error::Syntax);
        }
        let (value, end) = evaluate_number(tokens, *pos + 1, &mut self.mem, io)?;
        *pos = end;
        end_of_statement(tokens, pos)?;
        io.poke(to_address(address)?, to_byte(value)?);
        Ok(Flow::Next)
    }

    /// Print `BREAK IN <line>` (or just `BREAK` at the prompt).
    fn stop<I: Io>(&mut self, io: &mut I, line: Option<u16>) {
        self.fresh_line(io);
        let message = match line {
            Some(n) => format!("BREAK IN {n}\n"),
            None => String::from("BREAK\n"),
        };
        self.emit(io, &message);
    }

    /// `READ target[, target...]`.
    fn stmt_read<I: Io>(
        &mut self,
        io: &mut I,
        tokens: &[u8],
        pos: &mut usize,
    ) -> Result<Flow, Error> {
        loop {
            let target = self.parse_target(io, tokens, pos)?;
            let item = next_item(&self.program, &mut self.data)?;
            let value = convert_item(&target.name, &item)?;
            self.store(&target, value)?;
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

    /// `INPUT ["prompt";] target[, target...]`.
    fn stmt_input<I: Io>(
        &mut self,
        io: &mut I,
        line: Option<u16>,
        tokens: &[u8],
        pos: &mut usize,
    ) -> Result<Flow, Error> {
        skip_spaces(tokens, pos);
        let mut prompt = String::new();
        if tokens.get(*pos) == Some(&b'"') {
            let start = *pos + 1;
            let end = tokens[start..]
                .iter()
                .position(|&b| b == b'"')
                .map_or(tokens.len(), |i| start + i);
            prompt = tokens[start..end].iter().map(|&b| char::from(b)).collect();
            *pos = (end + 1).min(tokens.len());
            skip_spaces(tokens, pos);
            if tokens.get(*pos) != Some(&b';') {
                return Err(Error::Syntax);
            }
            *pos += 1;
        }
        let mut targets = Vec::new();
        loop {
            targets.push(self.parse_target(io, tokens, pos)?);
            skip_spaces(tokens, pos);
            if tokens.get(*pos) == Some(&b',') {
                *pos += 1;
            } else {
                break;
            }
        }
        end_of_statement(tokens, pos)?;

        'attempt: loop {
            self.emit(io, &prompt);
            self.emit(io, "? ");
            let mut items: Vec<DataItem> = Vec::new();
            loop {
                match io.read_line() {
                    Line::Text(text) => {
                        // The user pressed Enter, so the cursor is at a line start.
                        self.column = 0;
                        items.extend(split_input(&text));
                    }
                    Line::Break => {
                        self.stop(io, line);
                        return Ok(Flow::End);
                    }
                    Line::Eof => return Ok(Flow::End),
                }
                if items.len() >= targets.len() {
                    break;
                }
                self.emit(io, "?? ");
            }
            if items.len() > targets.len() {
                self.emit(io, "?EXTRA IGNORED\n");
                items.truncate(targets.len());
            }
            let mut values = Vec::new();
            for (target, item) in targets.iter().zip(&items) {
                match convert_item(&target.name, item) {
                    Ok(value) => values.push(value),
                    Err(Error::Syntax | Error::TypeMismatch | Error::Overflow) => {
                        self.emit(io, "?REDO FROM START\n");
                        continue 'attempt;
                    }
                    Err(other) => return Err(other),
                }
            }
            for (target, value) in targets.iter().zip(values) {
                self.store(target, value)?;
            }
            return Ok(Flow::Next);
        }
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
        self.mem.clear_variables();
        self.loops.clear();
        self.gosubs.clear();
        self.data = None;
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

/// Index of the colon that ends a `DATA` statement (one outside double
/// quotes), or the end of the line.
fn data_statement_end(tokens: &[u8], from: usize) -> usize {
    let mut in_quotes = false;
    for (i, &b) in tokens.iter().enumerate().skip(from) {
        match b {
            b'"' => in_quotes = !in_quotes,
            b':' if !in_quotes => return i,
            _ => {}
        }
    }
    tokens.len()
}

/// A colour for `COLOR`: 0 to 7.
fn colour_number(value: f64) -> Result<u8, Error> {
    match u8::try_from(to_count(value)?) {
        Ok(n) if n <= 7 => Ok(n),
        _ => Err(Error::IllegalQuantity),
    }
}

/// A row or column for `LOCATE`: 1 to 999.
fn screen_position(value: f64) -> Result<u16, Error> {
    match u16::try_from(to_count(value)?) {
        Ok(n) if (1..=999).contains(&n) => Ok(n),
        _ => Err(Error::IllegalQuantity),
    }
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
