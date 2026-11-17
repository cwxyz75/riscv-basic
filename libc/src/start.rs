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

extern "C" fn call_main(arg: usize) -> c_int {
    // SAFETY: `arg` points to the `(argc, argv)` pair `run_main` keeps alive for the call.
    let (argc, argv) = unsafe { *(arg as *const (c_int, *mut *mut c_char)) };
    // SAFETY: `main` is the C program's entry point.
    unsafe { main(argc, argv) }
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
    crate::spawn::init(env.handle(Role::ServiceSpawn).unwrap_or(0));
    let num = |name: &[u8]| {
        env.var(name)
            .and_then(|v| core::str::from_utf8(v).ok())
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0)
    };
    crate::sig::init(
        env.handle(Role::SignalChannel).unwrap_or(0),
        num(b"PID"),
        num(b"PPID"),
    );
    crate::socket::SVC.store(env.namespace(b"/svc").unwrap_or(0), Ordering::Relaxed);
    // the namespace table: every absolute prefix with its handle, and the cwd as a path (the `PWD` the parent set) plus handle
    let mut table = pathres::Table::default();
    for (prefix, i) in &env.startup.namespace {
        if let Some(&h) = env.handles.get(*i as usize) {
            if prefix.first() == Some(&b'/') {
                table.mounts.push((pathres::parse_abs(prefix), h));
            }
        }
    }
    table.cwd_handle = env.namespace(b".");
    table.cwd = env
        .var(b"PWD")
        .filter(|p| p.first() == Some(&b'/'))
        .map(pathres::parse_abs);
    crate::fd::set_table(table);
    if let Some(u) = env
        .var(b"UID")
        .and_then(|v| core::str::from_utf8(v).ok())
        .and_then(|v| v.parse::<u32>().ok())
    {
        crate::fd::DEFAULT_UID.store(u, Ordering::Relaxed);
    }
    if let Some(g) = env
        .var(b"GID")
        .and_then(|v| core::str::from_utf8(v).ok())
        .and_then(|v| v.parse::<u32>().ok())
    {
        crate::fd::DEFAULT_GID.store(g, Ordering::Relaxed);
    }
    crate::fd::TTY_IN.store(
        u32::from(env.var(b"STDIN") == Some(&b"tty"[..])),
        Ordering::Relaxed,
    );
    crate::fd::TTY_OUT.store(
        u32::from(env.var(b"STDOUT") == Some(&b"tty"[..])),
        Ordering::Relaxed,
    );
    // a standard stream that is a file (a redirection the starting program made): `STDIN=file:<mode>:<offset>`, and so on
    for (i, name) in [&b"STDIN"[..], b"STDOUT", b"STDERR"]
        .into_iter()
        .enumerate()
    {
        let Some(v) = env.var(name).and_then(|v| v.strip_prefix(b"file:")) else {
            continue;
        };
        let mut parts = v.split(|&b| b == b':');
        let mode = parts
            .next()
            .and_then(|p| core::str::from_utf8(p).ok())
            .and_then(|p| p.parse::<u8>().ok());
        let off = parts
            .next()
            .and_then(|p| core::str::from_utf8(p).ok())
            .and_then(|p| p.parse::<u64>().ok());
        let h = [
            env.handle(Role::Stdin),
            env.handle(Role::Stdout),
            env.handle(Role::Stderr),
        ][i];
        if let (Some(mode), Some(off), Some(h)) = (mode, off, h) {
            crate::fd::init_file_std(i as c_int, h, mode, off);
        }
    }
    crate::file::init_std_buffering();
    let argv = leak_array(&env.startup.args);
    // SAFETY: single-threaded start-up; the array is leaked and NULL-terminated.
    unsafe { crate::stdlib::ENVIRON = leak_array(&env.startup.env) };
    crate::thread::init_main_thread();
    // main runs on a 1 MiB stack (the loader's is 256 KiB)
    let code = crate::thread::on_big_stack(
        call_main,
        &(env.startup.args.len() as c_int, argv) as *const _ as usize,
    );
    crate::stdlib::run_atexit();
    crate::file::flush_all();
    code
}
