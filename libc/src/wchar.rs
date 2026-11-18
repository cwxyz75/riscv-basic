//! Wide characters for the one encoding the system has, UTF-8 (`<wchar.h>`, `<wctype.h>`, `<uchar.h>`): the decoding, encoding,
//! classes and widths are `cfmt::mb` (tested against `std`). `wchar_t` is a 32-bit `int` holding a code point. The classes follow
//! Unicode through `std` and are not locale-sensitive (there is one locale).

use crate::errno;
use cfmt::mb::{self, Decoded, State};
use core::ffi::{c_char, c_int, c_uint};

/// `mbstate_t` as the header declares it: eight bytes, of which the first five are used (up to four bytes of a partial character and
/// how many).
#[repr(C)]
pub struct MbState {
    bytes: [u8; 4],
    len: u8,
    _pad: [u8; 3],
}

impl MbState {
    fn load(&self) -> State {
        State {
            bytes: self.bytes,
            len: self.len.min(3),
        }
    }
    fn store(&mut self, s: State) {
        self.bytes = s.bytes;
        self.len = s.len;
    }
}

static SHARED: rt::sync::Mutex<State> = rt::sync::Mutex::new(State {
    bytes: [0; 4],
    len: 0,
});

const WEOF: c_uint = u32::MAX;

/// `mbrtowc`: the next character of `n` bytes at `s` (into `*pwc` when it is not NULL). 0 for the NUL character, the bytes used, `-2`
/// for a prefix of a character (the state keeps it) and `-1` with `EILSEQ` for bytes that are not UTF-8.
#[no_mangle]
pub unsafe extern "C" fn mbrtowc(
    pwc: *mut c_int,
    s: *const c_char,
    n: usize,
    ps: *mut MbState,
) -> usize {
    let mut shared = SHARED.lock();
    let mut st = if ps.is_null() { *shared } else { (*ps).load() };
    if s.is_null() {
        // (reset the state: the same as converting an empty string)
        if ps.is_null() {
            *shared = State::default();
        } else {
            (*ps).store(State::default());
        }
        return 0;
    }
    let input = core::slice::from_raw_parts(s as *const u8, n);
    let r = mb::decode(&mut st, input);
    let (ret, new) = match r {
        Decoded::Char(c, used) => {
            if !pwc.is_null() {
                *pwc = c as c_int;
            }
            (if c == 0 { 0 } else { used }, State::default())
        }
        Decoded::Incomplete => (usize::MAX - 1, st),
        Decoded::Invalid => {
            errno::set(84); // EILSEQ
            (usize::MAX, State::default())
        }
    };
    if ps.is_null() {
        *shared = new;
    } else {
        (*ps).store(new);
    }
    ret
}

#[no_mangle]
pub unsafe extern "C" fn wcrtomb(s: *mut c_char, wc: c_int, ps: *mut MbState) -> usize {
    if s.is_null() {
        if !ps.is_null() {
            (*ps).store(State::default());
        }
        return 1;
    }
    let mut out = [0u8; 4];
    match mb::encode(wc as u32, &mut out) {
        Some(n) => {
            core::ptr::copy_nonoverlapping(out.as_ptr(), s as *mut u8, n);
            n
        }
        None => {
            errno::set(84);
            usize::MAX
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn mbsinit(ps: *const MbState) -> c_int {
    c_int::from(ps.is_null() || (*ps).len == 0)
}

#[no_mangle]
pub unsafe extern "C" fn mbrlen(s: *const c_char, n: usize, ps: *mut MbState) -> usize {
    mbrtowc(core::ptr::null_mut(), s, n, ps)
}

#[no_mangle]
pub unsafe extern "C" fn btowc(c: c_int) -> c_uint {
    if (0..0x80).contains(&c) {
        c as c_uint
    } else {
        WEOF
    }
}

#[no_mangle]
pub extern "C" fn wctob(c: c_uint) -> c_int {
    if c < 0x80 {
        c as c_int
    } else {
        -1
    }
}

/// `mbstowcs`: at most `n` wide characters (just the count when `dst` is NULL); `-1` for bad bytes.
#[no_mangle]
pub unsafe extern "C" fn mbstowcs(dst: *mut c_int, src: *const c_char, n: usize) -> usize {
    let mut st = State::default();
    let mut p = src as *const u8;
    let mut count = 0usize;
    loop {
        // (one byte at a time: the state carries a partial character)
        match mb::decode(&mut st, core::slice::from_ref(&*p)) {
            Decoded::Char(c, _) => {
                p = p.add(1);
                if c == 0 {
                    if !dst.is_null() && count < n {
                        *dst.add(count) = 0;
                    }
                    return count;
                }
                if !dst.is_null() {
                    if count >= n {
                        return count;
                    }
                    *dst.add(count) = c as c_int;
                }
                count += 1;
            }
            Decoded::Incomplete => p = p.add(1),
            Decoded::Invalid => {
                errno::set(84);
                return usize::MAX;
            }
        }
    }
}

/// `wcstombs`: at most `n` bytes (just the count when `dst` is NULL; a character that does not fit is not written).
#[no_mangle]
pub unsafe extern "C" fn wcstombs(dst: *mut c_char, src: *const c_int, n: usize) -> usize {
    let mut total = 0usize;
    let mut i = 0;
    loop {
        let wc = *src.add(i);
        if wc == 0 {
            if !dst.is_null() && total < n {
                *dst.add(total) = 0;
            }
            return total;
        }
        let mut out = [0u8; 4];
        let Some(len) = mb::encode(wc as u32, &mut out) else {
            errno::set(84);
            return usize::MAX;
        };
        if !dst.is_null() {
            if total + len > n {
                return total;
            }
            core::ptr::copy_nonoverlapping(out.as_ptr(), dst.add(total) as *mut u8, len);
        }
        total += len;
        i += 1;
    }
}

// ---- wide strings ----

#[no_mangle]
pub unsafe extern "C" fn wcslen(s: *const c_int) -> usize {
    let mut n = 0;
    while *s.add(n) != 0 {
        n += 1;
    }
    n
}

#[no_mangle]
pub unsafe extern "C" fn wcscpy(d: *mut c_int, s: *const c_int) -> *mut c_int {
    let n = wcslen(s);
    core::ptr::copy(s, d, n + 1);
    d
}

#[no_mangle]
pub unsafe extern "C" fn wcsncpy(d: *mut c_int, s: *const c_int, n: usize) -> *mut c_int {
    let mut i = 0;
    while i < n && *s.add(i) != 0 {
        *d.add(i) = *s.add(i);
        i += 1;
    }
    while i < n {
        *d.add(i) = 0;
        i += 1;
    }
    d
}

#[no_mangle]
pub unsafe extern "C" fn wcscmp(a: *const c_int, b: *const c_int) -> c_int {
    wcsncmp(a, b, usize::MAX)
}

#[no_mangle]
pub unsafe extern "C" fn wcsncmp(a: *const c_int, b: *const c_int, n: usize) -> c_int {
    for i in 0..n {
        let (x, y) = (*a.add(i), *b.add(i));
        if x != y {
            return if x < y { -1 } else { 1 };
        }
        if x == 0 {
            break;
        }
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn wcschr(s: *const c_int, c: c_int) -> *mut c_int {
    let mut i = 0;
    loop {
        if *s.add(i) == c {
            return s.add(i) as *mut c_int;
        }
        if *s.add(i) == 0 {
            return core::ptr::null_mut();
        }
        i += 1;
    }
}

#[no_mangle]
pub unsafe extern "C" fn wcsrchr(s: *const c_int, c: c_int) -> *mut c_int {
    let mut found = core::ptr::null_mut();
    let mut i = 0;
    loop {
        if *s.add(i) == c {
            found = s.add(i) as *mut c_int;
        }
        if *s.add(i) == 0 {
            return found;
        }
        i += 1;
    }
}

#[no_mangle]
pub unsafe extern "C" fn wcscat(d: *mut c_int, s: *const c_int) -> *mut c_int {
    wcscpy(d.add(wcslen(d)), s);
    d
}

#[no_mangle]
pub unsafe extern "C" fn wmemcpy(d: *mut c_int, s: *const c_int, n: usize) -> *mut c_int {
    core::ptr::copy_nonoverlapping(s, d, n);
    d
}

#[no_mangle]
pub unsafe extern "C" fn wmemmove(d: *mut c_int, s: *const c_int, n: usize) -> *mut c_int {
    core::ptr::copy(s, d, n);
    d
}

#[no_mangle]
pub unsafe extern "C" fn wmemset(d: *mut c_int, c: c_int, n: usize) -> *mut c_int {
    for i in 0..n {
        *d.add(i) = c;
    }
    d
}

#[no_mangle]
pub unsafe extern "C" fn wmemcmp(a: *const c_int, b: *const c_int, n: usize) -> c_int {
    for i in 0..n {
        let (x, y) = (*a.add(i), *b.add(i));
        if x != y {
            return if x < y { -1 } else { 1 };
        }
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn wmemchr(s: *const c_int, c: c_int, n: usize) -> *mut c_int {
    for i in 0..n {
        if *s.add(i) == c {
            return s.add(i) as *mut c_int;
        }
    }
    core::ptr::null_mut()
}

// ---- classes, case and width ----

macro_rules! class {
    ($name:ident, $f:path) => {
        #[no_mangle]
        pub extern "C" fn $name(wc: c_uint) -> c_int {
            c_int::from($f(wc))
        }
    };
}
class!(iswalpha, mb::is_alpha);
class!(iswdigit, mb::is_digit);
class!(iswalnum, mb::is_alnum);
class!(iswspace, mb::is_space);
class!(iswblank, mb::is_blank);
class!(iswupper, mb::is_upper);
class!(iswlower, mb::is_lower);
class!(iswcntrl, mb::is_cntrl);
class!(iswprint, mb::is_print);
class!(iswgraph, mb::is_graph);
class!(iswpunct, mb::is_punct);
class!(iswxdigit, mb::is_xdigit);

#[no_mangle]
pub extern "C" fn towlower(wc: c_uint) -> c_uint {
    mb::to_lower(wc)
}

#[no_mangle]
pub extern "C" fn towupper(wc: c_uint) -> c_uint {
    mb::to_upper(wc)
}

/// `wctype`: the class named by `name` (an index from 1), 0 for an unknown name; `iswctype` tests it.
#[no_mangle]
pub unsafe extern "C" fn wctype(name: *const c_char) -> usize {
    const NAMES: [&[u8]; 12] = [
        b"alpha", b"digit", b"alnum", b"space", b"blank", b"upper", b"lower", b"cntrl", b"print",
        b"graph", b"punct", b"xdigit",
    ];
    let n = core::ffi::CStr::from_ptr(name).to_bytes();
    NAMES.iter().position(|x| *x == n).map_or(0, |i| i + 1)
}

#[no_mangle]
pub extern "C" fn iswctype(wc: c_uint, desc: usize) -> c_int {
    match desc {
        1 => iswalpha(wc),
        2 => iswdigit(wc),
        3 => iswalnum(wc),
        4 => iswspace(wc),
        5 => iswblank(wc),
        6 => iswupper(wc),
        7 => iswlower(wc),
        8 => iswcntrl(wc),
        9 => iswprint(wc),
        10 => iswgraph(wc),
        11 => iswpunct(wc),
        12 => iswxdigit(wc),
        _ => 0,
    }
}

#[no_mangle]
pub extern "C" fn wcwidth(wc: c_int) -> c_int {
    mb::width(wc as u32)
}

#[no_mangle]
pub unsafe extern "C" fn wcswidth(s: *const c_int, n: usize) -> c_int {
    let mut total = 0;
    for i in 0..n {
        let c = *s.add(i);
        if c == 0 {
            break;
        }
        let w = mb::width(c as u32);
        if w < 0 {
            return -1;
        }
        total += w;
    }
    total
}

// ---- uchar.h: char32_t is the same thing here ----

#[no_mangle]
pub unsafe extern "C" fn mbrtoc32(
    pc: *mut u32,
    s: *const c_char,
    n: usize,
    ps: *mut MbState,
) -> usize {
    mbrtowc(pc as *mut c_int, s, n, ps)
}

#[no_mangle]
pub unsafe extern "C" fn c32rtomb(s: *mut c_char, c: u32, ps: *mut MbState) -> usize {
    wcrtomb(s, c as c_int, ps)
}
