//! The two kinds of value a BASIC expression can have.

use crate::error::Error;
use alloc::string::String;

/// Longest string BASIC will make (see "Strings" in `BASIC.md`).
pub(crate) const MAX_STRING: usize = 255;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    Num(f64),
    Str(String),
}

impl Value {
    /// The number, or `?TYPE MISMATCH` for a string.
    pub(crate) fn num(self) -> Result<f64, Error> {
        match self {
            Value::Num(n) => Ok(n),
            Value::Str(_) => Err(Error::TypeMismatch),
        }
    }

    /// The string, or `?TYPE MISMATCH` for a number.
    pub(crate) fn string(self) -> Result<String, Error> {
        match self {
            Value::Str(s) => Ok(s),
            Value::Num(_) => Err(Error::TypeMismatch),
        }
    }
}

/// Reject a string longer than BASIC allows.
pub(crate) fn check_length(text: String) -> Result<String, Error> {
    if text.len() > MAX_STRING {
        Err(Error::StringTooLong)
    } else {
        Ok(text)
    }
}
