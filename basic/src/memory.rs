//! Everything an expression can read or change: the variables, and the random
//! number generator.

use crate::error::Error;
use crate::rng::Rng;
use crate::value::Value;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

/// All arrays together may hold this many elements (see "Arrays" in `BASIC.md`).
pub(crate) const MAX_ARRAY_ELEMENTS: usize = 40_000;

/// Bound used for each dimension of an array that was never `DIM`ed.
const DEFAULT_BOUND: usize = 10;

#[derive(Debug)]
enum Cells {
    Num(Vec<f64>),
    Str(Vec<String>),
}

#[derive(Debug)]
struct Array {
    /// Size of each dimension (its bound plus one).
    dims: Vec<usize>,
    cells: Cells,
}

impl Array {
    /// Position of the element at `subs` in row-major order.
    fn offset(&self, subs: &[f64]) -> Result<usize, Error> {
        if subs.len() != self.dims.len() {
            return Err(Error::BadSubscript);
        }
        let mut offset = 0;
        for (&size, &sub) in self.dims.iter().zip(subs) {
            let index = libm::trunc(sub);
            if index < 0.0 || index >= size as f64 {
                return Err(Error::BadSubscript);
            }
            offset = offset * size + index as usize;
        }
        Ok(offset)
    }
}

#[derive(Debug)]
pub(crate) struct Memory {
    numbers: BTreeMap<String, f64>,
    strings: BTreeMap<String, String>,
    arrays: BTreeMap<String, Array>,
    /// Elements held by all arrays, against `MAX_ARRAY_ELEMENTS`.
    array_elements: usize,
    rng: Rng,
}

impl Default for Memory {
    fn default() -> Self {
        Memory {
            numbers: BTreeMap::new(),
            strings: BTreeMap::new(),
            arrays: BTreeMap::new(),
            array_elements: 0,
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

    /// Forget all variables and arrays (`RUN`). The random sequence carries on.
    pub(crate) fn clear_variables(&mut self) {
        self.numbers.clear();
        self.strings.clear();
        self.arrays.clear();
        self.array_elements = 0;
    }

    /// `DIM name(bounds...)`: create an array. `bounds` are the declared upper
    /// subscripts (so each dimension holds bound + 1 elements).
    pub(crate) fn dim(&mut self, name: String, bounds: &[f64]) -> Result<(), Error> {
        if self.arrays.contains_key(&name) {
            return Err(Error::RedimdArray);
        }
        let mut sizes = Vec::with_capacity(bounds.len());
        for &bound in bounds {
            if bound < 0.0 {
                return Err(Error::IllegalQuantity);
            }
            // Anything this big is over the element limit anyway.
            let bound = libm::trunc(bound).min(MAX_ARRAY_ELEMENTS as f64) as usize;
            sizes.push(bound + 1);
        }
        self.create_array(name, sizes)
    }

    fn create_array(&mut self, name: String, sizes: Vec<usize>) -> Result<(), Error> {
        let mut total: usize = 1;
        for &size in &sizes {
            total = total.checked_mul(size).ok_or(Error::OutOfMemory)?;
        }
        if total > MAX_ARRAY_ELEMENTS - self.array_elements.min(MAX_ARRAY_ELEMENTS) {
            return Err(Error::OutOfMemory);
        }
        let cells = if is_string_name(&name) {
            Cells::Str(alloc::vec![String::new(); total])
        } else {
            Cells::Num(alloc::vec![0.0; total])
        };
        self.array_elements += total;
        self.arrays.insert(name, Array { dims: sizes, cells });
        Ok(())
    }

    /// The array `name`, created on the spot with the default size if it was
    /// never dimensioned.
    fn array_or_default(&mut self, name: &str, dimensions: usize) -> Result<&mut Array, Error> {
        if !self.arrays.contains_key(name) {
            self.create_array(
                String::from(name),
                alloc::vec![DEFAULT_BOUND + 1; dimensions],
            )?;
        }
        self.arrays.get_mut(name).ok_or(Error::BadSubscript)
    }

    /// Read `name(subs...)`.
    pub(crate) fn element(&mut self, name: &str, subs: &[f64]) -> Result<Value, Error> {
        let array = self.array_or_default(name, subs.len())?;
        let offset = array.offset(subs)?;
        Ok(match &array.cells {
            Cells::Num(cells) => Value::Num(cells[offset]),
            Cells::Str(cells) => Value::Str(cells[offset].clone()),
        })
    }

    /// Store into `name(subs...)`; the value's type must match the name's.
    pub(crate) fn set_element(
        &mut self,
        name: &str,
        subs: &[f64],
        value: Value,
    ) -> Result<(), Error> {
        let array = self.array_or_default(name, subs.len())?;
        let offset = array.offset(subs)?;
        match (&mut array.cells, value) {
            (Cells::Num(cells), Value::Num(n)) => cells[offset] = n,
            (Cells::Str(cells), Value::Str(s)) => cells[offset] = s,
            _ => return Err(Error::TypeMismatch),
        }
        Ok(())
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
