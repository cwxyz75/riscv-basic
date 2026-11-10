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
    UndefdStatement,
    NextWithoutFor,
    OutOfMemory,
    ReturnWithoutGosub,
    TypeMismatch,
    StringTooLong,
    BadSubscript,
    RedimdArray,
    OutOfData,
}

impl Error {
    /// The classic upper-case name, without the leading `?` or trailing `ERROR`.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Error::Syntax => "SYNTAX",
            Error::DivisionByZero => "DIVISION BY ZERO",
            Error::Overflow => "OVERFLOW",
            Error::IllegalQuantity => "ILLEGAL QUANTITY",
            Error::UndefdStatement => "UNDEF'D STATEMENT",
            Error::NextWithoutFor => "NEXT WITHOUT FOR",
            Error::OutOfMemory => "OUT OF MEMORY",
            Error::ReturnWithoutGosub => "RETURN WITHOUT GOSUB",
            Error::TypeMismatch => "TYPE MISMATCH",
            Error::StringTooLong => "STRING TOO LONG",
            Error::BadSubscript => "BAD SUBSCRIPT",
            Error::RedimdArray => "REDIM'D ARRAY",
            Error::OutOfData => "OUT OF DATA",
        }
    }
}

/// An error together with the program line it happened on (`None` when it
/// happened in immediate mode), as reported by `?<NAME> ERROR IN <line>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Fault {
    pub(crate) error: Error,
    pub(crate) line: Option<u16>,
}

impl From<Error> for Fault {
    fn from(error: Error) -> Self {
        Fault { error, line: None }
    }
}
