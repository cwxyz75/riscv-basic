//! Expression evaluation, directly over a line's token bytes (no AST: the
//! classic approach, and a loop body is simply re-evaluated each pass).
//! Precedence and fault rules are in `BASIC.md`.

use crate::error::Error;
use crate::io::Io;
use crate::token::{keyword, token};
use alloc::collections::BTreeMap;
use alloc::string::String;

/// Numeric variables by (upper-case) name.
pub(crate) type Variables = BTreeMap<String, f64>;

/// Evaluate one expression starting at `pos` in `src`. Returns the value and
/// the position just after the expression (trailing spaces not consumed).
/// `io` is used by the machine functions (`PEEK`, `TIMER`).
pub(crate) fn evaluate<I: Io>(
    src: &[u8],
    pos: usize,
    vars: &Variables,
    io: &mut I,
) -> Result<(f64, usize), Error> {
    let mut parser = Parser {
        src,
        pos,
        vars,
        io,
        and: token("AND"),
        or: token("OR"),
        not: token("NOT"),
    };
    let value = parser.or_expr()?;
    Ok((value, parser.pos))
}

/// Read a variable name at `*pos` (skipping leading spaces): a letter then
/// letters and digits. `None` if there is no name there. A `$` suffix is
/// rejected as a syntax error until strings exist.
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
        return Err(Error::Syntax);
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

/// Truncate toward zero to a 32-bit signed integer for the logic operators.
fn to_i32(value: f64) -> Result<i32, Error> {
    let truncated = libm::trunc(value);
    if (-2_147_483_648.0..=2_147_483_647.0).contains(&truncated) {
        Ok(truncated as i32)
    } else {
        Err(Error::IllegalQuantity)
    }
}

struct Parser<'a, I: Io> {
    src: &'a [u8],
    pos: usize,
    vars: &'a Variables,
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

    fn or_expr(&mut self) -> Result<f64, Error> {
        let mut left = self.and_expr()?;
        while self.peek() == Some(self.or) {
            self.pos += 1;
            let right = self.and_expr()?;
            left = f64::from(to_i32(left)? | to_i32(right)?);
        }
        Ok(left)
    }

    fn and_expr(&mut self) -> Result<f64, Error> {
        let mut left = self.not_expr()?;
        while self.peek() == Some(self.and) {
            self.pos += 1;
            let right = self.not_expr()?;
            left = f64::from(to_i32(left)? & to_i32(right)?);
        }
        Ok(left)
    }

    fn not_expr(&mut self) -> Result<f64, Error> {
        if self.peek() == Some(self.not) {
            self.pos += 1;
            let operand = self.not_expr()?;
            return Ok(f64::from(!to_i32(operand)?));
        }
        self.comparison()
    }

    fn comparison(&mut self) -> Result<f64, Error> {
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
            let holds =
                (less && left < right) || (equal && left == right) || (greater && left > right);
            left = if holds { -1.0 } else { 0.0 };
        }
    }

    fn additive(&mut self) -> Result<f64, Error> {
        let mut left = self.term()?;
        loop {
            match self.peek() {
                Some(b'+') => {
                    self.pos += 1;
                    left = check(left + self.term()?)?;
                }
                Some(b'-') => {
                    self.pos += 1;
                    left = check(left - self.term()?)?;
                }
                _ => return Ok(left),
            }
        }
    }

    fn term(&mut self) -> Result<f64, Error> {
        let mut left = self.unary()?;
        loop {
            match self.peek() {
                Some(b'*') => {
                    self.pos += 1;
                    left = check(left * self.unary()?)?;
                }
                Some(b'/') => {
                    self.pos += 1;
                    let right = self.unary()?;
                    if right == 0.0 {
                        return Err(Error::DivisionByZero);
                    }
                    left = check(left / right)?;
                }
                _ => return Ok(left),
            }
        }
    }

    /// Unary minus binds looser than `^`, so `-2^2` is `-4`.
    fn unary(&mut self) -> Result<f64, Error> {
        match self.peek() {
            Some(b'-') => {
                self.pos += 1;
                Ok(-self.unary()?)
            }
            Some(b'+') => {
                self.pos += 1;
                self.unary()
            }
            _ => self.power(),
        }
    }

    /// `^` groups left to right, so `2^3^2` is `64`.
    fn power(&mut self) -> Result<f64, Error> {
        let mut base = self.primary()?;
        while self.peek() == Some(b'^') {
            self.pos += 1;
            let exponent = self.signed_primary()?;
            base = check(libm::pow(base, exponent))?;
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
            _ => self.primary(),
        }
    }

    fn primary(&mut self) -> Result<f64, Error> {
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
            Some(b) if b.is_ascii_digit() || b == b'.' => self.number(),
            Some(b) if b.is_ascii_alphabetic() => {
                let name = parse_name(self.src, &mut self.pos)?.ok_or(Error::Syntax)?;
                Ok(self.vars.get(&name).copied().unwrap_or(0.0))
            }
            Some(b) => match keyword(b) {
                Some("PEEK") => {
                    self.pos += 1;
                    let address = self.parenthesised()?;
                    Ok(f64::from(self.io.peek(to_address(address)?)))
                }
                Some("TIMER") => {
                    self.pos += 1;
                    Ok(self.io.millis() as f64 / 1000.0)
                }
                _ => Err(Error::Syntax),
            },
            None => Err(Error::Syntax),
        }
    }

    /// `( expression )`, as the argument of a function.
    fn parenthesised(&mut self) -> Result<f64, Error> {
        if self.peek() != Some(b'(') {
            return Err(Error::Syntax);
        }
        self.pos += 1;
        let value = self.or_expr()?;
        if self.peek() != Some(b')') {
            return Err(Error::Syntax);
        }
        self.pos += 1;
        Ok(value)
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

    fn eval(text: &str) -> Result<f64, Error> {
        let tokens = tokenize(text).unwrap();
        let vars = Variables::new();
        let (value, end) = evaluate(&tokens, 0, &vars, &mut TestIo)?;
        assert!(tokens[end..].iter().all(|&b| b == b' '), "trailing input");
        Ok(value)
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
    fn variables_default_to_zero_and_reject_string_names() {
        let tokens = tokenize("X+1").unwrap();
        let mut vars = Variables::new();
        assert_eq!(evaluate(&tokens, 0, &vars, &mut TestIo).unwrap().0, 1.0);
        vars.insert("X".into(), 4.0);
        assert_eq!(evaluate(&tokens, 0, &vars, &mut TestIo).unwrap().0, 5.0);
        let tokens = tokenize("A$").unwrap();
        assert_eq!(evaluate(&tokens, 0, &vars, &mut TestIo), Err(Error::Syntax));
    }

    #[test]
    fn machine_functions() {
        assert_eq!(eval("TIMER"), Ok(1.5));
        assert_eq!(eval("PEEK(258)"), Ok(2.0));
        assert_eq!(eval("PEEK(-1)"), Err(Error::IllegalQuantity));
        assert_eq!(eval("PEEK 1"), Err(Error::Syntax));
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
