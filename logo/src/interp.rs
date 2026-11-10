//! The reference interpreter: the prompt loop, procedure definitions, dynamic
//! scope, and expression evaluation. Primitives live in `prims.rs`.

use crate::error::LogoError;
use crate::lex::{lex, Token};
use crate::parse::{Expr, Infix, Parser};
use crate::prims;
use crate::rng::Rng;
use crate::value::Value;
use crate::{Io, Line};
use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use turtle::{Canvas, Turtle};

/// Deepest procedure nesting before `Out of memory`.
const MAX_DEPTH: usize = 400;

/// Deepest nesting of `load` inside `load` before `Out of memory`.
const MAX_LOAD_DEPTH: usize = 8;

/// Why evaluation stopped early.
#[derive(Debug)]
pub(crate) enum Signal {
    Error(LogoError),
    /// `stop` inside a procedure.
    Stop,
    /// `output value` inside a procedure.
    Output(Value),
}

impl From<LogoError> for Signal {
    fn from(error: LogoError) -> Self {
        Signal::Error(error)
    }
}

/// The result of evaluating an expression: `None` for a command, a value for
/// an operation.
pub(crate) type Eval = Result<Option<Value>, Signal>;

/// The machine the interpreter runs on: console and screen.
pub(crate) struct Machine<'a, I: Io, C: Canvas> {
    pub(crate) io: &'a mut I,
    pub(crate) canvas: &'a mut C,
}

#[derive(Debug, Clone)]
struct Procedure {
    params: Vec<String>,
    /// The body, one source line each.
    body: Vec<String>,
}

#[derive(Debug, Default)]
struct Frame {
    vars: BTreeMap<String, Value>,
}

/// Which interpreter runs the code: the one over the syntax tree (the
/// reference), or the one over the lowered IR. Both must behave identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Engine {
    #[default]
    Ast,
    Ir,
    /// Compile to RV64 and run the machine code (needs an executor).
    Jit,
}

pub struct Interpreter {
    pub(crate) engine: Engine,
    /// How to run generated machine code, if this build can.
    pub(crate) executor: Option<crate::jit::ExecuteFn>,
    /// Compiled lines (procedure bodies mostly), by source text; cleared
    /// whenever a procedure is defined, since arities may change.
    jit_cache: BTreeMap<String, Rc<crate::jit::Compiled>>,
    procedures: BTreeMap<String, Procedure>,
    globals: BTreeMap<String, Value>,
    frames: Vec<Frame>,
    load_depth: usize,
    pub(crate) turtle: Turtle,
    pub(crate) rng: Rng,
}

impl Default for Interpreter {
    fn default() -> Self {
        Self::new()
    }
}

impl Interpreter {
    pub fn new() -> Self {
        Self::with_engine(Engine::Ast)
    }

    pub fn with_engine(engine: Engine) -> Self {
        Interpreter {
            engine,
            executor: None,
            jit_cache: BTreeMap::new(),
            procedures: BTreeMap::new(),
            globals: BTreeMap::new(),
            frames: Vec::new(),
            load_depth: 0,
            turtle: Turtle::new(),
            rng: Rng::new(),
        }
    }

    /// An interpreter that compiles to machine code, run by `execute`.
    pub fn with_executor(execute: crate::jit::ExecuteFn) -> Self {
        let mut interpreter = Self::with_engine(Engine::Jit);
        interpreter.executor = Some(execute);
        interpreter
    }

    /// Print the start-up banner.
    pub fn banner<I: Io>(&self, io: &mut I) {
        io.write_str("RISCV LOGO V0.2\n\n");
    }

    /// The prompt loop: show `? `, read a line, run it. Returns only when the
    /// console reports end of input.
    pub fn run_session<I: Io, C: Canvas>(&mut self, io: &mut I, canvas: &mut C) {
        self.turtle.refresh(canvas);
        let mut machine = Machine { io, canvas };
        loop {
            machine.io.write_str("? ");
            match machine.io.read_line() {
                Line::Text(text) => self.handle_line(&mut machine, &text),
                Line::Break => {}
                Line::Eof => return,
            }
        }
    }

    fn handle_line<I: Io, C: Canvas>(&mut self, m: &mut Machine<I, C>, text: &str) {
        let result = match lex(text) {
            Err(error) => Err(Signal::Error(error)),
            Ok(tokens) => {
                if tokens.first() == Some(&Token::Word(String::from("to"))) {
                    self.define(m, &tokens).map_err(Signal::Error)
                } else {
                    self.run_tokens(m, &tokens)
                }
            }
        };
        if let Err(Signal::Error(error)) = result {
            // Unwind whatever procedures were running.
            self.frames.clear();
            let mut message = error.message();
            message.push('\n');
            m.io.write_str(&message);
        }
    }

    /// `to name :a :b`, then body lines (prompt `> `) up to `end`.
    fn define<I: Io, C: Canvas>(
        &mut self,
        m: &mut Machine<I, C>,
        header: &[Token],
    ) -> Result<(), LogoError> {
        let (name, params) = Self::header(header)?;
        let mut body = Vec::new();
        loop {
            m.io.write_str("> ");
            match m.io.read_line() {
                Line::Text(line) => {
                    if line.trim().eq_ignore_ascii_case("end") {
                        break;
                    }
                    body.push(line);
                }
                Line::Break | Line::Eof => return Ok(()),
            }
        }
        self.install(name, params, body);
        Ok(())
    }

    /// The name and input names from a `to name :a :b` line.
    fn header(header: &[Token]) -> Result<(String, Vec<String>), LogoError> {
        let Some(Token::Word(name)) = header.get(1) else {
            return Err(LogoError::NotEnoughInputs(String::from("to")));
        };
        let params = header[2..]
            .iter()
            .filter_map(|t| match t {
                Token::Var(v) => Some(v.clone()),
                _ => None,
            })
            .collect();
        Ok((name.clone(), params))
    }

    fn install(&mut self, name: String, params: Vec<String>, body: Vec<String>) {
        self.procedures.insert(name, Procedure { params, body });
        // Inputs may have changed: anything compiled so far may have misread a call.
        self.jit_cache.clear();
    }

    /// Every procedure as source text, sorted by name (what `save` writes).
    pub(crate) fn procedures_text(&self) -> String {
        let mut text = String::new();
        for (name, procedure) in &self.procedures {
            text.push_str("to ");
            text.push_str(name);
            for param in &procedure.params {
                text.push_str(" :");
                text.push_str(param);
            }
            text.push('\n');
            for line in &procedure.body {
                text.push_str(line);
                text.push('\n');
            }
            text.push_str("end\n");
        }
        text
    }

    /// Run the text of a file as if typed, without prompts (`load`).
    pub(crate) fn load_text<I: Io, C: Canvas>(
        &mut self,
        m: &mut Machine<I, C>,
        text: &str,
    ) -> Result<(), Signal> {
        if self.load_depth >= MAX_LOAD_DEPTH {
            return Err(LogoError::OutOfMemory.into());
        }
        self.load_depth += 1;
        let result = self.load_lines(m, text);
        self.load_depth -= 1;
        result
    }

    fn load_lines<I: Io, C: Canvas>(
        &mut self,
        m: &mut Machine<I, C>,
        text: &str,
    ) -> Result<(), Signal> {
        let mut lines = text.lines();
        while let Some(line) = lines.next() {
            let tokens = lex(line)?;
            if tokens.first() == Some(&Token::Word(String::from("to"))) {
                let (name, params) = Self::header(&tokens)?;
                let mut body = Vec::new();
                let mut closed = false;
                for body_line in lines.by_ref() {
                    if body_line.trim().eq_ignore_ascii_case("end") {
                        closed = true;
                        break;
                    }
                    body.push(String::from(body_line));
                }
                if !closed {
                    return Err(LogoError::MissingEnd.into());
                }
                self.install(name, params, body);
            } else {
                self.run_tokens(m, &tokens)?;
            }
        }
        Ok(())
    }

    /// Number of inputs of a primitive or user procedure, if it exists.
    fn arity(&self, name: &str) -> Option<usize> {
        prims::arity(name).or_else(|| self.procedures.get(name).map(|p| p.params.len()))
    }

    /// Read and run `source` (a line, a procedure body line, or a list's text).
    pub(crate) fn run_source<I: Io, C: Canvas>(
        &mut self,
        m: &mut Machine<I, C>,
        source: &str,
    ) -> Result<(), Signal> {
        if let (Engine::Jit, Some(execute)) = (self.engine, self.executor) {
            let unit = match self.jit_cache.get(source) {
                Some(unit) => Rc::clone(unit),
                None => {
                    let tokens = lex(source)?;
                    let unit = Rc::new(self.compile_tokens(&tokens));
                    self.jit_cache
                        .insert(String::from(source), Rc::clone(&unit));
                    unit
                }
            };
            return crate::jit::run_unit(self, m, &unit, execute);
        }
        let tokens = lex(source)?;
        self.run_tokens(m, &tokens)
    }

    /// Lower and compile a line of tokens to machine code.
    fn compile_tokens(&self, tokens: &[Token]) -> crate::jit::Compiled {
        let arity = |name: &str| self.arity(name);
        crate::jit::compile(&crate::ir::lower(tokens, &arity))
    }

    /// Run instructions one at a time, each read just before it runs, so an
    /// error leaves the earlier ones done.
    pub(crate) fn run_tokens<I: Io, C: Canvas>(
        &mut self,
        m: &mut Machine<I, C>,
        tokens: &[Token],
    ) -> Result<(), Signal> {
        if let (Engine::Jit, Some(execute)) = (self.engine, self.executor) {
            let unit = self.compile_tokens(tokens);
            return crate::jit::run_unit(self, m, &unit, execute);
        }
        if self.engine == Engine::Ir {
            let lowered = {
                let arity = |name: &str| self.arity(name);
                crate::ir::lower(tokens, &arity)
            };
            return crate::exec::run(self, m, &lowered);
        }
        let mut pos = 0;
        while pos < tokens.len() {
            let expr = {
                let arity = |name: &str| self.arity(name);
                let mut parser = Parser::new(tokens, pos, &arity);
                let expr = parser.instruction()?;
                pos = parser.pos();
                expr
            };
            if let Some(value) = self.eval(m, &expr)? {
                return Err(LogoError::DontSayWhatToDo(value).into());
            }
        }
        Ok(())
    }

    // --- variables -------------------------------------------------------

    pub(crate) fn lookup(&self, name: &str) -> Option<Value> {
        self.frames
            .iter()
            .rev()
            .find_map(|frame| frame.vars.get(name))
            .or_else(|| self.globals.get(name))
            .cloned()
    }

    /// Set the variable where it is found (newest frame first), or make a global.
    pub(crate) fn assign(&mut self, name: String, value: Value) {
        match self
            .frames
            .iter_mut()
            .rev()
            .find(|frame| frame.vars.contains_key(&name))
        {
            Some(frame) => {
                frame.vars.insert(name, value);
            }
            None => {
                self.globals.insert(name, value);
            }
        }
    }

    pub(crate) fn in_procedure(&self) -> bool {
        !self.frames.is_empty()
    }

    // --- evaluation ------------------------------------------------------

    /// Evaluate `expr`, which must produce a value for `consumer`.
    fn value_of<I: Io, C: Canvas>(
        &mut self,
        m: &mut Machine<I, C>,
        expr: &Expr,
        consumer: &str,
    ) -> Result<Value, Signal> {
        match self.eval(m, expr)? {
            Some(value) => Ok(value),
            None => {
                let producer = match expr {
                    Expr::Call(name, _) => name.clone(),
                    _ => String::from("?"),
                };
                Err(LogoError::DidntOutput(producer, String::from(consumer)).into())
            }
        }
    }

    pub(crate) fn eval<I: Io, C: Canvas>(&mut self, m: &mut Machine<I, C>, expr: &Expr) -> Eval {
        match expr {
            Expr::Num(n) => Ok(Some(Value::Num(*n))),
            Expr::Word(w) => Ok(Some(Value::Word(w.clone()))),
            Expr::List(items) => Ok(Some(Value::List(items.clone()))),
            Expr::Var(name) => match self.lookup(name) {
                Some(value) => Ok(Some(value)),
                None => Err(LogoError::NoValue(name.clone()).into()),
            },
            Expr::Neg(inner) => {
                let value = self.value_of(m, inner, "-")?;
                match value {
                    Value::Num(n) => Ok(Some(Value::Num(-n))),
                    other => Err(LogoError::DoesntLike(String::from("-"), other).into()),
                }
            }
            Expr::Infix(op, left, right) => {
                let a = self.value_of(m, left, op.symbol())?;
                let b = self.value_of(m, right, op.symbol())?;
                Ok(Some(infix(*op, a, b)?))
            }
            Expr::Call(name, args) => {
                let mut values = Vec::with_capacity(args.len());
                for arg in args {
                    values.push(self.value_of(m, arg, name)?);
                }
                self.call(m, name, values)
            }
        }
    }

    /// Run a primitive or user procedure on already evaluated inputs.
    pub(crate) fn call<I: Io, C: Canvas>(
        &mut self,
        m: &mut Machine<I, C>,
        name: &str,
        args: Vec<Value>,
    ) -> Eval {
        if m.io.break_pressed() {
            return Err(LogoError::Stopped.into());
        }
        if prims::arity(name).is_some() {
            return prims::call(self, m, name, args);
        }
        let Some(procedure) = self.procedures.get(name).cloned() else {
            return Err(LogoError::DontKnowHow(String::from(name)).into());
        };
        if self.frames.len() >= MAX_DEPTH {
            return Err(LogoError::OutOfMemory.into());
        }
        let mut frame = Frame::default();
        for (param, value) in procedure.params.iter().zip(args) {
            frame.vars.insert(param.clone(), value);
        }
        self.frames.push(frame);
        let mut outcome: Eval = Ok(None);
        for line in &procedure.body {
            match self.run_source(m, line) {
                Ok(()) => {}
                Err(Signal::Stop) => break,
                Err(Signal::Output(value)) => {
                    outcome = Ok(Some(value));
                    break;
                }
                Err(error) => {
                    outcome = Err(error);
                    break;
                }
            }
        }
        self.frames.pop();
        outcome
    }
}

/// The infix operators.
pub(crate) fn infix(op: Infix, a: Value, b: Value) -> Result<Value, LogoError> {
    if op == Infix::Eq {
        return Ok(Value::boolean(a.equals(&b)));
    }
    let symbol = String::from(op.symbol());
    let (x, y) = match (&a, &b) {
        (Value::Num(x), Value::Num(y)) => (*x, *y),
        (Value::Num(_), _) => return Err(LogoError::DoesntLike(symbol, b)),
        _ => return Err(LogoError::DoesntLike(symbol, a)),
    };
    let result = match op {
        Infix::Add => x + y,
        Infix::Sub => x - y,
        Infix::Mul => x * y,
        Infix::Div => {
            if y == 0.0 {
                return Err(LogoError::DoesntLike(symbol, b));
            }
            x / y
        }
        Infix::Lt => return Ok(Value::boolean(x < y)),
        Infix::Gt => return Ok(Value::boolean(x > y)),
        Infix::Eq => unreachable!("handled above"),
    };
    if result.is_finite() {
        Ok(Value::Num(result))
    } else {
        Err(LogoError::OutOfRange)
    }
}
