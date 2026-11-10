//! The parser: tokens to expression trees, driven by arity. Each procedure
//! has a fixed number of inputs, and each input is a full expression.

use crate::error::LogoError;
use crate::lex::Token;
use crate::value::Value;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Infix {
    Add,
    Sub,
    Mul,
    Div,
    Eq,
    Lt,
    Gt,
}

impl Infix {
    pub(crate) fn symbol(self) -> &'static str {
        match self {
            Infix::Add => "+",
            Infix::Sub => "-",
            Infix::Mul => "*",
            Infix::Div => "/",
            Infix::Eq => "=",
            Infix::Lt => "<",
            Infix::Gt => ">",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum Expr {
    Num(f64),
    Word(String),
    List(Vec<Value>),
    Var(String),
    Neg(Box<Expr>),
    Infix(Infix, Box<Expr>, Box<Expr>),
    Call(String, Vec<Expr>),
}

pub(crate) struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
    arity: &'a dyn Fn(&str) -> Option<usize>,
}

impl<'a> Parser<'a> {
    pub(crate) fn new(
        tokens: &'a [Token],
        pos: usize,
        arity: &'a dyn Fn(&str) -> Option<usize>,
    ) -> Self {
        Parser { tokens, pos, arity }
    }

    pub(crate) fn pos(&self) -> usize {
        self.pos
    }

    /// One instruction: an expression, checked later to be a command.
    pub(crate) fn instruction(&mut self) -> Result<Expr, LogoError> {
        self.comparison()
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn starts_expression(&self) -> bool {
        matches!(
            self.peek(),
            Some(
                Token::Num(_)
                    | Token::Quoted(_)
                    | Token::Var(_)
                    | Token::Word(_)
                    | Token::List(_)
                    | Token::LParen
                    | Token::Neg
            )
        )
    }

    /// After an infix operator: there must be an operand.
    fn operand_follows(&self, op: Infix) -> Result<(), LogoError> {
        if self.starts_expression() {
            Ok(())
        } else {
            Err(LogoError::NotEnoughInputs(String::from(op.symbol())))
        }
    }

    fn comparison(&mut self) -> Result<Expr, LogoError> {
        let mut left = self.additive()?;
        loop {
            let op = match self.peek() {
                Some(Token::Eq) => Infix::Eq,
                Some(Token::Lt) => Infix::Lt,
                Some(Token::Gt) => Infix::Gt,
                _ => return Ok(left),
            };
            self.pos += 1;
            self.operand_follows(op)?;
            let right = self.additive()?;
            left = Expr::Infix(op, Box::new(left), Box::new(right));
        }
    }

    fn additive(&mut self) -> Result<Expr, LogoError> {
        let mut left = self.multiplicative()?;
        loop {
            let op = match self.peek() {
                Some(Token::Plus) => Infix::Add,
                Some(Token::Minus) => Infix::Sub,
                _ => return Ok(left),
            };
            self.pos += 1;
            self.operand_follows(op)?;
            let right = self.multiplicative()?;
            left = Expr::Infix(op, Box::new(left), Box::new(right));
        }
    }

    fn multiplicative(&mut self) -> Result<Expr, LogoError> {
        let mut left = self.unary()?;
        loop {
            let op = match self.peek() {
                Some(Token::Star) => Infix::Mul,
                Some(Token::Slash) => Infix::Div,
                _ => return Ok(left),
            };
            self.pos += 1;
            self.operand_follows(op)?;
            let right = self.unary()?;
            left = Expr::Infix(op, Box::new(left), Box::new(right));
        }
    }

    fn unary(&mut self) -> Result<Expr, LogoError> {
        if self.peek() == Some(&Token::Neg) {
            self.pos += 1;
            if !self.starts_expression() {
                return Err(LogoError::NotEnoughInputs(String::from("-")));
            }
            return Ok(Expr::Neg(Box::new(self.unary()?)));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr, LogoError> {
        let Some(token) = self.peek().cloned() else {
            return Err(LogoError::NotEnoughInputs(String::from("?")));
        };
        self.pos += 1;
        match token {
            Token::Num(n) => Ok(Expr::Num(n)),
            Token::Quoted(w) => Ok(Expr::Word(w)),
            Token::Var(v) => Ok(Expr::Var(v)),
            Token::List(l) => Ok(Expr::List(l)),
            Token::LParen => {
                let inner = self.comparison()?;
                if self.peek() != Some(&Token::RParen) {
                    return Err(LogoError::MissingParen);
                }
                self.pos += 1;
                Ok(inner)
            }
            Token::RParen => Err(LogoError::UnexpectedParen),
            Token::Word(name) => {
                // An unknown name is a procedure with no inputs, so that
                // `foo 10` reports `foo` before it looks at `10`.
                let inputs = (self.arity)(&name).unwrap_or(0);
                let mut args = Vec::with_capacity(inputs);
                for _ in 0..inputs {
                    if !self.starts_expression() {
                        return Err(LogoError::NotEnoughInputs(name));
                    }
                    args.push(self.comparison()?);
                }
                Ok(Expr::Call(name, args))
            }
            Token::Plus
            | Token::Minus
            | Token::Neg
            | Token::Star
            | Token::Slash
            | Token::Eq
            | Token::Lt
            | Token::Gt => Err(LogoError::DontKnowHow(String::from(match token {
                Token::Plus => "+",
                Token::Minus | Token::Neg => "-",
                Token::Star => "*",
                Token::Slash => "/",
                Token::Eq => "=",
                Token::Lt => "<",
                _ => ">",
            }))),
        }
    }
}
