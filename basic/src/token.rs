//! Tokenizer and detokenizer for program lines (decision D9, rules in the
//! "Program entry, tokenizing and LIST" section of `BASIC.md`).
//!
//! A stored line body is a `Vec<u8>` of ASCII bytes with every keyword
//! replaced by one byte `FIRST_TOKEN + index into KEYWORDS`. Because input must
//! be pure ASCII (all bytes below 0x80), token bytes can never collide with
//! text.

use crate::error::Error;
use alloc::string::String;
use alloc::vec::Vec;

/// First byte value used for a keyword token.
pub(crate) const FIRST_TOKEN: u8 = 0x80;

/// Every keyword, in token order. The position in this table *is* the token
/// value, so only ever append: reordering changes the stored format.
pub(crate) const KEYWORDS: &[&str] = &[
    "REM", "LET", "PRINT", "INPUT", "IF", "THEN", "GOTO", "GOSUB", "RETURN", "ON", "FOR", "TO",
    "STEP", "NEXT", "END", "STOP", "DIM", "DATA", "READ", "RESTORE", "CLS", "COLOR", "LOCATE",
    "POKE", "RUN", "LIST", "NEW", "AND", "OR", "NOT", "ABS", "INT", "SGN", "SQR", "SIN", "COS",
    "TAN", "ATN", "LOG", "EXP", "RND", "PEEK", "FRE", "TIMER", "LEN", "ASC", "VAL", "STR$", "CHR$",
    "LEFT$", "RIGHT$", "MID$", "TAB", "SPC", "BYE",
];

/// The token byte for a keyword. Panics only if `name` is not in `KEYWORDS`,
/// which is a programming error (callers pass literals).
pub(crate) fn token(name: &str) -> u8 {
    let index = KEYWORDS
        .iter()
        .position(|k| *k == name)
        .expect("keyword must be in KEYWORDS");
    FIRST_TOKEN + index as u8
}

/// The keyword a token byte stands for, if it is one.
pub(crate) fn keyword(byte: u8) -> Option<&'static str> {
    let index = usize::from(byte.checked_sub(FIRST_TOKEN)?);
    KEYWORDS.get(index).copied()
}

/// The longest keyword that starts `rest` (case-insensitively), as
/// `(table index, length in bytes)`.
fn longest_keyword(rest: &[u8]) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    for (index, keyword) in KEYWORDS.iter().enumerate() {
        let len = keyword.len();
        if rest.len() >= len
            && rest[..len].eq_ignore_ascii_case(keyword.as_bytes())
            && best.is_none_or(|(_, best_len)| len > best_len)
        {
            best = Some((index, len));
        }
    }
    best
}

/// Turn the text of a line (without its line number) into tokens.
///
/// Keywords are recognised anywhere outside string literals, `REM` text and
/// `DATA` items, taking the longest match, so `TOTAL` becomes `TO` + `T` `A` `L`
/// exactly as in classic BASIC. Letters outside those regions are folded to
/// upper case; everything else is kept as typed. Non-ASCII input is a syntax
/// error.
pub(crate) fn tokenize(text: &str) -> Result<Vec<u8>, Error> {
    if !text.is_ascii() {
        return Err(Error::Syntax);
    }
    let bytes = text.as_bytes();
    let rem = token("REM");
    let data = token("DATA");
    let print = token("PRINT");
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'"' {
            // String literal: copy through the closing quote, or to the end
            // of the line if it is never closed.
            out.push(b);
            i += 1;
            while i < bytes.len() {
                out.push(bytes[i]);
                i += 1;
                if bytes[i - 1] == b'"' {
                    break;
                }
            }
        } else if b == b'?' {
            out.push(print);
            i += 1;
        } else if let Some((index, len)) = longest_keyword(&bytes[i..]) {
            let tok = FIRST_TOKEN + index as u8;
            out.push(tok);
            i += len;
            if tok == rem {
                // Comment text is kept verbatim, colons and all.
                out.extend_from_slice(&bytes[i..]);
                i = bytes.len();
            } else if tok == data {
                // DATA items are kept verbatim up to the next unquoted colon.
                let mut in_quotes = false;
                while i < bytes.len() && (in_quotes || bytes[i] != b':') {
                    if bytes[i] == b'"' {
                        in_quotes = !in_quotes;
                    }
                    out.push(bytes[i]);
                    i += 1;
                }
            }
        } else {
            out.push(b.to_ascii_uppercase());
            i += 1;
        }
    }
    Ok(out)
}

/// Turn stored tokens back into text, as `LIST` prints them. Nothing is
/// inserted around keywords.
pub(crate) fn detokenize(tokens: &[u8]) -> String {
    let mut out = String::with_capacity(tokens.len());
    for &b in tokens {
        if b >= FIRST_TOKEN {
            match KEYWORDS.get(usize::from(b - FIRST_TOKEN)) {
                Some(keyword) => out.push_str(keyword),
                // Cannot happen for tokens we produced; never panic on it.
                None => out.push('?'),
            }
        } else {
            out.push(char::from(b));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn table_fits_in_one_byte_and_has_no_duplicates() {
        assert!(KEYWORDS.len() <= 128);
        for (i, a) in KEYWORDS.iter().enumerate() {
            for b in &KEYWORDS[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn keywords_are_split_out_of_names_like_classic_basic() {
        assert_eq!(
            tokenize("total").unwrap(),
            vec![token("TO"), b'T', b'A', b'L']
        );
        assert_eq!(
            tokenize("score").unwrap(),
            vec![b'S', b'C', token("OR"), b'E']
        );
    }

    #[test]
    fn longest_keyword_wins() {
        assert_eq!(tokenize("input").unwrap(), vec![token("INPUT")]);
        assert_eq!(tokenize("gosub").unwrap(), vec![token("GOSUB")]);
        assert_eq!(
            tokenize("left$(a$,1)").unwrap(),
            vec![token("LEFT$"), b'(', b'A', b'$', b',', b'1', b')']
        );
    }

    #[test]
    fn question_mark_is_print() {
        assert_eq!(tokenize("?1").unwrap(), vec![token("PRINT"), b'1']);
    }

    #[test]
    fn case_is_folded_except_in_literals() {
        assert_eq!(
            tokenize("Print \"Hi\":x").unwrap(),
            vec![token("PRINT"), b' ', b'"', b'H', b'i', b'"', b':', b'X']
        );
    }

    #[test]
    fn rem_and_data_text_is_verbatim() {
        let mut want = vec![token("REM")];
        want.extend_from_slice(b" a:print \"x");
        assert_eq!(tokenize("rem a:print \"x").unwrap(), want);

        let mut want = vec![token("DATA")];
        want.extend_from_slice(b" a,\"b:c\" ");
        want.push(b':');
        want.push(token("PRINT"));
        assert_eq!(tokenize("data a,\"b:c\" :print").unwrap(), want);
    }

    #[test]
    fn non_ascii_is_a_syntax_error() {
        assert_eq!(tokenize("print \"é\""), Err(Error::Syntax));
    }

    #[test]
    fn detokenize_round_trips_normalised_text() {
        for text in ["PRINT\"x\";A$", "FOR I=1 TO 3", "IF A>=1 THEN GOTO 10"] {
            assert_eq!(detokenize(&tokenize(text).unwrap()), text);
        }
        assert_eq!(detokenize(&tokenize("print  1").unwrap()), "PRINT  1");
    }

    #[test]
    fn detokenize_survives_unknown_tokens() {
        assert_eq!(detokenize(&[0xff]), "?");
    }
}
