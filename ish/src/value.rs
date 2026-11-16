//! Runtime values: the `data` value model with persistent (reference-counted, copy-on-write) lists and maps, plus
//! closures (SHELL-SPEC section 4). A closure cannot be serialized; neither can a value that holds one.

use crate::ast::ClosureDef;
use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;
use data::{Number, Value as Data};

/// A function value: code plus the variables it saw when it was made (captured by value; values are immutable, so this is safe).
#[derive(Debug)]
pub struct Closure {
    pub def: Rc<ClosureDef>,
    pub env: BTreeMap<String, Val>,
    /// The module (an imported file) the function was defined in: its sibling functions are found first.
    pub module: Option<Rc<str>>,
}

#[derive(Debug, Clone)]
pub enum Val {
    Null,
    Bool(bool),
    Num(Number),
    Str(Rc<str>),
    List(Rc<Vec<Val>>),
    Map(Rc<VMap>),
    Closure(Rc<Closure>),
    /// A background job (a handle, Q3): it can be stored and captured but not serialized.
    Job(Rc<RefCell<crate::job::JobState>>),
    /// What a call statement gives: no value. Using it as a value is an error that names the fix (N3).
    Void(Rc<NoValue>),
}

/// The absence of a value: which function ended with which call statement.
#[derive(Debug)]
pub struct NoValue {
    pub func: Option<Rc<str>>,
    pub call: Rc<str>,
}

/// An ordered map (insertion order, string keys) that is cheap to clone and update when uniquely held.
#[derive(Debug, Clone, Default)]
pub struct VMap {
    entries: Vec<(Rc<str>, Val)>,
    index: BTreeMap<Rc<str>, usize>,
}

impl VMap {
    pub fn new() -> VMap {
        VMap::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, k: &str) -> Option<&Val> {
        self.index.get(k).map(|&i| &self.entries[i].1)
    }

    /// Set a key: replaced in place, or appended.
    pub fn set(&mut self, k: &str, v: Val) {
        match self.index.get(k) {
            Some(&i) => self.entries[i].1 = v,
            None => {
                let key: Rc<str> = Rc::from(k);
                self.index.insert(key.clone(), self.entries.len());
                self.entries.push((key, v));
            }
        }
    }

    pub fn remove(&mut self, k: &str) -> bool {
        let Some(i) = self.index.remove(k) else {
            return false;
        };
        self.entries.remove(i);
        for idx in self.index.values_mut() {
            if *idx > i {
                *idx -= 1;
            }
        }
        true
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Val)> {
        self.entries.iter().map(|(k, v)| (&**k, v))
    }
}

impl Val {
    pub fn str(s: &str) -> Val {
        Val::Str(Rc::from(s))
    }

    pub fn string(s: String) -> Val {
        Val::Str(Rc::from(s))
    }

    pub fn int(i: i64) -> Val {
        Val::Num(Number::Int(i))
    }

    pub fn list(v: Vec<Val>) -> Val {
        Val::List(Rc::new(v))
    }

    pub fn map(m: VMap) -> Val {
        Val::Map(Rc::new(m))
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Val::Null => "null",
            Val::Bool(_) => "bool",
            Val::Num(_) => "number",
            Val::Str(_) => "string",
            Val::List(_) => "list",
            Val::Map(_) => "map",
            Val::Closure(_) => "closure",
            Val::Job(_) => "job",
            Val::Void(_) => "nothing",
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Val::Str(s) => Some(s),
            _ => None,
        }
    }

    /// Build a map from (key, value) pairs.
    pub fn record(pairs: Vec<(&str, Val)>) -> Val {
        let mut m = VMap::new();
        for (k, v) in pairs {
            m.set(k, v);
        }
        Val::map(m)
    }

    /// The text of a scalar when used as a word; `None` for null, lists, maps and closures.
    pub fn word(&self) -> Option<String> {
        match self {
            Val::Str(s) => Some(String::from(&**s)),
            Val::Num(n) => Some(n.format()),
            Val::Bool(b) => Some(String::from(if *b { "true" } else { "false" })),
            _ => None,
        }
    }

    /// Total structural equality: no coercion, numbers by value, maps ignore key order, closures by identity.
    pub fn equals(&self, o: &Val) -> bool {
        match (self, o) {
            (Val::Null, Val::Null) => true,
            (Val::Bool(a), Val::Bool(b)) => a == b,
            (Val::Num(a), Val::Num(b)) => a == b,
            (Val::Str(a), Val::Str(b)) => a == b,
            (Val::List(a), Val::List(b)) => {
                a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x.equals(y))
            }
            (Val::Map(a), Val::Map(b)) => {
                a.len() == b.len() && a.iter().all(|(k, v)| b.get(k).is_some_and(|w| v.equals(w)))
            }
            (Val::Closure(a), Val::Closure(b)) => Rc::ptr_eq(a, b),
            (Val::Job(a), Val::Job(b)) => Rc::ptr_eq(a, b),
            _ => false,
        }
    }

    /// Into the serializable model. `Err(kind)` names the first thing that cannot be serialized (a closure).
    pub fn to_data(&self) -> Result<Data, &'static str> {
        Ok(match self {
            Val::Null => Data::Null,
            Val::Bool(b) => Data::Bool(*b),
            Val::Num(n) => Data::Num(*n),
            Val::Str(s) => Data::Str(String::from(&**s)),
            Val::List(l) => Data::List(l.iter().map(Val::to_data).collect::<Result<_, _>>()?),
            Val::Map(m) => {
                let mut out = data::Map::new();
                for (k, v) in m.iter() {
                    let _ = out.insert(String::from(k), v.to_data()?);
                }
                Data::Map(out)
            }
            Val::Closure(_) => return Err("closure"),
            Val::Job(_) => return Err("job"),
            Val::Void(_) => return Err("nothing"),
        })
    }

    pub fn from_data(d: &Data) -> Val {
        match d {
            Data::Null => Val::Null,
            Data::Bool(b) => Val::Bool(*b),
            Data::Num(n) => Val::Num(*n),
            Data::Str(s) => Val::str(s),
            Data::List(l) => Val::list(l.iter().map(Val::from_data).collect()),
            Data::Map(m) => {
                let mut out = VMap::new();
                for (k, v) in m.iter() {
                    out.set(k, Val::from_data(v));
                }
                Val::map(out)
            }
        }
    }
}
