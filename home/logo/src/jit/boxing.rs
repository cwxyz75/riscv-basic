//! NaN-boxing and the value arena (layout in "Compilation to RV64" in `LOGO.md`).

use crate::error::LogoError;
use crate::value::Value;
use alloc::vec::Vec;

pub(crate) const TAG_WORD: u64 = 0xFFF9;
pub(crate) const TAG_LIST: u64 = 0xFFFA;
pub(crate) const TAG_NOTHING: u64 = 0xFFFB;

/// Most values one run's arena may hold.
const ARENA_LIMIT: usize = 262_144;

const PAYLOAD: u64 = (1 << 48) - 1;

/// "Nothing" produced by the instruction whose name is pool entry `producer`.
pub(crate) fn nothing(producer: usize) -> u64 {
    TAG_NOTHING << 48 | producer as u64
}

/// What a boxed word holds.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Unboxed {
    Value(Value),
    /// A command's non-result, with the pool index of the producer's name.
    Nothing(usize),
}

/// The arena for the words and lists of one run.
#[derive(Debug, Default)]
pub(crate) struct Boxes {
    values: Vec<Value>,
}

impl Boxes {
    pub(crate) fn new() -> Self {
        Boxes::default()
    }

    /// Box a value. Numbers are their own bits; words and lists go in the arena.
    pub(crate) fn put(&mut self, value: &Value) -> Result<u64, LogoError> {
        let tag = match value {
            Value::Num(n) if n.is_finite() => return Ok(n.to_bits()),
            Value::Num(_) => return Err(LogoError::OutOfRange),
            Value::Word(_) => TAG_WORD,
            Value::List(_) => TAG_LIST,
        };
        if self.values.len() >= ARENA_LIMIT {
            return Err(LogoError::OutOfMemory);
        }
        self.values.push(value.clone());
        Ok(tag << 48 | (self.values.len() - 1) as u64)
    }

    pub(crate) fn get(&self, bits: u64) -> Unboxed {
        match bits >> 48 {
            TAG_WORD | TAG_LIST => Unboxed::Value(self.values[(bits & PAYLOAD) as usize].clone()),
            TAG_NOTHING => Unboxed::Nothing((bits & PAYLOAD) as usize),
            _ => Unboxed::Value(Value::Num(f64::from_bits(bits))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;
    use alloc::vec;

    #[test]
    fn numbers_are_their_own_bits() {
        let mut boxes = Boxes::new();
        for n in [0.0, -0.0, 1.5, -2.5e300, 5e-324, f64::MAX, f64::MIN] {
            let bits = boxes.put(&Value::Num(n)).unwrap();
            assert_eq!(bits, n.to_bits());
            assert!(
                bits >> 48 < TAG_WORD,
                "a number must never look like a tag: {n}"
            );
            assert_eq!(boxes.get(bits), Unboxed::Value(Value::Num(n)));
        }
        assert!(boxes.values.is_empty(), "numbers do not use the arena");
    }

    #[test]
    fn words_and_lists_round_trip_through_the_arena() {
        let mut boxes = Boxes::new();
        let word = Value::Word(String::from("hello"));
        let list = Value::List(vec![Value::Num(1.0), word.clone()]);
        let (w, l) = (boxes.put(&word).unwrap(), boxes.put(&list).unwrap());
        assert_eq!(w >> 48, TAG_WORD);
        assert_eq!(l >> 48, TAG_LIST);
        assert_eq!(boxes.get(w), Unboxed::Value(word));
        assert_eq!(boxes.get(l), Unboxed::Value(list));
    }

    #[test]
    fn nothing_remembers_its_producer() {
        let boxes = Boxes::new();
        assert_eq!(nothing(7) >> 48, TAG_NOTHING);
        assert_eq!(boxes.get(nothing(7)), Unboxed::Nothing(7));
    }

    #[test]
    fn non_finite_numbers_are_refused_and_the_arena_is_bounded() {
        let mut boxes = Boxes::new();
        assert_eq!(boxes.put(&Value::Num(f64::NAN)), Err(LogoError::OutOfRange));
        assert_eq!(
            boxes.put(&Value::Num(f64::INFINITY)),
            Err(LogoError::OutOfRange)
        );
        let word = Value::Word(String::new());
        for _ in 0..ARENA_LIMIT {
            boxes.put(&word).unwrap();
        }
        assert_eq!(boxes.put(&word), Err(LogoError::OutOfMemory));
    }
}
