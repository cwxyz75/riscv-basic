//! Expression evaluation, directly over a line's token bytes (no AST: the
//! classic approach, and a loop body is simply re-evaluated each pass).
//! Precedence, typing and fault rules are in `BASIC.md`.

use crate::error::Error;
use crate::io::Io;
use crate::memory::Memory;
use crate::number::format_number;
use crate::token::{keyword, token};
use crate::value::{check_length, Value};
use alloc::string::String;
use alloc::vec::Vec;

/// Evaluate one expression starting at `pos` in `src`. Returns the value and
/// the position just after the expression (trailing spaces not consumed).
/// `io` is used by the machine functions (`PEEK`, `TIMER`).
pub(crate) fn evaluate<I: Io>(
    src: &[u8],
    pos: usize,
    mem: &mut Memory,
    io: &mut I,
) -> Result<(Value, usize), Error> {
    let mut parser = Parser {
        src,
        pos,
        mem,
        io,
        and: token("AND"),
        or: token("OR"),
        not: token("NOT"),
    };
    let value = parser.or_expr()?;
    Ok((value, parser.pos))
}

/// Like [`evaluate`], but the expression must be a number (`?TYPE MISMATCH`
/// otherwise).
pub(crate) fn evaluate_number<I: Io>(
    src: &[u8],
    pos: usize,
    mem: &mut Memory,
    io: &mut I,
) -> Result<(f64, usize), Error> {
    let (value, end) = evaluate(src, pos, mem, io)?;
    Ok((value.num()?, end))
}

/// Parse a subscript list `( n [, n ...] )` starting at `pos` (spaces before
/// the `(` are allowed). Returns the numbers and the position after the `)`.
pub(crate) fn evaluate_subscripts<I: Io>(
    src: &[u8],
    pos: usize,
    mem: &mut Memory,
    io: &mut I,
) -> Result<(Vec<f64>, usize), Error> {
    let mut parser = Parser {
        src,
        pos,
        mem,
        io,
        and: token("AND"),
        or: token("OR"),
        not: token("NOT"),
    };
    let subs = parser.subscripts()?;
    Ok((subs, parser.pos))
}

/// Read a variable name at `*pos` (skipping leading spaces): a letter, then
/// letters and digits, then an optional `$` for a string variable. `None` if
/// there is no name there.
pub(crate) fn parse_name(src: &[u8], pos: &mut usize) -> Result<Option<String>, Error> {
    while src.get(*pos) == Some(&b' ') {
        *pos += 1;
    }
    let start = *pos;
    if !src.get(start).is_some_and(u8::is_ascii_alphabetic) {
        return Ok(None);
    }
    let mut end = start + 1;
    while src.get(end).is_some_and(u8::is_ascii_alphanumeric) {
        end += 1;
    }
    if src.get(end) == Some(&b'$') {
        end += 1;
    }
    *pos = end;
    Ok(Some(
        src[start..end].iter().map(|&b| char::from(b)).collect(),
    ))
}

/// Reject results that are not ordinary numbers.
fn check(value: f64) -> Result<f64, Error> {
    if value.is_nan() {
        Err(Error::IllegalQuantity)
    } else if value.is_infinite() {
        Err(Error::Overflow)
    } else {
        Ok(value)
    }
}

/// The numeric functions that take one number and give one back.
fn math(name: &str, x: f64) -> Result<f64, Error> {
    let result = match name {
        "ABS" => x.abs(),
        "INT" => libm::floor(x),
        "SGN" => {
            if x > 0.0 {
                1.0
            } else if x < 0.0 {
                -1.0
            } else {
                0.0
            }
        }
        "SQR" if x < 0.0 => return Err(Error::IllegalQuantity),
        "SQR" => libm::sqrt(x),
        "SIN" => libm::sin(x),
        "COS" => libm::cos(x),
        "TAN" => libm::tan(x),
        "ATN" => libm::atan(x),
        "LOG" if x <= 0.0 => return Err(Error::IllegalQuantity),
        "LOG" => libm::log(x),
        "EXP" => libm::exp(x),
        _ => return Err(Error::Syntax),
    };
    check(result)
}

/// A machine address for `PEEK`/`POKE`: a non-negative integer-valued number
/// below 2^53, so it converts exactly.
pub(crate) fn to_address(value: f64) -> Result<u64, Error> {
    const LIMIT: f64 = 9_007_199_254_740_992.0; // 2^53
    if (0.0..LIMIT).contains(&value) && value == libm::trunc(value) {
        Ok(value as u64)
    } else {
        Err(Error::IllegalQuantity)
    }
}

/// A byte value for `POKE`: an integer-valued number from 0 to 255.
pub(crate) fn to_byte(value: f64) -> Result<u8, Error> {
    if (0.0..=255.0).contains(&value) && value == libm::trunc(value) {
        Ok(value as u8)
    } else {
        Err(Error::IllegalQuantity)
    }
}

/// A count or position for the string functions and `TAB`/`SPC`: truncated
/// toward zero, and negative is an error.
pub(crate) fn to_count(value: f64) -> Result<usize, Error> {
    if value < 0.0 {
        Err(Error::IllegalQuantity)
    } else {
        Ok(libm::trunc(value) as usize)
    }
}

/// Truncate toward zero to a 32-bit signed integer for the logic operators.
fn to_i32(value: f64) -> Result<i32, Error> {
    let truncated = libm::trunc(value);
    if (-2_147_483_648.0..=2_147_483_647.0).contains(&truncated) {
        Ok(truncated as i32)
    } else {
        Err(Error::IllegalQuantity)
    }
}

/// The number at the start of `text`, for `VAL`: optional spaces, a sign,
/// digits with an optional fraction and exponent. `0` if there is none.
fn leading_number(text: &str) -> Result<f64, Error> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while bytes.get(i) == Some(&b' ') {
        i += 1;
    }
    let start = i;
    if matches!(bytes.get(i), Some(b'+' | b'-')) {
        i += 1;
    }
    let mut digits = 0;
    while bytes.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
        digits += 1;
    }
    if bytes.get(i) == Some(&b'.') {
        i += 1;
        while bytes.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return Ok(0.0);
    }
    // An exponent only counts if it has digits after it.
    if matches!(bytes.get(i), Some(b'E' | b'e')) {
        let mut j = i + 1;
        if matches!(bytes.get(j), Some(b'+' | b'-')) {
            j += 1;
        }
        if bytes.get(j).is_some_and(u8::is_ascii_digit) {
            while bytes.get(j).is_some_and(u8::is_ascii_digit) {
                j += 1;
            }
            i = j;
        }
    }
    let value: f64 = text[start..i].parse().map_err(|_| Error::Syntax)?;
    check(value)
}

struct Parser<'a, I: Io> {
    src: &'a [u8],
    pos: usize,
    mem: &'a mut Memory,
    io: &'a mut I,
    and: u8,
    or: u8,
    not: u8,
}

impl<I: Io> Parser<'_, I> {
    /// The next byte, after skipping spaces, without consuming it.
    fn peek(&mut self) -> Option<u8> {
        while self.src.get(self.pos) == Some(&b' ') {
            self.pos += 1;
        }
        self.src.get(self.pos).copied()
    }

    fn or_expr(&mut self) -> Result<Value, Error> {
        let mut left = self.and_expr()?;
        while self.peek() == Some(self.or) {
            self.pos += 1;
            let right = self.and_expr()?;
            left = Value::Num(f64::from(to_i32(left.num()?)? | to_i32(right.num()?)?));
        }
        Ok(left)
    }

    fn and_expr(&mut self) -> Result<Value, Error> {
        let mut left = self.not_expr()?;
        while self.peek() == Some(self.and) {
            self.pos += 1;
            let right = self.not_expr()?;
            left = Value::Num(f64::from(to_i32(left.num()?)? & to_i32(right.num()?)?));
        }
        Ok(left)
    }

    fn not_expr(&mut self) -> Result<Value, Error> {
        if self.peek() == Some(self.not) {
            self.pos += 1;
            let operand = self.not_expr()?.num()?;
            return Ok(Value::Num(f64::from(!to_i32(operand)?)));
        }
        self.comparison()
    }

    fn comparison(&mut self) -> Result<Value, Error> {
        let mut left = self.additive()?;
        loop {
            let (less, equal, greater) = match self.peek() {
                Some(b'=') => {
                    self.pos += 1;
                    (false, true, false)
                }
                Some(b'<') => {
                    self.pos += 1;
                    match self.src.get(self.pos) {
                        Some(b'>') => {
                            self.pos += 1;
                            (true, false, true)
                        }
                        Some(b'=') => {
                            self.pos += 1;
                            (true, true, false)
                        }
                        _ => (true, false, false),
                    }
                }
                Some(b'>') => {
                    self.pos += 1;
                    match self.src.get(self.pos) {
                        Some(b'=') => {
                            self.pos += 1;
                            (false, true, true)
                        }
                        _ => (false, false, true),
                    }
                }
                _ => return Ok(left),
            };
            let right = self.additive()?;
            let ordering = match (&left, &right) {
                (Value::Num(a), Value::Num(b)) => a.partial_cmp(b),
                (Value::Str(a), Value::Str(b)) => Some(a.as_bytes().cmp(b.as_bytes())),
                _ => return Err(Error::TypeMismatch),
            };
            use core::cmp::Ordering::{Equal, Greater, Less};
            let holds = match ordering {
                Some(Less) => less,
                Some(Equal) => equal,
                Some(Greater) => greater,
                None => false,
            };
            left = Value::Num(if holds { -1.0 } else { 0.0 });
        }
    }

    fn additive(&mut self) -> Result<Value, Error> {
        let mut left = self.term()?;
        loop {
            match self.peek() {
                Some(b'+') => {
                    self.pos += 1;
                    let right = self.term()?;
                    left = match (left, right) {
                        (Value::Num(a), Value::Num(b)) => Value::Num(check(a + b)?),
                        (Value::Str(mut a), Value::Str(b)) => {
                            a.push_str(&b);
                            Value::Str(check_length(a)?)
                        }
                        _ => return Err(Error::TypeMismatch),
                    };
                }
                Some(b'-') => {
                    self.pos += 1;
                    let right = self.term()?.num()?;
                    left = Value::Num(check(left.num()? - right)?);
                }
                _ => return Ok(left),
            }
        }
    }

    fn term(&mut self) -> Result<Value, Error> {
        let mut left = self.unary()?;
        loop {
            match self.peek() {
                Some(b'*') => {
                    self.pos += 1;
                    let right = self.unary()?.num()?;
                    left = Value::Num(check(left.num()? * right)?);
                }
                Some(b'/') => {
                    self.pos += 1;
                    let right = self.unary()?.num()?;
                    let dividend = left.num()?;
                    if right == 0.0 {
                        return Err(Error::DivisionByZero);
                    }
                    left = Value::Num(check(dividend / right)?);
                }
                _ => return Ok(left),
            }
        }
    }

    /// Unary minus binds looser than `^`, so `-2^2` is `-4`.
    fn unary(&mut self) -> Result<Value, Error> {
        match self.peek() {
            Some(b'-') => {
                self.pos += 1;
                Ok(Value::Num(-self.unary()?.num()?))
            }
            Some(b'+') => {
                self.pos += 1;
                Ok(Value::Num(self.unary()?.num()?))
            }
            _ => self.power(),
        }
    }

    /// `^` groups left to right, so `2^3^2` is `64`.
    fn power(&mut self) -> Result<Value, Error> {
        let mut base = self.primary()?;
        while self.peek() == Some(b'^') {
            self.pos += 1;
            let exponent = self.signed_primary()?;
            base = Value::Num(check(libm::pow(base.num()?, exponent))?);
        }
        Ok(base)
    }

    /// The exponent of `^` may carry a sign: `2^-1`.
    fn signed_primary(&mut self) -> Result<f64, Error> {
        match self.peek() {
            Some(b'-') => {
                self.pos += 1;
                Ok(-self.signed_primary()?)
            }
            Some(b'+') => {
                self.pos += 1;
                self.signed_primary()
            }
            _ => self.primary()?.num(),
        }
    }

    fn primary(&mut self) -> Result<Value, Error> {
        match self.peek() {
            Some(b'(') => {
                self.pos += 1;
                let value = self.or_expr()?;
                if self.peek() != Some(b')') {
                    return Err(Error::Syntax);
                }
                self.pos += 1;
                Ok(value)
            }
            Some(b'"') => Ok(Value::Str(self.string_literal())),
            Some(b) if b.is_ascii_digit() || b == b'.' => self.number().map(Value::Num),
            Some(b) if b.is_ascii_alphabetic() => {
                let name = parse_name(self.src, &mut self.pos)?.ok_or(Error::Syntax)?;
                if self.src.get(self.pos) == Some(&b'(') {
                    let subs = self.subscripts()?;
                    return self.mem.element(&name, &subs);
                }
                Ok(self.mem.get(&name))
            }
            Some(b) => match keyword(b) {
                Some("TIMER") => {
                    self.pos += 1;
                    Ok(Value::Num(self.io.millis() as f64 / 1000.0))
                }
                Some(name) => {
                    self.pos += 1;
                    let args = self.arguments()?;
                    self.call(name, args)
                }
                None => Err(Error::Syntax),
            },
            None => Err(Error::Syntax),
        }
    }

    /// A string literal: from the opening quote to the closing one, or to the
    /// end of the line if it is never closed.
    fn string_literal(&mut self) -> String {
        let start = self.pos + 1;
        let end = self.src[start..]
            .iter()
            .position(|&b| b == b'"')
            .map_or(self.src.len(), |i| start + i);
        self.pos = (end + 1).min(self.src.len());
        self.src[start..end]
            .iter()
            .map(|&b| char::from(b))
            .collect()
    }

    /// A subscript list: one to three numbers in parentheses.
    fn subscripts(&mut self) -> Result<Vec<f64>, Error> {
        let args = self.arguments()?;
        if args.is_empty() || args.len() > 3 {
            return Err(Error::Syntax);
        }
        args.into_iter().map(Value::num).collect()
    }

    /// A function's argument list: `( [expression {, expression}] )`.
    fn arguments(&mut self) -> Result<Vec<Value>, Error> {
        if self.peek() != Some(b'(') {
            return Err(Error::Syntax);
        }
        self.pos += 1;
        let mut args = Vec::new();
        if self.peek() == Some(b')') {
            self.pos += 1;
            return Ok(args);
        }
        loop {
            args.push(self.or_expr()?);
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b')') => {
                    self.pos += 1;
                    return Ok(args);
                }
                _ => return Err(Error::Syntax),
            }
        }
    }

    /// Apply the function `name` to its arguments. A wrong number of arguments
    /// is a syntax error; a wrong type is a type mismatch.
    fn call(&mut self, name: &str, mut args: Vec<Value>) -> Result<Value, Error> {
        let arity = match name {
            "ABS" | "INT" | "SGN" | "SQR" | "SIN" | "COS" | "TAN" | "ATN" | "LOG" | "EXP"
            | "RND" | "PEEK" | "LEN" | "ASC" | "VAL" | "STR$" | "CHR$" => 1..=1,
            "LEFT$" | "RIGHT$" => 2..=2,
            "MID$" => 2..=3,
            _ => return Err(Error::Syntax),
        };
        if !arity.contains(&args.len()) {
            return Err(Error::Syntax);
        }
        // Take the arguments out front to back.
        let mut rest = args.drain(..);
        let first = rest.next().ok_or(Error::Syntax)?;
        let second = rest.next();
        let third = rest.next();

        match name {
            "RND" => Ok(Value::Num(self.mem.rnd(first.num()?))),
            "PEEK" => {
                let address = to_address(first.num()?)?;
                Ok(Value::Num(f64::from(self.io.peek(address))))
            }
            "LEN" => Ok(Value::Num(first.string()?.len() as f64)),
            "ASC" => match first.string()?.bytes().next() {
                Some(code) => Ok(Value::Num(f64::from(code))),
                None => Err(Error::IllegalQuantity),
            },
            "VAL" => Ok(Value::Num(leading_number(&first.string()?)?)),
            "STR$" => {
                let text = format_number(first.num()?);
                Ok(Value::Str(String::from(text.trim_end())))
            }
            "CHR$" => {
                let code = to_count(first.num()?)?;
                match u8::try_from(code) {
                    Ok(byte) if byte < 128 => Ok(Value::Str(String::from(char::from(byte)))),
                    _ => Err(Error::IllegalQuantity),
                }
            }
            "LEFT$" => {
                let text = first.string()?;
                let count = to_count(second.ok_or(Error::Syntax)?.num()?)?;
                Ok(Value::Str(String::from(&text[..count.min(text.len())])))
            }
            "RIGHT$" => {
                let text = first.string()?;
                let count = to_count(second.ok_or(Error::Syntax)?.num()?)?;
                Ok(Value::Str(String::from(
                    &text[text.len() - count.min(text.len())..],
                )))
            }
            "MID$" => {
                let text = first.string()?;
                let start = to_count(second.ok_or(Error::Syntax)?.num()?)?;
                if start < 1 {
                    return Err(Error::IllegalQuantity);
                }
                let length = match third {
                    Some(n) => to_count(n.num()?)?,
                    None => text.len(),
                };
                let from = (start - 1).min(text.len());
                let to = from.saturating_add(length).min(text.len());
                Ok(Value::Str(String::from(&text[from..to])))
            }
            _ => Ok(Value::Num(math(name, first.num()?)?)),
        }
    }

    /// A number literal: digits, optional fraction, optional `E[+-]digits`.
    fn number(&mut self) -> Result<f64, Error> {
        let start = self.pos;
        let mut digits = 0;
        while self.src.get(self.pos).is_some_and(u8::is_ascii_digit) {
            self.pos += 1;
            digits += 1;
        }
        if self.src.get(self.pos) == Some(&b'.') {
            self.pos += 1;
            while self.src.get(self.pos).is_some_and(u8::is_ascii_digit) {
                self.pos += 1;
                digits += 1;
            }
        }
        if digits == 0 {
            return Err(Error::Syntax);
        }
        if self.src.get(self.pos) == Some(&b'E') {
            self.pos += 1;
            if matches!(self.src.get(self.pos), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            let mut exponent_digits = 0;
            while self.src.get(self.pos).is_some_and(u8::is_ascii_digit) {
                self.pos += 1;
                exponent_digits += 1;
            }
            if exponent_digits == 0 {
                return Err(Error::Syntax);
            }
        }
        // The slice is ASCII digits, '.', 'E' and signs only.
        let text: String = self.src[start..self.pos]
            .iter()
            .map(|&b| char::from(b))
            .collect();
        let value: f64 = text.parse().map_err(|_| Error::Syntax)?;
        check(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::Line;
    use crate::token::tokenize;

    /// An `Io` whose memory reads back the low byte of the address and whose
    /// clock is fixed.
    struct TestIo;

    impl Io for TestIo {
        fn write_str(&mut self, _: &str) {}
        fn read_line(&mut self) -> Line {
            Line::Eof
        }
        fn break_pressed(&mut self) -> bool {
            false
        }
        fn millis(&mut self) -> u64 {
            1500
        }
        fn peek(&mut self, addr: u64) -> u8 {
            addr as u8
        }
        fn poke(&mut self, _: u64, _: u8) {}
    }

    fn eval_value(text: &str) -> Result<Value, Error> {
        let tokens = tokenize(text).unwrap();
        let mut mem = Memory::default();
        let (value, end) = evaluate(&tokens, 0, &mut mem, &mut TestIo)?;
        assert!(tokens[end..].iter().all(|&b| b == b' '), "trailing input");
        Ok(value)
    }

    fn eval(text: &str) -> Result<f64, Error> {
        eval_value(text)?.num()
    }

    fn eval_str(text: &str) -> Result<String, Error> {
        eval_value(text)?.string()
    }

    #[test]
    fn precedence_and_associativity() {
        assert_eq!(eval("2+3*4"), Ok(14.0));
        assert_eq!(eval("2^3^2"), Ok(64.0));
        assert_eq!(eval("-2^2"), Ok(-4.0));
        assert_eq!(eval("2^-1"), Ok(0.5));
        assert_eq!(eval("10-4-3"), Ok(3.0));
    }

    #[test]
    fn faults() {
        assert_eq!(eval("1/0"), Err(Error::DivisionByZero));
        assert_eq!(eval("10^400"), Err(Error::Overflow));
        assert_eq!(eval("(-8)^(1/3)"), Err(Error::IllegalQuantity));
        assert_eq!(eval("2 +"), Err(Error::Syntax));
        assert_eq!(eval("1E"), Err(Error::Syntax));
    }

    #[test]
    fn logic_truncates_to_32_bit() {
        assert_eq!(eval("NOT 1"), Ok(-2.0));
        assert_eq!(eval("5.9 AND 7"), Ok(5.0));
        assert_eq!(eval("2^31 AND 1"), Err(Error::IllegalQuantity));
    }

    #[test]
    fn variables_default_to_zero_or_empty() {
        let tokens = tokenize("X+1").unwrap();
        let mut mem = Memory::default();
        assert_eq!(
            evaluate(&tokens, 0, &mut mem, &mut TestIo).unwrap().0,
            Value::Num(1.0)
        );
        mem.set_number("X".into(), 4.0);
        assert_eq!(
            evaluate(&tokens, 0, &mut mem, &mut TestIo).unwrap().0,
            Value::Num(5.0)
        );
        let tokens = tokenize("A$").unwrap();
        assert_eq!(
            evaluate(&tokens, 0, &mut mem, &mut TestIo).unwrap().0,
            Value::Str(String::new())
        );
    }

    #[test]
    fn machine_functions() {
        assert_eq!(eval("TIMER"), Ok(1.5));
        assert_eq!(eval("PEEK(258)"), Ok(2.0));
        assert_eq!(eval("PEEK(-1)"), Err(Error::IllegalQuantity));
        assert_eq!(eval("PEEK 1"), Err(Error::Syntax));
    }

    #[test]
    fn strings_and_their_functions() {
        assert_eq!(eval_str("\"AB\"+\"C\""), Ok("ABC".into()));
        assert_eq!(eval_str("MID$(\"HELLO\",2,3)"), Ok("ELL".into()));
        assert_eq!(eval_str("RIGHT$(\"HELLO\",9)"), Ok("HELLO".into()));
        assert_eq!(eval_str("STR$(5)"), Ok(" 5".into()));
        assert_eq!(eval("VAL(\" 12abc\")"), Ok(12.0));
        assert_eq!(eval("VAL(\"1E\")"), Ok(1.0));
        assert_eq!(eval("\"A\"<\"a\""), Ok(-1.0));
        assert_eq!(eval("\"A\"+1"), Err(Error::TypeMismatch));
        assert_eq!(eval("LEFT$(\"A\")"), Err(Error::Syntax));
    }

    #[test]
    fn address_and_byte_limits() {
        assert_eq!(to_address(0.0), Ok(0));
        assert_eq!(
            to_address(9_007_199_254_740_991.0),
            Ok(9_007_199_254_740_991)
        );
        assert_eq!(
            to_address(9_007_199_254_740_992.0),
            Err(Error::IllegalQuantity)
        );
        assert_eq!(to_address(-1.0), Err(Error::IllegalQuantity));
        assert_eq!(to_address(0.5), Err(Error::IllegalQuantity));
        assert_eq!(to_byte(255.0), Ok(255));
        assert_eq!(to_byte(256.0), Err(Error::IllegalQuantity));
        assert_eq!(to_byte(-1.0), Err(Error::IllegalQuantity));
    }
}
