//! The parser: source text to [`Stmt`]s (grammar v2: SHELL-SPEC section 5.2). Hand-written recursive descent over
//! characters, because the language has two modes: **commands** take words (text), and **expressions** compute values.
//!
//! The rule that tells them apart (M3): in an expression, a leading bareword, or a second token that is not an operator,
//! makes it a **call**; anything else is a **formula**. A statement is a command when it is a call (M1).

use crate::ast::*;
use alloc::boxed::Box;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use data::Number;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub pos: Pos,
    pub msg: String,
    /// The input ended inside a construct (an unclosed block, bracket or pipe): more lines would complete it.
    pub incomplete: bool,
}

impl core::fmt::Display for ParseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{} at line {} column {}",
            self.msg, self.pos.line, self.pos.col
        )
    }
}

type R<T> = Result<T, ParseError>;

/// Words reserved wherever they appear unquoted (L5).
const RESERVED: &[&str] = &["do", "end", "else", "catch", "set"];
/// The most nesting of blocks, parentheses and literals.
pub const MAX_NEST: usize = 64;

pub fn parse(src: &str) -> R<Vec<Stmt>> {
    let mut p = Parser::new(src.chars().collect());
    let body = p.stmts(&[])?;
    if p.i < p.s.len() {
        return p.err("unexpected input");
    }
    Ok(body)
}

struct Parser {
    s: Vec<char>,
    i: usize,
    depth: usize,
    /// A condition is being read: a bareword `do` ends it.
    stop_do: bool,
    /// A slot of a `[ ]` or `{ }` literal is being read: `,` and `:` end barewords.
    slot: bool,
    /// Newlines are blanks (inside `( )`, `[ ]`, `{ }`).
    nl_ok: u32,
}

fn is_delim(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '(' | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | ';'
                | '|'
                | '&'
                | '>'
                | '<'
                | '$'
                | '\''
                | '"'
                | '`'
                | '^'
        )
}

fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn is_ident_cont(c: char) -> bool {
    is_ident(c) || c == '-' || c == '.' || c == '/'
}

impl Parser {
    fn new(s: Vec<char>) -> Parser {
        Parser {
            s,
            i: 0,
            depth: 0,
            stop_do: false,
            slot: false,
            nl_ok: 0,
        }
    }

    fn pos_at(&self, i: usize) -> Pos {
        let before = &self.s[..i.min(self.s.len())];
        let line = 1 + before.iter().filter(|&&c| c == '\n').count() as u32;
        let col = 1 + before.iter().rev().take_while(|&&c| c != '\n').count() as u32;
        Pos { line, col }
    }

    fn pos(&self) -> Pos {
        self.pos_at(self.i)
    }

    fn err<T>(&self, msg: &str) -> R<T> {
        Err(self.e(msg))
    }

    fn e(&self, msg: &str) -> ParseError {
        ParseError {
            pos: self.pos(),
            msg: String::from(msg),
            incomplete: self.i >= self.s.len() && !msg.starts_with("unterminated"),
        }
    }

    fn peek(&self) -> Option<char> {
        self.s.get(self.i).copied()
    }

    fn peek_at(&self, k: usize) -> Option<char> {
        self.s.get(self.i + k).copied()
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn nest<T>(&mut self, f: impl FnOnce(&mut Parser) -> R<T>) -> R<T> {
        self.depth += 1;
        if self.depth > MAX_NEST {
            return self.err("nested too deeply");
        }
        let r = f(self);
        self.depth -= 1;
        r
    }

    fn delim(&self, c: char) -> bool {
        is_delim(c) || (self.slot && matches!(c, ',' | ':'))
    }

    /// Blanks, comments and backslash-newline (and newlines where they are blanks).
    fn ws(&mut self) {
        loop {
            match self.peek() {
                Some(' ' | '\t' | '\r') => self.i += 1,
                Some('\n') if self.nl_ok > 0 => self.i += 1,
                Some('\\') if self.peek_at(1) == Some('\n') => self.i += 2,
                Some('#') => {
                    while self.peek().is_some_and(|c| c != '\n') {
                        self.i += 1;
                    }
                }
                _ => return,
            }
        }
    }

    /// Blanks, comments, newlines (after a `|`, `&&` or `||`, and inside brackets).
    fn wsn(&mut self) {
        loop {
            self.ws();
            if self.peek() == Some('\n') {
                self.i += 1;
            } else {
                return;
            }
        }
    }

    /// Statement separators: newlines and semicolons.
    fn seps(&mut self) {
        loop {
            self.ws();
            if matches!(self.peek(), Some('\n' | ';')) {
                self.i += 1;
            } else {
                return;
            }
        }
    }

    /// The bareword text at the cursor, without consuming (empty when the next thing is not a bareword).
    fn peek_bare(&self) -> String {
        let mut j = self.i;
        let mut t = String::new();
        while let Some(&c) = self.s.get(j) {
            if self.delim(c) {
                break;
            }
            t.push(c);
            j += 1;
        }
        t
    }

    fn at_word(&self, w: &str) -> bool {
        self.peek_bare() == w
    }

    fn take_bare(&mut self, w: &str) -> bool {
        if self.at_word(w) {
            self.i += w.chars().count();
            true
        } else {
            false
        }
    }

    fn expect_word(&mut self, w: &str) -> R<()> {
        self.ws();
        if self.take_bare(w) {
            Ok(())
        } else {
            self.err(&format!("expected `{w}`"))
        }
    }

    // ---- statements ----

    /// Statements until end of input or one of the `stop` words (`end`, `else`, `catch`) or a `)`; the stop is not consumed.
    fn stmts(&mut self, stop: &[&str]) -> R<Vec<Stmt>> {
        let mut out = Vec::new();
        loop {
            self.seps();
            match self.peek() {
                None | Some(')') => return Ok(out),
                _ => {}
            }
            let w = self.peek_bare();
            if stop.contains(&w.as_str()) {
                return Ok(out);
            }
            if RESERVED.contains(&w.as_str()) && w != "set" {
                return self.err(&format!("unexpected `{w}`"));
            }
            out.push(self.stmt()?);
            // a statement ends at a separator, a closing word or a `)`
            self.ws();
            match self.peek() {
                None | Some('\n' | ';' | ')') => {}
                _ => {
                    let w = self.peek_bare();
                    if !RESERVED.contains(&w.as_str()) {
                        return self.err("unexpected input after the command");
                    }
                }
            }
        }
    }

    /// A block's statements: newlines separate them even inside brackets, and a condition's `do` rule is off.
    fn block(&mut self, stop: &[&str]) -> R<Vec<Stmt>> {
        let (nl, sd, sl) = (self.nl_ok, self.stop_do, self.slot);
        self.nl_ok = 0;
        self.stop_do = false;
        self.slot = false;
        let r = self.nest(|p| p.stmts(stop));
        self.nl_ok = nl;
        self.stop_do = sd;
        self.slot = sl;
        r
    }

    fn kw_follows(&self, w: &str) -> bool {
        self.s
            .get(self.i + w.chars().count())
            .is_none_or(|c| !is_ident_cont(*c))
    }

    fn stmt(&mut self) -> R<Stmt> {
        let pos = self.pos();
        if self.peek() == Some('|') && self.peek_at(1) != Some('|') {
            return self.err("empty command in pipeline");
        }
        let w = self.peek_bare();
        if self.kw_follows(&w) {
            match w.as_str() {
                "fn" => return self.fn_stmt(),
                "if" => {
                    self.i += 2;
                    return self.if_rest(pos);
                }
                "for" => return self.for_stmt(pos),
                "while" | "until" => {
                    self.i += w.chars().count();
                    let cond = self.cond()?;
                    let body = self.block(&["end"])?;
                    self.expect_word("end")?;
                    return Ok(Stmt::While {
                        cond,
                        body,
                        until: w == "until",
                        pos,
                    });
                }
                "try" => return self.try_stmt(pos),
                "set" => return self.set_stmt(pos),
                "defer" => {
                    self.i += 5;
                    self.expect_word("do")?;
                    let body = self.block(&["end"])?;
                    self.expect_word("end")?;
                    return Ok(Stmt::Defer(Rc::new(ClosureDef {
                        params: vec![],
                        body,
                        name: None,
                    })));
                }
                _ => {}
            }
        }
        if let Some((name, after)) = self.assign_head() {
            self.i = after;
            let expr = self.rhs()?;
            return Ok(Stmt::Assign { name, expr, pos });
        }
        // a statement that starts with a bareword is a command (M1), even `true` and `false`; anything else follows the
        // expression rule and is a command when it is a call
        if self.starts_stmt_command() {
            let ao = self.and_or()?;
            return Ok(Stmt::Cmd(ao));
        }
        match self.expr()? {
            Expr::Call(ao) => Ok(Stmt::Cmd(*ao)),
            e => Ok(Stmt::Expr(e)),
        }
    }

    /// `name`, blanks and a lone `=`: returns the name and the cursor after the `=`.
    fn assign_head(&self) -> Option<(String, usize)> {
        let mut j = self.i;
        let start = j;
        while self.s.get(j).is_some_and(|&c| is_ident(c)) {
            j += 1;
        }
        if j == start || self.s[start].is_ascii_digit() {
            return None;
        }
        let name: String = self.s[start..j].iter().collect();
        let mut k = j;
        while matches!(self.s.get(k), Some(' ' | '\t')) {
            k += 1;
        }
        if k == j && self.s.get(k).is_some() {
            return None; // `x=5` is a word
        }
        if self.s.get(k) != Some(&'=') {
            return None;
        }
        match self.s.get(k + 1) {
            None | Some(' ' | '\t' | '\n' | '\r') => Some((name, k + 1)),
            _ => None,
        }
    }

    /// The right side of `=` and `set`: an expression, but a lone bareword is refused (M6).
    fn rhs(&mut self) -> R<Expr> {
        self.ws();
        let e = self.expr()?;
        self.no_lone_bareword(&e)?;
        Ok(e)
    }

    /// M6: the right side of `=` and `set` and a literal slot reject a lone bareword, naming both fixes.
    fn no_lone_bareword(&self, e: &Expr) -> R<()> {
        if let Expr::Call(ao) = e {
            if ao.rest.is_empty()
                && !ao.first.negate
                && ao.first.prefix.is_none()
                && ao.first.stages.len() == 1
            {
                let c = &ao.first.stages[0];
                if c.words.len() == 1 && c.redirs.is_empty() {
                    if let [Part::Lit(t) | Part::Glob(t)] = c.words[0].parts.as_slice() {
                        return Err(ParseError {
                            pos: c.words[0].pos,
                            msg: format!(
                                "`{t}` would run a command here: write '{t}' for the text, or ({t}) to run it"
                            ),
                            incomplete: false,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    fn set_stmt(&mut self, pos: Pos) -> R<Stmt> {
        self.i += 3;
        self.ws();
        let name = self.peek_bare_ident();
        if name.is_empty() {
            return self.err("expected a name after `set`");
        }
        self.i += name.chars().count();
        let mut path = Vec::new();
        while self.peek() == Some('.') {
            path.push(self.seg()?);
        }
        self.ws();
        if !(self.peek() == Some('=') && self.peek_at(1).is_none_or(|c| c.is_whitespace())) {
            return self.err("expected `=` after the name in `set`");
        }
        self.i += 1;
        let expr = self.rhs()?;
        Ok(Stmt::Set {
            name,
            path,
            expr,
            pos,
        })
    }

    fn peek_bare_ident(&self) -> String {
        let mut j = self.i;
        let mut t = String::new();
        while let Some(&c) = self.s.get(j) {
            if !is_ident(c) {
                break;
            }
            t.push(c);
            j += 1;
        }
        t
    }

    fn fn_stmt(&mut self) -> R<Stmt> {
        self.i += 2;
        self.ws();
        let name = self.peek_bare();
        if name.is_empty() || name.contains(['$', '/']) {
            return self.err("expected a function name");
        }
        self.i += name.chars().count();
        let mut params = Vec::new();
        loop {
            self.ws();
            if self.at_word("do") {
                self.i += 2;
                break;
            }
            let p = self.peek_bare();
            if p.is_empty() || !p.chars().all(is_ident) {
                return self.err("expected a parameter name or `do`");
            }
            self.i += p.chars().count();
            params.push(p);
        }
        let body = self.block(&["end"])?;
        self.expect_word("end")?;
        Ok(Stmt::Fn {
            name: name.clone(),
            def: Rc::new(ClosureDef {
                params,
                body,
                name: Some(name),
            }),
        })
    }

    /// A condition: an expression up to `do` (F2), which is consumed.
    fn cond(&mut self) -> R<Expr> {
        self.ws();
        let sd = self.stop_do;
        self.stop_do = true;
        let e = self.expr();
        self.stop_do = sd;
        let e = e?;
        self.expect_word("do")?;
        Ok(e)
    }

    /// After `if`: the condition, `do`, the body, and the `else` chain up to the one `end`.
    fn if_rest(&mut self, pos: Pos) -> R<Stmt> {
        let cond = self.cond()?;
        let then = self.block(&["else", "end"])?;
        self.seps();
        if self.take_bare("else") {
            self.ws();
            if self.at_word("if") {
                let p2 = self.pos();
                self.i += 2;
                let nested = self.if_rest(p2)?; // consumes the shared `end`
                return Ok(Stmt::If {
                    cond,
                    then,
                    els: Some(vec![nested]),
                    pos,
                });
            }
            let els = self.block(&["end"])?;
            self.expect_word("end")?;
            return Ok(Stmt::If {
                cond,
                then,
                els: Some(els),
                pos,
            });
        }
        self.expect_word("end")?;
        Ok(Stmt::If {
            cond,
            then,
            els: None,
            pos,
        })
    }

    fn for_stmt(&mut self, pos: Pos) -> R<Stmt> {
        self.i += 3;
        self.ws();
        let var = self.peek_bare();
        if var.is_empty() || !var.chars().all(is_ident) {
            return self.err("expected a variable name");
        }
        self.i += var.chars().count();
        self.expect_word("in")?;
        let mut items = Vec::new();
        loop {
            self.ws();
            if self.at_word("do") {
                self.i += 2;
                break;
            }
            if matches!(self.peek(), None | Some('\n' | ';')) {
                return self.err("expected `do`");
            }
            items.push(self.word()?);
        }
        let body = self.block(&["end"])?;
        self.expect_word("end")?;
        Ok(Stmt::For {
            var,
            items,
            body,
            pos,
        })
    }

    fn try_stmt(&mut self, pos: Pos) -> R<Stmt> {
        self.i += 3;
        self.expect_word("do")?;
        let body = self.block(&["end"])?;
        self.expect_word("end")?;
        self.seps();
        self.expect_word("catch")?;
        self.ws();
        let name = self.peek_bare();
        if name.is_empty() || !name.chars().all(is_ident) {
            return self.err("expected a name for the failure");
        }
        self.i += name.chars().count();
        self.ws();
        let mut code_name = None;
        if !self.at_word("do") {
            let c = self.peek_bare();
            if c.is_empty() || !c.chars().all(is_ident) {
                return self.err("expected `do`");
            }
            self.i += c.chars().count();
            code_name = Some(c);
        }
        self.expect_word("do")?;
        let handler = self.block(&["end"])?;
        self.expect_word("end")?;
        Ok(Stmt::Try {
            body,
            name,
            code_name,
            handler,
            pos,
        })
    }

    // ---- expressions (M2 to M4) ----

    /// An expression: a call when it starts with a bareword or has a second token that is not an operator (M3), else a
    /// formula.
    fn expr(&mut self) -> R<Expr> {
        self.ws();
        let pos = self.pos();
        let w = self.peek_bare();
        if self.kw_follows(&w) {
            match w.as_str() {
                "if" => {
                    self.i += 2;
                    let st = self.if_rest(pos)?;
                    return Ok(Expr::Block(Box::new(st)));
                }
                "try" => {
                    let st = self.try_stmt(pos)?;
                    return Ok(Expr::Block(Box::new(st)));
                }
                _ => {}
            }
        }
        if self.at_word("not") || (self.peek() == Some('-') && self.unary_minus_here()) {
            return self.formula(0);
        }
        if self.starts_call() {
            return self.call_expr();
        }
        // an operand first: a formula if an operator or the end follows it, else a call whose head it is
        let (save_i, save_depth) = (self.i, self.depth);
        let first = self.atom_expr()?;
        self.ws();
        if self.expr_ends() || self.op_at().is_some() || self.coalesce_at() {
            return self.formula_from(first, 0);
        }
        self.i = save_i;
        self.depth = save_depth;
        self.call_expr()
    }

    /// A statement that starts with a bareword (other than `not`, `do`, a negative number or a closure) is a command.
    fn starts_stmt_command(&self) -> bool {
        let w = self.peek_bare();
        if w.is_empty() || matches!(w.as_str(), "not" | "do") {
            return false;
        }
        let c = w.chars().next().unwrap_or(' ');
        if (c == '-' || c == '+') && w.chars().nth(1).is_some_and(|d| d.is_ascii_digit()) {
            return false;
        }
        c != '@'
    }

    fn call_expr(&mut self) -> R<Expr> {
        let ao = self.and_or()?;
        Ok(Expr::Call(Box::new(ao)))
    }

    /// Does the expression start with a bareword that is a command (not a number, `true`, `false`, `null`, `not`, `do`)?
    fn starts_call(&self) -> bool {
        let w = self.peek_bare();
        if w.is_empty() {
            return self.peek() == Some('!') && self.peek_at(1).is_some_and(|c| c.is_whitespace());
        }
        let c = w.chars().next().unwrap_or(' ');
        if matches!(w.as_str(), "true" | "false" | "null" | "not" | "do")
            || Number::parse(w.as_bytes()).is_ok()
        {
            return false;
        }
        // a leading `-` or `+` followed by a digit is a number; `@` starts a closure
        if (c == '-' || c == '+') && w.chars().nth(1).is_some_and(|d| d.is_ascii_digit()) {
            return false;
        }
        c != '@'
    }

    fn unary_minus_here(&self) -> bool {
        // `-` at the start of an expression is a negation when a value follows it (`-$x`, `-(a)`, `-3`)
        self.peek_at(1)
            .is_some_and(|c| c == '$' || c == '(' || c.is_ascii_digit())
    }

    /// Is the expression finished (a terminator follows)?
    fn expr_ends(&self) -> bool {
        match self.peek() {
            None => true,
            Some('\n' | ';' | ')' | ']' | '}') => true,
            Some(',' | ':') if self.slot => true,
            _ => {
                let w = self.peek_bare();
                matches!(w.as_str(), "do" | "end" | "else" | "catch")
            }
        }
    }

    fn coalesce_at(&self) -> bool {
        self.peek() == Some('?') && self.peek_at(1) == Some('?')
    }

    /// A binary operator at the cursor: (operator, precedence, length). Symbols need a blank, a value start or the end after
    /// them, so `$f -x` is a call and `$a - 1` a formula.
    fn op_at(&self) -> Option<(BinOp, u8, usize)> {
        let (c, d) = (self.peek()?, self.peek_at(1));
        let after = |n: usize| -> bool {
            match self.peek_at(n) {
                None => true,
                Some(ch) => {
                    ch.is_whitespace()
                        || matches!(ch, '$' | '(' | '\'' | '"' | '[' | '{' | '`')
                        || ch.is_ascii_digit()
                }
            }
        };
        let r = match (c, d) {
            ('=', Some('=')) if after(2) => (BinOp::Eq, 4, 2),
            ('!', Some('=')) if after(2) => (BinOp::Ne, 4, 2),
            ('<', Some('=')) if after(2) => (BinOp::Le, 4, 2),
            ('>', Some('=')) if after(2) => (BinOp::Ge, 4, 2),
            ('<', _) if after(1) => (BinOp::Lt, 4, 1),
            ('>', _) if after(1) => (BinOp::Gt, 4, 1),
            ('^', _) if after(1) => (BinOp::Cat, 5, 1),
            ('+', _) if after(1) => (BinOp::Add, 6, 1),
            ('-', _) if after(1) => (BinOp::Sub, 6, 1),
            ('*', _) if after(1) => (BinOp::Mul, 7, 1),
            ('/', Some('/')) if after(2) => (BinOp::IDiv, 7, 2),
            ('/', _) if after(1) => (BinOp::Div, 7, 1),
            ('%', _) if after(1) => (BinOp::Rem, 7, 1),
            _ => {
                let w = self.peek_bare();
                match w.as_str() {
                    "and" => (BinOp::And, 2, 3),
                    "or" => (BinOp::Or, 1, 2),
                    "in" => (BinOp::In, 4, 2),
                    "matches" => (BinOp::Matches, 4, 7),
                    _ => return None,
                }
            }
        };
        Some(r)
    }

    /// Precedence climbing (E1): `or`, `and`, `not`, comparisons, `^`, `+ -`, `* / // %`, unary `-`, `??`.
    fn formula(&mut self, min: u8) -> R<Expr> {
        self.ws();
        let lhs = if self.at_word("not") {
            self.i += 3;
            let e = self.formula(3)?;
            Expr::Un(UnOp::Not, Box::new(e))
        } else if self.peek() == Some('-') {
            self.i += 1;
            let e = self.formula(8)?;
            Expr::Un(UnOp::Neg, Box::new(e))
        } else {
            self.atom_expr()?
        };
        self.formula_from(lhs, min)
    }

    fn formula_from(&mut self, mut lhs: Expr, min: u8) -> R<Expr> {
        loop {
            self.ws();
            if (self.peek() == Some('&') && self.peek_at(1) == Some('&'))
                || (self.peek() == Some('|') && self.peek_at(1) == Some('|'))
            {
                return self.err("`&&` and `||` join commands; between values write `and` or `or`");
            }
            let Some((op, prec, len)) = self.op_at() else {
                break;
            };
            if prec < min {
                break;
            }
            self.i += len;
            let rhs = self.formula(prec + 1)?;
            lhs = Expr::Bin(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    /// One operand, with `??` (the tightest operator, E5).
    fn atom_expr(&mut self) -> R<Expr> {
        self.ws();
        let pos = self.pos();
        let mut parts = Vec::new();
        self.piece(&mut parts)?;
        let atom = Expr::Word(Word { parts, pos });
        self.ws();
        if self.coalesce_at() {
            let is_path = matches!(&atom, Expr::Word(w) if matches!(w.parts.as_slice(),
                [Part::Var(v)] if !v.path.is_empty())
                || matches!(w.parts.as_slice(), [Part::Paren(_, p)] if !p.is_empty()));
            if !is_path {
                return self.err("`??` needs a path on its left, such as `$m.key ?? 0`");
            }
            self.i += 2;
            let rhs = self.atom_expr()?;
            return Ok(Expr::Coalesce(Box::new(atom), Box::new(rhs)));
        }
        Ok(atom)
    }

    // ---- commands ----

    fn and_or(&mut self) -> R<AndOr> {
        let first = self.pipeline()?;
        let mut rest = Vec::new();
        loop {
            self.ws();
            let c = if self.peek() == Some('&') && self.peek_at(1) == Some('&') {
                Conn::And
            } else if self.peek() == Some('|') && self.peek_at(1) == Some('|') {
                Conn::Or
            } else {
                break;
            };
            self.i += 2;
            self.wsn();
            rest.push((c, self.pipeline()?));
        }
        Ok(AndOr { first, rest })
    }

    fn pipeline(&mut self) -> R<Pipeline> {
        self.ws();
        let mut negate = false;
        if self.peek() == Some('!') && self.peek_at(1).is_some_and(|c| c.is_whitespace()) {
            self.i += 1;
            negate = true;
            self.ws();
        }
        // a prefix takes the rest of the pipeline as its command (C4)
        let mut prefix = None;
        let w = self.peek_bare();
        let kind = match w.as_str() {
            "succeeds" => Some(PrefixKind::Succeeds),
            "run" => Some(PrefixKind::Run),
            "spawn" => Some(PrefixKind::Spawn),
            "with-env" => Some(PrefixKind::WithEnv),
            _ => None,
        };
        if let Some(kind) = kind {
            self.i += w.chars().count();
            self.ws();
            let arg = match kind {
                PrefixKind::Succeeds => None,
                PrefixKind::Run | PrefixKind::Spawn => {
                    if self.peek() == Some('{') {
                        Some(self.atom_expr()?)
                    } else {
                        None
                    }
                }
                PrefixKind::WithEnv => {
                    if self.expr_ends() {
                        return self.err("with-env needs a map of variables");
                    }
                    Some(self.atom_expr()?)
                }
            };
            prefix = Some(Prefix { kind, arg });
        }
        let mut stages = vec![self.command()?];
        loop {
            self.ws();
            if self.peek() == Some('|') && self.peek_at(1) != Some('|') {
                self.i += 1;
                self.wsn();
                stages.push(self.command()?);
            } else {
                break;
            }
        }
        Ok(Pipeline {
            negate,
            prefix,
            stages,
        })
    }

    fn command(&mut self) -> R<Command> {
        let pos = self.pos();
        let mut words = Vec::new();
        let mut redirs = Vec::new();
        loop {
            self.ws();
            match self.peek() {
                None | Some('\n' | ';' | ')' | '|' | ']' | '}') => break,
                Some(',') if self.slot => break,
                Some('&') => {
                    if self.peek_at(1) == Some('&') {
                        break;
                    }
                    return self.err("background jobs are written `spawn cmd`");
                }
                Some('>') | Some('<') => redirs.push(self.redir()?),
                _ => {
                    let w = self.peek_bare();
                    if !w.is_empty() && RESERVED.contains(&w.as_str()) && w != "do" {
                        break;
                    }
                    if w == "do" {
                        if self.stop_do {
                            break;
                        }
                        if words.is_empty() {
                            return self.err("unexpected `do`");
                        }
                    }
                    if words.is_empty() && matches!(w.as_str(), "and" | "or") {
                        return self.err(&format!(
                            "`{w}` joins values; between commands write `{}`",
                            if w == "and" { "&&" } else { "||" }
                        ));
                    }
                    words.push(self.word()?);
                }
            }
        }
        if words.is_empty() && redirs.is_empty() {
            let before = self.s[..self.i]
                .iter()
                .rev()
                .find(|c| !matches!(c, ' ' | '\t' | '\n'));
            if before == Some(&'|') || self.peek() == Some('|') {
                return self.err("empty command in pipeline");
            }
            return self.err("expected a command");
        }
        Ok(Command { words, redirs, pos })
    }

    fn redir(&mut self) -> R<Redir> {
        let c = self.peek().unwrap_or('>');
        self.i += 1;
        if c == '<' {
            self.ws();
            let target = self.word()?;
            return Ok(Redir {
                kind: RedirKind::Read,
                fd: 0,
                to: 0,
                target: Some(target),
            });
        }
        let append = self.eat('>');
        let mut fd = 1u8;
        if self.peek() == Some('[') {
            self.i += 1;
            let d = self
                .peek()
                .and_then(|c| c.to_digit(10))
                .ok_or_else(|| self.e("expected a descriptor"))?;
            self.i += 1;
            fd = d as u8;
            if self.eat('=') {
                let t = self
                    .peek()
                    .and_then(|c| c.to_digit(10))
                    .ok_or_else(|| self.e("expected a descriptor"))?;
                self.i += 1;
                if !self.eat(']') {
                    return self.err("expected `]`");
                }
                if fd > 2 || t > 2 {
                    return self.err("only descriptors 0 to 2");
                }
                return Ok(Redir {
                    kind: RedirKind::Dup,
                    fd,
                    to: t as u8,
                    target: None,
                });
            }
            if !self.eat(']') {
                return self.err("expected `]`");
            }
            if fd > 2 {
                return self.err("only descriptors 0 to 2");
            }
        }
        self.ws();
        let target = self.word()?;
        Ok(Redir {
            kind: if append {
                RedirKind::Append
            } else {
                RedirKind::Write
            },
            fd,
            to: 0,
            target: Some(target),
        })
    }

    // ---- words ----

    /// One word: adjacent pieces, joined by `^` (with or without blanks) into one.
    fn word(&mut self) -> R<Word> {
        let pos = self.pos();
        let mut parts = Vec::new();
        loop {
            self.piece(&mut parts)?;
            // adjacent pieces continue the word; `^` joins explicitly
            match self.peek() {
                Some(c) if !self.delim(c) || matches!(c, '$' | '\'' | '"' | '`') => continue,
                _ => {}
            }
            let save = self.i;
            self.ws();
            if self.peek() == Some('^') {
                self.i += 1;
                self.ws();
                continue;
            }
            self.i = save;
            break;
        }
        Ok(Word { parts, pos })
    }

    fn piece(&mut self, parts: &mut Vec<Part>) -> R<()> {
        let Some(c) = self.peek() else {
            return self.err("expected a word");
        };
        match c {
            '$' => parts.push(self.var()?),
            '\'' => parts.push(Part::Str(self.single_quoted()?)),
            '"' => parts.push(Part::Str(self.json_string()?)),
            '`' => parts.push(self.template()?),
            '(' => parts.push(self.paren()?),
            '[' => parts.push(self.list_lit()?),
            '{' => parts.push(self.map_lit()?),
            '@' if self.peek_at(1).is_none_or(|c| c.is_whitespace()) => parts.push(self.lambda()?),
            _ => {
                let t = self.peek_bare();
                if t.is_empty() {
                    return self.err("expected a word");
                }
                if t == "do" {
                    self.i += 2;
                    let body = self.block(&["end"])?;
                    self.expect_word("end")?;
                    parts.push(Part::Closure(Rc::new(ClosureDef {
                        params: vec![],
                        body,
                        name: None,
                    })));
                    return Ok(());
                }
                if RESERVED.contains(&t.as_str()) {
                    return self.err(&format!("unexpected `{t}` (quote it to use it as text)"));
                }
                self.i += t.chars().count();
                if t.contains(['*', '?']) && t != "??" {
                    parts.push(Part::Glob(t));
                } else {
                    parts.push(Part::Lit(t));
                }
            }
        }
        Ok(())
    }

    fn lambda(&mut self) -> R<Part> {
        self.i += 1;
        let mut params = Vec::new();
        loop {
            self.ws();
            if self.at_word("do") {
                self.i += 2;
                break;
            }
            let p = self.peek_bare();
            if p.is_empty() || !p.chars().all(is_ident) {
                return self.err("expected a parameter name or `do`");
            }
            self.i += p.chars().count();
            params.push(p);
        }
        let body = self.block(&["end"])?;
        self.expect_word("end")?;
        Ok(Part::Closure(Rc::new(ClosureDef {
            params,
            body,
            name: None,
        })))
    }

    fn single_quoted(&mut self) -> R<String> {
        self.i += 1;
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return self.err("unterminated quote"),
                Some('\'') => {
                    if self.peek_at(1) == Some('\'') {
                        out.push('\'');
                        self.i += 2;
                    } else {
                        self.i += 1;
                        return Ok(out);
                    }
                }
                Some(c) => {
                    out.push(c);
                    self.i += 1;
                }
            }
        }
    }

    /// One JSON escape after the backslash (the cursor is on the character after it).
    fn json_escape(&mut self) -> R<char> {
        let Some(e) = self.peek() else {
            return self.err("unterminated string");
        };
        self.i += 1;
        Ok(match e {
            '"' => '"',
            '\\' => '\\',
            '/' => '/',
            'b' => '\u{8}',
            'f' => '\u{c}',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            'u' => {
                let hi = self.hex4()?;
                match hi {
                    0xD800..=0xDBFF => {
                        if self.peek() == Some('\\') && self.peek_at(1) == Some('u') {
                            self.i += 2;
                            let lo = self.hex4()?;
                            if !(0xDC00..=0xDFFF).contains(&lo) {
                                return self.err("unpaired surrogate");
                            }
                            char::from_u32(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00))
                                .unwrap_or('\u{fffd}')
                        } else {
                            return self.err("unpaired surrogate");
                        }
                    }
                    0xDC00..=0xDFFF => return self.err("unpaired surrogate"),
                    _ => char::from_u32(hi).unwrap_or('\u{fffd}'),
                }
            }
            _ => return self.err("bad escape"),
        })
    }

    /// A JSON string literal (escapes, `\u`, surrogate pairs).
    fn json_string(&mut self) -> R<String> {
        self.i += 1;
        let mut out = String::new();
        loop {
            let Some(c) = self.peek() else {
                return self.err("unterminated string");
            };
            self.i += 1;
            match c {
                '"' => return Ok(out),
                '\\' => out.push(self.json_escape()?),
                c if (c as u32) < 0x20 && c != '\n' && c != '\t' => {
                    return self.err("control character in string")
                }
                c => out.push(c),
            }
        }
    }

    fn hex4(&mut self) -> R<u32> {
        let mut v = 0;
        for _ in 0..4 {
            let d = self
                .peek()
                .and_then(|c| c.to_digit(16))
                .ok_or_else(|| self.e("bad \\u escape"))?;
            self.i += 1;
            v = v * 16 + d;
        }
        Ok(v)
    }

    // ---- templates (T1) ----

    /// `` `text $name ${expression} \$ \` ` ``: the raw text is cut at the closing backtick (holes may contain quotes and
    /// braces), a template that starts with a newline loses that newline and the common indentation, then escapes and
    /// holes are read.
    fn template(&mut self) -> R<Part> {
        let start_pos = self.pos();
        self.i += 1;
        let mut raw = String::new();
        loop {
            let Some(c) = self.peek() else {
                return Err(ParseError {
                    pos: start_pos,
                    msg: String::from("unterminated template"),
                    incomplete: false,
                });
            };
            match c {
                '`' => {
                    self.i += 1;
                    break;
                }
                '\\' => {
                    raw.push('\\');
                    self.i += 1;
                    if let Some(n) = self.peek() {
                        raw.push(n);
                        self.i += 1;
                    }
                }
                '$' if self.peek_at(1) == Some('{') => {
                    // a hole: copy it whole, respecting nesting and quotes
                    raw.push_str("${");
                    self.i += 2;
                    let mut depth = 1;
                    while depth > 0 {
                        let Some(h) = self.peek() else {
                            return Err(ParseError {
                                pos: start_pos,
                                msg: String::from("unterminated template"),
                                incomplete: false,
                            });
                        };
                        self.i += 1;
                        raw.push(h);
                        match h {
                            '{' => depth += 1,
                            '}' => depth -= 1,
                            '\'' | '"' => loop {
                                let Some(q) = self.peek() else {
                                    return Err(ParseError {
                                        pos: start_pos,
                                        msg: String::from("unterminated template"),
                                        incomplete: false,
                                    });
                                };
                                self.i += 1;
                                raw.push(q);
                                if q == '\\' && h == '"' {
                                    if let Some(n) = self.peek() {
                                        raw.push(n);
                                        self.i += 1;
                                    }
                                } else if q == h {
                                    break;
                                }
                            },
                            _ => {}
                        }
                    }
                }
                c => {
                    raw.push(c);
                    self.i += 1;
                }
            }
        }
        // dedent
        let text = if let Some(rest) = raw.strip_prefix('\n') {
            let lines: Vec<&str> = rest.split('\n').collect();
            let indent = lines
                .iter()
                .filter(|l| !l.trim().is_empty())
                .map(|l| l.chars().take_while(|c| *c == ' ' || *c == '\t').count())
                .min()
                .unwrap_or(0);
            let cut: Vec<String> = lines
                .iter()
                .map(|l| {
                    if l.trim().is_empty() {
                        String::new()
                    } else {
                        l.chars().skip(indent).collect()
                    }
                })
                .collect();
            cut.join("\n")
        } else {
            raw
        };
        // escapes and holes, with a sub-parser over the dedented text
        let mut sub = Parser::new(text.chars().collect());
        sub.depth = self.depth;
        let mut parts: Vec<TplPart> = Vec::new();
        let mut cur = String::new();
        while let Some(c) = sub.peek() {
            match c {
                '\\' => {
                    sub.i += 1;
                    match sub.peek() {
                        Some('$') => {
                            sub.i += 1;
                            cur.push('$');
                        }
                        Some('`') => {
                            sub.i += 1;
                            cur.push('`');
                        }
                        _ => {
                            let ch = sub.json_escape().map_err(|mut e| {
                                e.pos = start_pos;
                                e
                            })?;
                            cur.push(ch);
                        }
                    }
                }
                '$' if sub.peek_at(1) == Some('{') => {
                    if !cur.is_empty() {
                        parts.push(TplPart::Text(core::mem::take(&mut cur)));
                    }
                    sub.i += 2;
                    let from = sub.i;
                    let mut depth = 1;
                    let mut j = sub.i;
                    // find the matching brace (quotes and nested braces were balanced when the text was cut)
                    while let Some(&h) = sub.s.get(j) {
                        match h {
                            '{' => depth += 1,
                            '}' => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            '\'' | '"' => {
                                j += 1;
                                while let Some(&q) = sub.s.get(j) {
                                    if q == '\\' && h == '"' {
                                        j += 1;
                                    } else if q == h {
                                        break;
                                    }
                                    j += 1;
                                }
                            }
                            _ => {}
                        }
                        j += 1;
                    }
                    let inner: Vec<char> = sub.s[from..j.min(sub.s.len())].to_vec();
                    let mut hp = Parser::new(inner);
                    hp.depth = self.depth + 1;
                    if hp.depth > MAX_NEST {
                        return self.err("nested too deeply");
                    }
                    hp.nl_ok = 1;
                    let e = hp.expr().map_err(|mut e| {
                        e.pos = start_pos;
                        e
                    })?;
                    hp.ws();
                    if hp.i < hp.s.len() {
                        return Err(ParseError {
                            pos: start_pos,
                            msg: String::from("unexpected input in a template hole"),
                            incomplete: false,
                        });
                    }
                    parts.push(TplPart::Hole(e));
                    sub.i = j + 1;
                }
                '$' if sub
                    .peek_at(1)
                    .is_some_and(|d| d.is_ascii_alphabetic() || d == '_') =>
                {
                    if !cur.is_empty() {
                        parts.push(TplPart::Text(core::mem::take(&mut cur)));
                    }
                    sub.i += 1;
                    let from = sub.i;
                    while sub.peek().is_some_and(is_ident) {
                        sub.i += 1;
                    }
                    let name: String = sub.s[from..sub.i].iter().collect();
                    parts.push(TplPart::Hole(Expr::Word(Word {
                        parts: vec![Part::Var(VarRef { name, path: vec![] })],
                        pos: start_pos,
                    })));
                }
                c => {
                    cur.push(c);
                    sub.i += 1;
                }
            }
        }
        if !cur.is_empty() {
            parts.push(TplPart::Text(cur));
        }
        Ok(Part::Template(parts))
    }

    // ---- variables and paths ----

    /// One path step: `.key`, `.0`, `.'a.b'`, `.($k)`, `.$k` (the cursor is on the dot).
    fn seg(&mut self) -> R<Seg> {
        self.i += 1;
        Ok(match self.peek() {
            Some('\'') => Seg::Name(self.single_quoted()?),
            Some('(') => {
                let p = self.paren()?;
                Seg::Dyn(Word {
                    parts: vec![p],
                    pos: self.pos(),
                })
            }
            Some('$') => {
                let p = self.var_bare()?;
                Seg::Dyn(Word {
                    parts: vec![p],
                    pos: self.pos(),
                })
            }
            _ => {
                let k = self.i;
                while self.peek().is_some_and(|c| is_ident(c) || c == '-') {
                    self.i += 1;
                }
                if self.i == k {
                    return self.err("expected a key after `.` (use `^` to join text)");
                }
                Seg::Name(self.s[k..self.i].iter().collect())
            }
        })
    }

    /// `$name` without a path.
    fn var_bare(&mut self) -> R<Part> {
        self.i += 1;
        let start = self.i;
        while self.peek().is_some_and(is_ident) {
            self.i += 1;
        }
        if self.i == start {
            return self.err("expected a variable name after `$`");
        }
        Ok(Part::Var(VarRef {
            name: self.s[start..self.i].iter().collect(),
            path: vec![],
        }))
    }

    fn var(&mut self) -> R<Part> {
        self.i += 1; // $
        let count = self.eat('#');
        let start = self.i;
        while self.peek().is_some_and(is_ident) {
            self.i += 1;
        }
        if self.i == start {
            return self.err("expected a variable name after `$`");
        }
        let name: String = self.s[start..self.i].iter().collect();
        if count {
            return Ok(Part::Count(name));
        }
        let mut path = Vec::new();
        while self.peek() == Some('.') {
            path.push(self.seg()?);
        }
        Ok(Part::Var(VarRef { name, path }))
    }

    /// `( expression )`, with an optional path after it (P2).
    fn paren(&mut self) -> R<Part> {
        self.i += 1;
        let (sd, sl) = (self.stop_do, self.slot);
        self.stop_do = false;
        self.slot = false;
        self.nl_ok += 1;
        let r = self.nest(|p| {
            p.wsn();
            if p.peek() == Some(')') {
                return p.err("empty parentheses");
            }
            let e = p.expr()?;
            p.wsn();
            if !p.eat(')') {
                return p.err("expected `)`");
            }
            Ok(e)
        });
        self.nl_ok -= 1;
        self.stop_do = sd;
        self.slot = sl;
        let e = r?;
        let mut path = Vec::new();
        while self.peek() == Some('.') {
            path.push(self.seg()?);
        }
        Ok(Part::Paren(Box::new(e), path))
    }

    // ---- literals: every slot is an expression ----

    fn slot_expr(&mut self) -> R<Expr> {
        let (sd, sl) = (self.stop_do, self.slot);
        self.stop_do = false;
        self.slot = true;
        let e = self.expr();
        self.slot = sl;
        self.stop_do = sd;
        let e = e?;
        self.no_lone_bareword(&e)?;
        Ok(e)
    }

    fn list_lit(&mut self) -> R<Part> {
        self.i += 1;
        self.nl_ok += 1;
        let r = self.nest(|p| {
            let mut items = Vec::new();
            p.wsn();
            if p.eat(']') {
                return Ok(Part::List(items));
            }
            loop {
                p.wsn();
                items.push(p.slot_expr()?);
                p.wsn();
                if p.eat(',') {
                    continue;
                }
                if p.eat(']') {
                    return Ok(Part::List(items));
                }
                return p.err("expected `,` or `]`");
            }
        });
        self.nl_ok -= 1;
        r
    }

    fn map_lit(&mut self) -> R<Part> {
        self.i += 1;
        self.nl_ok += 1;
        let r = self.nest(|p| {
            let mut items = Vec::new();
            p.wsn();
            if p.eat('}') {
                return Ok(Part::Map(items));
            }
            loop {
                p.wsn();
                let sl = p.slot;
                p.slot = true;
                let k = p.atom_expr();
                p.slot = sl;
                let k = k?;
                p.wsn();
                if !p.eat(':') {
                    return p.err("expected `:`");
                }
                let v = p.slot_expr()?;
                items.push((k, v));
                p.wsn();
                if p.eat(',') {
                    continue;
                }
                if p.eat('}') {
                    return Ok(Part::Map(items));
                }
                return p.err("expected `,` or `}`");
            }
        });
        self.nl_ok -= 1;
        r
    }
}
