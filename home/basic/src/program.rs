//! The tokenized program store: line number to token bytes, kept sorted.

use crate::error::Error;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::ops::Bound;

/// Highest legal line number (see `BASIC.md`).
pub(crate) const MAX_LINE: u16 = 63999;

/// Size of the program area in bytes (see "Program memory" in `BASIC.md`).
pub(crate) const PROGRAM_AREA: usize = 32768;

/// Bytes of bookkeeping charged per stored line, on top of its tokens.
const LINE_OVERHEAD: usize = 5;

fn line_cost(tokens: &[u8]) -> usize {
    tokens.len() + LINE_OVERHEAD
}

#[derive(Debug, Default)]
pub(crate) struct Program {
    lines: BTreeMap<u16, Vec<u8>>,
    /// Bytes of the program area in use.
    used: usize,
}

impl Program {
    /// Store a line, replacing any existing line with the same number. If the
    /// result would not fit in the program area, nothing changes and the error
    /// is `OutOfMemory`.
    pub(crate) fn set(&mut self, number: u16, tokens: Vec<u8>) -> Result<(), Error> {
        let old_cost = self.lines.get(&number).map_or(0, |old| line_cost(old));
        let used = self.used - old_cost + line_cost(&tokens);
        if used > PROGRAM_AREA {
            return Err(Error::OutOfMemory);
        }
        self.used = used;
        self.lines.insert(number, tokens);
        Ok(())
    }

    /// Delete a line. Deleting a line that does not exist does nothing.
    pub(crate) fn delete(&mut self, number: u16) {
        if let Some(old) = self.lines.remove(&number) {
            self.used -= line_cost(&old);
        }
    }

    /// Delete the whole program (`NEW`).
    pub(crate) fn clear(&mut self) {
        self.lines.clear();
        self.used = 0;
    }

    /// Bytes of the program area still free.
    pub(crate) fn bytes_free(&self) -> usize {
        PROGRAM_AREA - self.used
    }

    /// The tokens of line `number`, if it exists.
    pub(crate) fn get(&self, number: u16) -> Option<&[u8]> {
        self.lines.get(&number).map(Vec::as_slice)
    }

    pub(crate) fn contains(&self, number: u16) -> bool {
        self.lines.contains_key(&number)
    }

    /// The lowest line number in the program.
    pub(crate) fn first(&self) -> Option<u16> {
        self.lines.keys().next().copied()
    }

    /// The lowest line number strictly greater than `number`.
    pub(crate) fn next_after(&self, number: u16) -> Option<u16> {
        self.lines
            .range((Bound::Excluded(number), Bound::Unbounded))
            .next()
            .map(|(n, _)| *n)
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
        p.set(20, vec![1]).unwrap();
        p.set(10, vec![2]).unwrap();
        p.set(20, vec![3]).unwrap();
        let all: Vec<_> = p.range(0, MAX_LINE).collect();
        assert_eq!(all, vec![(10, &[2u8][..]), (20, &[3u8][..])]);
    }

    #[test]
    fn range_is_inclusive_and_reversed_is_empty() {
        let mut p = Program::default();
        for n in [10, 20, 30] {
            p.set(n, vec![]).unwrap();
        }
        assert_eq!(p.range(20, 30).count(), 2);
        assert_eq!(p.range(30, 10).count(), 0);
    }

    #[test]
    fn navigation_helpers() {
        let mut p = Program::default();
        assert_eq!(p.first(), None);
        for n in [30, 10, 20] {
            p.set(n, vec![n as u8]).unwrap();
        }
        assert_eq!(p.first(), Some(10));
        assert_eq!(p.next_after(10), Some(20));
        assert_eq!(p.next_after(15), Some(20));
        assert_eq!(p.next_after(30), None);
        assert!(p.contains(20) && !p.contains(25));
        assert_eq!(p.get(20), Some(&[20u8][..]));
    }

    #[test]
    fn accounting_and_out_of_memory() {
        let mut p = Program::default();
        assert_eq!(p.bytes_free(), PROGRAM_AREA);
        p.set(10, vec![0; 100]).unwrap();
        assert_eq!(p.bytes_free(), PROGRAM_AREA - 105);
        // Replacing is judged by net cost.
        p.set(10, vec![0; 50]).unwrap();
        assert_eq!(p.bytes_free(), PROGRAM_AREA - 55);
        // Too big: unchanged.
        assert_eq!(p.set(20, vec![0; PROGRAM_AREA]), Err(Error::OutOfMemory));
        assert_eq!(p.bytes_free(), PROGRAM_AREA - 55);
        assert_eq!(p.get(20), None);
        // Exactly full is allowed.
        p.set(30, vec![0; PROGRAM_AREA - 55 - 5]).unwrap();
        assert_eq!(p.bytes_free(), 0);
        p.delete(30);
        assert_eq!(p.bytes_free(), PROGRAM_AREA - 55);
        p.clear();
        assert_eq!(p.bytes_free(), PROGRAM_AREA);
    }

    #[test]
    fn delete_missing_line_is_harmless_and_clear_empties() {
        let mut p = Program::default();
        p.set(10, vec![]).unwrap();
        p.delete(99);
        assert_eq!(p.range(0, MAX_LINE).count(), 1);
        p.clear();
        assert_eq!(p.range(0, MAX_LINE).count(), 0);
    }
}
