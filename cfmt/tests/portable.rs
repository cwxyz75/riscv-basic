//! `fnmatch`, `getopt` and `strverscmp` (the pure cores in `cfmt::glob` and `cfmt::getopt`) against the host's own libc: generated
//! patterns, option strings and argument vectors. The host is Apple's libc locally and glibc in CI, so a case that the two disagree
//! about is left out: equivalence classes and collating symbols in brackets (`[[=a=]]`, `[[.a.]]`), unclosed brackets, GNU option
//! permutation (the host option string gets a leading `+` on glibc) and optional arguments (`::`), and, for `strverscmp` (glibc only), digit runs
//! with leading zeros, which glibc orders by a fractional rule this library does not have, a case class under `FNM_CASEFOLD`,
//! and where `optind` ends up after a missing argument at the end of the vector, and the value of `optarg` after a call that returned no argument (the host leaves the last one in it).
use cfmt::getopt::GetOpt;
use cfmt::glob::{fnmatch, strverscmp};
use std::ffi::{c_char, c_int, CString};

mod host {
    use std::ffi::{c_char, c_int};
    extern "C" {
        pub fn fnmatch(pattern: *const c_char, string: *const c_char, flags: c_int) -> c_int;
        pub fn getopt(argc: c_int, argv: *const *mut c_char, optstring: *const c_char) -> c_int;
        pub static mut optind: c_int;
        pub static mut opterr: c_int;
        pub static mut optopt: c_int;
        pub static mut optarg: *mut c_char;
        #[cfg(target_os = "macos")]
        pub static mut optreset: c_int;
        #[cfg(target_os = "linux")]
        pub fn strverscmp(a: *const c_char, b: *const c_char) -> c_int;
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
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len())]
    }
}

const FNM_NOESCAPE: i32 = 1;
const FNM_PATHNAME: i32 = 2;
const FNM_PERIOD: i32 = 4;
const FNM_CASEFOLD: i32 = 0x10;

#[test]
fn fnmatch_agrees_with_the_host() {
    let tokens = [
        "a",
        "b",
        "c",
        "A",
        "B",
        ".",
        "/",
        "-",
        "*",
        "*",
        "?",
        "?",
        "\\*",
        "\\a",
        "[a-c]",
        "[!a]",
        "[^b]",
        "[[:alpha:]]",
        "[[:digit:]]",
        "[[:upper:]]",
        "[]a]",
        "[a-]",
        "[!]a]",
        "[A-C]",
        "ab",
        "a.b",
    ];
    let chars = [
        "a", "b", "c", "A", "B", ".", "/", "-", "1", "]", "*", "?", "\\",
    ];
    let flag_sets = [
        0,
        FNM_PATHNAME,
        FNM_PERIOD,
        FNM_PATHNAME | FNM_PERIOD,
        FNM_NOESCAPE,
        FNM_CASEFOLD,
        FNM_CASEFOLD | FNM_PATHNAME,
    ];
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let (mut agreed, mut skipped) = (0, 0);
    for _ in 0..60_000 {
        let pat: String = (0..1 + rng.below(6)).map(|_| rng.pick(&tokens)).collect();
        let s: String = (0..rng.below(7)).map(|_| rng.pick(&chars)).collect();
        let flags = flag_sets[rng.below(flag_sets.len())];
        // glibc lets `*` before a bracket run across a `/` under FNM_PATHNAME (`fnmatch("*[[:alpha:]][!a]", "///ab", PATHNAME)` is a match
        // there); Apple's libc and the standard say no
        if flags & FNM_PATHNAME != 0 && pat.contains("*[") && s.contains('/') {
            skipped += 1;
            continue;
        }
        // a trailing lone backslash is undefined in the standard and the two libcs differ; so is a case class under CASEFOLD
        if (pat.ends_with('\\') && !pat.ends_with("\\\\"))
            || (flags & FNM_CASEFOLD != 0 && pat.contains("[[:upper:]]"))
        {
            skipped += 1;
            continue;
        }
        let (cp, cs) = (
            CString::new(pat.clone()).unwrap(),
            CString::new(s.clone()).unwrap(),
        );
        // SAFETY: NUL-terminated strings the host function only reads.
        let theirs = unsafe { host::fnmatch(cp.as_ptr(), cs.as_ptr(), flags as c_int) } == 0;
        let ours = fnmatch(pat.as_bytes(), s.as_bytes(), flags);
        assert_eq!(
            ours, theirs,
            "fnmatch({pat:?}, {s:?}, flags {flags:#x}): ours {ours}, host {theirs}"
        );
        agreed += 1;
    }
    assert!(
        agreed > 50_000,
        "only {agreed} cases compared ({skipped} skipped)"
    );
}

/// A fixed vector of the usual shell patterns.
#[test]
fn fnmatch_known_answers() {
    assert!(fnmatch(b"*.txt", b"a.txt", 0));
    assert!(!fnmatch(b"*.txt", b".txt", FNM_PERIOD));
    assert!(fnmatch(b"*", b"a/b", 0));
    assert!(!fnmatch(b"*", b"a/b", FNM_PATHNAME));
    assert!(fnmatch(b"[[:digit:]][a-c]", b"7b", 0));
    assert!(fnmatch(b"\\*", b"*", 0));
    assert!(!fnmatch(b"\\*", b"a", 0));
    assert!(fnmatch(b"a?c", b"abc", 0));
    assert!(fnmatch(b"ABC", b"abc", FNM_CASEFOLD));
    assert!(fnmatch(b"[!a]", b"b", 0));
    assert!(!fnmatch(b"[!a]", b"a", 0));
}

fn host_getopt_reset() {
    // SAFETY: single-threaded use of the host's getopt globals (the tests that use them hold `GETOPT_LOCK`).
    unsafe {
        #[cfg(target_os = "macos")]
        {
            host::optind = 1;
            host::optreset = 1;
        }
        #[cfg(target_os = "linux")]
        {
            host::optind = 0;
        }
        host::opterr = 0;
    }
}

static GETOPT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn getopt_agrees_with_the_host() {
    let _g = GETOPT_LOCK.lock().unwrap();
    // (glibc permutes the arguments unless the option string starts with `+`; POSIX getopt, ours and Apple's, stops at the first
    // non-option. `POSIXLY_CORRECT` would do it too but it is process-wide and changes how glibc's fnmatch reads `[^...]`.)
    let optstrings = [
        "ab", "abc:", "a:b", ":abc:", ":a:b:", "abcd", "c:d:", ":ab", "xa:",
    ];
    let words = [
        "-a", "-b", "-ab", "-abc", "-cX", "-c", "X", "--", "-", "foo", "-x", "-cfoo", "-bcfoo",
        "-d", "-ba", "-c-a", "-a-b", "-ac", "-acX", "-a:", "-:",
    ];
    let mut rng = Rng(0x1234_5678_9abc_def1);
    for _ in 0..20_000 {
        let optstring = rng.pick(&optstrings);
        let mut args: Vec<String> = vec!["prog".to_string()];
        for _ in 0..rng.below(7) {
            args.push(rng.pick(&words).to_string());
        }
        let cstrs: Vec<CString> = args
            .iter()
            .map(|a| CString::new(a.clone()).unwrap())
            .collect();
        let ptrs: Vec<*mut c_char> = cstrs.iter().map(|c| c.as_ptr() as *mut c_char).collect();
        let host_optstring = if cfg!(target_os = "linux") {
            format!("+{optstring}")
        } else {
            optstring.to_string()
        };
        let copt = CString::new(host_optstring).unwrap();
        // the host's whole run
        host_getopt_reset();
        let mut theirs = Vec::new();
        loop {
            // SAFETY: argv is argc valid NUL-terminated strings; the globals are the host's.
            let (r, oa, oi, oo) = unsafe {
                let r = host::getopt(ptrs.len() as c_int, ptrs.as_ptr(), copt.as_ptr());
                let oa = if host::optarg.is_null() {
                    None
                } else {
                    Some(std::ffi::CStr::from_ptr(host::optarg).to_bytes().to_vec())
                };
                (r, oa, host::optind, host::optopt)
            };
            let takes = r != '?' as c_int && r != ':' as c_int && r != -1;
            theirs.push((
                r,
                if takes { oa } else { None },
                oi as usize,
                if !takes && r != -1 { oo } else { 0 },
            ));
            if r == -1 || theirs.len() > 40 {
                break;
            }
        }
        // ours
        let argv: Vec<&[u8]> = args.iter().map(|a| a.as_bytes()).collect();
        let mut st = GetOpt::new();
        let mut ours = Vec::new();
        let mut missing = false;
        loop {
            let step = st.next(&argv, optstring.as_bytes());
            let oa = step.optarg.map(|(i, off)| argv[i][off..].to_vec());
            let oo = step.optopt.map_or(0, |c| c as c_int);
            let takes = step.ret != '?' as c_int && step.ret != ':' as c_int && step.ret != -1;
            ours.push((
                step.ret,
                if takes { oa } else { None },
                st.optind,
                if !takes && step.ret != -1 { oo } else { 0 },
            ));
            if step.ret == -1 || ours.len() > 40 {
                break;
            }
            // (where the index ends up after a missing argument at the end of the vector is not defined: the two libcs differ)
            if matches!(step.fault, Some(cfmt::getopt::Fault::MissingArgument(_))) {
                missing = true;
                break;
            }
        }
        if missing {
            // compare what came before the missing argument, and its return value
            let n = ours.len();
            assert_eq!(
                &ours[..n - 1],
                &theirs[..n - 1],
                "getopt({args:?}, {optstring:?}) before a missing argument"
            );
            assert_eq!(
                ours[n - 1].0,
                theirs[n - 1].0,
                "getopt({args:?}, {optstring:?}) at a missing argument"
            );
            continue;
        }
        assert_eq!(ours, theirs, "getopt({args:?}, {optstring:?})");
    }
}

#[test]
fn strverscmp_known_answers() {
    let sign = |x: i32| x.signum();
    assert_eq!(sign(strverscmp(b"file9", b"file10")), -1);
    assert_eq!(sign(strverscmp(b"file10", b"file9")), 1);
    assert_eq!(strverscmp(b"same1", b"same1"), 0);
    assert_eq!(sign(strverscmp(b"a", b"b")), -1);
    assert_eq!(sign(strverscmp(b"", b"a")), -1);
    assert_eq!(sign(strverscmp(b"x1y2", b"x1y10")), -1);
}

#[cfg(target_os = "linux")]
#[test]
fn strverscmp_agrees_with_glibc() {
    let tokens = [
        "a", "b", "1", "2", "9", "10", "11", "25", "100", ".", "-", "x", "0",
    ];
    let mut rng = Rng(0xdead_beef_cafe_f00d);
    let mut compared = 0;
    for _ in 0..60_000 {
        let a: String = (0..rng.below(5)).map(|_| rng.pick(&tokens)).collect();
        let b: String = (0..rng.below(5)).map(|_| rng.pick(&tokens)).collect();
        // digit runs with a leading zero: glibc's fractional rule is not ours
        let zero_run = |s: &str| {
            let b = s.as_bytes();
            (0..b.len()).any(|i| {
                b[i] == b'0'
                    && b.get(i + 1).is_some_and(|c| c.is_ascii_digit())
                    && (i == 0 || !b[i - 1].is_ascii_digit())
            }) || s.contains("00")
        };
        if zero_run(&a) || zero_run(&b) {
            continue;
        }
        let (ca, cb) = (
            CString::new(a.clone()).unwrap(),
            CString::new(b.clone()).unwrap(),
        );
        // SAFETY: NUL-terminated strings the host function only reads.
        let theirs = unsafe { host::strverscmp(ca.as_ptr(), cb.as_ptr()) }.signum();
        let ours = strverscmp(a.as_bytes(), b.as_bytes()).signum();
        assert_eq!(ours, theirs, "strverscmp({a:?}, {b:?})");
        compared += 1;
    }
    assert!(compared > 10_000);
}
