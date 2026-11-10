//! The tokenized program store: line number to token bytes, kept sorted.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

/// Highest legal line number (see `BASIC.md`).
pub(crate) const MAX_LINE: u16 = 63999;

#[derive(Debug, Default)]
pub(crate) struct Program {
    lines: BTreeMap<u16, Vec<u8>>,
}

impl Program {
    /// Store a line, replacing any existing line with the same number.
    pub(crate) fn set(&mut self, number: u16, tokens: Vec<u8>) {
        self.lines.insert(number, tokens);
    }

    /// Delete a line. Deleting a line that does not exist does nothing.
    pub(crate) fn delete(&mut self, number: u16) {
        self.lines.remove(&number);
    }

    /// Delete the whole program (`NEW`).
    pub(crate) fn clear(&mut self) {
        self.lines.clear();
    }

    /// Lines with `from <= number <= to`, in line-number order. An empty or
    /// reversed range yields nothing.
    pub(crate) fn range(&self, from: u16, to: u16) -> impl Iterator<Item = (u16, &[u8])> {
        self.lines
            .iter()
            .filter(move |(number, _)| (from..=to).contains(number))
            .map(|(number, tokens)| (*number, tokens.as_slice()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn lines_come_back_sorted_and_replaceable() {
        let mut p = Program::default();
        p.set(20, vec![1]);
        p.set(10, vec![2]);
        p.set(20, vec![3]);
        let all: Vec<_> = p.range(0, MAX_LINE).collect();
        assert_eq!(all, vec![(10, &[2u8][..]), (20, &[3u8][..])]);
    }

    #[test]
    fn range_is_inclusive_and_reversed_is_empty() {
        let mut p = Program::default();
        for n in [10, 20, 30] {
            p.set(n, vec![]);
        }
        assert_eq!(p.range(20, 30).count(), 2);
        assert_eq!(p.range(30, 10).count(), 0);
    }

    #[test]
    fn delete_missing_line_is_harmless_and_clear_empties() {
        let mut p = Program::default();
        p.set(10, vec![]);
        p.delete(99);
        assert_eq!(p.range(0, MAX_LINE).count(), 1);
        p.clear();
        assert_eq!(p.range(0, MAX_LINE).count(), 0);
    }
}
