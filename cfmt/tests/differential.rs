//! The formatting and parsing cores against the host's own libc: tens of thousands of generated
//! `snprintf` conversions, and `strtod`/`strtol`/`strtoul` over generated and boundary strings. The
//! host is Apple's libc locally and glibc in CI: a case must agree with whichever libc runs it.
//!
//! Left out because the C standard leaves them undefined or the two libcs differ (so a
//! disagreement would say nothing about us): flags a conversion does not define (`#` on `%d`,
//! `0` on `%s`, ...), a precision on `%c` and `%p`, the sign of a NaN, `%p` of NULL, and the
//! subnormal and precision-rounding corners of `%a`.
use cfmt::{format, strtod, strtol, strtoul, Args, Len};
use std::collections::VecDeque;
use std::ffi::{c_char, c_int, c_long, c_void, CString};

/// The host's own libc, the oracle.
mod host {
    use std::ffi::{c_char, c_int, c_long};
    extern "C" {
        pub fn snprintf(buf: *mut c_char, size: usize, fmt: *const c_char, ...) -> c_int;
        pub fn strtod(s: *const c_char, end: *mut *mut c_char) -> f64;
        pub fn strtol(s: *const c_char, end: *mut *mut c_char, base: c_int) -> c_long;
        pub fn strtoul(s: *const c_char, end: *mut *mut c_char, base: c_int) -> std::ffi::c_ulong;
    }
}
use host::snprintf;

#[cfg(target_os = "macos")]
extern "C" {
    fn __error() -> *mut c_int;
}
#[cfg(target_os = "linux")]
extern "C" {
    fn __errno_location() -> *mut c_int;
}

fn errno() -> *mut c_int {
    // SAFETY: the host libc's errno accessor.
    #[cfg(target_os = "macos")]
    unsafe {
        __error()
    }
    // SAFETY: the host libc's errno accessor.
    #[cfg(target_os = "linux")]
    unsafe {
        __errno_location()
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len() as u64) as usize]
    }
}

#[derive(Clone, Debug)]
enum Val {
    I(i64),
    U(u64),
    D(f64),
    S(Vec<u8>),
    C(u8),
    P(u64),
}

struct List(VecDeque<Val>);
impl Args for List {
    fn int(&mut self, _: Len) -> i64 {
        match self.0.pop_front() {
            Some(Val::I(v)) => v,
            Some(Val::C(c)) => i64::from(c),
            other => panic!("int expected, got {other:?}"),
        }
    }
    fn uint(&mut self, _: Len) -> u64 {
        match self.0.pop_front() {
            Some(Val::U(v)) => v,
            other => panic!("uint expected, got {other:?}"),
        }
    }
    fn double(&mut self) -> f64 {
        match self.0.pop_front() {
            Some(Val::D(v)) => v,
            other => panic!("double expected, got {other:?}"),
        }
    }
    fn ptr(&mut self) -> u64 {
        match self.0.pop_front() {
            Some(Val::P(v)) => v,
            other => panic!("ptr expected, got {other:?}"),
        }
    }
    fn string(&mut self, _max: Option<usize>) -> Option<Vec<u8>> {
        match self.0.pop_front() {
            Some(Val::S(s)) => Some(s),
            other => panic!("string expected, got {other:?}"),
        }
    }
}

fn ours(fmt: &str, v: Val) -> Vec<u8> {
    let mut out = Vec::new();
    format(fmt.as_bytes(), &mut List(VecDeque::from([v])), &mut out).expect("formats");
    out
}

fn libc(fmt: &str, v: &Val, wide_int: bool) -> Vec<u8> {
    let f = CString::new(fmt).unwrap();
    let mut buf = vec![0u8; 4096];
    let n = {
        let b = buf.as_mut_ptr() as *mut c_char;
        // SAFETY: `buf` has room for 4096 bytes and snprintf never writes more than `size`; `f` is a
        // NUL-terminated format whose single conversion matches the argument passed.
        unsafe {
            match v {
                Val::I(i) if wide_int => snprintf(b, 4096, f.as_ptr(), *i as c_long),
                Val::I(i) => snprintf(b, 4096, f.as_ptr(), *i as c_int),
                Val::U(u) if wide_int => snprintf(b, 4096, f.as_ptr(), *u as std::ffi::c_ulong),
                Val::U(u) => snprintf(b, 4096, f.as_ptr(), *u as u32),
                Val::D(d) => snprintf(b, 4096, f.as_ptr(), *d),
                Val::C(c) => snprintf(b, 4096, f.as_ptr(), c_int::from(*c)),
                Val::S(s) => {
                    let cs = CString::new(s.clone()).unwrap();
                    snprintf(b, 4096, f.as_ptr(), cs.as_ptr())
                }
                Val::P(p) => snprintf(b, 4096, f.as_ptr(), *p as *const c_void),
            }
        }
    };
    assert!((0..4096).contains(&n), "libc could not format {fmt:?}");
    buf.truncate(n as usize);
    buf
}

const INTS: [i64; 14] = [
    0,
    1,
    -1,
    7,
    42,
    -42,
    255,
    256,
    65535,
    i32::MAX as i64,
    i32::MIN as i64,
    i64::MAX,
    i64::MIN,
    0x1234_5678_9abc,
];

fn random_int(r: &mut Rng) -> i64 {
    match r.below(4) {
        0 => *r.pick(&INTS),
        1 => (r.next() % 2000) as i64 - 1000,
        2 => r.next() as i64,
        _ => (r.next() as i64) >> r.below(60),
    }
}

fn random_double(r: &mut Rng, for_a: bool) -> f64 {
    loop {
        let x = match r.below(6) {
            0 => f64::from_bits(r.next()),
            1 => (r.next() % 2_000_000) as f64 / 1000.0 - 1000.0,
            2 => *r.pick(&[
                0.0,
                -0.0,
                0.5,
                1.0,
                -1.0,
                2.5,
                0.1,
                1e22,
                1e-5,
                123456789.0,
                1e300,
                5e-324,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::MAX,
                f64::MIN_POSITIVE,
                0.000_123_456,
                99999.95,
                9.9999995,
                0.999_999_5,
            ]),
            3 => (r.next() as i64 as f64) / (1u64 << r.below(40)) as f64,
            4 => 10f64.powi(r.below(40) as i32 - 20) * ((r.next() % 1000) as f64 / 7.0),
            _ => (r.below(100_000) as f64) / (10f64.powi(r.below(8) as i32)),
        };
        if x.is_nan() {
            continue;
        }
        if for_a && (x != 0.0 && x.abs() < f64::MIN_POSITIVE) {
            continue; // subnormals print differently in the two libcs
        }
        return x;
    }
}

#[test]
fn printf_matches_the_host_libc() {
    let mut r = Rng(0x2545_f491_4f6c_dd1d);
    let mut bad: Vec<String> = Vec::new();
    let mut runs = 0;
    for _ in 0..80_000 {
        let conv = *r.pick(b"diuxXocspfFeEgGaA");
        let numeric = !matches!(conv, b'c' | b's' | b'p');
        let signed = matches!(conv, b'd' | b'i')
            || matches!(conv, b'f' | b'F' | b'e' | b'E' | b'g' | b'G' | b'a' | b'A');
        let mut f = String::from("%");
        if r.below(3) == 0 {
            f.push('-');
        }
        if signed && r.below(4) == 0 {
            f.push(if r.below(2) == 0 { '+' } else { ' ' });
        }
        if matches!(
            conv,
            b'o' | b'x' | b'X' | b'f' | b'F' | b'e' | b'E' | b'g' | b'G' | b'a' | b'A'
        ) && r.below(4) == 0
        {
            f.push('#');
        }
        if numeric && r.below(4) == 0 {
            f.push('0');
        }
        if r.below(2) == 0 {
            f.push_str(&r.below(40).to_string());
        }
        let allow_prec = !matches!(conv, b'c' | b'p');
        if allow_prec && r.below(2) == 0 {
            f.push('.');
            if r.below(8) != 0 {
                f.push_str(&r.below(25).to_string());
            }
        }
        let int_conv = matches!(conv, b'd' | b'i' | b'u' | b'x' | b'X' | b'o');
        let mut wide = false;
        if int_conv {
            let l = *r.pick(&["", "", "hh", "h", "l", "ll", "z", "j", "t"]);
            wide = matches!(l, "l" | "ll" | "z" | "j" | "t");
            f.push_str(l);
        }
        f.push(conv as char);
        let v = match conv {
            b'd' | b'i' => Val::I(random_int(&mut r)),
            b'u' | b'x' | b'X' | b'o' => Val::U(random_int(&mut r) as u64),
            b'c' => Val::C(1 + (r.below(126)) as u8),
            b's' => Val::S((0..r.below(12)).map(|_| b'a' + r.below(26) as u8).collect()),
            b'p' => Val::P(r.next() | 1),
            b'a' | b'A' => Val::D(random_double(&mut r, true)),
            _ => Val::D(random_double(&mut r, false)),
        };
        // `%a` with a precision that must round the value is implementation-defined (Apple's libc
        // truncates where glibc rounds to nearest even): off Linux, only compare when the digits are
        // exact; CI's glibc checks the rounding too
        if matches!(conv, b'a' | b'A') {
            if let (Some(dot), Val::D(d)) = (f.find('.'), &v) {
                let digits: String = f[dot + 1..]
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect();
                let prec: usize = digits.parse().unwrap_or(0);
                if cfg!(not(target_os = "linux"))
                    && prec < 13
                    && d.is_finite()
                    && (d.to_bits() & ((1u64 << ((13 - prec) * 4)) - 1)) != 0
                {
                    continue;
                }
            }
        }
        runs += 1;
        let got = ours(&f, v.clone());
        let want = libc(&f, &v, wide);
        if got != want && bad.len() < 12 {
            bad.push(format!(
                "{f:?} {v:?}: ours {:?}, libc {:?}",
                String::from_utf8_lossy(&got),
                String::from_utf8_lossy(&want)
            ));
        }
    }
    assert!(
        bad.is_empty(),
        "{} of {runs} cases differ, first:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

#[test]
fn star_width_and_precision_match_embedded_numbers() {
    // `%*d` with a negative width is `%-*d`; a negative `*` precision is no precision
    let run = |fmt: &str, args: Vec<Val>| {
        let mut out = Vec::new();
        format(fmt.as_bytes(), &mut List(args.into()), &mut out).unwrap();
        String::from_utf8(out).unwrap()
    };
    assert_eq!(run("%*d|", vec![Val::I(5), Val::I(42)]), "   42|");
    assert_eq!(run("%*d|", vec![Val::I(-5), Val::I(42)]), "42   |");
    assert_eq!(run("%.*f|", vec![Val::I(2), Val::D(3.25159)]), "3.25|");
    assert_eq!(run("%.*f|", vec![Val::I(-1), Val::D(3.5)]), "3.500000|");
    assert_eq!(
        run("%0*.*f|", vec![Val::I(9), Val::I(3), Val::D(-2.5)]),
        "-0002.500|"
    );
    assert_eq!(
        run("%%|%c|%s|", vec![Val::C(b'x'), Val::S(b"hi".to_vec())]),
        "%|x|hi|"
    );
}

#[test]
fn refused_and_malformed_formats() {
    use cfmt::FmtError;
    let go = |f: &str| format(f.as_bytes(), &mut List(VecDeque::new()), &mut Vec::new());
    assert_eq!(go("100%"), Err(FmtError::Truncated));
    assert_eq!(go("%5"), Err(FmtError::Truncated));
    assert_eq!(go("%y"), Err(FmtError::BadConversion(b'y')));
    assert_eq!(go("%n"), Err(FmtError::Unsupported(b'n')));
    assert_eq!(go("%Lf"), Err(FmtError::Unsupported(b'L')));
}

// ---- parsing ----

fn libc_strtod(s: &[u8]) -> (f64, usize, bool) {
    let c = CString::new(s).unwrap();
    // SAFETY: `c` is NUL-terminated; `end` receives a pointer inside it; errno is the host's.
    unsafe {
        *errno() = 0;
        let mut end: *mut c_char = std::ptr::null_mut();
        let v = host::strtod(c.as_ptr(), &mut end);
        (v, end as usize - c.as_ptr() as usize, *errno() == 34)
    }
}

fn gen_float_string(r: &mut Rng) -> Vec<u8> {
    let mut s = String::new();
    if r.below(4) == 0 {
        s.push_str(r.pick(&[" ", "\t", "  \n", "\r "]));
    }
    if r.below(3) == 0 {
        s.push(if r.below(2) == 0 { '-' } else { '+' });
    }
    match r.below(10) {
        0 => s.push_str(r.pick(&[
            "inf", "INF", "infinity", "Infinity", "infin", "nan", "NaN", "nan(0x1)", "nan(", "i",
            "-",
        ])),
        1 | 2 => {
            // hexadecimal
            s.push_str(if r.below(2) == 0 { "0x" } else { "0X" });
            let nd = r.below(20);
            for _ in 0..nd {
                s.push(*r.pick(&['0', '1', '7', '8', 'f', 'F', 'a', '3', 'c']));
            }
            if r.below(2) == 0 {
                s.push('.');
                for _ in 0..r.below(18) {
                    s.push(*r.pick(&['0', '1', '8', 'f', 'a', '5']));
                }
            }
            if r.below(2) == 0 {
                s.push(if r.below(2) == 0 { 'p' } else { 'P' });
                if r.below(2) == 0 {
                    s.push(if r.below(2) == 0 { '-' } else { '+' });
                }
                s.push_str(&r.below(1200).to_string());
            }
        }
        _ => {
            for _ in 0..r.below(25) {
                s.push(char::from(b'0' + r.below(10) as u8));
            }
            if r.below(2) == 0 {
                s.push('.');
                for _ in 0..r.below(25) {
                    s.push(char::from(b'0' + r.below(10) as u8));
                }
            }
            if r.below(2) == 0 {
                s.push(if r.below(2) == 0 { 'e' } else { 'E' });
                if r.below(2) == 0 {
                    s.push(if r.below(2) == 0 { '-' } else { '+' });
                }
                s.push_str(&r.below(400).to_string());
            }
        }
    }
    if r.below(3) == 0 {
        s.push_str(r.pick(&["x", " 12", "e", "e+", ".", "p3", "abc", "0x"]));
    }
    s.into_bytes()
}

#[test]
fn strtod_matches_the_host_libc() {
    let mut r = Rng(0x9e37_79b9_7f4a_7c15);
    let mut bad = Vec::new();
    let mut cases: Vec<Vec<u8>> = (0..60_000).map(|_| gen_float_string(&mut r)).collect();
    for s in [
        "0",
        "-0",
        "1",
        ".5",
        "5.",
        "1e",
        "1e+",
        "0x",
        "0x.",
        "0x.8",
        "0x1p",
        "0x1p-1074",
        "0x1p-1075",
        "0x1.fffffffffffff8p1023",
        "0x1p1024",
        "1.7976931348623157e308",
        "1.7976931348623159e308",
        "4.9e-324",
        "2e-324",
        "2.5e-324",
        "0.1",
        "9007199254740993",
        "123456789012345678901234567890",
        "1e-400",
        "1e400",
        "0x1.8p+1",
        "  +0x10",
        "0xg",
        "infx",
        "nanx",
        "",
    ] {
        cases.push(s.as_bytes().to_vec());
    }
    for s in &cases {
        let got = strtod(s);
        let (v, len, erange) = libc_strtod(s);
        let same_value = got.value.to_bits() == v.to_bits() || (got.value.is_nan() && v.is_nan());
        // an ERANGE on a subnormal result is glibc's business (it flags inexact denormals); ours
        // flags overflow and underflow to zero only
        let range_ok =
            got.range_error == erange || (v != 0.0 && v.is_finite() && !got.range_error && erange);
        if !(same_value && got.len == len && range_ok) && bad.len() < 12 {
            bad.push(format!(
                "{:?}: ours ({:?}, {}, {}), libc ({v:?}, {len}, {erange})",
                String::from_utf8_lossy(s),
                got.value,
                got.len,
                got.range_error
            ));
        }
    }
    assert!(bad.is_empty(), "strtod differs:\n{}", bad.join("\n"));
}

fn gen_int_string(r: &mut Rng) -> (Vec<u8>, u32) {
    let mut s = String::new();
    if r.below(4) == 0 {
        s.push_str(r.pick(&[" ", "\t", "\n "]));
    }
    if r.below(3) == 0 {
        s.push(if r.below(2) == 0 { '-' } else { '+' });
    }
    let base = *r.pick(&[0, 0, 10, 10, 16, 8, 2, 36, 7, 3]);
    if r.below(4) == 0 {
        s.push_str(r.pick(&["0x", "0X", "0", "0b", "0x0"]));
    }
    for _ in 0..1 + r.below(24) {
        s.push(*r.pick(&['0', '1', '2', '7', '8', '9', 'a', 'f', 'g', 'z', 'Z', 'F']));
    }
    if r.below(4) == 0 {
        s.push_str(r.pick(&["x", " 5", ".", "-"]));
    }
    (s.into_bytes(), base)
}

#[test]
fn strtol_and_strtoul_match_the_host_libc() {
    let mut r = Rng(0xdead_beef_cafe_f00d);
    let mut bad = Vec::new();
    let mut cases: Vec<(Vec<u8>, u32)> = (0..60_000).map(|_| gen_int_string(&mut r)).collect();
    for (s, b) in [
        ("9223372036854775807", 10),
        ("9223372036854775808", 10),
        ("-9223372036854775808", 10),
        ("-9223372036854775809", 10),
        ("18446744073709551615", 10),
        ("18446744073709551616", 10),
        ("-1", 10),
        ("0x", 16),
        ("0x", 0),
        ("0xz", 0),
        ("08", 0),
        ("  -0x7fffffffffffffff", 0),
        ("", 10),
        ("z", 36),
        ("1", 1),
        ("1", 37),
    ] {
        cases.push((s.as_bytes().to_vec(), b));
    }
    for (s, base) in &cases {
        let c = CString::new(s.clone()).unwrap();
        // SAFETY: as in `libc_strtod`.
        let (lv, lend, lerr, uv, uend, uerr) = unsafe {
            *errno() = 0;
            let mut end: *mut c_char = std::ptr::null_mut();
            let lv = host::strtol(c.as_ptr(), &mut end, *base as c_int);
            let lend = end as usize - c.as_ptr() as usize;
            let lerr = *errno() == 34;
            *errno() = 0;
            let uv = host::strtoul(c.as_ptr(), &mut end, *base as c_int);
            let uend = end as usize - c.as_ptr() as usize;
            (lv as i64, lend, lerr, uv as u64, uend, *errno() == 34)
        };
        let o = strtol(s, *base);
        let u = strtoul(s, *base);
        // (an invalid base: libc sets EINVAL and consumes nothing; ours consumes nothing as well)
        if (o.value != lv
            || o.len != lend
            || o.range_error != lerr
            || u.value != uv
            || u.len != uend
            || u.range_error != uerr)
            && bad.len() < 12
        {
            bad.push(format!("{:?} base {base}: ours l({}, {}, {}) u({}, {}, {}), libc l({lv}, {lend}, {lerr}) u({uv}, {uend}, {uerr})", String::from_utf8_lossy(s), o.value, o.len, o.range_error, u.value, u.len, u.range_error));
        }
    }
    assert!(bad.is_empty(), "strtol/strtoul differ:\n{}", bad.join("\n"));
}
