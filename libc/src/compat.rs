//! The small POSIX and GNU functions BusyBox and other ported software call that are thin glue over what the library already
//! has (UV8): the `*_unlocked` stdio names (this library takes one lock per call, so they are the plain ones), `dprintf`,
//! `vasprintf`, `getopt`, `dirname`, a few string functions and `dup2` for the standard streams.

use core::ffi::{c_char, c_int, c_void};

extern "C" {
    fn getc(f: *mut c_void) -> c_int;
    fn getchar() -> c_int;
    fn putc(c: c_int, f: *mut c_void) -> c_int;
    fn putchar(c: c_int) -> c_int;
    fn fputs(s: *const c_char, f: *mut c_void) -> c_int;
    fn fgets(s: *mut c_char, n: c_int, f: *mut c_void) -> *mut c_char;
    fn ferror(f: *mut c_void) -> c_int;
    fn feof(f: *mut c_void) -> c_int;
    fn fileno(f: *mut c_void) -> c_int;
    fn vsnprintf(buf: *mut c_char, n: usize, fmt: *const c_char, ap: *mut c_void) -> c_int;
    fn malloc(n: usize) -> *mut c_void;
    fn write(fd: c_int, buf: *const c_void, n: usize) -> isize;
    fn getenv(name: *const c_char) -> *mut c_char;
    fn open(path: *const c_char, flags: c_int, mode: c_int) -> c_int;
    fn read(fd: c_int, buf: *mut c_void, n: usize) -> isize;
    fn close(fd: c_int) -> c_int;
    fn fseek(f: *mut c_void, off: i64, whence: c_int) -> c_int;
}

// ---- stdio names ----

#[no_mangle]
pub unsafe extern "C" fn getc_unlocked(f: *mut c_void) -> c_int {
    getc(f)
}
#[no_mangle]
pub unsafe extern "C" fn getchar_unlocked() -> c_int {
    getchar()
}
#[no_mangle]
pub unsafe extern "C" fn putc_unlocked(c: c_int, f: *mut c_void) -> c_int {
    putc(c, f)
}
#[no_mangle]
pub unsafe extern "C" fn putchar_unlocked(c: c_int) -> c_int {
    putchar(c)
}
#[no_mangle]
pub unsafe extern "C" fn fputs_unlocked(s: *const c_char, f: *mut c_void) -> c_int {
    fputs(s, f)
}
#[no_mangle]
pub unsafe extern "C" fn fgets_unlocked(s: *mut c_char, n: c_int, f: *mut c_void) -> *mut c_char {
    fgets(s, n, f)
}
#[no_mangle]
pub unsafe extern "C" fn feof_unlocked(f: *mut c_void) -> c_int {
    feof(f)
}
#[no_mangle]
pub unsafe extern "C" fn ferror_unlocked(f: *mut c_void) -> c_int {
    ferror(f)
}
#[no_mangle]
pub unsafe extern "C" fn fileno_unlocked(f: *mut c_void) -> c_int {
    fileno(f)
}

/// `vasprintf`: format into a fresh `malloc`ed string (null on failure).
#[no_mangle]
pub unsafe extern "C" fn vasprintf(
    out: *mut *mut c_char,
    fmt: *const c_char,
    ap: *mut c_void,
) -> c_int {
    // (a `va_list` is passed on as it came: the size pass uses a copy, which the C ABI of this target makes a plain pointer)
    let n = vsnprintf(core::ptr::null_mut(), 0, fmt, ap);
    if n < 0 {
        return -1;
    }
    let buf = malloc(n as usize + 1) as *mut c_char;
    if buf.is_null() {
        return -1;
    }
    vsnprintf(buf, n as usize + 1, fmt, ap);
    *out = buf;
    n
}

/// `vdprintf` (`dprintf` is the variadic entry in `stdio.rs`): format and write to a descriptor.
#[no_mangle]
pub unsafe extern "C" fn vdprintf(fd: c_int, fmt: *const c_char, ap: *mut c_void) -> c_int {
    let mut s: *mut c_char = core::ptr::null_mut();
    let n = vasprintf(&mut s, fmt, ap);
    if n < 0 {
        return -1;
    }
    let mut done = 0usize;
    while done < n as usize {
        let w = write(fd, s.add(done) as *const c_void, n as usize - done);
        if w <= 0 {
            return -1;
        }
        done += w as usize;
    }
    n
}

// ---- strings ----

#[no_mangle]
pub unsafe extern "C" fn strchrnul(s: *const c_char, c: c_int) -> *mut c_char {
    let mut p = s;
    loop {
        if *p == 0 || *p as c_int == (c as c_char) as c_int {
            return p as *mut c_char;
        }
        p = p.add(1);
    }
}

fn lower(c: c_char) -> c_int {
    let c = c as u8;
    (if c.is_ascii_uppercase() { c + 32 } else { c }) as c_int
}

#[no_mangle]
pub unsafe extern "C" fn strcasecmp(a: *const c_char, b: *const c_char) -> c_int {
    strncasecmp(a, b, usize::MAX)
}

#[no_mangle]
pub unsafe extern "C" fn strncasecmp(a: *const c_char, b: *const c_char, n: usize) -> c_int {
    let mut i = 0;
    while i < n {
        let (x, y) = (lower(*a.add(i)), lower(*b.add(i)));
        if x != y {
            return x - y;
        }
        if x == 0 {
            return 0;
        }
        i += 1;
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn stpcpy(d: *mut c_char, s: *const c_char) -> *mut c_char {
    let mut i = 0;
    loop {
        *d.add(i) = *s.add(i);
        if *s.add(i) == 0 {
            return d.add(i);
        }
        i += 1;
    }
}

/// `strsep`: split off the next token at any of `delim`.
#[no_mangle]
pub unsafe extern "C" fn strsep(sp: *mut *mut c_char, delim: *const c_char) -> *mut c_char {
    let s = *sp;
    if s.is_null() {
        return s;
    }
    let mut p = s;
    while *p != 0 {
        let mut d = delim;
        while *d != 0 {
            if *d == *p {
                *p = 0;
                *sp = p.add(1);
                return s;
            }
            d = d.add(1);
        }
        p = p.add(1);
    }
    *sp = core::ptr::null_mut();
    s
}

/// `strverscmp`: compare with runs of digits as numbers (`file9` before `file10`).
#[no_mangle]
pub unsafe extern "C" fn strverscmp(a: *const c_char, b: *const c_char) -> c_int {
    cfmt::glob::strverscmp(
        core::ffi::CStr::from_ptr(a).to_bytes(),
        core::ffi::CStr::from_ptr(b).to_bytes(),
    )
}

// ---- libgen ----

/// `dirname`: the directory part, edited in place (`"."` for a name with no slash).
#[no_mangle]
pub unsafe extern "C" fn dirname(path: *mut c_char) -> *mut c_char {
    static DOT: [c_char; 2] = [b'.' as c_char, 0];
    if path.is_null() || *path == 0 {
        return DOT.as_ptr() as *mut c_char;
    }
    let mut end = crate::string::strlen(path) as usize;
    while end > 1 && *path.add(end - 1) == b'/' as c_char {
        end -= 1;
    }
    while end > 0 && *path.add(end - 1) != b'/' as c_char {
        end -= 1;
    }
    if end == 0 {
        return DOT.as_ptr() as *mut c_char;
    }
    while end > 1 && *path.add(end - 1) == b'/' as c_char {
        end -= 1;
    }
    *path.add(end) = 0;
    path
}

// ---- getopt ----

#[no_mangle]
pub static mut optarg: *mut c_char = core::ptr::null_mut();
#[no_mangle]
pub static mut optind: c_int = 1;
#[no_mangle]
pub static mut opterr: c_int = 1;
#[no_mangle]
pub static mut optopt: c_int = 0;
static GETOPT: rt::sync::Mutex<cfmt::getopt::GetOpt> =
    rt::sync::Mutex::new(cfmt::getopt::GetOpt::new());

/// POSIX `getopt` (the core is `cfmt::getopt`): options end at the first argument that does not start with `-` (no permutation),
/// `--` ends them, and a leading `:` in `optstring` makes a missing argument return `:` instead of `?`.
#[no_mangle]
pub unsafe extern "C" fn getopt(
    argc: c_int,
    argv: *const *mut c_char,
    optstring: *const c_char,
) -> c_int {
    optarg = core::ptr::null_mut();
    let mut st = GETOPT.lock();
    if optind == 0 {
        // the C library's way to ask for a fresh start
        optind = 1;
        st.reset();
    }
    // (a caller may set `optind` itself, as BusyBox does to skip arguments)
    st.optind = optind.max(0) as usize;
    let args: alloc::vec::Vec<&[u8]> = (0..argc.max(0) as usize)
        .map(|i| {
            let p = *argv.add(i);
            if p.is_null() {
                &[][..]
            } else {
                core::ffi::CStr::from_ptr(p).to_bytes()
            }
        })
        .collect();
    let step = st.next(&args, core::ffi::CStr::from_ptr(optstring).to_bytes());
    optind = st.optind as c_int;
    if let Some((i, off)) = step.optarg {
        optarg = (*argv.add(i)).add(off);
    }
    if let Some(c) = step.optopt {
        optopt = c as c_int;
    }
    let quiet = *optstring == b':' as c_char;
    if opterr != 0 && !quiet {
        let msg = match step.fault {
            Some(cfmt::getopt::Fault::Invalid(c)) => Some(alloc::format!(
                "getopt: invalid option -- '{}'\n",
                c as char
            )),
            Some(cfmt::getopt::Fault::MissingArgument(c)) => Some(alloc::format!(
                "getopt: option requires an argument -- '{}'\n",
                c as char
            )),
            None => None,
        };
        if let Some(m) = msg {
            write(2, m.as_ptr() as *const c_void, m.len());
        }
    }
    step.ret
}

// ---- identity ----

/// The number in the environment variable `name` (the session's profile puts `UID` and `GID` there), or `default`.
unsafe fn env_number(name: &[u8], default: u32) -> u32 {
    let p = getenv(name.as_ptr() as *const c_char);
    if p.is_null() {
        return default;
    }
    let mut n: u32 = 0;
    let mut i = 0;
    while (*p.add(i) as u8).is_ascii_digit() {
        n = n
            .wrapping_mul(10)
            .wrapping_add((*p.add(i) as u8 - b'0') as u32);
        i += 1;
    }
    if i == 0 {
        default
    } else {
        n
    }
}

#[no_mangle]
pub unsafe extern "C" fn getuid() -> u32 {
    env_number(b"UID\0", 0)
}
#[no_mangle]
pub unsafe extern "C" fn geteuid() -> u32 {
    env_number(b"UID\0", 0)
}
#[no_mangle]
pub unsafe extern "C" fn getgid() -> u32 {
    env_number(b"GID\0", 0)
}
#[no_mangle]
pub unsafe extern "C" fn getegid() -> u32 {
    env_number(b"GID\0", 0)
}
/// The one group of the session's identity.
#[no_mangle]
pub unsafe extern "C" fn getgroups(size: c_int, list: *mut u32) -> c_int {
    if size == 0 {
        return 1;
    }
    if size < 1 {
        return -1;
    }
    *list = getgid();
    1
}
#[no_mangle]
pub extern "C" fn getpagesize() -> c_int {
    4096
}

#[no_mangle]
pub unsafe extern "C" fn strndup(s: *const c_char, n: usize) -> *mut c_char {
    let mut len = 0;
    while len < n && *s.add(len) != 0 {
        len += 1;
    }
    let d = malloc(len + 1) as *mut c_char;
    if d.is_null() {
        return d;
    }
    core::ptr::copy_nonoverlapping(s, d, len);
    *d.add(len) = 0;
    d
}

#[no_mangle]
pub unsafe extern "C" fn fseeko(f: *mut c_void, off: i64, whence: c_int) -> c_int {
    fseek(f, off, whence)
}

/// Not built: there is no `mmap` (a program that needs it gets `MAP_FAILED` and `ENOSYS`) and no call that sets file times
/// (the file protocol has none yet: TIDY-UP T134).
#[no_mangle]
pub extern "C" fn mmap(
    _addr: *mut c_void,
    _len: usize,
    _prot: c_int,
    _flags: c_int,
    _fd: c_int,
    _off: i64,
) -> *mut c_void {
    crate::errno::set(38);
    usize::MAX as *mut c_void
}
#[no_mangle]
pub extern "C" fn munmap(_addr: *mut c_void, _len: usize) -> c_int {
    crate::errno::set(22);
    -1
}
#[no_mangle]
pub extern "C" fn utimensat(
    _dirfd: c_int,
    _path: *const c_char,
    _times: *const c_void,
    _flags: c_int,
) -> c_int {
    crate::errno::set(38);
    -1
}
#[no_mangle]
pub extern "C" fn futimens(_fd: c_int, _times: *const c_void) -> c_int {
    crate::errno::set(38);
    -1
}

// ---- directories ----

#[repr(C)]
pub struct Dirent {
    d_ino: u64,
    d_off: i64,
    d_reclen: u16,
    d_type: u8,
    d_name: [u8; 256],
}

/// `DIR`: the entries read when it was opened, and where `readdir` is in them.
pub struct Dir {
    entries: alloc::vec::Vec<(alloc::vec::Vec<u8>, bool, bool)>,
    next: usize,
    out: Dirent,
}

#[no_mangle]
pub unsafe extern "C" fn opendir(path: *const c_char) -> *mut Dir {
    let bytes = core::ffi::CStr::from_ptr(path).to_bytes();
    match crate::fd::dir_entries(bytes) {
        Ok(entries) => alloc::boxed::Box::into_raw(alloc::boxed::Box::new(Dir {
            entries,
            next: 0,
            out: Dirent {
                d_ino: 0,
                d_off: 0,
                d_reclen: 280,
                d_type: 0,
                d_name: [0; 256],
            },
        })),
        Err(e) => {
            crate::errno::set(e);
            core::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn readdir(d: *mut Dir) -> *mut Dirent {
    let d = &mut *d;
    let Some((name, is_dir, is_link)) = d.entries.get(d.next) else {
        return core::ptr::null_mut();
    };
    d.next += 1;
    let n = name.len().min(255);
    d.out.d_name = [0; 256];
    d.out.d_name[..n].copy_from_slice(&name[..n]);
    d.out.d_ino = d.next as u64;
    d.out.d_off = d.next as i64;
    d.out.d_type = if *is_link {
        10
    } else if *is_dir {
        4
    } else {
        8
    };
    &mut d.out
}

#[no_mangle]
pub unsafe extern "C" fn closedir(d: *mut Dir) -> c_int {
    drop(alloc::boxed::Box::from_raw(d));
    0
}
#[no_mangle]
pub unsafe extern "C" fn rewinddir(d: *mut Dir) {
    (*d).next = 0;
}

// ---- fnmatch ----

/// `fnmatch`: shell patterns (`*`, `?`, `[...]` with classes and ranges, `\` escapes) with `FNM_*` flags.
#[no_mangle]
pub unsafe extern "C" fn fnmatch(
    pattern: *const c_char,
    string: *const c_char,
    flags: c_int,
) -> c_int {
    let p = core::ffi::CStr::from_ptr(pattern).to_bytes();
    let s = core::ffi::CStr::from_ptr(string).to_bytes();
    // (the core is `cfmt::glob`; 0 is a match)
    if cfmt::glob::fnmatch(p, s, flags) {
        0
    } else {
        1
    }
}

// ---- descriptors ----

/// `dup2` for descriptors that are standard streams (a handle is duplicated); anything else is `ENOSYS`: duplicating a file
/// connection with its offset is not built (TIDY-UP).
#[no_mangle]
pub extern "C" fn dup2(from: c_int, to: c_int) -> c_int {
    if from == to {
        return to;
    }
    crate::fd::dup_stream(from, to)
}

#[no_mangle]
pub extern "C" fn dup(fd: c_int) -> c_int {
    crate::fd::dup_lowest(fd)
}

#[no_mangle]
pub extern "C" fn _exit(code: c_int) -> ! {
    crate::spawn::vfork_child_exit(code);
    rt::sys::exit(code)
}

// ---- regular expressions (the `pregex` crate) ----

#[repr(C)]
pub struct RegexT {
    re_nsub: usize,
    /// A `Box<pregex::Regex>` while compiled.
    private: *mut c_void,
}

#[repr(C)]
pub struct RegMatch {
    rm_so: i64,
    rm_eo: i64,
}

fn reg_error_code(e: pregex::Error) -> c_int {
    use pregex::Error::*;
    match e {
        BadPattern => 2,
        BadClass => 4,
        TrailingBackslash => 5,
        BadBackref => 6,
        UnmatchedBracket => 7,
        UnmatchedParen => 8,
        UnmatchedBrace => 9,
        BadBrace => 10,
        BadRange => 11,
        TooBig => 12,
        BadRepeat => 13,
    }
}

#[no_mangle]
pub unsafe extern "C" fn regcomp(
    preg: *mut RegexT,
    pattern: *const c_char,
    cflags: c_int,
) -> c_int {
    let pat = core::ffi::CStr::from_ptr(pattern).to_bytes();
    let flags = pregex::Flags {
        extended: cflags & 1 != 0,
        icase: cflags & 2 != 0,
        newline: cflags & 8 != 0,
    };
    match pregex::compile(pat, flags) {
        Ok(re) => {
            (*preg).re_nsub = re.groups();
            (*preg).private =
                alloc::boxed::Box::into_raw(alloc::boxed::Box::new(re)) as *mut c_void;
            0
        }
        Err(e) => reg_error_code(e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn regexec(
    preg: *const RegexT,
    string: *const c_char,
    nmatch: usize,
    pmatch: *mut RegMatch,
    eflags: c_int,
) -> c_int {
    let re = &*((*preg).private as *const pregex::Regex);
    let text = core::ffi::CStr::from_ptr(string).to_bytes();
    match re.exec(text, eflags & 1 != 0, eflags & 2 != 0) {
        None => 1, // REG_NOMATCH
        Some(caps) => {
            for i in 0..nmatch {
                let (so, eo) = match caps.get(i).copied().flatten() {
                    Some((s, e)) => (s as i64, e as i64),
                    None => (-1, -1),
                };
                *pmatch.add(i) = RegMatch {
                    rm_so: so,
                    rm_eo: eo,
                };
            }
            0
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn regfree(preg: *mut RegexT) {
    if !(*preg).private.is_null() {
        drop(alloc::boxed::Box::from_raw(
            (*preg).private as *mut pregex::Regex,
        ));
        (*preg).private = core::ptr::null_mut();
    }
}

#[no_mangle]
pub unsafe extern "C" fn regerror(
    code: c_int,
    _preg: *const RegexT,
    buf: *mut c_char,
    size: usize,
) -> usize {
    let msg: &[u8] = match code {
        1 => b"No match\0",
        2 => b"Invalid regular expression\0",
        4 => b"Invalid character class name\0",
        5 => b"Trailing backslash\0",
        6 => b"Invalid back reference\0",
        7 => b"Unmatched [ or [^\0",
        8 => b"Unmatched ( or \\(\0",
        9 => b"Unmatched { or \\{\0",
        10 => b"Invalid content of \\{\\}\0",
        11 => b"Invalid range end\0",
        12 => b"Pattern too complex\0",
        13 => b"Invalid preceding regular expression\0",
        _ => b"Unknown regular expression error\0",
    };
    if size > 0 {
        let n = msg.len().min(size);
        core::ptr::copy_nonoverlapping(msg.as_ptr() as *const c_char, buf, n);
        *buf.add(n - 1) = 0;
    }
    msg.len()
}

// ---- small system calls with honest answers ----

/// The processors, counted from `/proc/cpuinfo` (UV6); one if there is no `/proc`.
#[no_mangle]
pub unsafe extern "C" fn sched_getaffinity(_pid: c_int, size: usize, set: *mut u64) -> c_int {
    let mut n = 1usize;
    let fd = open(c"/proc/cpuinfo".as_ptr(), 0, 0);
    if fd >= 0 {
        let mut buf = [0u8; 2048];
        let r = read(fd, buf.as_mut_ptr() as *mut c_void, buf.len());
        if r > 0 {
            n = buf[..r as usize]
                .split(|&b| b == b'\n')
                .filter(|l| l.starts_with(b"processor"))
                .count()
                .max(1);
        }
        close(fd);
    }
    for i in 0..size / 8 {
        *set.add(i) = 0;
    }
    for c in 0..n.min(size * 8) {
        *set.add(c / 64) |= 1 << (c % 64);
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn __cpu_count(set: *const u64) -> c_int {
    (0..16).map(|i| (*set.add(i)).count_ones() as c_int).sum()
}

#[no_mangle]
pub extern "C" fn clock_settime(_clock: c_int, _ts: *const c_void) -> c_int {
    crate::errno::set(1); // EPERM: the clock is set by the clock service, not by a program
    -1
}

/// Not built (TIDY-UP T134): a program that parses dates with a format gets `NULL`.
#[no_mangle]
pub extern "C" fn strptime(
    _s: *const c_char,
    _fmt: *const c_char,
    _tm: *mut c_void,
) -> *mut c_char {
    core::ptr::null_mut()
}

/// There is no mount table to read: `getmntent` ends at once.
#[no_mangle]
pub extern "C" fn setmntent(_path: *const c_char, _mode: *const c_char) -> *mut c_void {
    crate::errno::set(2);
    core::ptr::null_mut()
}
#[no_mangle]
pub extern "C" fn getmntent(_f: *mut c_void) -> *mut c_void {
    core::ptr::null_mut()
}
#[no_mangle]
pub extern "C" fn endmntent(_f: *mut c_void) -> c_int {
    1
}

// ---- the environment ----

extern "C" {
    static mut environ: *mut *mut c_char;
    fn fgetc(f: *mut c_void) -> c_int;
    fn realloc(p: *mut c_void, n: usize) -> *mut c_void;
    fn clock_gettime(clock: c_int, ts: *mut i64) -> c_int;
    fn getcwd(buf: *mut c_char, size: usize) -> *mut c_char;
    fn stat(path: *const c_char, out: *mut c_void) -> c_int;
    fn chown(path: *const c_char, uid: c_int, gid: c_int) -> c_int;
    fn mkdir(path: *const c_char, mode: c_int) -> c_int;
}

unsafe fn env_entries() -> alloc::vec::Vec<*mut c_char> {
    let mut v = alloc::vec::Vec::new();
    let mut p = environ;
    if !p.is_null() {
        while !(*p).is_null() {
            v.push(*p);
            p = p.add(1);
        }
    }
    v
}

/// Replace the environment array with `v` (a fresh leaked array: a `getenv` result stays valid).
unsafe fn set_env_entries(mut v: alloc::vec::Vec<*mut c_char>) {
    v.push(core::ptr::null_mut());
    let b = alloc::boxed::Box::leak(v.into_boxed_slice());
    environ = b.as_mut_ptr();
}

unsafe fn env_index(v: &[*mut c_char], name: &[u8]) -> Option<usize> {
    v.iter().position(|&e| {
        let s = core::ffi::CStr::from_ptr(e).to_bytes();
        s.len() > name.len() && &s[..name.len()] == name && s[name.len()] == b'='
    })
}

#[no_mangle]
pub unsafe extern "C" fn putenv(s: *mut c_char) -> c_int {
    let b = core::ffi::CStr::from_ptr(s).to_bytes();
    let Some(eq) = b.iter().position(|&c| c == b'=') else {
        return unsetenv(s);
    };
    let mut v = env_entries();
    match env_index(&v, &b[..eq]) {
        Some(i) => v[i] = s,
        None => v.push(s),
    }
    set_env_entries(v);
    0
}

#[no_mangle]
pub unsafe extern "C" fn setenv(
    name: *const c_char,
    value: *const c_char,
    overwrite: c_int,
) -> c_int {
    let n = core::ffi::CStr::from_ptr(name).to_bytes();
    if n.is_empty() || n.contains(&b'=') {
        crate::errno::set(22);
        return -1;
    }
    let mut v = env_entries();
    let at = env_index(&v, n);
    if at.is_some() && overwrite == 0 {
        return 0;
    }
    let val = core::ffi::CStr::from_ptr(value).to_bytes();
    let mut entry: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    entry.extend_from_slice(n);
    entry.push(b'=');
    entry.extend_from_slice(val);
    entry.push(0);
    let p = alloc::boxed::Box::leak(entry.into_boxed_slice()).as_mut_ptr() as *mut c_char;
    match at {
        Some(i) => v[i] = p,
        None => v.push(p),
    }
    set_env_entries(v);
    0
}

#[no_mangle]
pub unsafe extern "C" fn unsetenv(name: *const c_char) -> c_int {
    let n = core::ffi::CStr::from_ptr(name).to_bytes();
    let mut v = env_entries();
    if let Some(i) = env_index(&v, n) {
        v.remove(i);
        set_env_entries(v);
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn clearenv() -> c_int {
    set_env_entries(alloc::vec::Vec::new());
    0
}

// ---- more strings, time, files ----

#[no_mangle]
pub unsafe extern "C" fn mempcpy(d: *mut c_void, s: *const c_void, n: usize) -> *mut c_void {
    core::ptr::copy_nonoverlapping(s as *const u8, d as *mut u8, n);
    (d as *mut u8).add(n) as *mut c_void
}

#[no_mangle]
pub unsafe extern "C" fn strcasestr(h: *const c_char, n: *const c_char) -> *mut c_char {
    let nl = crate::string::strlen(n);
    if nl == 0 {
        return h as *mut c_char;
    }
    let hl = crate::string::strlen(h);
    let mut i = 0;
    while i + nl <= hl {
        if strncasecmp(h.add(i), n, nl) == 0 {
            return h.add(i) as *mut c_char;
        }
        i += 1;
    }
    core::ptr::null_mut()
}

/// `getline`: read up to and including a newline into a growing buffer.
#[no_mangle]
pub unsafe extern "C" fn getline(line: *mut *mut c_char, cap: *mut usize, f: *mut c_void) -> isize {
    if (*line).is_null() || *cap == 0 {
        *cap = 128;
        *line = realloc(*line as *mut c_void, *cap) as *mut c_char;
        if (*line).is_null() {
            return -1;
        }
    }
    let mut n = 0usize;
    loop {
        let c = fgetc(f);
        if c < 0 {
            break;
        }
        if n + 2 > *cap {
            *cap *= 2;
            let p = realloc(*line as *mut c_void, *cap) as *mut c_char;
            if p.is_null() {
                return -1;
            }
            *line = p;
        }
        *(*line).add(n) = c as c_char;
        n += 1;
        if c == b'\n' as c_int {
            break;
        }
    }
    if n == 0 {
        return -1;
    }
    *(*line).add(n) = 0;
    n as isize
}

#[no_mangle]
pub unsafe extern "C" fn gettimeofday(tv: *mut i64, _tz: *mut c_void) -> c_int {
    let mut ts = [0i64; 2];
    if clock_gettime(0, ts.as_mut_ptr()) != 0 {
        return -1;
    }
    if !tv.is_null() {
        *tv = ts[0];
        *tv.add(1) = ts[1] / 1000;
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn uname(u: *mut c_char) -> c_int {
    // five 65-byte fields: sysname, nodename, release, version, machine (and a domain name)
    let fields: [&[u8]; 6] = [
        b"riscv-os",
        b"localhost",
        b"0.1",
        b"UV8",
        b"riscv64",
        b"(none)",
    ];
    for (i, f) in fields.iter().enumerate() {
        let d = u.add(i * 65);
        core::ptr::write_bytes(d, 0, 65);
        core::ptr::copy_nonoverlapping(f.as_ptr() as *const c_char, d, f.len());
    }
    0
}

/// A path made absolute and cleaned of `.`, `..` and repeated slashes, and checked to exist (symlinks are not resolved).
#[no_mangle]
pub unsafe extern "C" fn realpath(path: *const c_char, resolved: *mut c_char) -> *mut c_char {
    let p = core::ffi::CStr::from_ptr(path).to_bytes();
    let mut full: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    if p.first() != Some(&b'/') {
        let mut cwd = [0 as c_char; 4096];
        if getcwd(cwd.as_mut_ptr(), cwd.len()).is_null() {
            return core::ptr::null_mut();
        }
        full.extend_from_slice(core::ffi::CStr::from_ptr(cwd.as_ptr()).to_bytes());
        full.push(b'/');
    }
    full.extend_from_slice(p);
    let mut parts: alloc::vec::Vec<&[u8]> = alloc::vec::Vec::new();
    for comp in full.split(|&b| b == b'/') {
        match comp {
            b"" | b"." => {}
            b".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    let mut out: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    for c in &parts {
        out.push(b'/');
        out.extend_from_slice(c);
    }
    if out.is_empty() {
        out.push(b'/');
    }
    out.push(0);
    let mut st = [0u8; 160];
    if stat(
        out.as_ptr() as *const c_char,
        st.as_mut_ptr() as *mut c_void,
    ) != 0
    {
        return core::ptr::null_mut();
    }
    let dst = if resolved.is_null() {
        malloc(out.len()) as *mut c_char
    } else {
        resolved
    };
    if dst.is_null() {
        return dst;
    }
    core::ptr::copy_nonoverlapping(out.as_ptr() as *const c_char, dst, out.len());
    dst
}

static TEMP_COUNTER: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Fill the `XXXXXX` at the end of `tmpl` with letters made from the clock and a counter.
unsafe fn temp_fill(tmpl: *mut c_char) -> bool {
    let n = crate::string::strlen(tmpl);
    if n < 6 || core::slice::from_raw_parts(tmpl.add(n - 6) as *const u8, 6) != b"XXXXXX" {
        crate::errno::set(22);
        return false;
    }
    let mut ts = [0i64; 2];
    clock_gettime(1, ts.as_mut_ptr());
    let mut x = (ts[0] as u64)
        .wrapping_mul(1_000_000_007)
        .wrapping_add(ts[1] as u64)
        ^ (TEMP_COUNTER.fetch_add(1, core::sync::atomic::Ordering::Relaxed) << 17);
    for i in 0..6 {
        *tmpl.add(n - 6 + i) = b"abcdefghijklmnopqrstuvwxyz0123456789"[(x % 36) as usize] as c_char;
        x = (x / 36) ^ x.rotate_left(7);
    }
    true
}

#[no_mangle]
pub unsafe extern "C" fn mkstemp(tmpl: *mut c_char) -> c_int {
    for _ in 0..100 {
        if !temp_fill(tmpl) {
            return -1;
        }
        let fd = open(tmpl, 2 | 0x40 | 0x80, 0o600);
        if fd >= 0 || *crate::errno::__errno_location() != 17 {
            return fd;
        }
    }
    -1
}

#[no_mangle]
pub unsafe extern "C" fn mkdtemp(tmpl: *mut c_char) -> *mut c_char {
    for _ in 0..100 {
        if !temp_fill(tmpl) {
            return core::ptr::null_mut();
        }
        if mkdir(tmpl, 0o700) == 0 {
            return tmpl;
        }
        if *crate::errno::__errno_location() != 17 {
            return core::ptr::null_mut();
        }
    }
    core::ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn mktemp(tmpl: *mut c_char) -> *mut c_char {
    if temp_fill(tmpl) {
        tmpl
    } else {
        *tmpl = 0;
        tmpl
    }
}

#[no_mangle]
pub unsafe extern "C" fn lchown(path: *const c_char, uid: c_int, gid: c_int) -> c_int {
    chown(path, uid, gid)
}

/// Not built (TIDY-UP T134): no devices or fifos can be made by a program, no file-system statistics, no file times.
macro_rules! unsupported {
    ($($name:ident($($a:ident: $t:ty),*) -> $r:ty = $v:expr;)*) => {
        $( #[no_mangle] pub extern "C" fn $name($(_: $t),*) -> $r { crate::errno::set(38); $v } )*
    };
}
unsupported! {
    mkfifo(a: *const c_char, b: u32) -> c_int = -1;
    mknod(a: *const c_char, b: u32, c: u64) -> c_int = -1;
    statvfs(a: *const c_char, b: *mut c_void) -> c_int = -1;
    fstatvfs(a: c_int, b: *mut c_void) -> c_int = -1;
    utimes(a: *const c_char, b: *const c_void) -> c_int = -1;
    setsid() -> c_int = -1;
    getsid(a: c_int) -> c_int = -1;
    fork() -> c_int = -1;
}

/// Terminal attributes are not available to programs yet (TIDY-UP T124): a program asking is told it is not on a terminal.
#[no_mangle]
pub extern "C" fn tcgetattr(_fd: c_int, _t: *mut c_void) -> c_int {
    crate::errno::set(25); // ENOTTY
    -1
}

#[no_mangle]
pub extern "C" fn tcsetattr(_fd: c_int, _actions: c_int, _t: *const c_void) -> c_int {
    crate::errno::set(25);
    -1
}

// ---- the user database: the session's own identity (USER, UID, GID, HOME in the environment) ----

#[repr(C)]
pub struct Passwd {
    pw_name: *mut c_char,
    pw_passwd: *mut c_char,
    pw_uid: u32,
    pw_gid: u32,
    pw_gecos: *mut c_char,
    pw_dir: *mut c_char,
    pw_shell: *mut c_char,
}

#[repr(C)]
pub struct Group {
    gr_name: *mut c_char,
    gr_passwd: *mut c_char,
    gr_gid: u32,
    gr_mem: *mut *mut c_char,
}

static mut PW: Passwd = Passwd {
    pw_name: core::ptr::null_mut(),
    pw_passwd: core::ptr::null_mut(),
    pw_uid: 0,
    pw_gid: 0,
    pw_gecos: core::ptr::null_mut(),
    pw_dir: core::ptr::null_mut(),
    pw_shell: core::ptr::null_mut(),
};
static mut GR: Group = Group {
    gr_name: core::ptr::null_mut(),
    gr_passwd: core::ptr::null_mut(),
    gr_gid: 0,
    gr_mem: core::ptr::null_mut(),
};
static mut GR_MEM: [*mut c_char; 1] = [core::ptr::null_mut()];

unsafe fn env_or(name: &[u8], default: &'static [u8]) -> *mut c_char {
    let p = getenv(name.as_ptr() as *const c_char);
    if p.is_null() {
        default.as_ptr() as *mut c_char
    } else {
        p
    }
}

/// The one user this session knows: the entry for `USER` (default `user`) when `uid` or `name` matches it.
unsafe fn session_passwd() -> *mut Passwd {
    PW = Passwd {
        pw_name: env_or(b"USER\0", b"user\0"),
        pw_passwd: c"x".as_ptr() as *mut c_char,
        pw_uid: getuid(),
        pw_gid: getgid(),
        pw_gecos: c"".as_ptr() as *mut c_char,
        pw_dir: env_or(b"HOME\0", b"/home\0"),
        pw_shell: c"/bin/sh".as_ptr() as *mut c_char,
    };
    &raw mut PW
}

#[no_mangle]
pub unsafe extern "C" fn getpwuid(uid: u32) -> *mut Passwd {
    let p = session_passwd();
    if (*p).pw_uid == uid {
        p
    } else {
        core::ptr::null_mut()
    }
}

#[no_mangle]
pub unsafe extern "C" fn getpwnam(name: *const c_char) -> *mut Passwd {
    let p = session_passwd();
    if crate::string::strcmp(name, (*p).pw_name) == 0 {
        p
    } else {
        core::ptr::null_mut()
    }
}

unsafe fn session_group() -> *mut Group {
    GR = Group {
        gr_name: env_or(b"USER\0", b"user\0"),
        gr_passwd: c"x".as_ptr() as *mut c_char,
        gr_gid: getgid(),
        gr_mem: (&raw mut GR_MEM) as *mut *mut c_char,
    };
    &raw mut GR
}

#[no_mangle]
pub unsafe extern "C" fn getgrgid(gid: u32) -> *mut Group {
    let g = session_group();
    if (*g).gr_gid == gid {
        g
    } else {
        core::ptr::null_mut()
    }
}

#[no_mangle]
pub unsafe extern "C" fn getgrnam(name: *const c_char) -> *mut Group {
    let g = session_group();
    if crate::string::strcmp(name, (*g).gr_name) == 0 {
        g
    } else {
        core::ptr::null_mut()
    }
}

#[no_mangle]
pub unsafe extern "C" fn getgrouplist(
    _user: *const c_char,
    group: u32,
    groups: *mut u32,
    n: *mut c_int,
) -> c_int {
    if *n < 1 {
        *n = 1;
        return -1;
    }
    *groups = group;
    *n = 1;
    1
}

#[no_mangle]
pub unsafe extern "C" fn getlogin_r(buf: *mut c_char, n: usize) -> c_int {
    let u = env_or(b"USER\0", b"user\0");
    let l = crate::string::strlen(u);
    if l + 1 > n {
        return 34; // ERANGE
    }
    core::ptr::copy_nonoverlapping(u, buf, l + 1);
    0
}

// ---- sscanf ----

/// The arguments of a `va_list` are 8-byte slots (see `stdio.rs`).
struct ArgSlots(*const u64);
impl ArgSlots {
    unsafe fn next_ptr(&mut self) -> *mut c_void {
        let v = *self.0;
        self.0 = self.0.add(1);
        v as *mut c_void
    }
}

/// `vsscanf`: `%d %i %u %o %x %f %s %c %[set] %n %%`, with `*`, a field width and the `hh h l ll z j t L` modifiers.
#[no_mangle]
pub unsafe extern "C" fn vsscanf(s: *const c_char, fmt: *const c_char, ap: *const u64) -> c_int {
    let input = core::ffi::CStr::from_ptr(s).to_bytes();
    let f = core::ffi::CStr::from_ptr(fmt).to_bytes();
    let mut args = ArgSlots(ap);
    let (mut i, mut j, mut done) = (0usize, 0usize, 0 as c_int);
    let mut stored_any = false;
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
                return if i >= input.len() && !stored_any {
                    -1
                } else {
                    done
                };
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
        let mut len = 0u8; // 0 int, 1 long, 2 long long, 3 short, 4 char, 5 long double
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
            if input.get(i) != Some(&b'%') {
                return done;
            }
            i += 1;
            continue;
        }
        if conv == b'n' {
            if !suppress {
                *(args.next_ptr() as *mut c_int) = i as c_int;
            }
            continue;
        }
        if conv != b'c' && conv != b'[' {
            while input.get(i).is_some_and(|b| b.is_ascii_whitespace()) {
                i += 1;
            }
        }
        if i >= input.len() {
            return if stored_any { done } else { -1 };
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
                    return done;
                }
                let v = if neg {
                    (v as i64).wrapping_neg() as u64
                } else {
                    v
                };
                if !suppress {
                    let p = args.next_ptr();
                    match len {
                        0 => *(p as *mut u32) = v as u32,
                        1 | 2 | 5 => *(p as *mut u64) = v,
                        3 => *(p as *mut u16) = v as u16,
                        _ => *(p as *mut u8) = v as u8,
                    }
                    done += 1;
                    stored_any = true;
                }
            }
            b'f' | b'e' | b'g' | b'E' | b'G' | b'a' => {
                let start = i;
                while i - start < limit
                    && input.get(i).is_some_and(|b| {
                        b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.' | b'e' | b'E')
                    })
                {
                    i += 1;
                }
                let Ok(v) = core::str::from_utf8(&input[start..i])
                    .unwrap_or("")
                    .parse::<f64>()
                else {
                    return done;
                };
                if !suppress {
                    let p = args.next_ptr();
                    if len == 1 || len == 5 {
                        *(p as *mut f64) = v;
                    } else {
                        *(p as *mut f32) = v as f32;
                    }
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
                    let d = args.next_ptr() as *mut u8;
                    core::ptr::copy_nonoverlapping(input.as_ptr().add(start), d, i - start);
                    *d.add(i - start) = 0;
                    done += 1;
                    stored_any = true;
                }
            }
            b'c' => {
                let n = if width == 0 { 1 } else { width };
                if i + n > input.len() {
                    return if stored_any { done } else { -1 };
                }
                if !suppress {
                    let d = args.next_ptr() as *mut u8;
                    core::ptr::copy_nonoverlapping(input.as_ptr().add(i), d, n);
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
                let in_set = |b: u8| -> bool {
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
                };
                let start = i;
                while i - start < limit && input.get(i).is_some_and(|&b| in_set(b) != negate) {
                    i += 1;
                }
                if i == start {
                    return done;
                }
                if !suppress {
                    let d = args.next_ptr() as *mut u8;
                    core::ptr::copy_nonoverlapping(input.as_ptr().add(start), d, i - start);
                    *d.add(i - start) = 0;
                    done += 1;
                    stored_any = true;
                }
            }
            _ => return done,
        }
    }
    done
}

// ---- ctime ----

extern "C" {
    fn localtime(t: *const i64) -> *const [i32; 9];
}

static mut CTIME_BUF: [u8; 32] = [0; 32];

/// `asctime`: `Sun Jan  1 00:00:00 1970\n`, from a broken-down time (`struct tm`'s first nine ints).
#[no_mangle]
pub unsafe extern "C" fn asctime(tm: *const [i32; 9]) -> *mut c_char {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let t = &*tm; // sec, min, hour, mday, mon, year, wday, yday, isdst
    let s = alloc::format!(
        "{} {} {:2} {:02}:{:02}:{:02} {}\n",
        DAYS[(t[6].rem_euclid(7)) as usize],
        MONTHS[(t[4].rem_euclid(12)) as usize],
        t[3],
        t[2],
        t[1],
        t[0],
        t[5] + 1900
    );
    #[allow(clippy::deref_addrof)]
    let buf = &mut *(&raw mut CTIME_BUF); // (one static buffer, as ctime has always had)
    let n = s.len().min(buf.len() - 1);
    buf[..n].copy_from_slice(&s.as_bytes()[..n]);
    buf[n] = 0;
    buf.as_mut_ptr() as *mut c_char
}

#[no_mangle]
pub unsafe extern "C" fn ctime(t: *const i64) -> *mut c_char {
    asctime(localtime(t))
}

/// `ioctl`: the terminal calls (`TIOCGWINSZ`, `TCGETS`) are answered with `ENOTTY` for now: a program that asks for the window
/// size falls back to 80 columns (the terminal's own size is read through `consoled`'s control channel by the shell, TIDY-UP T134).
#[no_mangle]
pub extern "C" fn ioctl(_fd: c_int, _request: u64, _arg: u64) -> c_int {
    crate::errno::set(25);
    -1
}
