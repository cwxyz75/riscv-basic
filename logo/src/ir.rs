//! The IR and the lowering from tokens (rules in "The IR" in `LOGO.md`).

use crate::error::LogoError;
use crate::lex::{lex, Token};
use crate::parse::{Expr, Infix, Parser};
use crate::prims::source_of;
use crate::value::Value;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

/// A temp: a numbered slot holding a value or nothing.
pub(crate) type Temp = usize;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Instr {
    /// `dst` = a number, word or list literal.
    Const {
        dst: Temp,
        value: Value,
    },
    /// `dst` = the variable `name` (dynamic lookup).
    Var {
        dst: Temp,
        name: String,
    },
    Neg {
        dst: Temp,
        src: Temp,
    },
    Infix {
        dst: Temp,
        op: Infix,
        a: Temp,
        b: Temp,
    },
    /// Call a primitive or procedure on the values of `args`; `dst` gets its
    /// output, or nothing for a command.
    Call {
        dst: Temp,
        name: String,
        args: Vec<Temp>,
    },
    /// A statement's temp must hold nothing.
    Discard {
        src: Temp,
    },
    /// Run `body` `count` times (`repeat` with a literal list).
    Repeat {
        dst: Temp,
        count: Temp,
        body: Vec<Instr>,
    },
    /// Run one of two blocks (`if` with a literal list, or `ifelse` with two).
    If {
        dst: Temp,
        name: &'static str,
        cond: Temp,
        then_branch: Vec<Instr>,
        else_branch: Vec<Instr>,
    },
    /// Raise an error here (an instruction that could not be read).
    Fail(LogoError),
}

/// A lowered line: its instructions and how many temps they use.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Lowered {
    pub(crate) block: Vec<Instr>,
    pub(crate) temps: usize,
}

type Arity<'a> = &'a dyn Fn(&str) -> Option<usize>;

/// Lower a line of tokens. `arity` says how many inputs each name takes.
pub(crate) fn lower(tokens: &[Token], arity: Arity) -> Lowered {
    let mut lowerer = Lowerer { temps: 0 };
    let block = lowerer.tokens(tokens, arity);
    Lowered {
        block,
        temps: lowerer.temps,
    }
}

struct Lowerer {
    temps: usize,
}

impl Lowerer {
    fn fresh(&mut self) -> Temp {
        self.temps += 1;
        self.temps - 1
    }

    /// Instructions for a token sequence: each instruction read, lowered and
    /// followed by a `Discard`; an unreadable one becomes `Fail` and ends it.
    fn tokens(&mut self, tokens: &[Token], arity: Arity) -> Vec<Instr> {
        let mut out = Vec::new();
        let mut pos = 0;
        while pos < tokens.len() {
            let (parsed, next) = {
                let mut parser = Parser::new(tokens, pos, arity);
                let parsed = parser.instruction();
                (parsed, parser.pos())
            };
            match parsed {
                Ok(expr) => {
                    let src = self.expr(&expr, arity, &mut out);
                    out.push(Instr::Discard { src });
                    pos = next;
                }
                Err(error) => {
                    out.push(Instr::Fail(error));
                    break;
                }
            }
        }
        out
    }

    /// The instructions for a literal list used as code.
    fn body(&mut self, items: &[Value], arity: Arity) -> Vec<Instr> {
        match lex(&source_of(items)) {
            Ok(tokens) => self.tokens(&tokens, arity),
            Err(error) => vec![Instr::Fail(error)],
        }
    }

    /// Lower an expression, appending to `out`; returns the temp with its value.
    fn expr(&mut self, expr: &Expr, arity: Arity, out: &mut Vec<Instr>) -> Temp {
        match expr {
            Expr::Num(n) => self.constant(Value::Num(*n), out),
            Expr::Word(w) => self.constant(Value::Word(w.clone()), out),
            Expr::List(items) => self.constant(Value::List(items.clone()), out),
            Expr::Var(name) => {
                let dst = self.fresh();
                out.push(Instr::Var {
                    dst,
                    name: name.clone(),
                });
                dst
            }
            Expr::Neg(inner) => {
                let src = self.expr(inner, arity, out);
                let dst = self.fresh();
                out.push(Instr::Neg { dst, src });
                dst
            }
            Expr::Infix(op, left, right) => {
                let a = self.expr(left, arity, out);
                let b = self.expr(right, arity, out);
                let dst = self.fresh();
                out.push(Instr::Infix { dst, op: *op, a, b });
                dst
            }
            Expr::Call(name, args) => self.call(name, args, arity, out),
        }
    }

    fn constant(&mut self, value: Value, out: &mut Vec<Instr>) -> Temp {
        let dst = self.fresh();
        out.push(Instr::Const { dst, value });
        dst
    }

    fn call(&mut self, name: &str, args: &[Expr], arity: Arity, out: &mut Vec<Instr>) -> Temp {
        // Control flow with literal lists is lowered inline.
        match (name, args) {
            ("repeat", [count, Expr::List(items)]) => {
                let count = self.expr(count, arity, out);
                let body = self.body(items, arity);
                let dst = self.fresh();
                out.push(Instr::Repeat { dst, count, body });
                return dst;
            }
            ("if", [cond, Expr::List(items)]) => {
                let cond = self.expr(cond, arity, out);
                let then_branch = self.body(items, arity);
                let dst = self.fresh();
                out.push(Instr::If {
                    dst,
                    name: "if",
                    cond,
                    then_branch,
                    else_branch: Vec::new(),
                });
                return dst;
            }
            ("ifelse", [cond, Expr::List(yes), Expr::List(no)]) => {
                let cond = self.expr(cond, arity, out);
                let then_branch = self.body(yes, arity);
                let else_branch = self.body(no, arity);
                let dst = self.fresh();
                out.push(Instr::If {
                    dst,
                    name: "ifelse",
                    cond,
                    then_branch,
                    else_branch,
                });
                return dst;
            }
            _ => {}
        }
        let args: Vec<Temp> = args.iter().map(|arg| self.expr(arg, arity, out)).collect();
        let dst = self.fresh();
        out.push(Instr::Call {
            dst,
            name: String::from(name),
            args,
        });
        dst
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prims;

    fn lowered(text: &str) -> Lowered {
        lower(&lex(text).unwrap(), &|name| prims::arity(name))
    }

    fn num(dst: Temp, n: f64) -> Instr {
        Instr::Const {
            dst,
            value: Value::Num(n),
        }
    }

    fn call(dst: Temp, name: &str, args: &[Temp]) -> Instr {
        Instr::Call {
            dst,
            name: name.into(),
            args: args.to_vec(),
        }
    }

    #[test]
    fn commands_are_calls_followed_by_discard() {
        let l = lowered("fd 10 rt 90");
        assert_eq!(
            l.block,
            vec![
                num(0, 10.0),
                call(1, "fd", &[0]),
                Instr::Discard { src: 1 },
                num(2, 90.0),
                call(3, "rt", &[2]),
                Instr::Discard { src: 3 },
            ]
        );
        assert_eq!(l.temps, 4);
    }

    #[test]
    fn operands_come_before_the_instruction_that_uses_them() {
        let l = lowered("print 1 + 2");
        assert_eq!(
            l.block,
            vec![
                num(0, 1.0),
                num(1, 2.0),
                Instr::Infix {
                    dst: 2,
                    op: Infix::Add,
                    a: 0,
                    b: 1
                },
                call(3, "print", &[2]),
                Instr::Discard { src: 3 },
            ]
        );
    }

    #[test]
    fn unary_minus_and_variables() {
        let l = lowered("print -:x");
        assert_eq!(
            l.block,
            vec![
                Instr::Var {
                    dst: 0,
                    name: "x".into()
                },
                Instr::Neg { dst: 1, src: 0 },
                call(2, "print", &[1]),
                Instr::Discard { src: 2 },
            ]
        );
    }

    #[test]
    fn repeat_with_a_literal_list_is_inline() {
        let l = lowered("repeat 4 [fd 4 rt 90]");
        let body = vec![
            num(1, 4.0),
            call(2, "fd", &[1]),
            Instr::Discard { src: 2 },
            num(3, 90.0),
            call(4, "rt", &[3]),
            Instr::Discard { src: 4 },
        ];
        assert_eq!(
            l.block,
            vec![
                num(0, 4.0),
                Instr::Repeat {
                    dst: 5,
                    count: 0,
                    body
                },
                Instr::Discard { src: 5 }
            ]
        );
        assert_eq!(l.temps, 6);
    }

    #[test]
    fn if_and_ifelse_are_inline() {
        let l = lowered("if :x = 1 [print 2]");
        let then_branch = vec![
            num(3, 2.0),
            call(4, "print", &[3]),
            Instr::Discard { src: 4 },
        ];
        assert_eq!(
            l.block,
            vec![
                Instr::Var {
                    dst: 0,
                    name: "x".into()
                },
                num(1, 1.0),
                Instr::Infix {
                    dst: 2,
                    op: Infix::Eq,
                    a: 0,
                    b: 1
                },
                Instr::If {
                    dst: 5,
                    name: "if",
                    cond: 2,
                    then_branch,
                    else_branch: vec![]
                },
                Instr::Discard { src: 5 },
            ]
        );
        let l = lowered("ifelse 1 = 2 [a] [b]");
        assert_eq!(
            l.block,
            vec![
                num(0, 1.0),
                num(1, 2.0),
                Instr::Infix {
                    dst: 2,
                    op: Infix::Eq,
                    a: 0,
                    b: 1
                },
                Instr::If {
                    dst: 5,
                    name: "ifelse",
                    cond: 2,
                    then_branch: vec![call(3, "a", &[]), Instr::Discard { src: 3 }],
                    else_branch: vec![call(4, "b", &[]), Instr::Discard { src: 4 }],
                },
                Instr::Discard { src: 5 },
            ]
        );
    }

    #[test]
    fn a_list_that_is_not_a_literal_stays_a_call() {
        let l = lowered("repeat 3 :body");
        assert_eq!(
            l.block,
            vec![
                num(0, 3.0),
                Instr::Var {
                    dst: 1,
                    name: "body".into()
                },
                call(2, "repeat", &[0, 1]),
                Instr::Discard { src: 2 },
            ]
        );
    }

    #[test]
    fn an_input_that_is_a_command_is_just_a_call_result() {
        let l = lowered("print fd 10");
        assert_eq!(
            l.block,
            vec![
                num(0, 10.0),
                call(1, "fd", &[0]),
                call(2, "print", &[1]),
                Instr::Discard { src: 2 }
            ]
        );
    }

    #[test]
    fn an_unreadable_instruction_becomes_fail_after_the_earlier_ones() {
        let l = lowered("fd 10 fd");
        assert_eq!(
            l.block,
            vec![
                num(0, 10.0),
                call(1, "fd", &[0]),
                Instr::Discard { src: 1 },
                Instr::Fail(LogoError::NotEnoughInputs("fd".into())),
            ]
        );
    }
}
