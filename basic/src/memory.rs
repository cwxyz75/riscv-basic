//! Everything an expression can read or change: the variables, and the random
//! number generator.

use crate::rng::Rng;
use alloc::collections::BTreeMap;
use alloc::string::String;

#[derive(Debug)]
pub(crate) struct Memory {
    numbers: BTreeMap<String, f64>,
    rng: Rng,
}

impl Default for Memory {
    fn default() -> Self {
        Memory {
            numbers: BTreeMap::new(),
            rng: Rng::new(),
        }
    }
}

impl Memory {
    /// A numeric variable; one never assigned is `0`.
    pub(crate) fn number(&self, name: &str) -> f64 {
        self.numbers.get(name).copied().unwrap_or(0.0)
    }

    pub(crate) fn set_number(&mut self, name: String, value: f64) {
        self.numbers.insert(name, value);
    }

    /// Forget all variables (`RUN`). The random sequence carries on.
    pub(crate) fn clear_variables(&mut self) {
        self.numbers.clear();
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
