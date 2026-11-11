//! LOGO errors, with the wording specified in `LOGO.md`.

use crate::value::Value;
use alloc::format;
use alloc::string::String;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum LogoError {
    DontKnowHow(String),
    NoValue(String),
    NotEnoughInputs(String),
    DontSayWhatToDo(Value),
    DoesntLike(String, Value),
    DidntOutput(String, String),
    MissingBracket,
    UnexpectedBracket,
    MissingParen,
    UnexpectedParen,
    OnlyInProcedure,
    OutOfMemory,
    OutOfRange,
    Stopped,
    Disk(cartfs::Error),
    MissingEnd,
}

impl LogoError {
    /// The one-line message shown to the user.
    pub(crate) fn message(&self) -> String {
        match self {
            LogoError::DontKnowHow(name) => format!("I don't know how to {name}"),
            LogoError::NoValue(name) => format!("{name} has no value"),
            LogoError::NotEnoughInputs(name) => format!("Not enough inputs to {name}"),
            LogoError::DontSayWhatToDo(v) => {
                format!("You don't say what to do with {}", v.show_form())
            }
            LogoError::DoesntLike(name, v) => {
                format!("{name} doesn't like {} as input", v.show_form())
            }
            LogoError::DidntOutput(from, to) => format!("{from} didn't output to {to}"),
            LogoError::MissingBracket => String::from("Missing ]"),
            LogoError::UnexpectedBracket => String::from("Unexpected ]"),
            LogoError::MissingParen => String::from("Missing )"),
            LogoError::UnexpectedParen => String::from("Unexpected )"),
            LogoError::OnlyInProcedure => String::from("Can only do that inside a procedure"),
            LogoError::OutOfMemory => String::from("Out of memory"),
            LogoError::OutOfRange => String::from("Number out of range"),
            LogoError::Stopped => String::from("Stopped!"),
            LogoError::MissingEnd => String::from("Missing end"),
            LogoError::Disk(e) => String::from(match e {
                cartfs::Error::NoDevice => "No disk attached",
                cartfs::Error::NotFormatted => "Disk is not formatted",
                cartfs::Error::Device | cartfs::Error::Corrupt => "Disk error",
                cartfs::Error::NotFound => "File not found",
                cartfs::Error::TypeMismatch => "File is not a LOGO file",
                cartfs::Error::BadName => "Bad file name",
                cartfs::Error::Full | cartfs::Error::TooBig => "Disk full",
            }),
        }
    }
}
