//! `data`: the value type and the JSON, CSV and TSV parsers and emitters of the native shell `ish` (SHELL-SPEC sections 4
//! and 6; step SH1). Pure: `no_std` + `alloc`, no unsafe, no I/O.
//!
//! A [`Value`] is null, bool, number, string, list or map (closures and handles belong to the evaluator, which wraps this
//! type). Rules from SHELL-SPEC section 4: strings are UTF-8; numbers are 64-bit integers or **exact decimals** (never a
//! double, overflow is an error); maps keep insertion order, have string keys and a duplicate key is an error; values are
//! immutable (`set_in` and `del_in` build new values). Every parser is bounded by [`Limits`] and a limit hit is an error
//! value, never a panic.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod access;
mod csv;
mod json;
mod map;
pub mod number;

pub use access::{del_in, get, get_or, has, keys, len, require, set_in, values, Key, PathError};
pub use csv::{from_csv, from_tsv, to_csv, to_tsv, TableError, TableErrorKind, TableOptions};
pub use json::{from_json, to_json, to_json_pretty, Error, ErrorKind};
pub use map::{DuplicateKey, Map};
pub use number::Number;

use alloc::string::String;
use alloc::vec::Vec;

/// A shell data value (without closures and handles).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Num(Number),
    Str(String),
    List(Vec<Value>),
    Map(Map),
}

impl Value {
    pub fn str(s: &str) -> Value {
        Value::Str(String::from(s))
    }

    pub fn int(i: i64) -> Value {
        Value::Num(Number::Int(i))
    }

    /// The name of the value's kind, as the shell's diagnostics say it.
    pub fn kind(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Num(_) => "number",
            Value::Str(_) => "string",
            Value::List(_) => "list",
            Value::Map(_) => "map",
        }
    }

    /// Nesting depth: 0 for a scalar, 1 for a list or map of scalars, and so on.
    pub fn depth(&self) -> usize {
        match self {
            Value::List(l) => 1 + l.iter().map(Value::depth).max().unwrap_or(0),
            Value::Map(m) => 1 + m.iter().map(|(_, v)| v.depth()).max().unwrap_or(0),
            _ => 0,
        }
    }
}

/// The bounds every parser and emitter works within. Input from a file or a network is untrusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Most levels of list and map nesting.
    pub max_depth: usize,
    /// Most input bytes.
    pub max_bytes: usize,
    /// Most values (scalars, lists and maps all count) in one document.
    pub max_nodes: usize,
}

impl Limits {
    pub const DEFAULT: Limits = Limits {
        max_depth: 128,
        max_bytes: 16 * 1024 * 1024,
        max_nodes: 1 << 20,
    };
}

impl Default for Limits {
    fn default() -> Limits {
        Limits::DEFAULT
    }
}
