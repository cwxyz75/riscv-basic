//! Program start: read the startup message, set up `argv`, the environment and the standard
//! streams, run `main`.

use alloc::vec::Vec;
use core::ffi::{c_char, c_int};
use core::sync::atomic::Ordering;
use proto::startup::Role;

extern "C" {
    fn main(argc: c_int, argv: *mut *mut c_char) -> c_int;
}

/// A NUL-terminated copy of `s`, leaked (argv and environ live as long as the process).
fn leak_c_string(s: &[u8]) -> *mut c_char {
    let mut v: Vec<u8> = Vec::with_capacity(s.len() + 1);
    v.extend_from_slice(s);
    v.push(0);
    v.leak().as_mut_ptr() as *mut c_char
}

fn leak_array(items: &[Vec<u8>]) -> *mut *mut c_char {
    let mut v: Vec<*mut c_char> = items.iter().map(|s| leak_c_string(s)).collect();
    v.push(core::ptr::null_mut());
    v.leak().as_mut_ptr()
}

/// Start the C program: the exit status of `main`, after the `atexit` handlers have run (the
/// runtime's entry then exits with it).
pub fn run_main() -> i32 {
    let env = match rt::startup::read() {
        Ok(e) => e,
        Err(_) => return rt::startup::EXIT_BAD_STARTUP,
    };
    let out = env.handle(Role::Stdout).unwrap_or(0);
    crate::fd::init_std(
        env.handle(Role::Stdin).unwrap_or(0),
        out,
        // no separate standard error: it shares standard output
        env.handle(Role::Stderr).unwrap_or(out),
    );
    crate::fd::ROOT.store(env.namespace(b"/").unwrap_or(0), Ordering::Relaxed);
    crate::fd::CWD.store(env.namespace(b".").unwrap_or(0), Ordering::Relaxed);
    crate::fd::TTY_IN.store(
        u32::from(env.var(b"STDIN") == Some(&b"tty"[..])),
        Ordering::Relaxed,
    );
    crate::fd::TTY_OUT.store(
        u32::from(env.var(b"STDOUT") == Some(&b"tty"[..])),
        Ordering::Relaxed,
    );
    crate::file::init_std_buffering();
    let argv = leak_array(&env.startup.args);
    // SAFETY: single-threaded start-up; the array is leaked and NULL-terminated.
    unsafe { crate::stdlib::ENVIRON = leak_array(&env.startup.env) };
    // SAFETY: `main` is the C program's entry point.
    let code = unsafe { main(env.startup.args.len() as c_int, argv) };
    crate::stdlib::run_atexit();
    crate::file::flush_all();
    code
}
