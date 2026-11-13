//! `<string.h>`.

use core::ffi::{c_char, c_int, c_void};

unsafe fn len(s: *const c_char) -> usize {
    let mut n = 0;
    while *s.add(n) != 0 {
        n += 1;
    }
    n
}

#[no_mangle]
pub unsafe extern "C" fn strlen(s: *const c_char) -> usize {
    len(s)
}

#[no_mangle]
pub unsafe extern "C" fn strnlen(s: *const c_char, max: usize) -> usize {
    let mut n = 0;
    while n < max && *s.add(n) != 0 {
        n += 1;
    }
    n
}

#[no_mangle]
pub unsafe extern "C" fn strcpy(d: *mut c_char, s: *const c_char) -> *mut c_char {
    let mut i = 0;
    loop {
        let c = *s.add(i);
        *d.add(i) = c;
        if c == 0 {
            return d;
        }
        i += 1;
    }
}

#[no_mangle]
pub unsafe extern "C" fn strncpy(d: *mut c_char, s: *const c_char, n: usize) -> *mut c_char {
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
pub unsafe extern "C" fn strcat(d: *mut c_char, s: *const c_char) -> *mut c_char {
    strcpy(d.add(len(d)), s);
    d
}

#[no_mangle]
pub unsafe extern "C" fn strncat(d: *mut c_char, s: *const c_char, n: usize) -> *mut c_char {
    let end = d.add(len(d));
    let mut i = 0;
    while i < n && *s.add(i) != 0 {
        *end.add(i) = *s.add(i);
        i += 1;
    }
    *end.add(i) = 0;
    d
}

#[no_mangle]
pub unsafe extern "C" fn strcmp(a: *const c_char, b: *const c_char) -> c_int {
    let mut i = 0;
    loop {
        let (x, y) = (*a.add(i) as u8, *b.add(i) as u8);
        if x != y || x == 0 {
            return c_int::from(x) - c_int::from(y);
        }
        i += 1;
    }
}

#[no_mangle]
pub unsafe extern "C" fn strncmp(a: *const c_char, b: *const c_char, n: usize) -> c_int {
    for i in 0..n {
        let (x, y) = (*a.add(i) as u8, *b.add(i) as u8);
        if x != y || x == 0 {
            return c_int::from(x) - c_int::from(y);
        }
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn strcoll(a: *const c_char, b: *const c_char) -> c_int {
    strcmp(a, b) // the "C" locale
}

#[no_mangle]
pub unsafe extern "C" fn strchr(s: *const c_char, c: c_int) -> *mut c_char {
    let mut p = s;
    loop {
        if *p as u8 == c as u8 {
            return p as *mut c_char;
        }
        if *p == 0 {
            return core::ptr::null_mut();
        }
        p = p.add(1);
    }
}

#[no_mangle]
pub unsafe extern "C" fn strrchr(s: *const c_char, c: c_int) -> *mut c_char {
    let mut found = core::ptr::null_mut();
    let mut p = s;
    loop {
        if *p as u8 == c as u8 {
            found = p as *mut c_char;
        }
        if *p == 0 {
            return found;
        }
        p = p.add(1);
    }
}

#[no_mangle]
pub unsafe extern "C" fn strstr(h: *const c_char, n: *const c_char) -> *mut c_char {
    let nl = len(n);
    if nl == 0 {
        return h as *mut c_char;
    }
    let mut p = h;
    while *p != 0 {
        if strncmp(p, n, nl) == 0 {
            return p as *mut c_char;
        }
        p = p.add(1);
    }
    core::ptr::null_mut()
}

unsafe fn in_set(c: c_char, set: *const c_char) -> bool {
    let mut p = set;
    while *p != 0 {
        if *p == c {
            return true;
        }
        p = p.add(1);
    }
    false
}

#[no_mangle]
pub unsafe extern "C" fn strspn(s: *const c_char, accept: *const c_char) -> usize {
    let mut n = 0;
    while *s.add(n) != 0 && in_set(*s.add(n), accept) {
        n += 1;
    }
    n
}

#[no_mangle]
pub unsafe extern "C" fn strcspn(s: *const c_char, reject: *const c_char) -> usize {
    let mut n = 0;
    while *s.add(n) != 0 && !in_set(*s.add(n), reject) {
        n += 1;
    }
    n
}

#[no_mangle]
pub unsafe extern "C" fn strpbrk(s: *const c_char, set: *const c_char) -> *mut c_char {
    let mut p = s;
    while *p != 0 {
        if in_set(*p, set) {
            return p as *mut c_char;
        }
        p = p.add(1);
    }
    core::ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn memchr(s: *const c_void, c: c_int, n: usize) -> *mut c_void {
    let p = s as *const u8;
    for i in 0..n {
        if *p.add(i) == c as u8 {
            return p.add(i) as *mut c_void;
        }
    }
    core::ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn strdup(s: *const c_char) -> *mut c_char {
    let n = len(s) + 1;
    let d = crate::stdlib::malloc(n) as *mut c_char;
    if !d.is_null() {
        core::ptr::copy_nonoverlapping(s, d, n);
    }
    d
}

#[no_mangle]
pub unsafe extern "C" fn strerror(e: c_int) -> *const c_char {
    crate::errno::message(e).as_ptr() as *const c_char
}
