//! `<signal.h>` and `<locale.h>`: just enough for portable programs. There are no signals (S11):
//! `signal` records the handler and `raise` calls it, or aborts for a fatal default.

use core::ffi::{c_char, c_int};

pub type Handler = usize;
pub const SIG_DFL: Handler = 0;
pub const SIG_IGN: Handler = 1;
pub const SIG_ERR: Handler = usize::MAX;

static mut HANDLERS: [Handler; 32] = [0; 32];

#[no_mangle]
pub extern "C" fn signal(sig: c_int, h: Handler) -> Handler {
    if !(1..32).contains(&sig) {
        crate::errno::set(22);
        return SIG_ERR;
    }
    // SAFETY: single-threaded table.
    unsafe {
        let old = HANDLERS[sig as usize];
        HANDLERS[sig as usize] = h;
        old
    }
}

#[no_mangle]
pub extern "C" fn raise(sig: c_int) -> c_int {
    if !(1..32).contains(&sig) {
        return -1;
    }
    // SAFETY: as in `signal`.
    let h = unsafe { HANDLERS[sig as usize] };
    match h {
        SIG_IGN => 0,
        SIG_DFL => {
            if matches!(sig, 2 | 6 | 8 | 11 | 15) {
                crate::stdlib::abort() // SIGINT, SIGABRT, SIGFPE, SIGSEGV, SIGTERM: default is to end
            }
            0
        }
        f => {
            // SAFETY: a function pointer the program registered.
            let f: extern "C" fn(c_int) = unsafe { core::mem::transmute(f) };
            f(sig);
            0
        }
    }
}

#[repr(C)]
pub struct Lconv {
    decimal_point: *const c_char,
    thousands_sep: *const c_char,
    grouping: *const c_char,
}

static DOT: &[u8] = b".\0";
static EMPTY: &[u8] = b"\0";
static LCONV: Lconv = Lconv {
    decimal_point: DOT.as_ptr() as *const c_char,
    thousands_sep: EMPTY.as_ptr() as *const c_char,
    grouping: EMPTY.as_ptr() as *const c_char,
};

// SAFETY: the pointers name immutable static strings.
unsafe impl Sync for Lconv {}

#[no_mangle]
pub extern "C" fn localeconv() -> *const Lconv {
    &LCONV
}

#[no_mangle]
pub unsafe extern "C" fn setlocale(_category: c_int, name: *const c_char) -> *const c_char {
    // only "C" (and the default "") exist
    if name.is_null() || *name == 0 || core::ffi::CStr::from_ptr(name).to_bytes() == b"C" {
        return c"C".as_ptr();
    }
    core::ptr::null()
}
