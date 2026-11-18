//! The core of `sscanf`: the format is applied to the input and the values it would store are returned, in order, for the caller
//! (the C library) to write through the pointers it was given. Pure and host-tested against the host's own `sscanf`.
//!
//! Conversions: `%d %i %u %o %x %X`, `%f %e %g %E %G %a`, `%s`, `%c`, `%[set]` / `%[^set]`, `%n`, `%%`, with `*`, a field width and the
//! `hh h l ll z j t L` modifiers. White space in the format skips any amount of white space in the input.
use alloc::vec::Vec;

/// One value a successful conversion would store.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    /// An integer conversion: the value (two's complement in 64 bits) and the modifier class: 0 `int`, 1 `long`/`size_t`/..., 2 `long long`,
    /// 3 `short`, 4 `char`, 5 `long double` (not valid for integers).
    Int { value: u64, len: u8 },
    /// A floating conversion: `len` 0 is `float`, 1 `double`, 5 `long double` (stored as a `double` by the library).
    Float { value: f64, len: u8 },
    /// `%s` and `%[`: the bytes, to be stored followed by a NUL.
    Str(Vec<u8>),
    /// `%c`: exactly the bytes, no NUL.
    Chars(Vec<u8>),
    /// `%n`: the number of input bytes consumed so far.
    Count(i32),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Scanned {
    /// The values to store, in argument order (suppressed conversions are not here).
    pub items: Vec<Item>,
    /// What `sscanf` returns: the number of assigned conversions (`%n` does not count), or -1 when the input ended before the first
    /// conversion (or the first literal) matched.
    pub ret: i32,
    /// How many input bytes the scan used (what a stream reader has to take out of the stream).
    pub consumed: usize,
}

fn in_set(set: &[u8], b: u8) -> bool {
    let mut k = 0;
    while k < set.len() {
        if k + 2 < set.len() && set[k + 1] == b'-' {
            if set[k] <= b && b <= set[k + 2] {
                return true;
            }
            k += 3;
        } else {
            if set[k] == b {
                return true;
            }
            k += 1;
        }
    }
    false
}

/// Where a floating-point number that starts at `i` ends (at most `limit` bytes): `[+-] digits [. digits] [e [+-] digits]`, or `.digits`,
/// `inf`, `infinity`, `nan` in any case. An exponent that has no digits is not part of the number.
fn float_end(input: &[u8], i: usize, limit: usize) -> usize {
    let at = |k: usize| {
        if k - i < limit {
            input.get(k).copied()
        } else {
            None
        }
    };
    let mut k = i;
    if matches!(at(k), Some(b'+') | Some(b'-')) {
        k += 1;
    }
    let word = |k: usize, w: &[u8]| {
        (0..w.len()).all(|d| at(k + d).is_some_and(|b| b.to_ascii_lowercase() == w[d]))
    };
    if word(k, b"infinity") {
        return k + 8;
    }
    if word(k, b"inf") || word(k, b"nan") {
        return k + 3;
    }
    if let Some(end) = hex_float_end(input, i, k, limit) {
        return end;
    }
    let digits_from = k;
    while at(k).is_some_and(|b| b.is_ascii_digit()) {
        k += 1;
    }
    let mut ndigits = k - digits_from;
    if at(k) == Some(b'.') {
        k += 1;
        let frac = k;
        while at(k).is_some_and(|b| b.is_ascii_digit()) {
            k += 1;
        }
        ndigits += k - frac;
    }
    if ndigits == 0 {
        return i;
    }
    if matches!(at(k), Some(b'e') | Some(b'E')) {
        let mut e = k + 1;
        if matches!(at(e), Some(b'+') | Some(b'-')) {
            e += 1;
        }
        if at(e).is_some_and(|b| b.is_ascii_digit()) {
            while at(e).is_some_and(|b| b.is_ascii_digit()) {
                e += 1;
            }
            k = e;
        }
    }
    k
}

/// A hexadecimal floating-point number (`0x1.8p3`): where it ends, if `0x` and at least one hex digit start at `k`.
fn hex_float_end(input: &[u8], _i: usize, k: usize, limit: usize) -> Option<usize> {
    let start = _i;
    let at = |k: usize| {
        if k - start < limit {
            input.get(k).copied()
        } else {
            None
        }
    };
    if at(k) != Some(b'0') || !matches!(at(k + 1), Some(b'x') | Some(b'X')) {
        return None;
    }
    let mut e = k + 2;
    let digits = e;
    while at(e).is_some_and(|b| b.is_ascii_hexdigit()) {
        e += 1;
    }
    let mut n = e - digits;
    if at(e) == Some(b'.') {
        e += 1;
        let f = e;
        while at(e).is_some_and(|b| b.is_ascii_hexdigit()) {
            e += 1;
        }
        n += e - f;
    }
    if n == 0 {
        return None;
    }
    if matches!(at(e), Some(b'p') | Some(b'P')) {
        let mut x = e + 1;
        if matches!(at(x), Some(b'+') | Some(b'-')) {
            x += 1;
        }
        if at(x).is_some_and(|b| b.is_ascii_digit()) {
            while at(x).is_some_and(|b| b.is_ascii_digit()) {
                x += 1;
            }
            e = x;
        }
    }
    Some(e)
}

/// The value of a hexadecimal floating-point text accepted by `hex_float_end` (`[+-]0xH[.H][pN]`).
fn hex_float_value(t: &[u8]) -> f64 {
    let (neg, t) = match t.first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let t = &t[2..]; // 0x
    let mut m = 0f64;
    let mut scale = 0i32;
    let mut seen_dot = false;
    let mut k = 0;
    while k < t.len() {
        let b = t[k];
        if b == b'.' {
            seen_dot = true;
        } else if let Some(d) = (b as char).to_digit(16) {
            m = m * 16.0 + f64::from(d);
            if seen_dot {
                scale -= 4;
            }
        } else {
            break;
        }
        k += 1;
    }
    let mut exp = 0i32;
    if matches!(t.get(k), Some(b'p') | Some(b'P')) {
        let mut x = k + 1;
        let mut eneg = false;
        if matches!(t.get(x), Some(b'+') | Some(b'-')) {
            eneg = t[x] == b'-';
            x += 1;
        }
        while let Some(d) = t.get(x).filter(|b| b.is_ascii_digit()) {
            exp = exp.saturating_mul(10).saturating_add(i32::from(d - b'0'));
            x += 1;
        }
        if eneg {
            exp = -exp;
        }
    }
    let mut v = m;
    let mut e = scale.saturating_add(exp);
    while e > 0 && v.is_finite() {
        v *= 2.0;
        e -= 1;
    }
    while e < 0 && v != 0.0 {
        v /= 2.0;
        e += 1;
    }
    if neg {
        -v
    } else {
        v
    }
}

pub fn scan(input: &[u8], f: &[u8]) -> Scanned {
    let mut items = Vec::new();
    let (mut i, mut j, mut done) = (0usize, 0usize, 0i32);
    let mut stored_any = false;
    macro_rules! out {
        ($v:expr) => {
            return Scanned {
                items,
                ret: $v,
                consumed: i,
            }
        };
    }
    while j < f.len() {
        let c = f[j];
        if c.is_ascii_whitespace() {
            while input.get(i).is_some_and(|b| b.is_ascii_whitespace()) {
                i += 1;
            }
            j += 1;
            continue;
        }
        if c != b'%' {
            if input.get(i) != Some(&c) {
                out!(if i >= input.len() && !stored_any {
                    -1
                } else {
                    done
                });
            }
            i += 1;
            j += 1;
            continue;
        }
        j += 1;
        let suppress = f.get(j) == Some(&b'*');
        if suppress {
            j += 1;
        }
        let mut width = 0usize;
        while f.get(j).is_some_and(|b| b.is_ascii_digit()) {
            width = width * 10 + usize::from(f[j] - b'0');
            j += 1;
        }
        let mut len = 0u8;
        loop {
            match f.get(j) {
                Some(b'l') => len = if len == 1 { 2 } else { 1 },
                Some(b'h') => len = if len == 3 { 4 } else { 3 },
                Some(b'z') | Some(b'j') | Some(b't') => len = 1,
                Some(b'L') => len = 5,
                _ => break,
            }
            j += 1;
        }
        let Some(&conv) = f.get(j) else { break };
        j += 1;
        if conv == b'%' {
            while input.get(i).is_some_and(|b| b.is_ascii_whitespace()) {
                i += 1;
            }
            if i >= input.len() {
                out!(if stored_any { done } else { -1 });
            }
            if input.get(i) != Some(&b'%') {
                out!(done);
            }
            i += 1;
            continue;
        }
        if conv == b'n' {
            if !suppress {
                items.push(Item::Count(i as i32));
            }
            continue;
        }
        if conv != b'c' && conv != b'[' {
            while input.get(i).is_some_and(|b| b.is_ascii_whitespace()) {
                i += 1;
            }
        }
        if i >= input.len() {
            out!(if stored_any { done } else { -1 });
        }
        let limit = if width == 0 { usize::MAX } else { width };
        match conv {
            b'd' | b'i' | b'u' | b'o' | b'x' | b'X' => {
                let start = i;
                let mut neg = false;
                if matches!(input.get(i), Some(b'+') | Some(b'-')) && i - start < limit {
                    neg = input[i] == b'-';
                    i += 1;
                }
                let mut base = match conv {
                    b'o' => 8,
                    b'x' | b'X' => 16,
                    _ => 10,
                };
                if (conv == b'i' || base == 16)
                    && input.get(i) == Some(&b'0')
                    && matches!(input.get(i + 1), Some(b'x') | Some(b'X'))
                    && i + 2 - start < limit
                    && input.get(i + 2).is_some_and(|b| b.is_ascii_hexdigit())
                {
                    base = 16;
                    i += 2;
                } else if conv == b'i' && input.get(i) == Some(&b'0') {
                    base = 8;
                }
                let digits = i;
                let mut v: u64 = 0;
                while i - start < limit && input.get(i).is_some_and(|b| (*b as char).is_digit(base))
                {
                    v = v
                        .wrapping_mul(u64::from(base))
                        .wrapping_add(u64::from((input[i] as char).to_digit(base).unwrap()));
                    i += 1;
                }
                if i == digits {
                    out!(done);
                }
                let v = if neg {
                    (v as i64).wrapping_neg() as u64
                } else {
                    v
                };
                if !suppress {
                    items.push(Item::Int { value: v, len });
                    done += 1;
                    stored_any = true;
                }
            }
            b'f' | b'e' | b'g' | b'E' | b'G' | b'a' => {
                let start = i;
                let end = float_end(input, i, limit);
                i = end;
                let text = &input[start..end];
                let hex = text.iter().any(|b| matches!(b, b'x' | b'X'));
                let parsed = if hex {
                    Ok(hex_float_value(text))
                } else {
                    core::str::from_utf8(text).unwrap_or("").parse::<f64>()
                };
                let Ok(v) = parsed else {
                    // (nothing that looks like a number was consumed beyond what a failed match leaves: the input stays)
                    out!(done);
                };
                if !suppress {
                    items.push(Item::Float { value: v, len });
                    done += 1;
                    stored_any = true;
                }
            }
            b's' => {
                let start = i;
                while i - start < limit && input.get(i).is_some_and(|b| !b.is_ascii_whitespace()) {
                    i += 1;
                }
                if !suppress {
                    items.push(Item::Str(input[start..i].to_vec()));
                    done += 1;
                    stored_any = true;
                }
            }
            b'c' => {
                let n = if width == 0 { 1 } else { width };
                if i + n > input.len() {
                    out!(if stored_any { done } else { -1 });
                }
                if !suppress {
                    items.push(Item::Chars(input[i..i + n].to_vec()));
                    done += 1;
                    stored_any = true;
                }
                i += n;
            }
            b'[' => {
                let negate = f.get(j) == Some(&b'^');
                if negate {
                    j += 1;
                }
                let set_start = j;
                if f.get(j) == Some(&b']') {
                    j += 1;
                }
                while f.get(j).is_some_and(|&b| b != b']') {
                    j += 1;
                }
                let set = &f[set_start..j];
                j += 1; // ]
                let start = i;
                while i - start < limit && input.get(i).is_some_and(|&b| in_set(set, b) != negate) {
                    i += 1;
                }
                if i == start {
                    out!(done);
                }
                if !suppress {
                    items.push(Item::Str(input[start..i].to_vec()));
                    done += 1;
                    stored_any = true;
                }
            }
            _ => out!(done),
        }
    }
    Scanned {
        items,
        ret: done,
        consumed: i,
    }
}
