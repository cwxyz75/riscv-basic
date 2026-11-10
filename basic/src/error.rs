/// A BASIC error, reported to the user as `?<NAME> ERROR` (see `BASIC.md`).
///
/// User input must never cause a Rust panic: every failure becomes one of
/// these. More variants arrive as the language grows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Error {
    Syntax,
    DivisionByZero,
    Overflow,
    IllegalQuantity,
}

impl Error {
    /// The classic upper-case name, without the leading `?` or trailing `ERROR`.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Error::Syntax => "SYNTAX",
            Error::DivisionByZero => "DIVISION BY ZERO",
            Error::Overflow => "OVERFLOW",
            Error::IllegalQuantity => "ILLEGAL QUANTITY",
        }
    }
}
