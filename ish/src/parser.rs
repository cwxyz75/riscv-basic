//! The parser: source text to [`Stmt`]s (grammar: SHELL-SPEC section 5.1). Hand-written recursive descent over characters,
//! because the language has three lexical modes (command words, JSON-mode literals, infix islands).

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

/// Words that end a command wherever they appear unquoted.
const RESERVED: &[&str] = &["do", "end", "else", "catch"];
/// The most nesting of blocks, parentheses and literals.
pub const MAX_NEST: usize = 64;

pub fn parse(src: &str) -> R<Vec<Stmt>> {
    let mut p = Parser {
        s: src.chars().collect(),
        i: 0,
        depth: 0,
    };
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
                | '^'
        )
}

fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

impl Parser {
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

    /// Blanks, comments and backslash-newline; not newlines.
    fn ws(&mut self) {
        loop {
            match self.peek() {
                Some(' ' | '\t' | '\r') => self.i += 1,
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

    /// Blanks, comments, newlines (inside brackets, where lines do not end statements).
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
            if is_delim(c) {
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
                None => return Ok(out),
                Some(')') => return Ok(out),
                _ => {}
            }
            let w = self.peek_bare();
            if stop.contains(&w.as_str()) {
                return Ok(out);
            }
            if RESERVED.contains(&w.as_str()) {
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

    fn block(&mut self, stop: &[&str]) -> R<Vec<Stmt>> {
        self.nest(|p| p.stmts(stop))
    }

    fn stmt(&mut self) -> R<Stmt> {
        let pos = self.pos();
        let w = self.peek_bare();
        let kw_follows = self
            .s
            .get(self.i + w.chars().count())
            .is_none_or(|c| !is_ident_cont(*c));
        if kw_follows {
            match w.as_str() {
                "fn" => return self.fn_stmt(),
                "if" => {
                    self.i += 2;
                    return self.if_rest(pos);
                }
                "for" => return self.for_stmt(pos),
                "while" | "until" => {
                    self.i += w.chars().count();
                    self.ws();
                    let cond = self.word()?;
                    self.expect_word("do")?;
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
        if let Some((name, path, after)) = self.assign_head() {
            self.i = after;
            self.ws();
            let rhs = self.rhs()?;
            return Ok(Stmt::Assign {
                name,
                path,
                rhs,
                pos,
            });
        }
        self.command_stmt()
    }

    /// `name`, `name.key.key`, then blanks and a lone `=`: returns the target and the cursor after the `=`.
    fn assign_head(&self) -> Option<(String, Vec<Seg>, usize)> {
        let mut j = self.i;
        let start = j;
        while self.s.get(j).is_some_and(|&c| is_ident(c)) {
            j += 1;
        }
        if j == start || self.s[start].is_ascii_digit() {
            return None;
        }
        let name: String = self.s[start..j].iter().collect();
        let mut path = Vec::new();
        while self.s.get(j) == Some(&'.') {
            let k = j + 1;
            let mut e = k;
            while self.s.get(e).is_some_and(|&c| is_ident(c) || c == '-') {
                e += 1;
            }
            if e == k {
                return None;
            }
            path.push(Seg::Name(self.s[k..e].iter().collect()));
            j = e;
        }
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
            None | Some(' ' | '\t' | '\n' | '\r') => Some((name, path, k + 1)),
            _ => None,
        }
    }

    fn rhs(&mut self) -> R<Rhs> {
        let w = self.peek_bare();
        if w == "try" || w == "if" {
            let pos = self.pos();
            let st = if w == "try" {
                self.try_stmt(pos)?
            } else {
                self.i += 2;
                self.if_rest(pos)?
            };
            return Ok(Rhs::Stmt(Box::new(st)));
        }
        let ao = self.and_or()?;
        // a lone word is a value; anything else is a call
        if ao.rest.is_empty() && !ao.first.negate && ao.first.stages.len() == 1 {
            let c = &ao.first.stages[0];
            if c.words.len() == 1 && c.redirs.is_empty() {
                return Ok(Rhs::Atom(c.words[0].clone()));
            }
        }
        if ao.first.stages.iter().all(|c| c.words.is_empty()) {
            return self.err("nothing to assign");
        }
        Ok(Rhs::Call(ao))
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

    /// After `if`: the condition, `do`, the body, and the `else` chain up to the one `end`.
    fn if_rest(&mut self, pos: Pos) -> R<Stmt> {
        self.ws();
        let cond = self.word()?;
        self.expect_word("do")?;
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

    // ---- commands ----

    fn command_stmt(&mut self) -> R<Stmt> {
        let ao = self.and_or()?;
        if ao.rest.is_empty() && !ao.first.negate && ao.first.stages.len() == 1 {
            let c = &ao.first.stages[0];
            if c.words.len() == 1 && c.redirs.is_empty() && !is_bare(&c.words[0]) {
                return Ok(Stmt::Value(c.words[0].clone()));
            }
        }
        Ok(Stmt::Cmd(ao))
    }

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
        Ok(Pipeline { negate, stages })
    }

    fn command(&mut self) -> R<Command> {
        let pos = self.pos();
        let mut words = Vec::new();
        let mut redirs = Vec::new();
        loop {
            self.ws();
            match self.peek() {
                None | Some('\n' | ';' | ')' | '|') => break,
                Some('&') => {
                    if self.peek_at(1) == Some('&') {
                        break;
                    }
                    return self.err("background jobs are not supported");
                }
                Some('>') | Some('<') => redirs.push(self.redir()?),
                _ => {
                    let w = self.peek_bare();
                    if !w.is_empty() && RESERVED.contains(&w.as_str()) && w != "do" {
                        break;
                    }
                    if w == "do" && words.is_empty() {
                        return self.err("unexpected `do`");
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

    fn e(&self, msg: &str) -> ParseError {
        ParseError {
            pos: self.pos(),
            msg: String::from(msg),
            incomplete: self.i >= self.s.len() && !msg.starts_with("unterminated"),
        }
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
                Some(c) if !is_delim(c) || matches!(c, '$' | '\'' | '"') => continue,
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
            '(' => parts.push(self.paren()?),
            '[' => parts.push(self.json_list()?),
            '{' => parts.push(self.json_map()?),
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
                if t.contains(['*', '?']) {
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
                '\\' => {
                    let Some(e) = self.peek() else {
                        return self.err("unterminated string");
                    };
                    self.i += 1;
                    out.push(match e {
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
                                        char::from_u32(
                                            0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00),
                                        )
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
                    });
                }
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
            self.i += 1;
            match self.peek() {
                Some('\'') => path.push(Seg::Name(self.single_quoted()?)),
                Some('(') => {
                    let p = self.paren()?;
                    path.push(Seg::Dyn(Word {
                        parts: vec![p],
                        pos: self.pos(),
                    }));
                }
                Some('$') => {
                    let p = self.var()?;
                    path.push(Seg::Dyn(Word {
                        parts: vec![p],
                        pos: self.pos(),
                    }));
                }
                _ => {
                    let k = self.i;
                    while self.peek().is_some_and(|c| is_ident(c) || c == '-') {
                        self.i += 1;
                    }
                    if self.i == k {
                        return self.err("expected a key after `.` (use `^` to join text)");
                    }
                    path.push(Seg::Name(self.s[k..self.i].iter().collect()));
                }
            }
        }
        Ok(Part::Var(VarRef { name, path }))
    }

    /// `( ... )`: an island if it starts with `calc` or `is`, else statements evaluated in value position.
    fn paren(&mut self) -> R<Part> {
        self.i += 1;
        self.nest(|p| {
            p.wsn();
            let w = p.peek_bare();
            let kind = match w.as_str() {
                "calc" => Some(IslKind::Calc),
                "is" => Some(IslKind::Is),
                _ => None,
            };
            if let Some(kind) = kind {
                p.i += w.chars().count();
                let e = p.isl(0)?;
                p.wsn();
                if !p.eat(')') {
                    return p.err("expected `)`");
                }
                return Ok(Part::Island(kind, Box::new(e)));
            }
            let body = p.stmts(&[])?;
            p.wsn();
            if !p.eat(')') {
                return p.err("expected `)`");
            }
            if body.is_empty() {
                return p.err("empty parentheses");
            }
            Ok(Part::Sub(body))
        })
    }

    // ---- JSON mode ----

    fn json_value(&mut self) -> R<Word> {
        self.wsn();
        let pos = self.pos();
        let part = match self.peek() {
            Some('"') => Part::Str(self.json_string()?),
            Some('[') => self.json_list()?,
            Some('{') => self.json_map()?,
            Some('$') => self.var()?,
            Some('(') => self.paren()?,
            Some(c) if c == '-' || c.is_ascii_digit() => {
                let start = self.i;
                while self
                    .peek()
                    .is_some_and(|c| c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E'))
                {
                    self.i += 1;
                }
                let t: String = self.s[start..self.i].iter().collect();
                match Number::parse(t.as_bytes()) {
                    Ok(n) => Part::Num(n),
                    Err(_) => {
                        return Err(ParseError {
                            incomplete: false,
                            pos,
                            msg: format!("bad number `{t}`"),
                        })
                    }
                }
            }
            Some(_) => {
                let t = self.peek_bare();
                self.i += t.chars().count().max(1);
                match t.as_str() {
                    "true" => Part::Bool(true),
                    "false" => Part::Bool(false),
                    "null" => Part::Null,
                    _ => {
                        return Err(ParseError {
                            incomplete: false,
                            pos,
                            msg: format!("`{t}` is not a value here: quote it (barewords are not values in a literal)"),
                        })
                    }
                }
            }
            None => return self.err("unexpected end of input in a literal"),
        };
        Ok(Word {
            parts: vec![part],
            pos,
        })
    }

    fn json_list(&mut self) -> R<Part> {
        self.i += 1;
        self.nest(|p| {
            let mut items = Vec::new();
            p.wsn();
            if p.eat(']') {
                return Ok(Part::List(items));
            }
            loop {
                items.push(p.json_value()?);
                p.wsn();
                if p.eat(',') {
                    continue;
                }
                if p.eat(']') {
                    return Ok(Part::List(items));
                }
                return p.err("expected `,` or `]`");
            }
        })
    }

    fn json_map(&mut self) -> R<Part> {
        self.i += 1;
        self.nest(|p| {
            let mut items = Vec::new();
            p.wsn();
            if p.eat('}') {
                return Ok(Part::Map(items));
            }
            loop {
                let k = p.json_value()?;
                p.wsn();
                if !p.eat(':') {
                    return p.err("expected `:`");
                }
                let v = p.json_value()?;
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
        })
    }

    // ---- infix islands ----

    /// Precedence climbing. Levels (low to high): or(1) and(2) comparison(4) `^`(5) additive(6) multiplicative(7).
    /// `not` binds below comparison and above `and`; unary minus binds tightest.
    fn isl(&mut self, min: u8) -> R<Isl> {
        self.wsn();
        let mut lhs = if self.at_word("not") {
            self.i += 3;
            let e = self.isl(3)?;
            Isl::Un(UnOp::Not, Box::new(e))
        } else if self.peek() == Some('-') {
            self.i += 1;
            let e = self.isl(8)?;
            Isl::Un(UnOp::Neg, Box::new(e))
        } else if self.peek() == Some('(') {
            match self.isl_group() {
                Some(e) => e,
                None => Isl::Operand(self.isl_operand()?),
            }
        } else {
            Isl::Operand(self.isl_operand()?)
        };
        loop {
            self.wsn();
            let Some((op, prec, len)) = self.isl_op() else {
                break;
            };
            if prec < min {
                break;
            }
            self.i += len;
            let rhs = self.isl(prec + 1)?;
            lhs = Isl::Bin(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn isl_op(&self) -> Option<(BinOp, u8, usize)> {
        let (c, d) = (self.peek()?, self.peek_at(1));
        Some(match (c, d) {
            ('=', Some('=')) => (BinOp::Eq, 4, 2),
            ('!', Some('=')) => (BinOp::Ne, 4, 2),
            ('<', Some('=')) => (BinOp::Le, 4, 2),
            ('>', Some('=')) => (BinOp::Ge, 4, 2),
            ('<', _) => (BinOp::Lt, 4, 1),
            ('>', _) => (BinOp::Gt, 4, 1),
            ('^', _) => (BinOp::Cat, 5, 1),
            ('+', _) => (BinOp::Add, 6, 1),
            ('-', _) => (BinOp::Sub, 6, 1),
            ('*', _) => (BinOp::Mul, 7, 1),
            ('/', Some('/')) => (BinOp::IDiv, 7, 2),
            ('/', _) => (BinOp::Div, 7, 1),
            ('%', _) => (BinOp::Rem, 7, 1),
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
        })
    }

    /// A `(` inside an island groups when what follows is an island expression that ends at the `)`; otherwise it is a call
    /// (`(len $x)`, `($f 1 2)`, `(calc ...)`). The cursor is left after the `)` on success and where it was on failure.
    fn isl_group(&mut self) -> Option<Isl> {
        let (save_i, save_depth) = (self.i, self.depth);
        self.i += 1;
        self.depth += 1;
        let ok = self.depth <= MAX_NEST;
        let r = if ok { self.isl(0).ok() } else { None };
        self.wsn();
        if let Some(e) = r {
            if self.eat(')') {
                self.depth = save_depth;
                return Some(e);
            }
        }
        self.i = save_i;
        self.depth = save_depth;
        None
    }

    fn isl_operand(&mut self) -> R<Word> {
        self.wsn();
        let pos = self.pos();
        match self.peek() {
            Some(c) if c.is_ascii_digit() => {
                let start = self.i;
                let (n, len) = self.number_token()?;
                let _ = start;
                self.i += len;
                Ok(Word {
                    parts: vec![Part::Num(n)],
                    pos,
                })
            }
            Some('\'') => Ok(Word {
                parts: vec![Part::Str(self.single_quoted()?)],
                pos,
            }),
            Some('"' | '[' | '{' | '$' | '(') => self.json_value(),
            _ => {
                let t = self.peek_bare();
                match t.as_str() {
                    "true" | "false" | "null" => self.json_value(),
                    _ => self.err("expected a value in the expression (strings are quoted)"),
                }
            }
        }
    }

    /// A number literal in an island: decimal (with fraction and exponent), `0x` hex or `0b` binary.
    fn number_token(&self) -> R<(Number, usize)> {
        let rest: String = self.s[self.i..].iter().take(80).collect();
        let b = rest.as_bytes();
        if b.len() > 2 && b[0] == b'0' && (b[1] == b'x' || b[1] == b'b') {
            let radix = if b[1] == b'x' { 16 } else { 2 };
            let digits: String = rest[2..]
                .chars()
                .take_while(|c| c.is_digit(radix))
                .collect();
            if digits.is_empty() {
                return self.err("bad number");
            }
            let mut v: i128 = 0;
            for c in digits.chars() {
                v = v
                    .checked_mul(i128::from(radix))
                    .and_then(|v| v.checked_add(i128::from(c.to_digit(radix).unwrap_or(0))))
                    .ok_or_else(|| self.e("number too large"))?;
            }
            return Number::from_parts(v, 0)
                .map(|n| (n, 2 + digits.len()))
                .map_err(|_| self.e("number too large"));
        }
        let mut len = 0;
        while len < b.len() && (b[len].is_ascii_digit() || matches!(b[len], b'.' | b'e' | b'E')) {
            // an exponent sign belongs to the number only right after the `e`
            len += 1;
            if matches!(b[len - 1], b'e' | b'E') && len < b.len() && matches!(b[len], b'+' | b'-') {
                len += 1;
            }
        }
        // a trailing dot is not part of the number
        while len > 0 && b[len - 1] == b'.' {
            len -= 1;
        }
        match Number::parse(&b[..len]) {
            Ok(n) => Ok((n, len)),
            Err(_) => self.err("bad number"),
        }
    }
}

fn is_ident_cont(c: char) -> bool {
    is_ident(c) || c == '-' || c == '.' || c == '/'
}

/// Is this word a bareword (text only)? A lone bareword in statement position is a command.
fn is_bare(w: &Word) -> bool {
    w.parts
        .iter()
        .all(|p| matches!(p, Part::Lit(_) | Part::Glob(_)))
}
