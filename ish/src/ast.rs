//! The syntax tree of `ish` (grammar: SHELL-SPEC section 5.1).

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
    /// Bareword text. As the only part of a word in value position it may read as a number, `true`, `false` or `null`.
    Lit(String),
    /// Quoted text: always a string.
    Str(String),
    /// A number, `true`, `false` or `null` written in a literal or an island.
    Num(Number),
    Bool(bool),
    Null,
    Var(VarRef),
    /// `$#name`
    Count(String),
    /// `( statements )` evaluated in value position.
    Sub(Vec<Stmt>),
    /// `(calc ...)` or `(is ...)`.
    Island(IslKind, Box<Isl>),
    List(Vec<Word>),
    Map(Vec<(Word, Word)>),
    Closure(Rc<ClosureDef>),
    /// A bareword with `*` or `?`: expanded by the host.
    Glob(String),
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
    pub stages: Vec<Command>,
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
pub enum Rhs {
    /// One word: a value (a lone bareword is a string).
    Atom(Word),
    /// A call, or a pipeline, captured.
    Call(AndOr),
    /// `try ... end catch ...` or `if ... end` used as a value.
    Stmt(Box<Stmt>),
}

#[derive(Debug, Clone)]
pub enum Stmt {
    Assign {
        name: String,
        path: Vec<Seg>,
        rhs: Rhs,
        pos: Pos,
    },
    Fn {
        name: String,
        def: Rc<ClosureDef>,
    },
    If {
        cond: Word,
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
        cond: Word,
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
    Cmd(AndOr),
    /// A statement that is a single non-bareword word: `$x`, `(calc ...)`, `5`, a literal.
    Value(Word),
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

/// An infix expression island: `(calc ...)` yields a number, `(is ...)` a boolean.
#[derive(Debug, Clone)]
pub enum Isl {
    Operand(Word),
    Un(UnOp, Box<Isl>),
    Bin(BinOp, Box<Isl>, Box<Isl>),
}

/// Which marker opened an island.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IslKind {
    Calc,
    Is,
}
