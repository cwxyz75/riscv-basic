//! `errno` (Linux numbering, which the headers publish) and `strerror`'s messages.

use core::ffi::c_int;

/// `errno` is `(*__errno_location())` in the header: per thread (the thread control block `tp`
/// points to), with a process-wide fallback for a thread this library did not create.
#[no_mangle]
pub extern "C" fn __errno_location() -> *mut c_int {
    crate::thread::errno_ptr()
}

pub fn set(e: c_int) {
    // SAFETY: the calling thread's errno.
    unsafe { *__errno_location() = e }
}

pub fn get() -> c_int {
    // SAFETY: the calling thread's errno.
    unsafe { *__errno_location() }
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
        3 => b"No such process\0",
        4 => b"Interrupted system call\0",
        5 => b"Input/output error\0",
        7 => b"Argument list too long\0",
        10 => b"No child processes\0",
        11 => b"Resource temporarily unavailable\0",
        14 => b"Bad address\0",
        110 => b"Connection timed out\0",
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
        84 => b"Illegal byte sequence\0",
        33 => b"Numerical argument out of domain\0",
        34 => b"Numerical result out of range\0",
        38 => b"Function not implemented\0",
        6 => b"No such device or address\0",
        8 => b"Exec format error\0",
        15 => b"Block device required\0",
        16 => b"Device or resource busy\0",
        18 => b"Invalid cross-device link\0",
        19 => b"No such device\0",
        23 => b"Too many open files in system\0",
        26 => b"Text file busy\0",
        27 => b"File too large\0",
        30 => b"Read-only file system\0",
        31 => b"Too many links\0",
        35 => b"Resource deadlock avoided\0",
        37 => b"No locks available\0",
        40 => b"Too many levels of symbolic links\0",
        42 => b"No message of desired type\0",
        43 => b"Identifier removed\0",
        60 => b"Device not a stream\0",
        61 => b"No data available\0",
        62 => b"Timer expired\0",
        63 => b"Out of streams resources\0",
        67 => b"Link has been severed\0",
        71 => b"Protocol error\0",
        72 => b"Multihop attempted\0",
        74 => b"Bad message\0",
        88 => b"Socket operation on non-socket\0",
        89 => b"Destination address required\0",
        90 => b"Message too long\0",
        91 => b"Protocol wrong type for socket\0",
        92 => b"Protocol not available\0",
        93 => b"Protocol not supported\0",
        95 => b"Operation not supported\0",
        97 => b"Address family not supported by protocol\0",
        98 => b"Address already in use\0",
        99 => b"Cannot assign requested address\0",
        100 => b"Network is down\0",
        101 => b"Network is unreachable\0",
        102 => b"Network dropped connection on reset\0",
        103 => b"Software caused connection abort\0",
        104 => b"Connection reset by peer\0",
        105 => b"No buffer space available\0",
        106 => b"Transport endpoint is already connected\0",
        107 => b"Transport endpoint is not connected\0",
        111 => b"Connection refused\0",
        113 => b"No route to host\0",
        114 => b"Operation already in progress\0",
        115 => b"Operation now in progress\0",
        116 => b"Stale file handle\0",
        122 => b"Disk quota exceeded\0",
        125 => b"Operation canceled\0",
        130 => b"Owner died\0",
        131 => b"State not recoverable\0",
        _ => b"Unknown error\0",
    }
}
