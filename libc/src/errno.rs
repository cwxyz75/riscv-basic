//! `errno` (Linux numbering, which the headers publish) and `strerror`'s messages.

use core::ffi::c_int;

static mut ERRNO: c_int = 0;

/// `errno` is `(*__errno_location())` in the header. One per process for now; per-thread storage
/// arrives with pthreads (S7c-3).
#[no_mangle]
pub extern "C" fn __errno_location() -> *mut c_int {
    core::ptr::addr_of_mut!(ERRNO)
}

pub fn set(e: c_int) {
    // SAFETY: a plain integer store to the process's errno (single-threaded until pthreads).
    unsafe { ERRNO = e }
}

pub const ERANGE: c_int = 34;
pub const EINVAL: c_int = 22;
pub const ENOMEM: c_int = 12;

/// NUL-terminated message for `e`.
pub fn message(e: c_int) -> &'static [u8] {
    match e {
        0 => b"Success\0",
        1 => b"Operation not permitted\0",
        2 => b"No such file or directory\0",
        5 => b"Input/output error\0",
        9 => b"Bad file descriptor\0",
        12 => b"Cannot allocate memory\0",
        13 => b"Permission denied\0",
        17 => b"File exists\0",
        20 => b"Not a directory\0",
        21 => b"Is a directory\0",
        22 => b"Invalid argument\0",
        24 => b"Too many open files\0",
        25 => b"Inappropriate ioctl for device\0",
        28 => b"No space left on device\0",
        29 => b"Illegal seek\0",
        32 => b"Broken pipe\0",
        36 => b"File name too long\0",
        39 => b"Directory not empty\0",
        75 => b"Value too large for defined data type\0",
        33 => b"Numerical argument out of domain\0",
        34 => b"Numerical result out of range\0",
        38 => b"Function not implemented\0",
        _ => b"Unknown error\0",
    }
}
