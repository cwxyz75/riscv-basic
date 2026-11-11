/// A BASIC error, reported to the user as `?<NAME> ERROR` (see `BASIC.md`).
///
/// User input must never cause a Rust panic: every failure becomes one of
/// these. More variants arrive as the language grows.
// The classic error names happen to share words (DISK FULL, DISK ERROR, ...).
#[allow(clippy::enum_variant_names)]
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
    DeviceNotPresent,
    DiskNotFormatted,
    DiskError,
    FileNotFound,
    FileTypeMismatch,
    DiskFull,
    BadFileName,
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
            Error::DeviceNotPresent => "DEVICE NOT PRESENT",
            Error::DiskNotFormatted => "DISK NOT FORMATTED",
            Error::DiskError => "DISK ERROR",
            Error::FileNotFound => "FILE NOT FOUND",
            Error::FileTypeMismatch => "FILE TYPE MISMATCH",
            Error::DiskFull => "DISK FULL",
            Error::BadFileName => "BAD FILE NAME",
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

impl From<cartfs::Error> for Error {
    fn from(error: cartfs::Error) -> Self {
        use cartfs::Error as D;
        match error {
            D::NoDevice => Error::DeviceNotPresent,
            D::NotFormatted => Error::DiskNotFormatted,
            D::Device | D::Corrupt => Error::DiskError,
            D::NotFound => Error::FileNotFound,
            D::TypeMismatch => Error::FileTypeMismatch,
            D::BadName => Error::BadFileName,
            D::Full | D::TooBig => Error::DiskFull,
        }
    }
}
