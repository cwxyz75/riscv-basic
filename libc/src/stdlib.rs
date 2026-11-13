//! `<stdlib.h>`: allocation, number parsing (over `cfmt`), `qsort`/`bsearch`, `rand`, `exit`.

use crate::errno;
use alloc::alloc::{alloc, dealloc, realloc as rust_realloc, Layout};
use core::ffi::{c_char, c_int, c_long, c_longlong, c_ulong, c_ulonglong, c_void};

const HEADER: usize = 16;
const ALIGN: usize = 16;

/// Every block carries its size in a 16-byte header, so `free` and `realloc` need nothing else.
#[no_mangle]
pub extern "C" fn malloc(n: usize) -> *mut c_void {
    let Some(total) = n.checked_add(HEADER) else {
        errno::set(errno::ENOMEM);
        return core::ptr::null_mut();
    };
    let Ok(layout) = Layout::from_size_align(total, ALIGN) else {
        errno::set(errno::ENOMEM);
        return core::ptr::null_mut();
    };
    // SAFETY: a non-zero-size layout; the allocator is the runtime's heap.
    let p = unsafe { alloc(layout) };
    if p.is_null() {
        errno::set(errno::ENOMEM);
        return p as *mut c_void;
    }
    // SAFETY: `p` is valid for `total >= HEADER` bytes and 16-aligned.
    unsafe {
        *(p as *mut usize) = n;
        p.add(HEADER) as *mut c_void
    }
}

#[no_mangle]
pub extern "C" fn calloc(count: usize, size: usize) -> *mut c_void {
    let Some(n) = count.checked_mul(size) else {
        errno::set(errno::ENOMEM);
        return core::ptr::null_mut();
    };
    let p = malloc(n);
    if !p.is_null() {
        // SAFETY: `p` is valid for `n` bytes.
        unsafe { core::ptr::write_bytes(p as *mut u8, 0, n) };
    }
    p
}

#[no_mangle]
pub unsafe extern "C" fn free(p: *mut c_void) {
    if p.is_null() {
        return;
    }
    let base = (p as *mut u8).sub(HEADER);
    let n = *(base as *const usize);
    dealloc(base, Layout::from_size_align_unchecked(n + HEADER, ALIGN));
}

#[no_mangle]
pub unsafe extern "C" fn realloc(p: *mut c_void, n: usize) -> *mut c_void {
    if p.is_null() {
        return malloc(n);
    }
    if n == 0 {
        free(p);
        return core::ptr::null_mut();
    }
    let base = (p as *mut u8).sub(HEADER);
    let old = *(base as *const usize);
    // the same layout check as `malloc`: an absurd size is ENOMEM, not undefined behaviour
    let Some(total) = n
        .checked_add(HEADER)
        .filter(|t| Layout::from_size_align(*t, ALIGN).is_ok())
    else {
        errno::set(errno::ENOMEM);
        return core::ptr::null_mut();
    };
    let q = rust_realloc(
        base,
        Layout::from_size_align_unchecked(old + HEADER, ALIGN),
        total,
    );
    if q.is_null() {
        errno::set(errno::ENOMEM);
        return core::ptr::null_mut();
    }
    *(q as *mut usize) = n;
    q.add(HEADER) as *mut c_void
}

#[no_mangle]
pub extern "C" fn abs(x: c_int) -> c_int {
    x.wrapping_abs()
}
#[no_mangle]
pub extern "C" fn labs(x: c_long) -> c_long {
    x.wrapping_abs()
}
#[no_mangle]
pub extern "C" fn llabs(x: c_longlong) -> c_longlong {
    x.wrapping_abs()
}

/// The bytes of the C string at `s` (without the NUL), for the parsers. This scans to the NUL, so
/// each `strtol`/`strtod` call is O(strlen); fine for the short buffers C programs tokenise.
unsafe fn text<'a>(s: *const c_char) -> &'a [u8] {
    let mut n = 0;
    while *s.add(n) != 0 {
        n += 1;
    }
    core::slice::from_raw_parts(s as *const u8, n)
}

unsafe fn set_end(end: *mut *mut c_char, s: *const c_char, used: usize) {
    if !end.is_null() {
        *end = s.add(used) as *mut c_char;
    }
}

#[no_mangle]
pub unsafe extern "C" fn strtol(s: *const c_char, end: *mut *mut c_char, base: c_int) -> c_long {
    let r = cfmt::strtol(text(s), base as u32);
    set_end(end, s, r.len);
    if r.range_error {
        errno::set(errno::ERANGE);
    }
    r.value as c_long
}

#[no_mangle]
pub unsafe extern "C" fn strtoll(
    s: *const c_char,
    end: *mut *mut c_char,
    base: c_int,
) -> c_longlong {
    strtol(s, end, base) as c_longlong
}

#[no_mangle]
pub unsafe extern "C" fn strtoul(s: *const c_char, end: *mut *mut c_char, base: c_int) -> c_ulong {
    let r = cfmt::strtoul(text(s), base as u32);
    set_end(end, s, r.len);
    if r.range_error {
        errno::set(errno::ERANGE);
    }
    r.value as c_ulong
}

#[no_mangle]
pub unsafe extern "C" fn strtoull(
    s: *const c_char,
    end: *mut *mut c_char,
    base: c_int,
) -> c_ulonglong {
    strtoul(s, end, base) as c_ulonglong
}

#[no_mangle]
pub unsafe extern "C" fn strtod(s: *const c_char, end: *mut *mut c_char) -> f64 {
    let r = cfmt::strtod(text(s));
    set_end(end, s, r.len);
    if r.range_error {
        errno::set(errno::ERANGE);
    }
    r.value
}

#[no_mangle]
pub unsafe extern "C" fn atoi(s: *const c_char) -> c_int {
    strtol(s, core::ptr::null_mut(), 10) as c_int
}
#[no_mangle]
pub unsafe extern "C" fn atol(s: *const c_char) -> c_long {
    strtol(s, core::ptr::null_mut(), 10)
}
#[no_mangle]
pub unsafe extern "C" fn atof(s: *const c_char) -> f64 {
    strtod(s, core::ptr::null_mut())
}

type Compare = unsafe extern "C" fn(*const c_void, *const c_void) -> c_int;

unsafe fn swap(a: *mut u8, b: *mut u8, size: usize) {
    if a != b {
        core::ptr::swap_nonoverlapping(a, b, size);
    }
}

/// Heapsort: in place, O(n log n), no recursion (qsort need not be stable).
#[no_mangle]
pub unsafe extern "C" fn qsort(base: *mut c_void, n: usize, size: usize, cmp: Compare) {
    if n < 2 || size == 0 {
        return;
    }
    let at = |i: usize| (base as *mut u8).add(i * size);
    let sift = |mut root: usize, end: usize| loop {
        let mut child = 2 * root + 1;
        if child >= end {
            return;
        }
        if child + 1 < end && cmp(at(child) as *const c_void, at(child + 1) as *const c_void) < 0 {
            child += 1;
        }
        if cmp(at(root) as *const c_void, at(child) as *const c_void) >= 0 {
            return;
        }
        swap(at(root), at(child), size);
        root = child;
    };
    for start in (0..n / 2).rev() {
        sift(start, n);
    }
    for end in (1..n).rev() {
        swap(at(0), at(end), size);
        sift(0, end);
    }
}

#[no_mangle]
pub unsafe extern "C" fn bsearch(
    key: *const c_void,
    base: *const c_void,
    n: usize,
    size: usize,
    cmp: Compare,
) -> *mut c_void {
    let (mut lo, mut hi) = (0usize, n);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let p = (base as *const u8).add(mid * size);
        let c = cmp(key, p as *const c_void);
        if c == 0 {
            return p as *mut c_void;
        }
        if c < 0 {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    core::ptr::null_mut()
}

static mut RAND_STATE: u64 = 1;

#[no_mangle]
pub extern "C" fn srand(seed: u32) {
    // SAFETY: a plain store (one thread until pthreads; rand is not required to be thread safe).
    unsafe { RAND_STATE = u64::from(seed) }
}

/// A 64-bit LCG (Knuth's MMIX constants); the high bits are returned, 31 of them.
#[no_mangle]
pub extern "C" fn rand() -> c_int {
    // SAFETY: as in `srand`.
    unsafe {
        RAND_STATE = RAND_STATE
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (RAND_STATE >> 33) as c_int
    }
}

static mut ATEXIT: [Option<extern "C" fn()>; 32] = [None; 32];
static mut ATEXIT_N: usize = 0;

#[no_mangle]
pub extern "C" fn atexit(f: extern "C" fn()) -> c_int {
    // SAFETY: single-threaded registration table.
    unsafe {
        let n = ATEXIT_N;
        if n >= 32 {
            return -1;
        }
        ATEXIT[n] = Some(f);
        ATEXIT_N = n + 1;
    }
    0
}

/// Run the registered handlers, last registered first (each at most once).
pub fn run_atexit() {
    // SAFETY: as in `atexit`.
    unsafe {
        while ATEXIT_N > 0 {
            ATEXIT_N -= 1;
            if let Some(f) = ATEXIT[ATEXIT_N].take() {
                f();
            }
        }
    }
}

#[no_mangle]
pub extern "C" fn exit(code: c_int) -> ! {
    run_atexit();
    crate::file::flush_all();
    rt::sys::exit(code)
}

#[no_mangle]
pub extern "C" fn _Exit(code: c_int) -> ! {
    rt::sys::exit(code)
}

#[no_mangle]
pub extern "C" fn abort() -> ! {
    rt::sys::exit(134) // 128 + SIGABRT, as a shell would show
}

#[no_mangle]
pub extern "C" fn system(_cmd: *const c_char) -> c_int {
    -1
}

/// The environment (`NAME=value` strings, NUL-terminated), set by `run_main`.
pub static mut ENVIRON: *mut *mut c_char = core::ptr::null_mut();

#[no_mangle]
pub unsafe extern "C" fn getenv(name: *const c_char) -> *mut c_char {
    let want = text(name);
    let mut p = ENVIRON;
    if p.is_null() {
        return core::ptr::null_mut();
    }
    while !(*p).is_null() {
        let e = text(*p);
        if e.len() > want.len() && &e[..want.len()] == want && e[want.len()] == b'=' {
            return (*p).add(want.len() + 1);
        }
        p = p.add(1);
    }
    core::ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn __assert_fail(expr: *const c_char, file: *const c_char, line: c_int) -> ! {
    let mut msg: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    msg.extend_from_slice(b"assertion failed: ");
    msg.extend_from_slice(text(expr));
    msg.extend_from_slice(b", file ");
    msg.extend_from_slice(text(file));
    msg.extend_from_slice(b", line ");
    msg.extend_from_slice(alloc::format!("{line}\n").as_bytes());
    crate::file::write_stderr(&msg);
    abort()
}
