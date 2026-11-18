//! `sscanf`, `fnmatch`, `getopt` and `strverscmp` (the pure cores in `cfmt::glob` and `cfmt::getopt`) against the host's own libc: generated
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

// (ours are the Linux values; Apple's libc swaps the first two, `host_flags` translates)
const FNM_PATHNAME: i32 = 1;
const FNM_NOESCAPE: i32 = 2;
const FNM_PERIOD: i32 = 4;
const FNM_CASEFOLD: i32 = 0x10;

fn host_flags(f: i32) -> i32 {
    if cfg!(target_os = "macos") {
        (f & !3) | ((f & 1) << 1) | ((f & 2) >> 1)
    } else {
        f
    }
}

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
        let theirs =
            unsafe { host::fnmatch(cp.as_ptr(), cs.as_ptr(), host_flags(flags) as c_int) } == 0;
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

// ---- sscanf ----

mod host_scan {
    use std::ffi::{c_char, c_int};
    extern "C" {
        pub fn sscanf(s: *const c_char, fmt: *const c_char, ...) -> c_int;
    }
}

/// What the host's `sscanf` did with `fmt` on `input`: the return value and, for each of the conversions in `kinds`, the bytes it wrote
/// (each into a 64-byte buffer that started as 0xAA, so "wrote nothing" shows). Up to four conversions are supported.
fn host_sscanf(input: &str, fmt: &str, nargs: usize) -> (c_int, Vec<Vec<u8>>) {
    let s = CString::new(input).unwrap();
    let f = CString::new(fmt).unwrap();
    let mut bufs = vec![[0xAAu8; 64]; 4];
    let p: Vec<*mut u8> = bufs.iter_mut().map(|b| b.as_mut_ptr()).collect();
    let r = unsafe {
        match nargs {
            0 => host_scan::sscanf(s.as_ptr(), f.as_ptr()),
            1 => host_scan::sscanf(s.as_ptr(), f.as_ptr(), p[0]),
            2 => host_scan::sscanf(s.as_ptr(), f.as_ptr(), p[0], p[1]),
            3 => host_scan::sscanf(s.as_ptr(), f.as_ptr(), p[0], p[1], p[2]),
            _ => host_scan::sscanf(s.as_ptr(), f.as_ptr(), p[0], p[1], p[2], p[3]),
        }
    };
    (r, bufs.iter().map(|b| b.to_vec()).collect())
}

/// The same, through `cfmt::scan`: the bytes the library would store in the same buffers.
fn our_sscanf(input: &str, fmt: &str, nargs: usize) -> (c_int, Vec<Vec<u8>>) {
    use cfmt::scan::{scan, Item};
    let r = scan(input.as_bytes(), fmt.as_bytes());
    let mut bufs = vec![vec![0xAAu8; 64]; 4];
    assert!(r.items.len() <= nargs.max(r.items.len()));
    for (k, item) in r.items.iter().enumerate() {
        let b = &mut bufs[k];
        match item {
            Item::Int { value, len } => match len {
                0 => b[..4].copy_from_slice(&(*value as u32).to_le_bytes()),
                1 | 2 | 5 => b[..8].copy_from_slice(&value.to_le_bytes()),
                3 => b[..2].copy_from_slice(&(*value as u16).to_le_bytes()),
                _ => b[0] = *value as u8,
            },
            Item::Float { value, len } => {
                if *len == 1 || *len == 5 {
                    b[..8].copy_from_slice(&value.to_le_bytes());
                } else {
                    b[..4].copy_from_slice(&(*value as f32).to_le_bytes());
                }
            }
            Item::Str(v) => {
                b[..v.len()].copy_from_slice(v);
                b[v.len()] = 0;
            }
            Item::Chars(v) => b[..v.len()].copy_from_slice(v),
            Item::Count(n) => b[..4].copy_from_slice(&n.to_le_bytes()),
        }
    }
    (r.ret, bufs)
}

#[test]
fn sscanf_matches_the_host() {
    let mut rng = Rng(0x5eed_5ca1);
    let convs = [
        "%d", "%i", "%u", "%o", "%x", "%ld", "%hd", "%hhd", "%lld", "%3d", "%s", "%4s", "%c",
        "%[a-c]", "%[^,]", "%[0-9]", "%n", "%*d", "%*s", "%f", "%lf", "%%",
    ];
    let seps = ["", " ", ",", "-", " , ", "x", ":"];
    let tokens = [
        "0", "7", "-12", "+5", "42", "0x1f", "017", "999999", "abc", "a,b", "hello", "w0rld",
        "3.5", "-2.25e2", "1e3", "%", " ", "  7", "", "zz", "-", "12abc",
    ];
    let mut bad = Vec::new();
    for _ in 0..40000 {
        let nconv = 1 + rng.below(3);
        let mut fmt = String::new();
        let mut nargs = 0;
        for k in 0..nconv {
            if k > 0 {
                fmt.push_str(rng.pick(&seps));
            }
            let c = rng.pick(&convs);
            fmt.push_str(c);
            if !c.contains('*') && c != "%%" {
                nargs += 1;
            }
        }
        let mut input = String::new();
        for k in 0..1 + rng.below(4) {
            if k > 0 {
                input.push_str(rng.pick(&seps));
            }
            input.push_str(rng.pick(&tokens));
        }
        if nargs > 4 {
            continue;
        }
        // (the host needs an argument per conversion; float conversions write 4 or 8 bytes: compare what was written)
        let h = host_sscanf(&input, &fmt, nargs);
        let o = our_sscanf(&input, &fmt, nargs);
        if h != o {
            bad.push(format!(
                "fmt {fmt:?} input {input:?}: host {:?} ours {:?}",
                h.0, o.0
            ));
        }
    }
    assert!(
        bad.is_empty(),
        "{} disagreements, first ones:\n{}",
        bad.len(),
        bad.iter().take(12).cloned().collect::<Vec<_>>().join("\n")
    );
}

// ---- the signal table ----

mod host_sig {
    use std::ffi::c_int;
    #[cfg(target_os = "macos")]
    #[repr(C)]
    pub struct SigAction {
        pub handler: usize,
        pub mask: u32,
        pub flags: c_int,
    }
    #[cfg(target_os = "linux")]
    #[repr(C)]
    #[allow(dead_code)] // (the kernel's layout has a field the test never reads)
    pub struct SigAction {
        pub handler: usize,
        pub mask: [u64; 16],
        pub flags: c_int,
        pub restorer: usize,
    }
    #[cfg(target_os = "macos")]
    pub type SigSet = u32;
    #[cfg(target_os = "linux")]
    pub type SigSet = [u64; 16];
    extern "C" {
        pub fn sigaction(sig: c_int, act: *const SigAction, old: *mut SigAction) -> c_int;
        pub fn sigprocmask(how: c_int, set: *const SigSet, old: *mut SigSet) -> c_int;
        pub fn sigemptyset(set: *mut SigSet) -> c_int;
        pub fn sigaddset(set: *mut SigSet, sig: c_int) -> c_int;
        pub fn sigismember(set: *const SigSet, sig: c_int) -> c_int;
        pub fn raise(sig: c_int) -> c_int;
    }
    #[cfg(target_os = "macos")]
    pub const NODEFER: c_int = 0x10;
    #[cfg(target_os = "macos")]
    pub const RESETHAND: c_int = 0x4;
    #[cfg(target_os = "linux")]
    pub const NODEFER: c_int = 0x4000_0000;
    #[cfg(target_os = "linux")]
    pub const RESETHAND: c_int = 0x8000_0000u32 as c_int;
    pub const SIG_BLOCK: c_int = if cfg!(target_os = "macos") { 1 } else { 0 };
    pub const SIG_UNBLOCK: c_int = if cfg!(target_os = "macos") { 2 } else { 1 };
    pub const SIGS: [c_int; 3] = [
        1,
        if cfg!(target_os = "macos") { 30 } else { 10 },
        if cfg!(target_os = "macos") { 31 } else { 12 },
    ];
}

/// One scenario for three signals: each handler logs, then raises some signals (only the first time it runs).
#[derive(Clone)]
struct SigCfg {
    mask: [bool; 3],
    nodefer: bool,
    resethand: bool,
    raises: Vec<usize>,
}

static SIG_LOG: std::sync::Mutex<Vec<(u8, usize, u8)>> = std::sync::Mutex::new(Vec::new());
static SIG_CFGS: std::sync::Mutex<Vec<SigCfg>> = std::sync::Mutex::new(Vec::new());
static SIG_SEEN: std::sync::Mutex<[bool; 3]> = std::sync::Mutex::new([false; 3]);

fn host_blocked_bits() -> u8 {
    let mut cur: host_sig::SigSet = Default::default();
    unsafe {
        host_sig::sigprocmask(host_sig::SIG_BLOCK, std::ptr::null(), &mut cur);
    }
    let mut b = 0;
    for (k, s) in host_sig::SIGS.iter().enumerate() {
        if unsafe { host_sig::sigismember(&cur, *s) } == 1 {
            b |= 1 << k;
        }
    }
    b
}

fn host_install(k: usize, cfg: &SigCfg, handler: usize) {
    let mut set: host_sig::SigSet = Default::default();
    unsafe {
        host_sig::sigemptyset(&mut set);
        for (m, on) in cfg.mask.iter().enumerate() {
            if *on {
                host_sig::sigaddset(&mut set, host_sig::SIGS[m]);
            }
        }
    }
    let flags = if cfg.nodefer { host_sig::NODEFER } else { 0 }
        | if cfg.resethand {
            host_sig::RESETHAND
        } else {
            0
        };
    #[cfg(target_os = "macos")]
    let act = host_sig::SigAction {
        handler,
        mask: set,
        flags,
    };
    #[cfg(target_os = "linux")]
    let act = host_sig::SigAction {
        handler,
        mask: set,
        flags,
        restorer: 0,
    };
    assert_eq!(
        unsafe { host_sig::sigaction(host_sig::SIGS[k], &act, std::ptr::null_mut()) },
        0
    );
}

extern "C" fn host_handler(sig: c_int) {
    let k = host_sig::SIGS.iter().position(|s| *s == sig).unwrap();
    let cfg = SIG_CFGS.lock().unwrap()[k].clone();
    // (a one-shot handler is the default now: observe it, then install it again so a later raise does not end the process)
    let mut old = std::mem::MaybeUninit::<host_sig::SigAction>::zeroed();
    unsafe { host_sig::sigaction(sig, std::ptr::null(), old.as_mut_ptr()) };
    let was_default = unsafe { old.assume_init_ref().handler } == 0;
    SIG_LOG.lock().unwrap().push((
        b'e',
        k,
        host_blocked_bits() | if was_default { 8 } else { 0 },
    ));
    if was_default {
        host_install(k, &cfg, host_handler as *const () as usize);
    }
    let first = !std::mem::replace(&mut SIG_SEEN.lock().unwrap()[k], true);
    if first {
        for r in &cfg.raises {
            unsafe { host_sig::raise(host_sig::SIGS[*r]) };
        }
    }
    SIG_LOG.lock().unwrap().push((b'x', k, 0));
}

/// The same scenario on `SigTable`.
/// Also says whether two different signals were ever ready together: the order between them is the system's (unspecified by POSIX,
/// and Apple's differs from Linux's), so such scenarios are not compared.
fn model_run(cfgs: &[SigCfg], initial: usize, blocked_first: bool) -> (Vec<(u8, usize, u8)>, bool) {
    use cfmt::sigtab::{bit, SigTable, SIG_DFL};
    struct M<'a> {
        t: SigTable,
        cfgs: &'a [SigCfg],
        log: Vec<(u8, usize, u8)>,
        seen: [bool; 3],
        ambiguous: bool,
    }
    fn blocked_bits(t: &SigTable) -> u8 {
        let mut b = 0;
        for (k, s) in host_sig::SIGS.iter().enumerate() {
            if t.blocked & bit(*s) != 0 {
                b |= 1 << k;
            }
        }
        b
    }
    fn install(m: &mut M, k: usize) {
        let s = host_sig::SIGS[k] as usize;
        let c = &m.cfgs[k];
        m.t.handler[s] = 2;
        m.t.mask[s] = c
            .mask
            .iter()
            .enumerate()
            .filter(|(_, on)| **on)
            .map(|(i, _)| bit(host_sig::SIGS[i]))
            .fold(0, |a, b| a | b);
        m.t.flags[s] = (if c.nodefer { host_sig::NODEFER } else { 0 })
            | (if c.resethand { host_sig::RESETHAND } else { 0 });
    }
    fn deliver(m: &mut M) {
        loop {
            if (m.t.pending & !m.t.blocked).count_ones() > 1 {
                m.ambiguous = true;
            }
            let Some(r) = m.t.take_ready(host_sig::NODEFER, host_sig::RESETHAND) else {
                break;
            };
            let k = host_sig::SIGS.iter().position(|s| *s == r.sig).unwrap();
            assert!(r.handler > 1);
            let was_default = m.t.handler[r.sig as usize] == SIG_DFL;
            let b = blocked_bits(&m.t) | if was_default { 8 } else { 0 };
            m.log.push((b'e', k, b));
            if was_default {
                install(m, k);
            }
            let first = !std::mem::replace(&mut m.seen[k], true);
            if first {
                for r2 in m.cfgs[k].raises.clone() {
                    m.t.pending |= bit(host_sig::SIGS[r2]);
                    deliver(m);
                }
            }
            m.log.push((b'x', k, 0));
            m.t.blocked = r.old_blocked;
        }
    }
    let mut m = M {
        t: SigTable::new(),
        cfgs,
        log: Vec::new(),
        seen: [false; 3],
        ambiguous: false,
    };
    for k in 0..3 {
        install(&mut m, k);
    }
    if blocked_first {
        for s in host_sig::SIGS {
            m.t.blocked |= bit(s);
        }
    }
    m.t.pending |= bit(host_sig::SIGS[initial]);
    deliver(&mut m);
    if blocked_first {
        m.t.blocked = 0;
        deliver(&mut m);
    }
    (m.log, m.ambiguous)
}

#[test]
fn the_signal_table_matches_the_host() {
    let mut rng = Rng(0x516_7ab1e);
    let mut bad = Vec::new();
    let mut skipped = 0;
    for round in 0..1500 {
        let mut cfgs: Vec<SigCfg> = Vec::new();
        for k in 0..3 {
            let mask = [rng.below(2) == 1, rng.below(2) == 1, rng.below(2) == 1];
            // (Apple's `SA_RESETHAND` does not behave as the standard says in a nested handler: the scenario uses it on Linux only)
            let (nodefer, resethand) = (
                rng.below(3) == 0,
                cfg!(target_os = "linux") && rng.below(3) == 0,
            );
            let mut raises = Vec::new();
            for _ in 0..rng.below(3) {
                let r = rng.below(3);
                // (a handler that raises itself runs again at once only without a mask: the scenario bounds it by running once)
                if r != k || rng.below(2) == 0 {
                    raises.push(r);
                }
            }
            cfgs.push(SigCfg {
                mask,
                nodefer,
                resethand,
                raises,
            });
        }
        let initial = rng.below(3);
        let blocked_first = rng.below(2) == 1;
        // the host
        *SIG_CFGS.lock().unwrap() = cfgs.clone();
        *SIG_SEEN.lock().unwrap() = [false; 3];
        SIG_LOG.lock().unwrap().clear();
        for (k, c) in cfgs.iter().enumerate() {
            host_install(k, c, host_handler as *const () as usize);
        }
        unsafe {
            if blocked_first {
                let mut all: host_sig::SigSet = Default::default();
                host_sig::sigemptyset(&mut all);
                for s in host_sig::SIGS {
                    host_sig::sigaddset(&mut all, s);
                }
                host_sig::sigprocmask(host_sig::SIG_BLOCK, &all, std::ptr::null_mut());
                host_sig::raise(host_sig::SIGS[initial]);
                host_sig::sigprocmask(host_sig::SIG_UNBLOCK, &all, std::ptr::null_mut());
            } else {
                host_sig::raise(host_sig::SIGS[initial]);
            }
        }
        let host = SIG_LOG.lock().unwrap().clone();
        let (model, ambiguous) = model_run(&cfgs, initial, blocked_first);
        if ambiguous {
            skipped += 1;
            continue;
        }
        if host != model {
            bad.push(format!("round {round}: initial {initial} blocked_first {blocked_first}\n  host  {host:?}\n  model {model:?}\n  masks {:?} nodefer {:?} resethand {:?} raises {:?}", cfgs.iter().map(|c| c.mask).collect::<Vec<_>>(), cfgs.iter().map(|c| c.nodefer).collect::<Vec<_>>(), cfgs.iter().map(|c| c.resethand).collect::<Vec<_>>(), cfgs.iter().map(|c| c.raises.clone()).collect::<Vec<_>>()));
        }
    }
    assert!(
        skipped < 1300,
        "too many scenarios skipped as ambiguous: {skipped}"
    );
    assert!(
        bad.is_empty(),
        "{} disagreements; first:\n{}",
        bad.len(),
        bad.iter().take(2).cloned().collect::<Vec<_>>().join("\n")
    );
}

// ---- getopt_long ----

mod host_long {
    use std::ffi::{c_char, c_int};
    #[repr(C)]
    pub struct Opt {
        pub name: *const c_char,
        pub has_arg: c_int,
        pub flag: *mut c_int,
        pub val: c_int,
    }
    extern "C" {
        pub fn getopt_long(
            argc: c_int,
            argv: *const *mut c_char,
            optstring: *const c_char,
            longopts: *const Opt,
            longindex: *mut c_int,
        ) -> c_int;
    }
}

#[test]
fn getopt_long_agrees_with_the_host() {
    use cfmt::getopt_long::{GetOptLong, LongOpt};
    let _g = GETOPT_LOCK.lock().unwrap();
    let optstrings = ["ab", "abc:", ":ab:", "a:bd", "+ab", "+abc:"];
    let longs: [(&str, u8, i32); 5] = [
        ("alpha", 0, 1001),
        ("alpine", 0, 1002),
        ("beta", 1, 1003),
        ("gamma", 2, 1004),
        ("verbose", 0, 1005),
    ];
    let words = [
        "-a",
        "-b",
        "-c",
        "-cX",
        "-ab",
        "-ba",
        "-bcX",
        "--alpha",
        "--al",
        "--alp",
        "--alph",
        "--beta",
        "--beta=1",
        "--bet",
        "--gamma",
        "--gamma=2",
        "--gam",
        "--verb",
        "--verbose",
        "--zz",
        "--alpha=3",
        "X",
        "Y",
        "--",
        "-",
        "--beta=",
        "1",
        "-d",
        "--v=1",
    ];
    let mut rng = Rng(0x0dd5_eed5_1234_5678);
    let (mut compared, mut skipped) = (0, 0);
    for _ in 0..30_000 {
        let optstring = rng.pick(&optstrings);
        let mut args: Vec<String> = vec!["prog".to_string()];
        for _ in 0..rng.below(7) {
            args.push(rng.pick(&words).to_string());
        }
        // ours
        let mut argv: Vec<&[u8]> = args.iter().map(|a| a.as_bytes()).collect();
        let lo: Vec<LongOpt> = longs
            .iter()
            .map(|(n, h, v)| LongOpt {
                name: n.as_bytes(),
                has_arg: *h,
                flag: false,
                val: *v,
            })
            .collect();
        let mut st = GetOptLong::new();
        let mut ours: Vec<(i32, Option<String>, Option<usize>)> = Vec::new();
        let mut had_fault = false;
        let mut delicate = false;
        loop {
            let s = st.next(&mut argv, optstring.as_bytes(), &lo, false);
            if s.ret == -1 {
                break;
            }
            had_fault |= s.fault.is_some();
            delicate |= matches!(
                s.fault,
                Some(
                    cfmt::getopt_long::Fault::MissingShort(_)
                        | cfmt::getopt_long::Fault::MissingLong(_)
                        | cfmt::getopt_long::Fault::NoArgument(_)
                )
            );
            let oa = s
                .optarg
                .map(|(i, off)| String::from_utf8_lossy(&argv[i][off..]).into_owned());
            ours.push((s.ret, oa, s.longind));
        }
        let ours_argv: Vec<String> = argv
            .iter()
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        let ours_optind = st.optind;
        // the host's (glibc permutes unless `+`; Apple's does the same)
        let cstrs: Vec<CString> = args
            .iter()
            .map(|a| CString::new(a.clone()).unwrap())
            .collect();
        let mut ptrs: Vec<*mut c_char> = cstrs.iter().map(|c| c.as_ptr() as *mut c_char).collect();
        let argc = ptrs.len();
        ptrs.push(std::ptr::null_mut()); // (an argument vector ends with a NULL)
        let names: Vec<CString> = longs
            .iter()
            .map(|(n, _, _)| CString::new(*n).unwrap())
            .collect();
        let mut hl: Vec<host_long::Opt> = longs
            .iter()
            .zip(&names)
            .map(|((_, h, v), n)| host_long::Opt {
                name: n.as_ptr(),
                has_arg: i32::from(*h),
                flag: std::ptr::null_mut(),
                val: *v,
            })
            .collect();
        hl.push(host_long::Opt {
            name: std::ptr::null(),
            has_arg: 0,
            flag: std::ptr::null_mut(),
            val: 0,
        });
        let copt = CString::new(optstring).unwrap();
        host_getopt_reset();
        let mut theirs: Vec<(i32, Option<String>, Option<usize>)> = Vec::new();
        loop {
            let mut idx: c_int = -1;
            // (an `optarg` left by an earlier run would point into freed memory)
            unsafe { host::optarg = std::ptr::null_mut() };
            // SAFETY: argv is argc valid NUL-terminated strings, the option table ends with a NULL name; the globals are the host's.
            let r = unsafe {
                host_long::getopt_long(
                    argc as c_int,
                    ptrs.as_ptr(),
                    copt.as_ptr(),
                    hl.as_ptr(),
                    &mut idx,
                )
            };
            if r == -1 {
                break;
            }
            let oa = unsafe {
                if host::optarg.is_null() {
                    None
                } else {
                    Some(
                        std::ffi::CStr::from_ptr(host::optarg)
                            .to_string_lossy()
                            .into_owned(),
                    )
                }
            };
            let long_hit = r >= 1001;
            // (the host leaves `optarg` stale after an option that takes none, and `longindex` stale for short options)
            theirs.push((r, oa, if long_hit { Some(idx as usize) } else { None }));
        }
        let host_argv: Vec<String> = ptrs[..argc]
            .iter()
            .map(|p| unsafe { std::ffi::CStr::from_ptr(*p).to_string_lossy().into_owned() })
            .collect();
        let host_optind = unsafe { host::optind } as usize;
        // what each side says about `optarg` is only meaningful for options that take one
        let norm = |v: &[(i32, Option<String>, Option<usize>)]| -> Vec<(i32, Option<String>)> {
            v.iter()
                .map(|(r, oa, _)| {
                    (
                        *r,
                        if matches!(*r, 1001 | 1002 | 1005) {
                            None
                        } else {
                            oa.clone()
                        },
                    )
                })
                .collect()
        };
        // a fault (an unknown or ambiguous option, a missing argument, an argument to a flag-less no-argument option) is where the two
        // libcs differ in details (where `optind` stops, the stale `optarg`): such runs compare only the returned options
        if had_fault {
            skipped += 1;
            if delicate {
                // (a missing or unwanted argument: Apple's libc returns the option's value for a missing long argument at the end)
                continue;
            }
            let (a, b): (Vec<i32>, Vec<i32>) = (
                ours.iter().map(|x| x.0).collect(),
                theirs.iter().map(|x| x.0).collect(),
            );
            assert_eq!(a, b, "options of {args:?} with {optstring:?}");
            continue;
        }
        assert_eq!(norm(&ours), norm(&theirs), "{args:?} with {optstring:?}");
        assert_eq!(
            ours.iter().map(|x| x.2).collect::<Vec<_>>(),
            theirs.iter().map(|x| x.2).collect::<Vec<_>>(),
            "long indexes of {args:?} with {optstring:?}"
        );
        assert_eq!(
            ours_argv, host_argv,
            "the permuted vector of {args:?} with {optstring:?}"
        );
        assert_eq!(
            ours_optind, host_optind,
            "optind of {args:?} with {optstring:?}"
        );
        compared += 1;
    }
    assert!(
        compared > 8000,
        "compared {compared} runs ({skipped} with faults)"
    );
}
