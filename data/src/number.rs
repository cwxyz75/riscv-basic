//! Numbers (SHELL-SPEC section 4 rule 5 and section 7): 64-bit integers and exact decimals. There is no double. A decimal
//! is `mant / 10^scale` with at most [`MAX_DIGITS`] digits and a scale of at most [`MAX_DIGITS`]; anything beyond is an
//! error, never a rounding. Numbers are kept canonical: no trailing zero in the fraction, and an integer that fits `i64`
//! is always `Int`, so `1.0`, `1e0` and `1` are the same number and `-0` is `0`.

use alloc::string::String;

/// The most digits (and the largest scale) of a decimal: what an `i128` holds.
pub const MAX_DIGITS: u32 = 38;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumError {
    /// More digits, or a larger scale, than a decimal holds.
    TooPrecise,
    /// Not a number.
    Syntax,
}

/// An exact decimal that is not an `i64`: `mant / 10^scale`, canonical (see the module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decimal {
    mant: i128,
    scale: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Number {
    Int(i64),
    Dec(Decimal),
}

const LIMIT: i128 = 10i128.pow(MAX_DIGITS); // exclusive bound on |mant|

impl Decimal {
    pub fn mantissa(&self) -> i128 {
        self.mant
    }

    pub fn scale(&self) -> u32 {
        u32::from(self.scale)
    }
}

impl Number {
    /// `mant / 10^scale`, made canonical. `Err` when the result does not fit (more than 38 digits, or scale over 38).
    pub fn from_parts(mut mant: i128, mut scale: u32) -> Result<Number, NumError> {
        if mant == 0 {
            return Ok(Number::Int(0));
        }
        while scale > 0 && mant % 10 == 0 {
            mant /= 10;
            scale -= 1;
        }
        if mant <= -LIMIT || mant >= LIMIT || scale > MAX_DIGITS {
            return Err(NumError::TooPrecise);
        }
        if scale == 0 {
            if let Ok(i) = i64::try_from(mant) {
                return Ok(Number::Int(i));
            }
        }
        Ok(Number::Dec(Decimal {
            mant,
            scale: scale as u8,
        }))
    }

    /// As (mantissa, scale).
    pub fn parts(&self) -> (i128, u32) {
        match self {
            Number::Int(i) => (i128::from(*i), 0),
            Number::Dec(d) => (d.mant, u32::from(d.scale)),
        }
    }

    pub fn is_integer(&self) -> bool {
        self.parts().1 == 0
    }

    /// Parse JSON number syntax (`-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?`), exactly. The whole of `s` must be the number.
    pub fn parse(s: &[u8]) -> Result<Number, NumError> {
        let mut i = 0;
        let neg = s.first() == Some(&b'-');
        if neg {
            i += 1;
        }
        let int_start = i;
        match s.get(i) {
            Some(b'0') => i += 1,
            Some(b'1'..=b'9') => {
                while s.get(i).is_some_and(u8::is_ascii_digit) {
                    i += 1;
                }
            }
            _ => return Err(NumError::Syntax),
        }
        let int_digits = &s[int_start..i];
        let mut frac_digits: &[u8] = &[];
        if s.get(i) == Some(&b'.') {
            i += 1;
            let f = i;
            while s.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
            if i == f {
                return Err(NumError::Syntax);
            }
            frac_digits = &s[f..i];
        }
        let mut exp: i64 = 0;
        if matches!(s.get(i), Some(b'e' | b'E')) {
            i += 1;
            let eneg = match s.get(i) {
                Some(b'-') => {
                    i += 1;
                    true
                }
                Some(b'+') => {
                    i += 1;
                    false
                }
                _ => false,
            };
            let e = i;
            while let Some(d) = s.get(i).filter(|c| c.is_ascii_digit()) {
                exp = (exp * 10 + i64::from(d - b'0')).min(1_000_000);
                i += 1;
            }
            if i == e {
                return Err(NumError::Syntax);
            }
            if eneg {
                exp = -exp;
            }
        }
        if i != s.len() {
            return Err(NumError::Syntax);
        }
        // digits without the point, leading zeros dropped, trailing zeros dropped (each lowers the scale)
        let mut scale: i64 = frac_digits.len() as i64 - exp;
        let all = int_digits.iter().chain(frac_digits.iter()).copied();
        let mut digits: alloc::vec::Vec<u8> = all.skip_while(|&d| d == b'0').collect();
        while digits.last() == Some(&b'0') {
            digits.pop();
            scale -= 1;
        }
        if digits.is_empty() {
            return Ok(Number::Int(0));
        }
        if digits.len() as i64 > i64::from(MAX_DIGITS) {
            return Err(NumError::TooPrecise);
        }
        let mut mant: i128 = 0;
        for d in &digits {
            mant = mant * 10 + i128::from(d - b'0');
        }
        if scale < 0 {
            if digits.len() as i64 - scale > i64::from(MAX_DIGITS) {
                return Err(NumError::TooPrecise);
            }
            mant *= 10i128.pow((-scale) as u32);
            scale = 0;
        }
        if scale > i64::from(MAX_DIGITS) {
            return Err(NumError::TooPrecise);
        }
        if neg {
            mant = -mant;
        }
        Number::from_parts(mant, scale as u32)
    }

    /// Canonical text: plain digits, a point and fraction only when there is one, never an exponent. Parses back to the
    /// same number.
    pub fn format(&self) -> String {
        use alloc::string::ToString;
        match self {
            Number::Int(i) => i.to_string(),
            Number::Dec(d) => {
                let neg = d.mant < 0;
                let mut digits = d.mant.unsigned_abs().to_string();
                let scale = usize::from(d.scale);
                if scale > 0 {
                    while digits.len() <= scale {
                        digits.insert(0, '0');
                    }
                    digits.insert(digits.len() - scale, '.');
                }
                if neg {
                    digits.insert(0, '-');
                }
                digits
            }
        }
    }
}

impl core::fmt::Display for Number {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.format())
    }
}
