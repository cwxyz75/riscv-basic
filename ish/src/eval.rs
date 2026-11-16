//! The evaluator: statements, words, calls and pipelines over a [`Host`] (SHELL-SPEC sections 5 and 9).
//!
//! Position decides how a command runs: as a statement it streams to the current stdout and its value is dropped; in value
//! position (the right side of `=`, an argument in parentheses) its output is captured as a list of lines. A function's result
//! is the value of its last statement; a call written as a statement has no value (N2, N3).

use crate::ast::*;
use crate::host::*;
use crate::value::{Closure, NoValue, VMap, Val};
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use data::{ArithError, Number};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailKind {
    /// An external program exited non-zero (or a pipeline stage did).
    Exit,
    /// A program could not be started: always an error, never a status.
    Spawn,
    /// `fail value [code]`.
    User,
    /// A language error: a wrong type, an undefined name, a limit.
    Error,
    /// A key or index that is not there (`$m.key`, `require`).
    MissingKey,
    /// A capture past its cap.
    CaptureLimit,
    /// A bounded wait or `run` that ran out of time.
    Timeout,
    /// The reader of our output is gone.
    Pipe,
    /// The user pressed Ctrl-C: ends whatever the shell was running, and is never caught.
    Interrupt,
}

#[derive(Debug, Clone)]
pub struct Fail {
    pub value: Val,
    pub code: i32,
    pub kind: FailKind,
}

#[derive(Debug, Clone)]
pub enum Flow {
    Fail(Fail),
    /// `ok value`: leave the function with a value.
    Ok(Val),
    Exit(i32),
    Break,
    Continue,
}

pub type Res<T> = Result<T, Flow>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Streams to the current standard output.
    Stmt,
    /// Captured: a program's output is its list of lines.
    Value,
}

#[derive(Clone)]
pub enum Sink {
    Host(Stream),
    Mem(Rc<RefCell<Vec<u8>>>),
    Null,
}

#[derive(Clone)]
pub struct Io {
    pub stdin: Stream,
    pub stdout: Sink,
    pub stderr: Sink,
}

/// One function's bindings: scopes from the captured values (if any) outwards in, and the `defer`s to run at its exit.
struct Frame {
    scopes: Vec<BTreeMap<String, Val>>,
    defers: Vec<Rc<Closure>>,
    /// How many leading scopes are captured values (not bindings of this function, B1).
    captured: usize,
    /// A module's frame does not see the importer's top-level names.
    isolated: bool,
}

impl Frame {
    fn new(scopes: Vec<BTreeMap<String, Val>>, captured: usize, isolated: bool) -> Frame {
        Frame {
            scopes,
            defers: Vec::new(),
            captured,
            isolated,
        }
    }
}

pub const MAX_CALL_DEPTH: usize = 48;
pub const DEFAULT_CAP: usize = 1 << 20;

pub struct Shell<H: Host> {
    pub host: H,
    frames: Vec<Frame>,
    pub(crate) fns: BTreeMap<String, Rc<Closure>>,
    pub(crate) io: Io,
    pub cap: usize,
    pub(crate) pos: Pos,
    pub(crate) imports: BTreeMap<String, Val>,
    pub(crate) importing: Vec<String>,
    depth: usize,
    /// The module being run (import), for finding sibling functions.
    pub(crate) module: Option<Rc<str>>,
    /// The name messages start with (`ish: ...`).
    pub name: String,
    /// The S4e compatibility profile of the OS shell: `sh:` messages, `[exit N]` reports, `cd` printing its own errors.
    pub legacy: bool,
    /// What `help` prints.
    pub help: String,
    /// Set when `exit` (or `halt`) ended the shell with a status.
    exit_requested: Option<i32>,
    /// Counts statements and loop iterations, so the host is asked about interrupts every 4096th (a system call each time
    /// would starve everything else on the machine).
    poll_tick: u32,
    /// Background jobs of this run (killed at the end unless detached, J8).
    pub(crate) jobs: Vec<Rc<RefCell<crate::job::JobState>>>,
    /// `with-env` additions for the command being run.
    pub(crate) env_extra: Vec<(String, String)>,
    /// An interrupt is being handled: a second one ends the script at once (J9).
    interrupting: bool,
}

/// Process-level result of running a script or a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub status: i32,
}

pub(crate) fn err_val(kind: &str, msg: &str) -> Val {
    Val::record(vec![("kind", Val::str(kind)), ("message", Val::str(msg))])
}

pub(crate) fn error(msg: &str) -> Flow {
    Flow::Fail(Fail {
        value: err_val("error", msg),
        code: 1,
        kind: FailKind::Error,
    })
}

impl<H: Host> Shell<H> {
    pub fn new(host: H) -> Shell<H> {
        let mut sh = Shell::bare(host);
        sh.set_args(Vec::new());
        // `$env` is an ordinary binding (A2): the environment the shell started with
        let mut m = VMap::new();
        for (k, v) in sh.host.environ() {
            m.set(&k, Val::string(v));
        }
        sh.set_var("env", Val::map(m));
        sh
    }

    fn bare(host: H) -> Shell<H> {
        let io = Io {
            stdin: host.stdin(),
            stdout: Sink::Host(host.stdout()),
            stderr: Sink::Host(host.stderr()),
        };
        Shell {
            host,
            frames: vec![Frame::new(vec![BTreeMap::new()], 0, false)],
            fns: BTreeMap::new(),
            io,
            cap: DEFAULT_CAP,
            pos: Pos::default(),
            imports: BTreeMap::new(),
            importing: Vec::new(),
            depth: 0,
            module: None,
            name: String::from("ish"),
            legacy: false,
            help: String::new(),
            exit_requested: None,
            poll_tick: 0,
            jobs: Vec::new(),
            env_extra: Vec::new(),
            interrupting: false,
        }
    }

    pub fn set_args(&mut self, args: Vec<String>) {
        let l = Val::list(args.iter().map(|a| Val::str(a)).collect());
        self.frames[0].scopes[0].insert(String::from("args"), l);
    }

    pub fn set_var(&mut self, name: &str, v: Val) {
        self.frames[0].scopes[0].insert(String::from(name), v);
    }

    /// The names of the variables visible now (for completion).
    pub fn var_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.frames[0]
            .scopes
            .iter()
            .flat_map(|s| s.keys().cloned())
            .collect();
        if let Some(top) = self.frames.last() {
            v.extend(top.scopes.iter().flat_map(|s| s.keys().cloned()));
        }
        v.sort();
        v.dedup();
        v
    }

    /// The names of the shell functions (for completion).
    pub fn fn_names(&self) -> Vec<String> {
        self.fns
            .keys()
            .filter(|k| !k.contains("::"))
            .cloned()
            .collect()
    }

    pub fn var(&self, name: &str) -> Option<Val> {
        self.lookup(name)
    }

    // ---- output ----

    pub(crate) fn write_sink(&mut self, sink: &Sink, bytes: &[u8]) -> Res<()> {
        match sink {
            Sink::Host(s) => match self.host.write(*s, bytes) {
                Ok(()) => Ok(()),
                Err(HostError::Closed) => Err(Flow::Fail(Fail {
                    value: Val::str("broken pipe"),
                    code: 141,
                    kind: FailKind::Pipe,
                })),
                Err(e) => Err(error(&format!("write failed: {}", e.message()))),
            },
            Sink::Mem(m) => {
                let mut b = m.borrow_mut();
                if b.len() + bytes.len() > self.cap {
                    return Err(Flow::Fail(Fail {
                        value: err_val("capture-limit", "output too large to capture"),
                        code: 1,
                        kind: FailKind::CaptureLimit,
                    }));
                }
                b.extend_from_slice(bytes);
                Ok(())
            }
            Sink::Null => Ok(()),
        }
    }

    pub(crate) fn out(&mut self, bytes: &[u8]) -> Res<()> {
        let s = self.io.stdout.clone();
        self.write_sink(&s, bytes)
    }

    pub(crate) fn err_out(&mut self, bytes: &[u8]) -> Res<()> {
        let s = self.io.stderr.clone();
        self.write_sink(&s, bytes)
    }

    // ---- variables and scopes (B1 to B4) ----

    fn lookup(&self, name: &str) -> Option<Val> {
        let top = self.frames.last()?;
        for sc in top.scopes.iter().rev() {
            if let Some(v) = sc.get(name) {
                return Some(v.clone());
            }
        }
        if !top.isolated {
            for sc in self.frames[0].scopes.iter().rev() {
                if let Some(v) = sc.get(name) {
                    return Some(v.clone());
                }
            }
        }
        None
    }

    fn bound_here(&self, name: &str) -> bool {
        self.frames
            .last()
            .is_some_and(|f| f.scopes[f.captured..].iter().any(|s| s.contains_key(name)))
    }

    /// `x = v` (B1): a new binding, an error if the function already has one.
    fn bind_new(&mut self, name: &str, v: Val) -> Res<()> {
        if self.bound_here(name) {
            return Err(error(&format!(
                "`{name}` is already bound here; to change it write `set {name} = ...`"
            )));
        }
        self.bind_loop(name, v);
        Ok(())
    }

    /// A binding that needs no check: a loop variable, a catch name, a parameter.
    fn bind_loop(&mut self, name: &str, v: Val) {
        let top = self.frames.len() - 1;
        if let Some(sc) = self.frames[top].scopes.last_mut() {
            sc.insert(String::from(name), v);
        }
    }

    fn push_scope(&mut self) {
        let top = self.frames.len() - 1;
        self.frames[top].scopes.push(BTreeMap::new());
    }

    fn pop_scope(&mut self) {
        let top = self.frames.len() - 1;
        self.frames[top].scopes.pop();
    }

    /// The binding `set` changes: this function's (including captured values), never another function's.
    fn binding_mut(&mut self, name: &str) -> Option<&mut Val> {
        let top = self.frames.len() - 1;
        self.frames[top]
            .scopes
            .iter_mut()
            .rev()
            .find_map(|s| s.get_mut(name))
    }

    fn snapshot(&self) -> BTreeMap<String, Val> {
        let mut m = BTreeMap::new();
        if let Some(f) = self.frames.last() {
            for sc in &f.scopes {
                for (k, v) in sc {
                    m.insert(k.clone(), v.clone());
                }
            }
        }
        m
    }

    /// The environment a program starts with: the value of `$env`, plus `with-env` additions (A2, A3). A value that is not
    /// text is an error naming the key.
    pub(crate) fn child_env(&self) -> Res<Vec<(String, String)>> {
        let mut out: Vec<(String, String)> = Vec::new();
        if let Some(Val::Map(m)) = self.lookup("env") {
            for (k, v) in m.iter() {
                match v.word() {
                    Some(t) => out.push((String::from(k), t)),
                    None => {
                        return Err(error(&format!(
                            "$env.{k} is a {}, not text: a program's environment holds only text",
                            v.kind()
                        )))
                    }
                }
            }
        }
        for (k, v) in &self.env_extra {
            out.retain(|(n, _)| n != k);
            out.push((k.clone(), v.clone()));
        }
        Ok(out)
    }

    // ---- running scripts and lines ----

    /// Parse and run a whole script. Parse and uncaught errors go to stderr; the result is the process status.
    pub fn run_script(&mut self, src: &str) -> i32 {
        match crate::parser::parse(src) {
            Err(e) => {
                let _ = self.err_out(format!("{}: {e}\n", self.name).as_bytes());
                2
            }
            Ok(stmts) => {
                let r = self.exec_block(&stmts, false);
                let r = self.run_defers_top(r);
                self.kill_jobs();
                self.finish(r.map(|_| ()))
            }
        }
    }

    /// Run one interactive line: errors are shown and the shell goes on. At the prompt, and only there, a top-level call to
    /// a builtin or shell function that gives a value shows it (I1), a list of strings as lines and anything else as JSON.
    /// Returns the status of the line.
    pub fn run_line(&mut self, src: &str) -> i32 {
        match crate::parser::parse(src) {
            Err(e) => {
                let text = if self.legacy {
                    e.msg.clone()
                } else {
                    e.to_string()
                };
                let _ = self.err_out(format!("{}: {text}\n", self.name).as_bytes());
                2
            }
            Ok(stmts) => {
                let r = self.exec_line(&stmts);
                match r {
                    Ok(Val::Null | Val::Void(_)) => 0,
                    Ok(v) => {
                        let text = crate::builtins::echo_text(&v);
                        let _ = self.out(text.as_bytes());
                        0
                    }
                    Err(Flow::Fail(f)) if self.legacy && f.kind == FailKind::Exit => {
                        // S4e style: one `[exit N]` per stage that did not end with 0 (a closed pipe is not reported)
                        let list: Vec<i64> = match &f.value {
                            Val::Map(m) => match m.get("statuses") {
                                Some(Val::List(l)) => l
                                    .iter()
                                    .filter_map(|v| match v {
                                        Val::Num(Number::Int(i)) => Some(*i),
                                        _ => None,
                                    })
                                    .collect(),
                                _ => Vec::new(),
                            },
                            _ => Vec::new(),
                        };
                        let mut text = String::new();
                        for st in list.into_iter().filter(|&s| s != 0 && s != 141) {
                            text.push_str(&format!("[exit {st}]\n"));
                        }
                        let _ = self.out(text.as_bytes());
                        f.code
                    }
                    Err(f) => self.finish(Err(f)),
                }
            }
        }
    }

    /// The statements of one prompt line. The last one, if it is a call to a builtin or a shell function, is run for its
    /// value (I1); an expression statement gives its value too.
    fn exec_line(&mut self, stmts: &[Stmt]) -> Res<Val> {
        let Some((last, init)) = stmts.split_last() else {
            return Ok(Val::Null);
        };
        self.exec_block(init, false)?;
        self.check_interrupt()?;
        match last {
            Stmt::Cmd(ao) if self.is_shell_call(ao) => {
                self.pos = ao.first.stages[0].pos;
                self.exec_and_or(ao, Mode::Value)
            }
            Stmt::Expr(e) => self.eval_expr(e),
            s => {
                self.exec(s, false)?;
                Ok(Val::Null)
            }
        }
    }

    /// A lone call to a builtin or a shell function (no pipe, no list, no prefix, no redirection).
    fn is_shell_call(&self, ao: &AndOr) -> bool {
        if !ao.rest.is_empty()
            || ao.first.negate
            || ao.first.prefix.is_some()
            || ao.first.stages.len() != 1
        {
            return false;
        }
        let c = &ao.first.stages[0];
        if !c.redirs.is_empty() {
            return false;
        }
        match c.words.first().map(|w| w.parts.as_slice()) {
            Some([Part::Lit(t)]) => self.is_callable(t),
            _ => false,
        }
    }

    fn run_defers_top(&mut self, r: Res<Val>) -> Res<Val> {
        let ds: Vec<Rc<Closure>> = core::mem::take(&mut self.frames[0].defers);
        let mut r = r;
        for d in ds.into_iter().rev() {
            // a second interrupt while cleaning up ends the script at once, skipping the rest (J9)
            if self.interrupting && self.host.interrupted() {
                return Err(Flow::Exit(130));
            }
            if let Err(e) = self.call_closure(&d, vec![]) {
                if r.is_ok() {
                    r = Err(e);
                }
            }
        }
        r
    }

    /// The status `exit` asked for, if the last thing run ended the shell.
    pub fn take_exit(&mut self) -> Option<i32> {
        self.exit_requested.take()
    }

    /// Report an uncaught flow; the status.
    fn finish(&mut self, r: Res<()>) -> i32 {
        match r {
            Ok(()) => 0,
            Err(Flow::Ok(_)) => 0,
            Err(Flow::Exit(n)) => {
                self.exit_requested = Some(n);
                n
            }
            Err(Flow::Break | Flow::Continue) => {
                let _ = self.err_out(
                    format!("{}: break or continue outside a loop\n", self.name).as_bytes(),
                );
                1
            }
            Err(Flow::Fail(f)) => {
                if f.kind == FailKind::Interrupt {
                    let _ = self.out(b"^C\n");
                } else if f.kind != FailKind::Pipe
                    && !matches!(&f.value, Val::Str(s) if s.is_empty())
                {
                    let text = match &f.value {
                        Val::Str(s) => String::from(&**s),
                        Val::Map(m) if matches!(m.get("kind"), Some(Val::Str(k)) if &**k == "exit") =>
                        {
                            let cmd = m.get("command").and_then(Val::as_str).unwrap_or("command");
                            format!("{cmd}: exit status {}", f.code)
                        }
                        Val::Map(m) if m.get("message").is_some() => {
                            let msg = m.get("message").and_then(Val::as_str).unwrap_or("");
                            match m.get("kind").and_then(Val::as_str) {
                                Some("spawn") => {
                                    let cmd = m.get("command").and_then(Val::as_str).unwrap_or("");
                                    format!("{cmd}: {msg}")
                                }
                                _ => String::from(msg),
                            }
                        }
                        v => crate::builtins::json_text(v),
                    };
                    let _ = self.err_out(format!("{}: {text}\n", self.name).as_bytes());
                }
                f.code
            }
        }
    }

    // ---- statements ----

    fn check_interrupt(&mut self) -> Res<()> {
        self.poll_tick = self.poll_tick.wrapping_add(1);
        if self.poll_tick & 4095 != 1 {
            return Ok(());
        }
        self.poll_interrupt()
    }

    /// Ask the host now (a statement boundary, a loop turn, the inside of a wait): an interrupt is a failure of kind
    /// `interrupt` that `defer` runs for and `try` can catch (J9).
    pub(crate) fn poll_interrupt(&mut self) -> Res<()> {
        if self.host.interrupted() {
            self.interrupting = true;
            return Err(Flow::Fail(Fail {
                value: Val::str("interrupted"),
                code: 130,
                kind: FailKind::Interrupt,
            }));
        }
        Ok(())
    }

    /// Run statements in the current scope; with `value` the last one's value is the result (a call statement gives none).
    pub fn exec_block(&mut self, stmts: &[Stmt], value: bool) -> Res<Val> {
        let mut last = Val::Null;
        for s in stmts {
            self.check_interrupt()?;
            last = self.exec(s, value)?;
        }
        Ok(last)
    }

    /// A block with its own scope: what it binds ends at its `end` (B3).
    fn exec_scoped(&mut self, stmts: &[Stmt], value: bool) -> Res<Val> {
        self.push_scope();
        let r = self.exec_block(stmts, value);
        self.pop_scope();
        r
    }

    /// The name a call statement is shown by in the no-value error.
    fn call_name(ao: &AndOr) -> Rc<str> {
        let first = ao.first.stages.first().and_then(|c| c.words.first());
        match first.map(|w| w.parts.as_slice()) {
            Some([Part::Lit(t)]) => Rc::from(t.as_str()),
            _ => Rc::from("call"),
        }
    }

    /// A value that is no value (N3).
    pub(crate) fn need_value(&self, v: Val) -> Res<Val> {
        if let Val::Void(nv) = &v {
            let func = nv.func.as_deref().unwrap_or("that block");
            return Err(error(&format!(
                "{func} ended with a call statement, so it has no value; write ({} ...) or ok ({} ...)",
                nv.call, nv.call
            )));
        }
        Ok(v)
    }

    fn exec(&mut self, s: &Stmt, value: bool) -> Res<Val> {
        match s {
            Stmt::Assign { name, expr, pos } => {
                self.pos = *pos;
                let v = self.eval_expr(expr)?;
                self.bind_new(name, v)?;
                Ok(Val::Null)
            }
            Stmt::Set {
                name,
                path,
                expr,
                pos,
            } => {
                self.pos = *pos;
                let v = self.eval_expr(expr)?;
                self.set_binding(name, path, v)?;
                Ok(Val::Null)
            }
            Stmt::Fn { name, def } => {
                let c = Rc::new(Closure {
                    def: def.clone(),
                    env: BTreeMap::new(),
                    module: self.module.clone(),
                });
                let key = match &self.module {
                    Some(m) => format!("{m}::{name}"),
                    None => name.clone(),
                };
                self.fns.insert(key, c);
                Ok(Val::Null)
            }
            Stmt::If {
                cond,
                then,
                els,
                pos,
            } => {
                self.pos = *pos;
                let c = self.eval_expr(cond)?;
                let Val::Bool(b) = c else {
                    return Err(error(&format!("`if` needs a boolean, got {}", c.kind())));
                };
                if b {
                    self.exec_scoped(then, value)
                } else if let Some(e) = els {
                    self.exec_scoped(e, value)
                } else {
                    Ok(Val::Null)
                }
            }
            Stmt::While {
                cond,
                body,
                until,
                pos,
            } => {
                self.pos = *pos;
                loop {
                    self.check_interrupt()?;
                    let c = self.eval_expr(cond)?;
                    let Val::Bool(b) = c else {
                        return Err(error(&format!("`while` needs a boolean, got {}", c.kind())));
                    };
                    if b == *until {
                        break;
                    }
                    match self.exec_scoped(body, false) {
                        Ok(_) | Err(Flow::Continue) => {}
                        Err(Flow::Break) => break,
                        Err(e) => return Err(e),
                    }
                }
                Ok(Val::Null)
            }
            Stmt::For {
                var,
                items,
                body,
                pos,
            } => {
                self.pos = *pos;
                let mut list = Vec::new();
                for w in items {
                    let mut texts: Vec<Val> = Vec::new();
                    self.eval_word_for(w, &mut texts)?;
                    list.extend(texts);
                }
                for item in list {
                    self.check_interrupt()?;
                    self.push_scope();
                    self.bind_loop(var, item);
                    let r = self.exec_block(body, false);
                    self.pop_scope();
                    match r {
                        Ok(_) | Err(Flow::Continue) => {}
                        Err(Flow::Break) => break,
                        Err(e) => return Err(e),
                    }
                }
                Ok(Val::Null)
            }
            Stmt::Try {
                body,
                name,
                code_name,
                handler,
                pos,
            } => {
                self.pos = *pos;
                match self.exec_scoped(body, value) {
                    Err(Flow::Fail(f)) if !matches!(f.kind, FailKind::Pipe) => {
                        self.push_scope();
                        self.bind_loop(name, f.value.clone());
                        if let Some(c) = code_name {
                            self.bind_loop(c, Val::int(i64::from(f.code)));
                        }
                        let r = self.exec_block(handler, value);
                        self.pop_scope();
                        r
                    }
                    r => r,
                }
            }
            Stmt::Defer(def) => {
                let c = Rc::new(Closure {
                    def: def.clone(),
                    env: self.snapshot(),
                    module: self.module.clone(),
                });
                let top = self.frames.len() - 1;
                self.frames[top].defers.push(c);
                Ok(Val::Null)
            }
            Stmt::Expr(e) => {
                if let Expr::Word(w) = e {
                    self.pos = w.pos;
                }
                self.eval_expr(e)
            }
            Stmt::Cmd(ao) => {
                self.exec_and_or(ao, Mode::Stmt)?;
                // a call written as a statement has no value (N2)
                Ok(Val::Void(Rc::new(NoValue {
                    func: None,
                    call: Self::call_name(ao),
                })))
            }
        }
    }

    /// `set x.a.b = v` (B2): change a binding of this function.
    fn set_binding(&mut self, name: &str, path: &[Seg], v: Val) -> Res<()> {
        let keys: Vec<Val> = path
            .iter()
            .map(|s| match s {
                Seg::Name(n) => Ok(Val::str(n)),
                Seg::Dyn(w) => self.eval_word(w),
            })
            .collect::<Res<_>>()?;
        let Some(slot) = self.binding_mut(name) else {
            return Err(error(&format!(
                "there is no binding `{name}` to change here (`set` changes only bindings of the current function; write `{name} = ...` to make one)"
            )));
        };
        if keys.is_empty() {
            *slot = v;
            return Ok(());
        }
        let cur = take_val(slot);
        match crate::builtins::set_path(cur.clone(), &keys, v) {
            Ok(nv) => {
                if let Some(slot) = self.binding_mut(name) {
                    *slot = nv;
                }
                Ok(())
            }
            Err(e) => {
                if let Some(slot) = self.binding_mut(name) {
                    *slot = cur;
                }
                Err(e)
            }
        }
    }

    // ---- callables ----

    pub(crate) fn find_fn(&self, name: &str) -> Option<Rc<Closure>> {
        if let Some(m) = &self.module {
            if let Some(c) = self.fns.get(&format!("{m}::{name}")) {
                return Some(c.clone());
            }
        }
        self.fns.get(name).cloned()
    }

    pub(crate) fn is_callable(&self, name: &str) -> bool {
        self.find_fn(name).is_some() || crate::builtins::is_builtin(name)
    }

    /// Call a closure with values. The function's value is its last statement's (a call statement gives none), or an `ok`.
    pub fn call_closure(&mut self, c: &Rc<Closure>, args: Vec<Val>) -> Res<Val> {
        if self.depth >= MAX_CALL_DEPTH {
            return Err(error("calls nested too deeply"));
        }
        let def = c.def.clone();
        let mut params = BTreeMap::new();
        if def.params.is_empty() {
            params.insert(String::from("args"), Val::list(args));
        } else {
            if args.len() != def.params.len() {
                let who = def.name.as_deref().unwrap_or("function");
                return Err(error(&format!(
                    "{who} takes {} argument(s), got {}",
                    def.params.len(),
                    args.len()
                )));
            }
            for (p, a) in def.params.iter().zip(args) {
                params.insert(p.clone(), a);
            }
        }
        self.frames
            .push(Frame::new(vec![c.env.clone(), params], 1, false));
        self.depth += 1;
        let saved_module = core::mem::replace(&mut self.module, c.module.clone());
        let r = self.exec_block(&def.body, true);
        let frame_defers = self
            .frames
            .last_mut()
            .map(|f| core::mem::take(&mut f.defers))
            .unwrap_or_default();
        let mut r = match r {
            Err(Flow::Ok(v)) => Ok(v),
            Ok(Val::Void(nv)) => Ok(Val::Void(Rc::new(NoValue {
                func: Some(Rc::from(def.name.as_deref().unwrap_or("that function"))),
                call: nv.call.clone(),
            }))),
            r => r,
        };
        for d in frame_defers.into_iter().rev() {
            if self.interrupting && self.host.interrupted() {
                r = Err(Flow::Exit(130));
                break;
            }
            if let Err(e) = self.call_closure(&d, vec![]) {
                if r.is_ok() {
                    r = Err(e);
                }
            }
        }
        self.module = saved_module;
        self.depth -= 1;
        self.frames.pop();
        match r {
            Err(Flow::Break | Flow::Continue) => Err(error("break or continue outside a loop")),
            r => r,
        }
    }

    /// Run an imported script in a frame of its own; its top-level names (bindings and functions) are the result (N5).
    pub(crate) fn run_module(&mut self, stmts: &[Stmt]) -> Res<VMap> {
        let mut base = BTreeMap::new();
        if let Some(e) = self.lookup("env") {
            base.insert(String::from("env"), e);
        }
        base.insert(String::from("args"), Val::list(Vec::new()));
        self.frames.push(Frame::new(vec![base], 0, true));
        let r = self.exec_block(stmts, false);
        let frame = self.frames.pop();
        r?;
        let mut m = VMap::new();
        if let Some(f) = frame {
            // in name order, so the result does not depend on how the module was written
            for (k, v) in &f.scopes[0] {
                if k != "env" && k != "args" {
                    m.set(k, v.clone());
                }
            }
        }
        Ok(m)
    }

    // ---- expressions and words ----

    /// An expression's value (M2 to M4, E1 to E5).
    pub fn eval_expr(&mut self, e: &Expr) -> Res<Val> {
        match e {
            Expr::Word(w) => self.eval_word(w),
            Expr::Call(ao) => {
                let v = self.exec_and_or(ao, Mode::Value)?;
                self.need_value(v)
            }
            Expr::Block(st) => {
                let v = self.exec(st, true)?;
                self.need_value(v)
            }
            Expr::Un(op, x) => {
                let v = self.eval_expr(x)?;
                match (op, v) {
                    (UnOp::Neg, Val::Num(n)) => n.neg().map(Val::Num).map_err(arith_err),
                    (UnOp::Neg, v) => Err(error(&format!(
                        "unary - needs a number, got a {}",
                        v.kind()
                    ))),
                    (UnOp::Not, Val::Bool(b)) => Ok(Val::Bool(!b)),
                    (UnOp::Not, v) => {
                        Err(error(&format!("`not` needs a boolean, got a {}", v.kind())))
                    }
                }
            }
            Expr::Coalesce(p, d) => match self.eval_expr(p) {
                Err(Flow::Fail(f)) if f.kind == FailKind::MissingKey => self.eval_expr(d),
                r => r,
            },
            Expr::Bin(op, a, b) => {
                if matches!(op, BinOp::And | BinOp::Or) {
                    let name = if *op == BinOp::And { "and" } else { "or" };
                    let l = self.eval_expr(a)?;
                    let Val::Bool(lb) = l else {
                        return Err(error(&format!(
                            "`{name}` needs booleans, got a {}",
                            l.kind()
                        )));
                    };
                    if (*op == BinOp::And && !lb) || (*op == BinOp::Or && lb) {
                        return Ok(Val::Bool(lb));
                    }
                    let r = self.eval_expr(b)?;
                    return match r {
                        Val::Bool(rb) => Ok(Val::Bool(rb)),
                        r => Err(error(&format!(
                            "`{name}` needs booleans, got a {}",
                            r.kind()
                        ))),
                    };
                }
                let l = self.eval_expr(a)?;
                let r = self.eval_expr(b)?;
                binary(*op, &l, &r)
            }
        }
    }

    /// A word in value position: one value.
    pub fn eval_word(&mut self, w: &Word) -> Res<Val> {
        if let [one] = w.parts.as_slice() {
            return self.eval_part(one, true);
        }
        let mut s = String::new();
        for p in &w.parts {
            let v = self.eval_part(p, false)?;
            match v.word() {
                Some(t) => s.push_str(&t),
                None => return Err(error(&format!("a {} cannot be joined into text", v.kind()))),
            }
        }
        Ok(Val::string(s))
    }

    /// The items a `for` loop goes over: words as a command takes them (lists splice, patterns expand).
    fn eval_word_for(&mut self, w: &Word, out: &mut Vec<Val>) -> Res<()> {
        match self.eval_word(w)? {
            Val::List(l) => out.extend(l.iter().cloned()),
            Val::Null => return Err(error("cannot loop over null")),
            v @ (Val::Map(_) | Val::Closure(_) | Val::Job(_)) => {
                return Err(error(&format!("cannot loop over a {}", v.kind())))
            }
            v => out.push(v),
        }
        Ok(())
    }

    /// A word as text for an external program: a list splices (one level), a glob expands.
    fn eval_word_texts(&mut self, w: &Word, out: &mut Vec<String>) -> Res<()> {
        if let [Part::Lit(t)] = w.parts.as_slice() {
            out.push(t.clone());
            return Ok(());
        }
        match self.eval_word(w)? {
            Val::List(l) => {
                for x in l.iter() {
                    match x.word() {
                        Some(t) => out.push(t),
                        None => {
                            return Err(error(&format!("a {} cannot be an argument", x.kind())))
                        }
                    }
                }
            }
            v => match v.word() {
                Some(t) => out.push(t),
                None => return Err(error(&format!("a {} cannot be an argument", v.kind()))),
            },
        }
        Ok(())
    }

    fn eval_part(&mut self, p: &Part, sole: bool) -> Res<Val> {
        Ok(match p {
            Part::Lit(t) => {
                if sole {
                    literal_value(t)
                } else {
                    Val::str(t)
                }
            }
            Part::Str(t) => Val::str(t),
            Part::Num(n) => Val::Num(*n),
            Part::Bool(b) => Val::Bool(*b),
            Part::Null => Val::Null,
            Part::Var(r) => self.eval_var(r)?,
            Part::Count(n) => {
                let v = self
                    .lookup(n)
                    .ok_or_else(|| error(&format!("`{n}` is not set")))?;
                match data_len(&v) {
                    Some(k) => Val::int(k as i64),
                    None => return Err(error(&format!("cannot count a {}", v.kind()))),
                }
            }
            Part::Paren(e, path) => {
                let saved_pos = self.pos;
                let mut v = self.eval_expr(e)?;
                self.pos = saved_pos;
                for seg in path {
                    let key = match seg {
                        Seg::Name(n) => Val::str(n),
                        Seg::Dyn(w) => self.eval_word(w)?,
                    };
                    v = crate::builtins::index(&v, &key)?;
                }
                v
            }
            Part::List(items) => {
                let mut v = Vec::new();
                for it in items {
                    v.push(self.eval_expr(it)?);
                }
                Val::list(v)
            }
            Part::Map(items) => {
                let mut m = VMap::new();
                for (k, v) in items {
                    let key = match self.eval_expr(k)? {
                        Val::Str(s) => s,
                        other => {
                            return Err(error(&format!(
                                "a map key must be a string, got a {}",
                                other.kind()
                            )))
                        }
                    };
                    if m.get(&key).is_some() {
                        return Err(error(&format!("duplicate key `{key}`")));
                    }
                    let value = self.eval_expr(v)?;
                    m.set(&key, value);
                }
                Val::map(m)
            }
            Part::Template(parts) => {
                // a hole inserts text as it is and a number, boolean or null as its JSON text (T2)
                let mut s = String::new();
                for tp in parts {
                    match tp {
                        TplPart::Text(t) => s.push_str(t),
                        TplPart::Hole(e) => {
                            let v = self.eval_expr(e)?;
                            match v {
                                Val::Str(t) => s.push_str(&t),
                                Val::Num(_) | Val::Bool(_) | Val::Null => {
                                    s.push_str(&crate::builtins::echo_line(&v))
                                }
                                v => {
                                    return Err(error(&format!(
                                    "a {} cannot go in a template hole (join a list explicitly)",
                                    v.kind()
                                )))
                                }
                            }
                        }
                    }
                }
                Val::string(s)
            }
            Part::Closure(def) => Val::Closure(Rc::new(Closure {
                def: def.clone(),
                env: self.snapshot(),
                module: self.module.clone(),
            })),
            Part::Glob(pat) => {
                if !sole {
                    return Err(error(
                        "a pattern cannot be joined with other text (quote it)",
                    ));
                }
                let m = self.host.glob(pat);
                if m.is_empty() {
                    return Err(error(&format!("no match for {pat}")));
                }
                Val::list(m.into_iter().map(Val::string).collect())
            }
        })
    }

    fn eval_var(&mut self, r: &VarRef) -> Res<Val> {
        let mut v = self
            .lookup(&r.name)
            .ok_or_else(|| error(&format!("`{}` is not set", r.name)))?;
        for seg in &r.path {
            let key = match seg {
                Seg::Name(n) => Val::str(n),
                Seg::Dyn(w) => self.eval_word(w)?,
            };
            v = crate::builtins::index(&v, &key)?;
        }
        Ok(v)
    }

    // ---- and/or lists and pipelines ----

    fn exec_and_or(&mut self, ao: &AndOr, mode: Mode) -> Res<Val> {
        if ao.rest.is_empty() {
            return self.exec_pipeline(&ao.first, mode);
        }
        // a list: every operand but the last is a test, so a failing operand is a false, not a raise
        let n = ao.rest.len();
        let mut cur = self.exec_pipeline(&ao.first, Mode::Stmt);
        let mut ran_last = false;
        for (i, (conn, pl)) in ao.rest.iter().enumerate() {
            let ok = cur.is_ok();
            match &cur {
                Err(Flow::Fail(f)) if is_test_failure(f) => {}
                Err(_) => return cur,
                Ok(_) => {}
            }
            let run = match conn {
                Conn::And => ok,
                Conn::Or => !ok,
            };
            if run {
                let last = i + 1 == n;
                cur = self.exec_pipeline(pl, if last { mode } else { Mode::Stmt });
                ran_last = last;
            } else {
                ran_last = false; // short-circuited: `cur` still stands
            }
        }
        match cur {
            // the last operand ran: its outcome is the list's; otherwise a tested failure is only a false
            Err(Flow::Fail(f)) if !ran_last && is_test_failure(&f) => Ok(Val::Bool(false)),
            r => r,
        }
    }

    fn exec_pipeline(&mut self, pl: &Pipeline, mode: Mode) -> Res<Val> {
        if pl.negate {
            let r = self.exec_pipeline_inner(pl, Mode::Stmt);
            return match r {
                Ok(_) => Err(Flow::Fail(Fail {
                    value: Val::record(vec![
                        ("kind", Val::str("exit")),
                        ("status", Val::int(1)),
                        ("command", Val::str("!")),
                    ]),
                    code: 1,
                    kind: FailKind::Exit,
                })),
                Err(Flow::Fail(f)) if is_test_failure(&f) => Ok(Val::Bool(true)),
                Err(e) => Err(e),
            };
        }
        self.exec_pipeline_inner(pl, mode)
    }

    fn exec_pipeline_inner(&mut self, pl: &Pipeline, mode: Mode) -> Res<Val> {
        match &pl.prefix {
            None => self.exec_stages(&pl.stages, mode),
            Some(p) => self.exec_prefixed(p, &pl.stages, mode),
        }
    }
}

fn is_test_failure(f: &Fail) -> bool {
    // `succeeds`, `run`, `&&`, `||` and `!` absorb exit failures and never spawn failures (X2)
    f.kind == FailKind::Exit
}

pub(crate) fn arith_err(e: ArithError) -> Flow {
    match e {
        ArithError::Overflow => error("number too large (at most 38 digits)"),
        ArithError::DivideByZero => error("division by zero"),
    }
}

fn data_len(v: &Val) -> Option<usize> {
    match v {
        Val::List(l) => Some(l.len()),
        Val::Map(m) => Some(m.len()),
        Val::Str(s) => Some(s.chars().count()),
        _ => None,
    }
}

/// A lone bareword read as a value: a number, `true`, `false`, `null`, else text.
pub(crate) fn literal_value(t: &str) -> Val {
    match t {
        "true" => Val::Bool(true),
        "false" => Val::Bool(false),
        "null" => Val::Null,
        _ => match Number::parse(t.as_bytes()) {
            Ok(n) => Val::Num(n),
            Err(_) => Val::str(t),
        },
    }
}

fn binary(op: BinOp, l: &Val, r: &Val) -> Res<Val> {
    use core::cmp::Ordering::*;
    let name = |op: BinOp| match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::IDiv => "//",
        BinOp::Rem => "%",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        _ => "?",
    };
    match op {
        BinOp::Eq => Ok(Val::Bool(l.equals(r))),
        BinOp::Ne => Ok(Val::Bool(!l.equals(r))),
        BinOp::Cat => match (l.word(), r.word()) {
            (Some(a), Some(b)) => Ok(Val::string(a + &b)),
            _ => Err(error(&format!(
                "`^` joins text, got a {} and a {}",
                l.kind(),
                r.kind()
            ))),
        },
        BinOp::In => match r {
            Val::List(items) => Ok(Val::Bool(items.iter().any(|x| x.equals(l)))),
            Val::Map(m) => match l {
                Val::Str(k) => Ok(Val::Bool(m.get(k).is_some())),
                _ => Err(error(&format!(
                    "`in` a map needs a string key, got a {}",
                    l.kind()
                ))),
            },
            _ => Err(error(&format!(
                "`in` needs a list or a map, got a {}",
                r.kind()
            ))),
        },
        BinOp::Matches => match (l, r) {
            (Val::Str(s), Val::Str(p)) => Ok(Val::Bool(crate::builtins::glob_match(p, s))),
            _ => Err(error(&format!(
                "`matches` needs two strings, got a {} and a {}",
                l.kind(),
                r.kind()
            ))),
        },
        _ => {
            let (Val::Num(a), Val::Num(b)) = (l, r) else {
                return Err(error(&format!(
                    "`{}` needs numbers, got a {} and a {}",
                    name(op),
                    l.kind(),
                    r.kind()
                )));
            };
            Ok(match op {
                BinOp::Add => Val::Num(a.add(b).map_err(arith_err)?),
                BinOp::Sub => Val::Num(a.sub(b).map_err(arith_err)?),
                BinOp::Mul => Val::Num(a.mul(b).map_err(arith_err)?),
                BinOp::Div => Val::Num(a.div(b).map_err(arith_err)?),
                BinOp::IDiv => Val::Num(a.idiv(b).map_err(arith_err)?),
                BinOp::Rem => Val::Num(a.rem(b).map_err(arith_err)?),
                BinOp::Lt => Val::Bool(a.compare(b).map_err(arith_err)? == Less),
                BinOp::Le => Val::Bool(a.compare(b).map_err(arith_err)? != Greater),
                BinOp::Gt => Val::Bool(a.compare(b).map_err(arith_err)? == Greater),
                BinOp::Ge => Val::Bool(a.compare(b).map_err(arith_err)? != Less),
                _ => unreachable!(),
            })
        }
    }
}

// ---- pipelines in detail ----

/// A stage whose words have been evaluated.
pub(crate) enum StageKind {
    Ext(Vec<String>),
    Proc {
        name: String,
        args: Vec<Val>,
        closure: Option<Rc<Closure>>,
    },
}

pub(crate) struct Stage {
    pub kind: StageKind,
    pub redirs: Vec<(RedirKind, u8, u8, Option<String>)>,
}

fn take_val(v: &mut Val) -> Val {
    core::mem::replace(v, Val::Null)
}

impl<H: Host> Shell<H> {
    fn resolve_stage(&mut self, c: &Command) -> Res<Stage> {
        self.pos = c.pos;
        let mut redirs = Vec::new();
        for r in &c.redirs {
            let t = match &r.target {
                Some(w) => match self.eval_word(w)? {
                    Val::Str(s) => Some(String::from(&*s)),
                    v => match v.word() {
                        Some(t) => Some(t),
                        None => {
                            return Err(error(&format!(
                                "a {} cannot be a redirection target",
                                v.kind()
                            )))
                        }
                    },
                },
                None => None,
            };
            redirs.push((r.kind, r.fd, r.to, t));
        }
        let Some(head) = c.words.first() else {
            return Err(error("empty command"));
        };
        // the head names a function (text) or is a closure value
        let head_val = match head.parts.as_slice() {
            [Part::Lit(t)] if t.contains('.') && !t.starts_with('.') && !t.contains('/') => {
                // `module.function`: a member of a variable that holds a map
                let (v, rest) = t.split_once('.').expect("has a dot");
                match self.lookup(v) {
                    Some(m @ Val::Map(_)) => {
                        let mut cur = m;
                        for k in rest.split('.') {
                            cur = crate::builtins::index(&cur, &Val::str(k))?;
                        }
                        cur
                    }
                    _ => Val::str(t),
                }
            }
            [Part::Lit(t)] => Val::str(t),
            _ => self.eval_word(head)?,
        };
        match head_val {
            Val::Closure(cl) => {
                let args = self.eval_args_values(&c.words[1..])?;
                Ok(Stage {
                    kind: StageKind::Proc {
                        name: String::from("(closure)"),
                        args,
                        closure: Some(cl),
                    },
                    redirs,
                })
            }
            other => {
                let Some(name) = other.word() else {
                    return Err(error(&format!(
                        "a {} cannot be run as a command",
                        other.kind()
                    )));
                };
                if let Some(cl) = self.find_fn(&name) {
                    let args = self.eval_args_values(&c.words[1..])?;
                    return Ok(Stage {
                        kind: StageKind::Proc {
                            name,
                            args,
                            closure: Some(cl),
                        },
                        redirs,
                    });
                }
                if crate::builtins::is_builtin(&name) {
                    let args = self.eval_args_values(&c.words[1..])?;
                    return Ok(Stage {
                        kind: StageKind::Proc {
                            name,
                            args,
                            closure: None,
                        },
                        redirs,
                    });
                }
                let mut argv = vec![name];
                for w in &c.words[1..] {
                    self.eval_word_texts(w, &mut argv)?;
                }
                Ok(Stage {
                    kind: StageKind::Ext(argv),
                    redirs,
                })
            }
        }
    }

    fn eval_args_values(&mut self, words: &[Word]) -> Res<Vec<Val>> {
        let mut out = Vec::new();
        for w in words {
            out.push(self.eval_word(w)?);
        }
        Ok(out)
    }

    pub(crate) fn exec_stages(&mut self, cmds: &[Command], mode: Mode) -> Res<Val> {
        let mut stages = Vec::new();
        for c in cmds {
            stages.push(self.resolve_stage(c)?);
        }
        self.run_stages(stages, mode)
    }
}

pub(crate) fn split_lines_val(t: &str) -> Val {
    Val::list(split_lines(t).into_iter().map(Val::string).collect())
}

/// Split on newline only; a trailing newline is dropped; empty text is no lines.
pub(crate) fn split_lines(t: &str) -> Vec<String> {
    if t.is_empty() {
        return Vec::new();
    }
    let t = t.strip_suffix('\n').unwrap_or(t);
    t.split('\n').map(String::from).collect()
}

mod jobs;
mod pipeline;
