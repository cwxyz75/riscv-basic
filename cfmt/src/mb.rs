//! The multibyte core of the wide-character functions, for the one encoding the system has (UTF-8, FS-SPEC F6): incremental decoding
//! with a state (`mbrtowc`), encoding (`wcrtomb`), and the character classes and widths. Pure; tested against `std`.
//!
//! The classes are Unicode's through `std` (`char::is_alphabetic` and friends), except `digit` (ASCII only, as C says), `space`
//! (White_Space without the no-break spaces, as glibc has it) and `punct` (printable, not a letter, digit or space). `width` is the
//! terminal column count from a small table of the common combining and wide ranges (not the whole of UAX 11).

/// A partly decoded character: the bytes seen so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct State {
    pub bytes: [u8; 4],
    pub len: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decoded {
    /// A character and how many of the *new* bytes it used (the bytes of an earlier partial character are not counted).
    Char(u32, usize),
    /// The bytes so far are a valid prefix of a character: more are needed (all the new bytes were used).
    Incomplete,
    /// Not UTF-8 (`EILSEQ`).
    Invalid,
}

fn need(lead: u8) -> usize {
    match lead {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 0,
    }
}

/// Decode one character from `input`, continuing the one in `state`. On `Char` or `Invalid` the state is cleared.
pub fn decode(state: &mut State, input: &[u8]) -> Decoded {
    let mut used = 0;
    loop {
        let have = state.len as usize;
        if have == 0 {
            let Some(&b) = input.get(used) else {
                return Decoded::Incomplete;
            };
            if b < 0x80 {
                return Decoded::Char(u32::from(b), used + 1);
            }
            if need(b) == 0 {
                return Decoded::Invalid;
            }
        }
        let Some(&b) = input.get(used) else {
            return Decoded::Incomplete;
        };
        state.bytes[state.len as usize] = b;
        state.len += 1;
        used += 1;
        let n = state.len as usize;
        let seq = &state.bytes[..n];
        let want = need(seq[0]);
        // validate what is there (the standard's rules for the second byte; core::str does the rest on the whole sequence)
        if n < want {
            let ok = match core::str::from_utf8(seq) {
                Ok(_) => true,
                Err(e) => e.error_len().is_none(), // truncated, not wrong
            };
            if !ok {
                *state = State::default();
                return Decoded::Invalid;
            }
            continue;
        }
        let decoded = core::str::from_utf8(&state.bytes[..want])
            .ok()
            .and_then(|s| s.chars().next())
            .map(u32::from);
        *state = State::default();
        return match decoded {
            Some(c) => Decoded::Char(c, used),
            None => Decoded::Invalid,
        };
    }
}

/// The UTF-8 bytes of a code point (`None` for a surrogate or above U+10FFFF).
pub fn encode(c: u32, out: &mut [u8; 4]) -> Option<usize> {
    let ch = char::from_u32(c)?;
    Some(ch.encode_utf8(out).len())
}

pub fn is_alpha(c: u32) -> bool {
    char::from_u32(c).is_some_and(char::is_alphabetic)
}
pub fn is_digit(c: u32) -> bool {
    (u32::from(b'0')..=u32::from(b'9')).contains(&c)
}
pub fn is_alnum(c: u32) -> bool {
    is_alpha(c) || char::from_u32(c).is_some_and(char::is_numeric)
}
pub fn is_space(c: u32) -> bool {
    char::from_u32(c)
        .is_some_and(|ch| ch.is_whitespace() && !matches!(ch, '\u{a0}' | '\u{2007}' | '\u{202f}'))
}
pub fn is_blank(c: u32) -> bool {
    c == 9
        || (char::from_u32(c).is_some_and(|ch| ch.is_whitespace())
            && !matches!(c, 10..=13 | 0x85 | 0x2028 | 0x2029)
            && c != 0xa0
            && c != 0x2007
            && c != 0x202f)
}
pub fn is_upper(c: u32) -> bool {
    char::from_u32(c).is_some_and(char::is_uppercase)
}
pub fn is_lower(c: u32) -> bool {
    char::from_u32(c).is_some_and(char::is_lowercase)
}
pub fn is_cntrl(c: u32) -> bool {
    char::from_u32(c).is_some_and(char::is_control) || c == 0x2028 || c == 0x2029
}
pub fn is_print(c: u32) -> bool {
    char::from_u32(c).is_some_and(|ch| !ch.is_control()) && c != 0x2028 && c != 0x2029
}
pub fn is_graph(c: u32) -> bool {
    is_print(c) && !char::from_u32(c).is_some_and(char::is_whitespace)
}
pub fn is_punct(c: u32) -> bool {
    is_graph(c) && !is_alnum(c)
}
pub fn is_xdigit(c: u32) -> bool {
    char::from_u32(c).is_some_and(|ch| ch.is_ascii_hexdigit())
}

/// Lower and upper case when the mapping is to one character (otherwise the character itself).
pub fn to_lower(c: u32) -> u32 {
    let Some(ch) = char::from_u32(c) else {
        return c;
    };
    let mut it = ch.to_lowercase();
    match (it.next(), it.next()) {
        (Some(l), None) => u32::from(l),
        _ => c,
    }
}
pub fn to_upper(c: u32) -> u32 {
    let Some(ch) = char::from_u32(c) else {
        return c;
    };
    let mut it = ch.to_uppercase();
    match (it.next(), it.next()) {
        (Some(u), None) => u32::from(u),
        _ => c,
    }
}

/// Terminal columns: 0 for NUL and combining characters, 2 for the common wide ranges, -1 for controls and anything not a character.
pub fn width(c: u32) -> i32 {
    if c == 0 {
        return 0;
    }
    let Some(ch) = char::from_u32(c) else {
        return -1;
    };
    if ch.is_control() {
        return -1;
    }
    const ZERO: &[(u32, u32)] = &[
        (0x0300, 0x036f),
        (0x0483, 0x0489),
        (0x0591, 0x05bd),
        (0x0610, 0x061a),
        (0x064b, 0x065f),
        (0x0e31, 0x0e31),
        (0x0e34, 0x0e3a),
        (0x1ab0, 0x1aff),
        (0x1dc0, 0x1dff),
        (0x200b, 0x200f),
        (0x2028, 0x202e),
        (0x2060, 0x2064),
        (0x20d0, 0x20ff),
        (0xfe00, 0xfe0f),
        (0xfe20, 0xfe2f),
        (0xfeff, 0xfeff),
        (0xe0100, 0xe01ef),
    ];
    const WIDE: &[(u32, u32)] = &[
        (0x1100, 0x115f),
        (0x2329, 0x232a),
        (0x2e80, 0x303e),
        (0x3041, 0x33ff),
        (0x3400, 0x4dbf),
        (0x4e00, 0xa4cf),
        (0xa960, 0xa97f),
        (0xac00, 0xd7a3),
        (0xf900, 0xfaff),
        (0xfe30, 0xfe6f),
        (0xff00, 0xff60),
        (0xffe0, 0xffe6),
        (0x1f300, 0x1f64f),
        (0x1f900, 0x1f9ff),
        (0x20000, 0x3fffd),
    ];
    if ZERO.iter().any(|&(a, b)| (a..=b).contains(&c)) {
        return 0;
    }
    if WIDE.iter().any(|&(a, b)| (a..=b).contains(&c)) {
        return 2;
    }
    1
}
