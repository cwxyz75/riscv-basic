//! More of POSIX, from the gap survey (`LIBC-GAPS.md`): the `*at` family over the path calls, `error`, the variadic `exec*`, the
//! `strings.h` functions, `creat`, `ftello`, `mkostemp`, `pthread_sigmask` and a few no-ops.

use crate::{errno, fd};
use core::ffi::{c_char, c_int, c_void};

const AT_SYMLINK_NOFOLLOW: c_int = 0x100;
const AT_REMOVEDIR: c_int = 0x200;
const ENOSYS: c_int = 38;
const EPERM: c_int = 1;

/// Run `f` on the resolved path of (`dirfd`, `path`), or set `errno` and return -1.
unsafe fn at<R: From<i8>>(
    dirfd: c_int,
    path: *const c_char,
    f: impl FnOnce(*const c_char) -> R,
) -> R {
    match fd::resolve_at(dirfd, path) {
        Ok(p) => f(p.as_ptr() as *const c_char),
        Err(e) => {
            errno::set(e);
            R::from(-1)
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn fstatat(
    dirfd: c_int,
    path: *const c_char,
    out: *mut fd::Stat,
    flags: c_int,
) -> c_int {
    at(dirfd, path, |p| {
        if flags & AT_SYMLINK_NOFOLLOW != 0 {
            fd::lstat(p, out)
        } else {
            fd::stat(p, out)
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn unlinkat(dirfd: c_int, path: *const c_char, flags: c_int) -> c_int {
    at(dirfd, path, |p| {
        if flags & AT_REMOVEDIR != 0 {
            fd::rmdir(p)
        } else {
            fd::unlink(p)
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn faccessat(
    dirfd: c_int,
    path: *const c_char,
    mode: c_int,
    _flags: c_int,
) -> c_int {
    at(dirfd, path, |p| fd::access(p, mode))
}

#[no_mangle]
pub unsafe extern "C" fn fchmodat(
    dirfd: c_int,
    path: *const c_char,
    mode: u32,
    _flags: c_int,
) -> c_int {
    at(dirfd, path, |p| fd::chmod(p, mode))
}

#[no_mangle]
pub unsafe extern "C" fn fchownat(
    dirfd: c_int,
    path: *const c_char,
    uid: c_int,
    gid: c_int,
    flags: c_int,
) -> c_int {
    at(dirfd, path, |p| {
        if flags & AT_SYMLINK_NOFOLLOW != 0 {
            crate::compat::lchown(p, uid, gid)
        } else {
            fd::chown(p, uid, gid)
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn mkdirat(dirfd: c_int, path: *const c_char, mode: u32) -> c_int {
    at(dirfd, path, |p| fd::mkdir(p, mode as c_int))
}

#[no_mangle]
pub unsafe extern "C" fn readlinkat(
    dirfd: c_int,
    path: *const c_char,
    buf: *mut c_char,
    size: usize,
) -> isize {
    at(dirfd, path, |p| fd::readlink(p, buf, size) as i64 as isize) as isize
}

#[no_mangle]
pub unsafe extern "C" fn symlinkat(
    target: *const c_char,
    dirfd: c_int,
    linkpath: *const c_char,
) -> c_int {
    at(dirfd, linkpath, |p| fd::symlink(target, p))
}

#[no_mangle]
pub unsafe extern "C" fn renameat(
    olddirfd: c_int,
    old: *const c_char,
    newdirfd: c_int,
    new: *const c_char,
) -> c_int {
    let (Ok(a), Ok(b)) = (fd::resolve_at(olddirfd, old), fd::resolve_at(newdirfd, new)) else {
        errno::set(9);
        return -1;
    };
    fd::rename(a.as_ptr() as *const c_char, b.as_ptr() as *const c_char)
}

/// Hard links are refused (the file protocol has none: `EPERM`, as `link`).
#[no_mangle]
pub extern "C" fn linkat(
    _oldfd: c_int,
    _old: *const c_char,
    _newfd: c_int,
    _new: *const c_char,
    _flags: c_int,
) -> c_int {
    errno::set(EPERM);
    -1
}

/// No device nodes or named pipes in the file protocol.
#[no_mangle]
pub extern "C" fn mkfifoat(_dirfd: c_int, _path: *const c_char, _mode: u32) -> c_int {
    errno::set(ENOSYS);
    -1
}

#[no_mangle]
pub extern "C" fn mknodat(_dirfd: c_int, _path: *const c_char, _mode: u32, _dev: u64) -> c_int {
    errno::set(ENOSYS);
    -1
}

// ---- error.h ----

#[no_mangle]
pub static mut error_message_count: u32 = 0;
#[no_mangle]
pub static mut error_print_progname: *const c_void = core::ptr::null();

/// `error`: `program: message[: strerror]\n` on standard error; ends the program when `status` is not 0.
#[no_mangle]
pub unsafe extern "C" fn verror(status: c_int, errnum: c_int, fmt: *const c_char, ap: *const u64) {
    let err = crate::file::stderr.0;
    crate::file::fflush(crate::file::stdout.0);
    let prog = crate::start::program_name();
    crate::file::fputs(prog, err);
    crate::file::fputs(c": ".as_ptr(), err);
    crate::file::vfprintf(err, fmt, ap);
    if errnum != 0 {
        crate::file::fputs(c": ".as_ptr(), err);
        crate::file::fputs(crate::string::strerror(errnum), err);
    }
    crate::file::fputc(b'\n' as c_int, err);
    crate::file::fflush(err);
    error_message_count = error_message_count.wrapping_add(1);
    if status != 0 {
        crate::stdlib::exit(status);
    }
}

/// `error_at_line`: the same with `file:line: ` after the program name.
#[no_mangle]
pub unsafe extern "C" fn verror_at_line(
    status: c_int,
    errnum: c_int,
    file: *const c_char,
    line: u32,
    fmt: *const c_char,
    ap: *const u64,
) {
    let err = crate::file::stderr.0;
    crate::file::fflush(crate::file::stdout.0);
    let prog = crate::start::program_name();
    crate::file::fputs(prog, err);
    crate::file::fputs(c":".as_ptr(), err);
    crate::file::fputs(file, err);
    let l = alloc::format!(":{line}: \0");
    crate::file::fputs(l.as_ptr() as *const c_char, err);
    crate::file::vfprintf(err, fmt, ap);
    if errnum != 0 {
        crate::file::fputs(c": ".as_ptr(), err);
        crate::file::fputs(crate::string::strerror(errnum), err);
    }
    crate::file::fputc(b'\n' as c_int, err);
    crate::file::fflush(err);
    error_message_count = error_message_count.wrapping_add(1);
    if status != 0 {
        crate::stdlib::exit(status);
    }
}

core::arch::global_asm!(include_str!("variadic_posix2.s"));

// ---- exec with a list of arguments ----

unsafe fn collect(
    first: *const c_char,
    ap: *const u64,
) -> (alloc::vec::Vec<*const c_char>, *const u64) {
    let mut args = alloc::vec::Vec::new();
    args.push(first);
    let mut p = ap;
    loop {
        let v = *p as *const c_char;
        p = p.add(1);
        args.push(v);
        if v.is_null() {
            break;
        }
    }
    (args, p)
}

#[no_mangle]
pub unsafe extern "C" fn vexecl(path: *const c_char, arg0: *const c_char, ap: *const u64) -> c_int {
    let (args, _) = collect(arg0, ap);
    crate::spawn::execv(path, args.as_ptr())
}

#[no_mangle]
pub unsafe extern "C" fn vexeclp(
    file: *const c_char,
    arg0: *const c_char,
    ap: *const u64,
) -> c_int {
    let (args, _) = collect(arg0, ap);
    crate::spawn::execvp(file, args.as_ptr())
}

#[no_mangle]
pub unsafe extern "C" fn vexecle(
    path: *const c_char,
    arg0: *const c_char,
    ap: *const u64,
) -> c_int {
    let (args, after) = collect(arg0, ap);
    let envp = *after as *const *const c_char;
    crate::spawn::execve(path, args.as_ptr(), envp)
}

// ---- strings.h and small things ----

#[no_mangle]
pub unsafe extern "C" fn bcmp(a: *const c_void, b: *const c_void, n: usize) -> c_int {
    let (a, b) = (a as *const u8, b as *const u8);
    for i in 0..n {
        let d = i32::from(*a.add(i)) - i32::from(*b.add(i));
        if d != 0 {
            return d;
        }
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn bcopy(src: *const c_void, dst: *mut c_void, n: usize) {
    core::ptr::copy(src as *const u8, dst as *mut u8, n);
}

#[no_mangle]
pub unsafe extern "C" fn index(s: *const c_char, c: c_int) -> *mut c_char {
    crate::string::strchr(s, c)
}

#[no_mangle]
pub unsafe extern "C" fn rindex(s: *const c_char, c: c_int) -> *mut c_char {
    crate::string::strrchr(s, c)
}

#[no_mangle]
pub unsafe extern "C" fn creat(path: *const c_char, mode: c_int) -> c_int {
    // O_WRONLY | O_CREAT | O_TRUNC
    fd::open(path, 1 | 0x40 | 0x200, mode)
}

#[no_mangle]
pub unsafe extern "C" fn ftello(f: *mut crate::file::File) -> i64 {
    crate::file::ftell(f) as i64
}

/// `mkostemp`: `mkstemp` (the extra flags, `O_CLOEXEC` and the like, change nothing here).
#[no_mangle]
pub unsafe extern "C" fn mkostemp(tmpl: *mut c_char, _flags: c_int) -> c_int {
    crate::compat::mkstemp(tmpl)
}

#[no_mangle]
pub unsafe extern "C" fn pthread_sigmask(how: c_int, set: *const u64, old: *mut u64) -> c_int {
    if crate::sig::sigprocmask(how, set, old) == 0 {
        0
    } else {
        22
    }
}

/// The C locale only: nothing to read.
#[no_mangle]
pub extern "C" fn tzset() {}

/// `strxfrm` in the C locale: a copy.
#[no_mangle]
pub unsafe extern "C" fn strxfrm(dst: *mut c_char, src: *const c_char, n: usize) -> usize {
    let len = crate::string::strlen(src);
    if n > 0 {
        let k = len.min(n - 1);
        core::ptr::copy_nonoverlapping(src, dst, k);
        *dst.add(k) = 0;
    }
    len
}

/// `fpurge` (the BSD name) and `__fpurge` (glibc's): drop what is buffered in the stream (no read-ahead is kept here beyond one pushed-back character).
#[no_mangle]
pub unsafe extern "C" fn __fpurge(_f: *mut crate::file::File) {}

#[no_mangle]
pub unsafe extern "C" fn fpurge(_f: *mut crate::file::File) -> c_int {
    0
}

/// `flock`: advisory locks do nothing here (see `sys/file.h`).
#[no_mangle]
pub extern "C" fn flock(_fd: c_int, _op: c_int) -> c_int {
    0
}
