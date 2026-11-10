//! `DATA` statements: finding the next item for `READ`, and splitting typed
//! input into items for `INPUT` (rules in "Data and input" in `BASIC.md`).

use crate::error::Error;
use crate::program::Program;
use crate::token::token;
use alloc::string::String;
use alloc::vec::Vec;

/// One item of data: a quoted string (taken exactly) or bare text (trimmed).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DataItem {
    Quoted(String),
    Bare(String),
}

/// Where the next `READ` continues. `None` (held by the interpreter) means
/// "from the first item of the program".
#[derive(Debug, Clone, Copy)]
pub(crate) struct DataPtr {
    line: u16,
    pos: usize,
    /// `pos` is inside a `DATA` statement.
    in_data: bool,
    /// A comma was just consumed, so another item (possibly empty) follows.
    pending: bool,
}

fn text(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}

fn trim_spaces(bytes: &[u8]) -> &[u8] {
    let start = bytes.iter().position(|&b| b != b' ').unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|&b| b != b' ')
        .map_or(start, |i| i + 1);
    &bytes[start..end]
}

/// The next `DATA` item of `program`, advancing `ptr`; `OutOfData` when none is
/// left (the pointer then stays at the end, so it keeps failing).
pub(crate) fn next_item(program: &Program, ptr: &mut Option<DataPtr>) -> Result<DataItem, Error> {
    let data_token = token("DATA");
    let mut p = match *ptr {
        Some(p) => p,
        None => DataPtr {
            line: program.first().ok_or(Error::OutOfData)?,
            pos: 0,
            in_data: false,
            pending: false,
        },
    };
    loop {
        let Some(tokens) = program.get(p.line) else {
            // The line was deleted since: carry on with the one after it.
            match program.next_after(p.line) {
                Some(next) => {
                    p = DataPtr {
                        line: next,
                        pos: 0,
                        in_data: false,
                        pending: false,
                    };
                    continue;
                }
                None => {
                    *ptr = Some(p);
                    return Err(Error::OutOfData);
                }
            }
        };
        let from = p.pos.min(tokens.len());

        if !p.in_data {
            match tokens[from..].iter().position(|&b| b == data_token) {
                Some(offset) => {
                    p.pos = from + offset + 1;
                    p.in_data = true;
                    p.pending = false;
                }
                None => match program.next_after(p.line) {
                    Some(next) => {
                        p = DataPtr {
                            line: next,
                            pos: 0,
                            in_data: false,
                            pending: false,
                        };
                    }
                    None => {
                        *ptr = Some(p);
                        return Err(Error::OutOfData);
                    }
                },
            }
            continue;
        }

        // Inside a DATA statement.
        let mut i = from;
        while tokens.get(i) == Some(&b' ') {
            i += 1;
        }
        let at_end = i >= tokens.len() || tokens[i] == b':';
        if at_end && !p.pending {
            p.pos = i;
            p.in_data = false;
            continue;
        }
        let item = if tokens.get(i) == Some(&b'"') {
            let start = i + 1;
            let end = tokens[start..]
                .iter()
                .position(|&b| b == b'"')
                .map_or(tokens.len(), |k| start + k);
            i = (end + 1).min(tokens.len());
            // Anything between the closing quote and the next comma is ignored.
            while i < tokens.len() && tokens[i] != b',' && tokens[i] != b':' {
                i += 1;
            }
            DataItem::Quoted(text(&tokens[start..end]))
        } else {
            let start = i;
            while i < tokens.len() && tokens[i] != b',' && tokens[i] != b':' {
                i += 1;
            }
            DataItem::Bare(text(trim_spaces(&tokens[start..i])))
        };
        p.pending = tokens.get(i) == Some(&b',');
        if p.pending {
            i += 1;
        }
        p.pos = i;
        *ptr = Some(p);
        return Ok(item);
    }
}

/// Split a line typed for `INPUT` at commas into items. Quoted items may hold
/// commas; an empty line is one empty item.
pub(crate) fn split_input(line: &str) -> Vec<DataItem> {
    let bytes = line.as_bytes();
    let mut items = Vec::new();
    let mut i = 0;
    loop {
        while bytes.get(i) == Some(&b' ') {
            i += 1;
        }
        if bytes.get(i) == Some(&b'"') {
            let start = i + 1;
            let end = bytes[start..]
                .iter()
                .position(|&b| b == b'"')
                .map_or(bytes.len(), |k| start + k);
            items.push(DataItem::Quoted(String::from(&line[start..end])));
            i = (end + 1).min(bytes.len());
            while i < bytes.len() && bytes[i] != b',' {
                i += 1;
            }
        } else {
            let start = i;
            while i < bytes.len() && bytes[i] != b',' {
                i += 1;
            }
            items.push(DataItem::Bare(String::from(
                line[start..i].trim_matches(' '),
            )));
        }
        if bytes.get(i) == Some(&b',') {
            i += 1;
        } else {
            return items;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::tokenize;
    use alloc::vec;

    fn program(lines: &[(u16, &str)]) -> Program {
        let mut p = Program::default();
        for &(n, body) in lines {
            p.set(n, tokenize(body).unwrap()).unwrap();
        }
        p
    }

    fn all_items(p: &Program) -> Vec<DataItem> {
        let mut ptr = None;
        let mut items = Vec::new();
        while let Ok(item) = next_item(p, &mut ptr) {
            items.push(item);
        }
        items
    }

    fn bare(s: &str) -> DataItem {
        DataItem::Bare(s.into())
    }

    #[test]
    fn items_across_lines_and_statements() {
        let p = program(&[
            (10, "DATA 1, 2 ,\"A, B\""),
            (20, "PRINT 5: DATA x"),
            (30, "REM no data here"),
            (40, "DATA 7: DATA 8"),
        ]);
        assert_eq!(
            all_items(&p),
            vec![
                bare("1"),
                bare("2"),
                DataItem::Quoted("A, B".into()),
                bare("x"),
                bare("7"),
                bare("8"),
            ]
        );
    }

    #[test]
    fn empty_items() {
        let p = program(&[(10, "DATA 1,,3"), (20, "DATA 4,")]);
        assert_eq!(
            all_items(&p),
            vec![bare("1"), bare(""), bare("3"), bare("4"), bare("")]
        );
        // A DATA with nothing after it holds no items.
        assert_eq!(all_items(&program(&[(10, "DATA")])), vec![]);
    }

    #[test]
    fn exhausted_stays_exhausted() {
        let p = program(&[(10, "DATA 1")]);
        let mut ptr = None;
        assert!(next_item(&p, &mut ptr).is_ok());
        assert_eq!(next_item(&p, &mut ptr), Err(Error::OutOfData));
        assert_eq!(next_item(&p, &mut ptr), Err(Error::OutOfData));
        assert_eq!(
            next_item(&Program::default(), &mut None),
            Err(Error::OutOfData)
        );
    }

    #[test]
    fn input_lines_split_at_commas() {
        assert_eq!(split_input(""), vec![bare("")]);
        assert_eq!(
            split_input(" 3, X Y ,4.5"),
            vec![bare("3"), bare("X Y"), bare("4.5")]
        );
        assert_eq!(
            split_input("\"P,Q\",7"),
            vec![DataItem::Quoted("P,Q".into()), bare("7")]
        );
        assert_eq!(split_input("1,"), vec![bare("1"), bare("")]);
    }
}
