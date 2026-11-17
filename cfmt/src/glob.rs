//! Shell pattern matching and version comparison, the cores of `fnmatch` and `strverscmp` (S11 follow-up: the C library's pure
//! parts, tested on the host against the host's own libc, `tests/portable.rs`).

fn class_match(name: &[u8], c: u8) -> bool {
    match name {
        b"alpha" => c.is_ascii_alphabetic(),
        b"digit" => c.is_ascii_digit(),
        b"alnum" => c.is_ascii_alphanumeric(),
        b"upper" => c.is_ascii_uppercase(),
        b"lower" => c.is_ascii_lowercase(),
        b"space" => c.is_ascii_whitespace() || c == 0x0b,
        b"blank" => c == b' ' || c == b'\t',
        b"punct" => c.is_ascii_punctuation(),
        b"print" => (0x20..0x7f).contains(&c),
        b"graph" => (0x21..0x7f).contains(&c),
        b"cntrl" => c < 0x20 || c == 0x7f,
        b"xdigit" => c.is_ascii_hexdigit(),
        _ => false,
    }
}

/// Match one `[...]` set at the start of `p` against `c`; `(matched, bytes of the set)`, or `None` if the set is not closed.
fn bracket(p: &[u8], c: u8, fold: bool) -> Option<(bool, usize)> {
    let mut i = 1;
    let negate = matches!(p.get(i), Some(b'!') | Some(b'^'));
    if negate {
        i += 1;
    }
    let mut hit = false;
    let mut first = true;
    loop {
        let ch = *p.get(i)?;
        if ch == b']' && !first {
            return Some((hit != negate, i + 1));
        }
        first = false;
        if ch == b'[' && p.get(i + 1) == Some(&b':') {
            if let Some(end) = p[i + 2..].windows(2).position(|w| w == b":]") {
                let name = &p[i + 2..i + 2 + end];
                if class_match(name, c) {
                    hit = true;
                }
                i += 2 + end + 2;
                continue;
            }
        }
        let lo = ch;
        if p.get(i + 1) == Some(&b'-') && p.get(i + 2).is_some_and(|&x| x != b']') {
            let hi = p[i + 2];
            let (c2, lo2, hi2) = if fold {
                (
                    c.to_ascii_lowercase(),
                    lo.to_ascii_lowercase(),
                    hi.to_ascii_lowercase(),
                )
            } else {
                (c, lo, hi)
            };
            if lo2 <= c2 && c2 <= hi2 {
                hit = true;
            }
            i += 3;
        } else {
            if lo == c || (fold && lo.eq_ignore_ascii_case(&c)) {
                hit = true;
            }
            i += 1;
        }
    }
}

fn fnm(p: &[u8], s: &[u8], flags: i32, at_start: bool) -> bool {
    let (noescape, pathname, period, fold) = (
        flags & 2 != 0,
        flags & 1 != 0,
        flags & 4 != 0,
        flags & 0x10 != 0,
    );
    let Some(&pc) = p.first() else {
        return s.is_empty();
    };
    let leading_dot = period && at_start && s.first() == Some(&b'.');
    match pc {
        b'?' => {
            !s.is_empty()
                && !(pathname && s[0] == b'/')
                && !leading_dot
                && fnm(&p[1..], &s[1..], flags, false)
        }
        b'*' => {
            if leading_dot {
                return false;
            }
            let rest = &p[1..];
            for k in 0..=s.len() {
                if fnm(rest, &s[k..], flags, false) {
                    return true;
                }
                if k < s.len() && pathname && s[k] == b'/' {
                    break;
                }
            }
            false
        }
        b'[' => match (s.first(), bracket(p, *s.first().unwrap_or(&0), fold)) {
            (Some(&c), Some((true, n))) if !(pathname && c == b'/') && !leading_dot => {
                fnm(&p[n..], &s[1..], flags, false)
            }
            (Some(&c), None) => c == b'[' && fnm(&p[1..], &s[1..], flags, false),
            _ => false,
        },
        b'\\' if !noescape && p.len() > 1 => {
            // (an escaped character is a literal one, and still folds under FNM_CASEFOLD)
            s.first()
                .is_some_and(|&x| x == p[1] || (fold && x.eq_ignore_ascii_case(&p[1])))
                && fnm(&p[2..], &s[1..], flags, false)
        }
        c => {
            let eq = |x: u8| x == c || (fold && x.eq_ignore_ascii_case(&c));
            s.first().is_some_and(|&x| eq(x))
                && fnm(&p[1..], &s[1..], flags, pathname && c == b'/' && period)
        }
    }
}

/// `fnmatch`: does `string` match `pattern` under the `FNM_*` flags (the Linux values: `PATHNAME` 1, `NOESCAPE` 2, `PERIOD` 4,
/// `CASEFOLD` 0x10)?
pub fn fnmatch(pattern: &[u8], string: &[u8], flags: i32) -> bool {
    fnm(pattern, string, flags, true)
}

/// `strverscmp`: compare with runs of digits as numbers (`file9` before `file10`); the sign is the answer.
pub fn strverscmp(a: &[u8], b: &[u8]) -> i32 {
    let at = |s: &[u8], i: usize| s.get(i).copied().unwrap_or(0);
    let (mut i, mut j) = (0usize, 0usize);
    loop {
        let (x, y) = (at(a, i), at(b, j));
        if x.is_ascii_digit() && y.is_ascii_digit() {
            let (si, sj) = (i, j);
            while at(a, i).is_ascii_digit() {
                i += 1;
            }
            while at(b, j).is_ascii_digit() {
                j += 1;
            }
            // skip leading zeros, then a longer run is the larger number, equal lengths compare as text
            let (mut ai, mut bj) = (si, sj);
            while ai + 1 < i && at(a, ai) == b'0' {
                ai += 1;
            }
            while bj + 1 < j && at(b, bj) == b'0' {
                bj += 1;
            }
            let (la, lb) = (i - ai, j - bj);
            if la != lb {
                return if la < lb { -1 } else { 1 };
            }
            for k in 0..la {
                let (p, q) = (at(a, ai + k), at(b, bj + k));
                if p != q {
                    return i32::from(p) - i32::from(q);
                }
            }
            continue;
        }
        if x != y {
            return i32::from(x) - i32::from(y);
        }
        if x == 0 {
            return 0;
        }
        i += 1;
        j += 1;
    }
}
