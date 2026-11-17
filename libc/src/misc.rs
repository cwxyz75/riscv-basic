//! `<locale.h>`: just enough for portable programs. (Signals are in `sig.rs`.)

use core::ffi::{c_char, c_int};

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
