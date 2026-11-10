//! Everything an expression can read or change: the variables, and the random
//! number generator.

use crate::error::Error;
use crate::rng::Rng;
use crate::value::Value;
use alloc::collections::BTreeMap;
use alloc::string::String;

#[derive(Debug)]
pub(crate) struct Memory {
    numbers: BTreeMap<String, f64>,
    strings: BTreeMap<String, String>,
    rng: Rng,
}

impl Default for Memory {
    fn default() -> Self {
        Memory {
            numbers: BTreeMap::new(),
            strings: BTreeMap::new(),
            rng: Rng::new(),
        }
    }
}

/// A name ending in `$` is a string variable.
pub(crate) fn is_string_name(name: &str) -> bool {
    name.ends_with('$')
}

impl Memory {
    /// A numeric variable; one never assigned is `0`.
    pub(crate) fn number(&self, name: &str) -> f64 {
        self.numbers.get(name).copied().unwrap_or(0.0)
    }

    pub(crate) fn set_number(&mut self, name: String, value: f64) {
        self.numbers.insert(name, value);
    }

    /// The value of a variable of either type (an unset string is `""`).
    pub(crate) fn get(&self, name: &str) -> Value {
        if is_string_name(name) {
            Value::Str(self.strings.get(name).cloned().unwrap_or_default())
        } else {
            Value::Num(self.number(name))
        }
    }

    /// Assign to a variable; the value's type must match the name's.
    pub(crate) fn assign(&mut self, name: String, value: Value) -> Result<(), Error> {
        match (is_string_name(&name), value) {
            (false, Value::Num(n)) => {
                self.numbers.insert(name, n);
                Ok(())
            }
            (true, Value::Str(s)) => {
                self.strings.insert(name, s);
                Ok(())
            }
            _ => Err(Error::TypeMismatch),
        }
    }

    /// Forget all variables (`RUN`). The random sequence carries on.
    pub(crate) fn clear_variables(&mut self) {
        self.numbers.clear();
        self.strings.clear();
    }

    /// `RND(x)`: next number for `x > 0`, the previous one again for `x = 0`,
    /// and for `x < 0` reseed from `x` and return the new sequence's first.
    pub(crate) fn rnd(&mut self, x: f64) -> f64 {
        if x > 0.0 {
            self.rng.generate()
        } else if x == 0.0 {
            self.rng.last()
        } else {
            self.rng.reseed(x);
            self.rng.generate()
        }
    }
}
