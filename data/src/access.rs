//! The access primitives (SHELL-SPEC section 6): `get_or`, `require`, `set_in`, `del_in`, `keys`, `values`, `len`, `has`.
//! A path is a list of [`Key`]s. A name selects a map entry; an index selects a list element, and on a map it means the key
//! spelled as that decimal number. Values are immutable: updates build new values.

use crate::Value;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Name(String),
    Index(usize),
}

impl Key {
    pub fn name(s: &str) -> Key {
        Key::Name(String::from(s))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathError {
    /// The key or index at this path position is not there (the shell's `missing-key`).
    Missing(usize),
    /// The value at this path position is not a list or map, or a name was used on a list.
    NotContainer(usize),
}

fn step<'a>(v: &'a Value, k: &Key) -> Result<&'a Value, Option<()>> {
    // Err(None): wrong container for the key; Err(Some): missing
    match (v, k) {
        (Value::Map(m), Key::Name(n)) => m.get(n).ok_or(Some(())),
        (Value::Map(m), Key::Index(i)) => m.get(&i.to_string()).ok_or(Some(())),
        (Value::List(l), Key::Index(i)) => l.get(*i).ok_or(Some(())),
        _ => Err(None),
    }
}

/// The value at `path`, or the reason there is none.
pub fn require<'a>(v: &'a Value, path: &[Key]) -> Result<&'a Value, PathError> {
    let mut cur = v;
    for (n, k) in path.iter().enumerate() {
        cur = step(cur, k).map_err(|e| match e {
            Some(()) => PathError::Missing(n),
            None => PathError::NotContainer(n),
        })?;
    }
    Ok(cur)
}

pub fn get<'a>(v: &'a Value, path: &[Key]) -> Option<&'a Value> {
    require(v, path).ok()
}

/// The value at `path`, or `default` when it is missing (a path through a non-container is still an error).
pub fn get_or(v: &Value, path: &[Key], default: Value) -> Result<Value, PathError> {
    match require(v, path) {
        Ok(x) => Ok(x.clone()),
        Err(PathError::Missing(_)) => Ok(default),
        Err(e) => Err(e),
    }
}

pub fn has(v: &Value, path: &[Key]) -> bool {
    require(v, path).is_ok()
}

/// `v` with the value at `path` replaced (a map entry that is not there yet is added at the end; a list index must exist).
pub fn set_in(v: &Value, path: &[Key], new: Value) -> Result<Value, PathError> {
    fn go(v: &Value, path: &[Key], at: usize, new: Value) -> Result<Value, PathError> {
        let Some((k, rest)) = path.split_first() else {
            return Ok(new);
        };
        match (v, k) {
            (Value::Map(m), Key::Name(_) | Key::Index(_)) => {
                let name = match k {
                    Key::Name(n) => n.clone(),
                    Key::Index(i) => i.to_string(),
                };
                if rest.is_empty() {
                    return Ok(Value::Map(m.with(&name, new)));
                }
                let child = m.get(&name).ok_or(PathError::Missing(at))?;
                Ok(Value::Map(m.with(&name, go(child, rest, at + 1, new)?)))
            }
            (Value::List(l), Key::Index(i)) => {
                let old = l.get(*i).ok_or(PathError::Missing(at))?;
                let mut out = l.clone();
                out[*i] = go(old, rest, at + 1, new)?;
                Ok(Value::List(out))
            }
            _ => Err(PathError::NotContainer(at)),
        }
    }
    go(v, path, 0, new)
}

/// `v` without the entry or element at `path` (an error if it is not there).
pub fn del_in(v: &Value, path: &[Key]) -> Result<Value, PathError> {
    fn go(v: &Value, path: &[Key], at: usize) -> Result<Value, PathError> {
        let Some((k, rest)) = path.split_first() else {
            return Err(PathError::Missing(at));
        };
        match (v, k) {
            (Value::Map(m), Key::Name(_) | Key::Index(_)) => {
                let name = match k {
                    Key::Name(n) => n.clone(),
                    Key::Index(i) => i.to_string(),
                };
                let child = m.get(&name).ok_or(PathError::Missing(at))?;
                if rest.is_empty() {
                    return Ok(Value::Map(m.without(&name)));
                }
                Ok(Value::Map(m.with(&name, go(child, rest, at + 1)?)))
            }
            (Value::List(l), Key::Index(i)) => {
                let old = l.get(*i).ok_or(PathError::Missing(at))?;
                let mut out = l.clone();
                if rest.is_empty() {
                    out.remove(*i);
                } else {
                    out[*i] = go(old, rest, at + 1)?;
                }
                Ok(Value::List(out))
            }
            _ => Err(PathError::NotContainer(at)),
        }
    }
    go(v, path, 0)
}

/// The keys of a map in insertion order (`None` for anything else).
pub fn keys(v: &Value) -> Option<Vec<String>> {
    match v {
        Value::Map(m) => Some(m.keys().map(String::from).collect()),
        _ => None,
    }
}

/// The values of a map in insertion order, or the elements of a list.
pub fn values(v: &Value) -> Option<Vec<Value>> {
    match v {
        Value::Map(m) => Some(m.iter().map(|(_, x)| x.clone()).collect()),
        Value::List(l) => Some(l.clone()),
        _ => None,
    }
}

/// Elements of a list, entries of a map, characters (Unicode scalar values) of a string.
pub fn len(v: &Value) -> Option<usize> {
    match v {
        Value::List(l) => Some(l.len()),
        Value::Map(m) => Some(m.len()),
        Value::Str(s) => Some(s.chars().count()),
        _ => None,
    }
}
