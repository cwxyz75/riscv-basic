//! `probe`: tries to step outside what it was given and prints what the system answered, one
//! line per attempt. It is the confinement test of the shell (S4-SPEC section 8): a program
//! started without the trusted list's extra authority can name nothing but its working
//! directory, read-only, and has no way to start another process.
#![no_std]
#![no_main]

extern crate alloc;

#[path = "../prog.rs"]
mod prog;

use alloc::format;
use proto::file::{flags, rights};
use rt::fs::Fs;
use rt::sys;

fn code<T>(r: Result<T, i32>) -> i32 {
    match r {
        Ok(_) => 0,
        Err(e) => e,
    }
}

#[no_mangle]
pub fn app_main() -> i32 {
    let p = match prog::start() {
        Ok(p) => p,
        Err(c) => return c,
    };
    let Some(cwd) = p.env.namespace(b".") else {
        return match p.out(b"probe: no working directory\n") {
            Ok(()) => 1,
            Err(c) => c,
        };
    };
    let mut dir = Fs::new(cwd);
    let read = rights::READ | rights::ENUMERATE;
    let mut lines = alloc::vec::Vec::new();
    // an absolute path, and a path out of the directory: the wire has neither
    let r = code(dir.open(b"/etc/trusted", 0, rights::READ).map(Fs::close));
    lines.push(format!("open /etc/trusted: {r}\n"));
    let r = code(dir.open(b"../x", 0, rights::READ).map(Fs::close));
    lines.push(format!("open ../x: {r}\n"));
    // the directory is read-only: creating a file needs rights the connection does not have
    let r = code(
        dir.open(b"f", flags::CREATE, rights::READ | rights::WRITE)
            .map(Fs::close),
    );
    lines.push(format!("write ./f: {r}\n"));
    // rights only shrink: an equal clone works, a wider one is refused
    match dir.clone_conn(read) {
        Ok(c) => {
            c.close();
            lines.push(alloc::string::String::from("clone: ok\n"));
        }
        Err(e) => lines.push(format!("clone: {e}\n")),
    }
    let r = code(dir.clone_conn(rights::ALL).map(Fs::close));
    lines.push(format!("clone all: {r}\n"));
    // there is no launcher among the handles: a made-up handle number is no handle at all
    let msg = [0u8; 8];
    let w = sys::channel_write(1000, msg.as_ptr(), msg.len(), core::ptr::null(), 0);
    lines.push(format!("spawn: {}\n", w.status as i32));
    for l in &lines {
        if let Err(c) = p.out(l.as_bytes()) {
            return c;
        }
    }
    0
}
