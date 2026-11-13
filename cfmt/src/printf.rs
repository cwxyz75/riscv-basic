//! `printf`'s formatting engine (C11 7.21.6.1 as the C library needs it): `%d %i %u %x %X %o %c %s
//! %p %f %F %e %E %g %G %a %A %%`, flags `- + space # 0`, width and precision (also `*`), and the
//! length modifiers `hh h l ll z j t`. `%n` and `L` (long double) are refused: nothing here writes
//! through an argument pointer and RISC-V's `long double` is binary128.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Len {
    None,
    HH,
    H,
    L,
    LL,
    Z,
    J,
    T,
}

/// Where the arguments come from (a C `va_list`, or a list in a test). Each call takes the next one.
pub trait Args {
    /// A signed integer argument for the given length modifier (the value as passed; the formatter
    /// narrows it, so `%hhd` of 300 prints 44).
    fn int(&mut self, len: Len) -> i64;
    fn uint(&mut self, len: Len) -> u64;
    fn double(&mut self) -> f64;
    /// A pointer argument (`%p`).
    fn ptr(&mut self) -> u64;
    /// A C string argument (`%s`), at most `max` bytes of it (a precision: the string need not be
    /// terminated within them); `None` for a null pointer.
    fn string(&mut self, max: Option<usize>) -> Option<Vec<u8>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FmtError {
    /// The format ends inside a conversion.
    Truncated,
    /// Not a conversion this engine knows.
    BadConversion(u8),
    /// `%n` or a `long double` modifier.
    Unsupported(u8),
}

#[derive(Default, Clone, Copy)]
struct Spec {
    left: bool,
    plus: bool,
    space: bool,
    alt: bool,
    zero: bool,
    width: usize,
    prec: Option<usize>,
}

fn pad(out: &mut Vec<u8>, spec: &Spec, prefix: &[u8], body: &[u8], zero_ok: bool) {
    let len = prefix.len() + body.len();
    let fill = spec.width.saturating_sub(len);
    if spec.left {
        out.extend_from_slice(prefix);
        out.extend_from_slice(body);
        out.resize(out.len() + fill, b' ');
    } else if spec.zero && zero_ok {
        out.extend_from_slice(prefix);
        out.resize(out.len() + fill, b'0');
        out.extend_from_slice(body);
    } else {
        out.resize(out.len() + fill, b' ');
        out.extend_from_slice(prefix);
        out.extend_from_slice(body);
    }
}

fn sign_prefix(spec: &Spec, negative: bool) -> &'static [u8] {
    if negative {
        b"-"
    } else if spec.plus {
        b"+"
    } else if spec.space {
        b" "
    } else {
        b""
    }
}

fn int_conv(out: &mut Vec<u8>, spec: &Spec, conv: u8, negative: bool, mag: u64) {
    let (base, upper) = match conv {
        b'o' => (8, false),
        b'x' => (16, false),
        b'X' => (16, true),
        _ => (10, false),
    };
    let signed = matches!(conv, b'd' | b'i');
    let mut digits: Vec<u8> = Vec::new();
    let mut m = mag;
    while m > 0 {
        let d = (m % base) as u8;
        digits.push(match d {
            0..=9 => b'0' + d,
            _ if upper => b'A' + d - 10,
            _ => b'a' + d - 10,
        });
        m /= base;
    }
    if digits.is_empty() {
        digits.push(b'0'); // zero is one digit (unless the precision says none)
    }
    digits.reverse();
    if spec.prec == Some(0) && mag == 0 {
        digits.clear();
    }
    if let Some(p) = spec.prec {
        if digits.len() < p {
            let mut z = alloc::vec![b'0'; p - digits.len()];
            z.extend_from_slice(&digits);
            digits = z;
        }
    }
    let mut prefix: Vec<u8> = Vec::new();
    if signed {
        prefix.extend_from_slice(sign_prefix(spec, negative));
    }
    if spec.alt {
        if base == 16 && mag != 0 {
            prefix.extend_from_slice(if upper { b"0X" } else { b"0x" });
        } else if base == 8 && digits.first() != Some(&b'0') {
            digits.insert(0, b'0');
        }
    }
    pad(out, spec, &prefix, &digits, spec.prec.is_none());
}

fn narrow_signed(v: i64, len: Len) -> i64 {
    match len {
        Len::HH => i64::from(v as i8),
        Len::H => i64::from(v as i16),
        Len::None => i64::from(v as i32),
        _ => v,
    }
}

fn narrow_unsigned(v: u64, len: Len) -> u64 {
    match len {
        Len::HH => u64::from(v as u8),
        Len::H => u64::from(v as u16),
        Len::None => u64::from(v as u32),
        _ => v,
    }
}

// ---- floating point ----

/// `d.ddd` digits and exponent of `x >= 0` with `prec` digits after the point, correctly rounded.
fn sci_parts(x: f64, prec: usize) -> (String, i32) {
    let s = format!("{:.*e}", prec, x); // "d.ddde-5" or "de5"
    let (mant, exp) = s.split_once('e').unwrap_or((&s, "0"));
    (String::from(mant), exp.parse().unwrap_or(0))
}

fn exp_suffix(e: i32, upper: bool) -> String {
    let sign = if e < 0 { '-' } else { '+' };
    let a = e.unsigned_abs();
    let c = if upper { 'E' } else { 'e' };
    if a < 10 {
        format!("{c}{sign}0{a}")
    } else {
        format!("{c}{sign}{a}")
    }
}

fn strip_zeros(s: &mut String) {
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
}

fn float_body(spec: &Spec, conv: u8, x: f64) -> String {
    let upper = conv.is_ascii_uppercase();
    let prec = spec.prec.unwrap_or(6);
    match conv.to_ascii_lowercase() {
        b'f' => {
            let mut s = format!("{:.*}", prec, x);
            if prec == 0 && spec.alt {
                s.push('.');
            }
            s
        }
        b'e' => {
            let (mut m, e) = sci_parts(x, prec);
            if prec == 0 && spec.alt {
                m.push('.');
            }
            m + &exp_suffix(e, upper)
        }
        b'g' => {
            let p = if prec == 0 { 1 } else { prec };
            let (_, x_exp) = sci_parts(x, p - 1);
            if (x_exp as i64) >= -4 && (x_exp as i64) < p as i64 {
                let mut s = format!("{:.*}", (p as i64 - 1 - x_exp as i64) as usize, x);
                if !spec.alt {
                    strip_zeros(&mut s);
                } else if !s.contains('.') {
                    s.push('.');
                }
                s
            } else {
                let (mut m, e) = sci_parts(x, p - 1);
                if !spec.alt {
                    strip_zeros(&mut m);
                } else if !m.contains('.') {
                    m.push('.');
                }
                m + &exp_suffix(e, upper)
            }
        }
        _ => hex_float(spec, x, upper),
    }
}

/// `%a`: `0x1.8p+1` (glibc style: a subnormal keeps its leading zero, `0x0.0000000000001p-1022`).
fn hex_float(spec: &Spec, x: f64, upper: bool) -> String {
    let bits = x.to_bits();
    let exp_bits = ((bits >> 52) & 0x7ff) as i32;
    let mut frac = bits & ((1u64 << 52) - 1);
    let (mut lead, e) = match (exp_bits, frac) {
        (0, 0) => (0u64, 0i32),
        (0, _) => (0, -1022),
        _ => (1, exp_bits - 1023),
    };
    // 13 hex digits of fraction; round to the requested precision (half to even)
    let mut digits = 13usize;
    if let Some(p) = spec.prec {
        if p < 13 {
            let drop = (13 - p) * 4;
            let rest = frac & ((1u64 << drop) - 1);
            frac >>= drop;
            let half = 1u64 << (drop - 1);
            if rest > half || (rest == half && (frac & 1 == 1 || (p == 0 && lead & 1 == 1))) {
                frac += 1;
                if frac >> (p * 4) != 0 {
                    frac = 0; // carried out of the fraction into the leading digit (0x2p+0, as glibc)
                    lead += 1;
                }
            }
            digits = p;
        }
    }
    let mut hex = String::new();
    if digits > 0 {
        let width = digits.min(13);
        hex = format!("{:0width$x}", frac, width = width);
        if spec.prec.is_none() {
            while hex.ends_with('0') {
                hex.pop();
            }
        } else if let Some(p) = spec.prec {
            while hex.len() < p {
                hex.push('0');
            }
        }
    }
    let mut s = format!("{lead:x}");
    if !hex.is_empty() || spec.alt {
        s.push('.');
    }
    s.push_str(&hex);
    let sign = if e < 0 { '-' } else { '+' };
    let out = format!("0x{s}p{sign}{}", e.unsigned_abs());
    if upper {
        out.to_ascii_uppercase()
    } else {
        out
    }
}

fn float_conv(out: &mut Vec<u8>, spec: &Spec, conv: u8, x: f64) {
    let negative = x.is_sign_negative();
    let prefix = sign_prefix(spec, negative);
    let upper = conv.is_ascii_uppercase();
    if x.is_nan() || x.is_infinite() {
        let word: &[u8] = match (x.is_nan(), upper) {
            (true, false) => b"nan",
            (true, true) => b"NAN",
            (false, false) => b"inf",
            (false, true) => b"INF",
        };
        pad(out, spec, prefix, word, false);
        return;
    }
    let body = float_body(spec, conv, x.abs());
    // `0x` of %a is a prefix for zero padding: the zeros go between it and the digits
    if conv.eq_ignore_ascii_case(&b'a') {
        let (p0, rest) = body.split_at(2);
        let mut full = Vec::from(prefix);
        full.extend_from_slice(p0.as_bytes());
        pad(out, spec, &full, rest.as_bytes(), true);
    } else {
        pad(out, spec, prefix, body.as_bytes(), true);
    }
}

/// Format `fmt` with `args`, appending to `out`.
pub fn format(fmt: &[u8], args: &mut dyn Args, out: &mut Vec<u8>) -> Result<(), FmtError> {
    let mut i = 0;
    while i < fmt.len() {
        if fmt[i] != b'%' {
            out.push(fmt[i]);
            i += 1;
            continue;
        }
        i += 1;
        let mut spec = Spec::default();
        // flags
        while let Some(&c) = fmt.get(i) {
            match c {
                b'-' => spec.left = true,
                b'+' => spec.plus = true,
                b' ' => spec.space = true,
                b'#' => spec.alt = true,
                b'0' => spec.zero = true,
                _ => break,
            }
            i += 1;
        }
        // width
        if fmt.get(i) == Some(&b'*') {
            let w = args.int(Len::None) as i32;
            if w < 0 {
                spec.left = true;
            }
            spec.width = w.unsigned_abs() as usize;
            i += 1;
        } else {
            while let Some(d) = fmt.get(i).filter(|d| d.is_ascii_digit()) {
                spec.width = spec
                    .width
                    .saturating_mul(10)
                    .saturating_add(usize::from(d - b'0'));
                i += 1;
            }
        }
        // precision
        if fmt.get(i) == Some(&b'.') {
            i += 1;
            if fmt.get(i) == Some(&b'*') {
                let p = args.int(Len::None) as i32;
                spec.prec = (p >= 0).then_some(p as usize);
                i += 1;
            } else {
                let mut p = 0usize;
                while let Some(d) = fmt.get(i).filter(|d| d.is_ascii_digit()) {
                    p = p.saturating_mul(10).saturating_add(usize::from(d - b'0'));
                    i += 1;
                }
                spec.prec = Some(p);
            }
        }
        // length
        let mut len = Len::None;
        loop {
            match fmt.get(i) {
                Some(b'h') => {
                    len = if len == Len::H { Len::HH } else { Len::H };
                }
                Some(b'l') => {
                    len = if len == Len::L { Len::LL } else { Len::L };
                }
                Some(b'z') => len = Len::Z,
                Some(b'j') => len = Len::J,
                Some(b't') => len = Len::T,
                Some(b'L') => return Err(FmtError::Unsupported(b'L')),
                _ => break,
            }
            i += 1;
        }
        let Some(&conv) = fmt.get(i) else {
            return Err(FmtError::Truncated);
        };
        i += 1;
        match conv {
            b'%' => out.push(b'%'),
            b'd' | b'i' => {
                let v = narrow_signed(args.int(len), len);
                int_conv(out, &spec, conv, v < 0, v.unsigned_abs());
            }
            b'u' | b'x' | b'X' | b'o' => {
                let v = narrow_unsigned(args.uint(len), len);
                int_conv(out, &spec, conv, false, v);
            }
            b'c' => {
                let c = args.int(Len::None) as u8;
                pad(out, &spec, b"", &[c], false);
            }
            b's' => {
                let s = args.string(spec.prec);
                let body: &[u8] = match &s {
                    Some(s) => &s[..spec.prec.map_or(s.len(), |p| p.min(s.len()))],
                    None => b"(null)",
                };
                let body = match (&s, spec.prec) {
                    (None, Some(p)) if p < 6 => &body[..0],
                    _ => body,
                };
                pad(out, &spec, b"", body, false);
            }
            b'p' => {
                let v = args.ptr();
                if v == 0 {
                    pad(out, &spec, b"", b"(nil)", false);
                } else {
                    let s = Spec {
                        alt: true,
                        prec: None,
                        ..spec
                    };
                    int_conv(out, &s, b'x', false, v);
                }
            }
            b'f' | b'F' | b'e' | b'E' | b'g' | b'G' | b'a' | b'A' => {
                float_conv(out, &spec, conv, args.double());
            }
            b'n' => return Err(FmtError::Unsupported(b'n')),
            other => return Err(FmtError::BadConversion(other)),
        }
    }
    Ok(())
}
