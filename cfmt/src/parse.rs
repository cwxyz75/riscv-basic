//! `strtod`, `strtol` and `strtoul` (C11 7.22.1): the longest valid prefix, how many bytes it used
//! (0 if there is no number: the caller then reports `endptr == nptr`), and whether the result was
//! out of range (`ERANGE`). Decimal floating point goes through Rust's correctly rounded parser;
//! hexadecimal floating point (which Lua relies on) is rounded here, half to even.

use alloc::string::String;

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParsedFloat {
    pub value: f64,
    /// Bytes consumed from the start of the input, white space included; 0: no conversion.
    pub len: usize,
    /// Overflow, or underflow to zero (`ERANGE`).
    pub range_error: bool,
}

fn hexval(b: u8) -> Option<u32> {
    (b as char).to_digit(16)
}

/// Round `m * 2^e2` (with `sticky` set if nonzero bits were dropped below `m`) to the nearest
/// double, half to even; handles subnormals and overflow.
fn round_to_f64(m: u64, sticky: bool, e2: i64) -> f64 {
    if m == 0 {
        return 0.0;
    }
    let lz = i64::from(m.leading_zeros());
    let m = m << lz; // top bit at 63
    let e = e2 - lz + 63; // exponent of the leading bit
    if e >= 1024 {
        return f64::INFINITY;
    }
    // how many of the 64 bits are kept: 53 for a normal number, fewer for a subnormal
    let keep: i64 = if e >= -1022 { 53 } else { 53 - (-1022 - e) };
    if keep < 0 {
        return 0.0;
    }
    let drop = (64 - keep) as u32;
    let (kept, rest, half) = if drop >= 64 {
        (0u64, m, 1u64 << 63)
    } else {
        (m >> drop, m & ((1u64 << drop) - 1), 1u64 << (drop - 1))
    };
    let round_up =
        rest > half || (rest == half && (sticky || kept & 1 == 1)) || (rest == half && sticky);
    let mut kept = kept + u64::from(round_up);
    let mut e = e;
    if keep == 53 {
        if kept >> 53 != 0 {
            kept >>= 1;
            e += 1;
            if e >= 1024 {
                return f64::INFINITY;
            }
        }
        let bits = (((e + 1023) as u64) << 52) | (kept & ((1u64 << 52) - 1));
        f64::from_bits(bits)
    } else {
        // subnormal: `kept` counts units of 2^-1074 (it may have rounded up into the smallest normal)
        f64::from_bits(kept)
    }
}

/// `strtod` of the longest valid prefix of `s`.
pub fn strtod(s: &[u8]) -> ParsedFloat {
    let none = ParsedFloat {
        value: 0.0,
        len: 0,
        range_error: false,
    };
    let mut i = 0;
    while s.get(i).copied().is_some_and(is_space) {
        i += 1;
    }
    let negative = match s.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let apply = |v: f64| if negative { -v } else { v };
    let rest = &s[i.min(s.len())..];
    let lower = |k: usize| rest.get(k).map(u8::to_ascii_lowercase);
    // infinity and NaN
    if lower(0) == Some(b'i') && lower(1) == Some(b'n') && lower(2) == Some(b'f') {
        let long = b"inity"
            .iter()
            .enumerate()
            .all(|(k, c)| lower(3 + k) == Some(*c));
        return ParsedFloat {
            value: apply(f64::INFINITY),
            len: i + if long { 8 } else { 3 },
            range_error: false,
        };
    }
    if lower(0) == Some(b'n') && lower(1) == Some(b'a') && lower(2) == Some(b'n') {
        let mut k = 3;
        if rest.get(k) == Some(&b'(') {
            let mut j = k + 1;
            while rest
                .get(j)
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
            {
                j += 1;
            }
            if rest.get(j) == Some(&b')') {
                k = j + 1;
            }
        }
        return ParsedFloat {
            value: apply(f64::NAN),
            len: i + k,
            range_error: false,
        };
    }
    // hexadecimal: 0x, hex digits with an optional point, an optional binary exponent
    if rest.first() == Some(&b'0') && matches!(lower(1), Some(b'x')) {
        let mut k = 2;
        let mut m: u64 = 0;
        let mut sticky = false;
        let mut digits = 0;
        let mut e2: i64 = 0;
        let mut seen_point = false;
        let mut nonzero = false;
        loop {
            match rest.get(k) {
                Some(b'.') if !seen_point => {
                    seen_point = true;
                }
                Some(&c) if hexval(c).is_some() => {
                    let d = u64::from(hexval(c).unwrap_or(0));
                    digits += 1;
                    if d != 0 {
                        nonzero = true;
                    }
                    if m >> 60 == 0 {
                        m = (m << 4) | d;
                        if seen_point {
                            e2 -= 4;
                        }
                    } else {
                        // no room: this digit only counts as sticky, and scales by 16 if before the point
                        sticky |= d != 0;
                        if !seen_point {
                            e2 += 4;
                        }
                    }
                }
                _ => break,
            }
            k += 1;
        }
        if digits == 0 {
            // "0x" with no digits: just the "0" converts
            return ParsedFloat {
                value: apply(0.0),
                len: i + 1,
                range_error: false,
            };
        }
        // binary exponent
        if matches!(rest.get(k).map(u8::to_ascii_lowercase), Some(b'p')) {
            let mut j = k + 1;
            let neg = match rest.get(j) {
                Some(b'-') => {
                    j += 1;
                    true
                }
                Some(b'+') => {
                    j += 1;
                    false
                }
                _ => false,
            };
            if rest.get(j).is_some_and(u8::is_ascii_digit) {
                let mut x: i64 = 0;
                while let Some(d) = rest.get(j).filter(|d| d.is_ascii_digit()) {
                    x = (x * 10 + i64::from(d - b'0')).min(1 << 40);
                    j += 1;
                }
                e2 += if neg { -x } else { x };
                k = j;
            }
        }
        let v = round_to_f64(m, sticky, e2);
        let range_error = v.is_infinite() || (v == 0.0 && nonzero);
        return ParsedFloat {
            value: apply(v),
            len: i + k,
            range_error,
        };
    }
    // decimal: digits [. digits] [e [sign] digits]
    let mut k = 0;
    let mut int_digits = 0;
    while rest.get(k).is_some_and(u8::is_ascii_digit) {
        k += 1;
        int_digits += 1;
    }
    let mut frac_digits = 0;
    if rest.get(k) == Some(&b'.') {
        let mut j = k + 1;
        while rest.get(j).is_some_and(u8::is_ascii_digit) {
            j += 1;
            frac_digits += 1;
        }
        if int_digits + frac_digits > 0 {
            k = j;
        }
    }
    if int_digits + frac_digits == 0 {
        return none;
    }
    if matches!(rest.get(k).map(u8::to_ascii_lowercase), Some(b'e')) {
        let mut j = k + 1;
        if matches!(rest.get(j), Some(b'+') | Some(b'-')) {
            j += 1;
        }
        if rest.get(j).is_some_and(u8::is_ascii_digit) {
            while rest.get(j).is_some_and(u8::is_ascii_digit) {
                j += 1;
            }
            k = j;
        }
    }
    let text = String::from_utf8_lossy(&rest[..k]);
    let v: f64 = text.parse().unwrap_or(0.0);
    let nonzero = rest[..k]
        .iter()
        .take_while(|c| !matches!(c, b'e' | b'E'))
        .any(|c| (b'1'..=b'9').contains(c));
    let range_error = v.is_infinite() || (v == 0.0 && nonzero);
    ParsedFloat {
        value: apply(v),
        len: i + k,
        range_error,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedInt {
    pub value: i64,
    pub len: usize,
    pub range_error: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedUInt {
    pub value: u64,
    pub len: usize,
    pub range_error: bool,
}

/// The digits of an integer in `base` (0: decide from the prefix). `(magnitude, negative, len,
/// overflowed)`, or `None` when nothing converts.
fn integer(s: &[u8], base: u32) -> Option<(u64, bool, usize, bool)> {
    if base == 1 || base > 36 {
        return None;
    }
    let mut i = 0;
    while s.get(i).copied().is_some_and(is_space) {
        i += 1;
    }
    let negative = match s.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let mut base = base;
    let has_hex_prefix = s.get(i) == Some(&b'0')
        && matches!(s.get(i + 1), Some(b'x') | Some(b'X'))
        && s.get(i + 2).is_some_and(|c| hexval(*c).is_some());
    if (base == 0 || base == 16) && has_hex_prefix {
        i += 2;
        base = 16;
    } else if base == 0 {
        base = if s.get(i) == Some(&b'0') { 8 } else { 10 };
    }
    let start = i;
    let mut value: u64 = 0;
    let mut overflow = false;
    while let Some(d) = s.get(i).and_then(|c| (*c as char).to_digit(base)) {
        match value
            .checked_mul(u64::from(base))
            .and_then(|v| v.checked_add(u64::from(d)))
        {
            Some(v) => value = v,
            None => overflow = true,
        }
        i += 1;
    }
    if i == start {
        return None;
    }
    Some((value, negative, i, overflow))
}

pub fn strtol(s: &[u8], base: u32) -> ParsedInt {
    let Some((mag, neg, len, over)) = integer(s, base) else {
        return ParsedInt {
            value: 0,
            len: 0,
            range_error: false,
        };
    };
    if neg {
        if over || mag > 1 << 63 {
            return ParsedInt {
                value: i64::MIN,
                len,
                range_error: true,
            };
        }
        return ParsedInt {
            value: (mag as i64).wrapping_neg(),
            len,
            range_error: false,
        };
    }
    if over || mag > i64::MAX as u64 {
        return ParsedInt {
            value: i64::MAX,
            len,
            range_error: true,
        };
    }
    ParsedInt {
        value: mag as i64,
        len,
        range_error: false,
    }
}

pub fn strtoul(s: &[u8], base: u32) -> ParsedUInt {
    let Some((mag, neg, len, over)) = integer(s, base) else {
        return ParsedUInt {
            value: 0,
            len: 0,
            range_error: false,
        };
    };
    if over {
        return ParsedUInt {
            value: u64::MAX,
            len,
            range_error: true,
        };
    }
    ParsedUInt {
        value: if neg { mag.wrapping_neg() } else { mag },
        len,
        range_error: false,
    }
}
