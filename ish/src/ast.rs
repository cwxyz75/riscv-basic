//! The syntax tree of `ish` (grammar v2: SHELL-SPEC section 5.2).

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use data::Number;

/// A place in the source (1-based).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pos {
    pub line: u32,
    pub col: u32,
}

/// One path step after a variable: `$m.key`, `$l.0`, `$m.'a.b'`, `$m.($k)`.
#[derive(Debug, Clone)]
pub enum Seg {
    Name(String),
    Dyn(Word),
}

#[derive(Debug, Clone)]
pub struct VarRef {
    pub name: String,
    pub path: Vec<Seg>,
}

/// A word: adjacent pieces that are joined into one value (or, if there is a single non-text piece, that piece's value).
#[derive(Debug, Clone)]
pub struct Word {
    pub parts: Vec<Part>,
    pub pos: Pos,
}

#[derive(Debug, Clone)]
pub enum Part {
    /// Bareword text. As the only part of a word it may read as a number, `true`, `false` or `null`.
    Lit(String),
    /// Quoted text: always a string.
    Str(String),
    /// A number, `true`, `false` or `null` written in a literal or an expression.
    Num(Number),
    Bool(bool),
    Null,
    Var(VarRef),
    /// `$#name`
    Count(String),
    /// `( expression )`, with an optional path after it (`(split ' ' $l).1`).
    Paren(Box<Expr>, Vec<Seg>),
    List(Vec<Expr>),
    Map(Vec<(Expr, Expr)>),
    Closure(Rc<ClosureDef>),
    /// A bareword with `*` or `?`: expanded by the host.
    Glob(String),
    /// A backtick template.
    Template(Vec<TplPart>),
}

#[derive(Debug, Clone)]
pub enum TplPart {
    Text(String),
    /// `$name` or `${expression}`.
    Hole(Expr),
}

/// An expression (SHELL-SPEC M2 to M4): a call, a formula, or one operand.
#[derive(Debug, Clone)]
pub enum Expr {
    /// One operand: a word (a single piece in a formula).
    Word(Word),
    /// A call: a command line evaluated for its value.
    Call(Box<AndOr>),
    Un(UnOp, Box<Expr>),
    Bin(BinOp, Box<Expr>, Box<Expr>),
    /// `path ?? default`.
    Coalesce(Box<Expr>, Box<Expr>),
    /// `if ... end` or `try ... end catch ... end` used as a value.
    Block(Box<Stmt>),
}

#[derive(Debug)]
pub struct ClosureDef {
    pub params: Vec<String>,
    pub body: Vec<Stmt>,
    /// For diagnostics (`fn name`).
    pub name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirKind {
    /// `> file` or `>[n] file`
    Write,
    /// `>> file`
    Append,
    /// `< file`
    Read,
    /// `>[a=b]`: descriptor a goes where b goes
    Dup,
}

#[derive(Debug, Clone)]
pub struct Redir {
    pub kind: RedirKind,
    /// The descriptor redirected (0, 1 or 2).
    pub fd: u8,
    /// For `Dup`: the descriptor it is copied from.
    pub to: u8,
    pub target: Option<Word>,
}

#[derive(Debug, Clone)]
pub struct Command {
    pub words: Vec<Word>,
    pub redirs: Vec<Redir>,
    pub pos: Pos,
}

#[derive(Debug, Clone)]
pub struct Pipeline {
    pub negate: bool,
    /// `succeeds`, `run`, `spawn` or `with-env`: it takes the rest of the pipeline as its command.
    pub prefix: Option<Prefix>,
    pub stages: Vec<Command>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefixKind {
    Succeeds,
    Run,
    Spawn,
    WithEnv,
}

#[derive(Debug, Clone)]
pub struct Prefix {
    pub kind: PrefixKind,
    /// The options map of `run` and `spawn`, or the environment map of `with-env`.
    pub arg: Option<Expr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conn {
    And,
    Or,
}

#[derive(Debug, Clone)]
pub struct AndOr {
    pub first: Pipeline,
    pub rest: Vec<(Conn, Pipeline)>,
}

#[derive(Debug, Clone)]
pub enum Stmt {
    /// `x = expression`: creates a binding.
    Assign {
        name: String,
        expr: Expr,
        pos: Pos,
    },
    /// `set x.a.b = expression`: changes an existing binding.
    Set {
        name: String,
        path: Vec<Seg>,
        expr: Expr,
        pos: Pos,
    },
    Fn {
        name: String,
        def: Rc<ClosureDef>,
    },
    If {
        cond: Expr,
        then: Vec<Stmt>,
        els: Option<Vec<Stmt>>,
        pos: Pos,
    },
    For {
        var: String,
        items: Vec<Word>,
        body: Vec<Stmt>,
        pos: Pos,
    },
    While {
        cond: Expr,
        body: Vec<Stmt>,
        until: bool,
        pos: Pos,
    },
    Try {
        body: Vec<Stmt>,
        name: String,
        code_name: Option<String>,
        handler: Vec<Stmt>,
        pos: Pos,
    },
    Defer(Rc<ClosureDef>),
    /// A call written as a statement: it streams and has no value.
    Cmd(AndOr),
    /// An expression that is not a call: `$x`, `1 + 2`, `(date)`.
    Expr(Expr),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Or,
    And,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    In,
    Matches,
    Cat,
    Add,
    Sub,
    Mul,
    Div,
    IDiv,
    Rem,
}
