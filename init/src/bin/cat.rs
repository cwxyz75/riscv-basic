//! `cat [file...]`: the files, or STDIN, copied to STDOUT. A file that cannot be read is reported
//! (`cat: <file>: not found`) and the exit code is 1, but the other files are still copied.
#![no_std]
#![no_main]

extern crate alloc;

#[path = "../prog.rs"]
mod prog;

use proto::file::rights;
use rt::fs::Fs;
use rt::io;

fn copy_stdin(p: &prog::Prog) -> Result<(), i32> {
    let Some(stdin) = p.stdin else {
        return Err(1);
    };
    let mut buf = [0u8; 1024];
    loop {
        match io::read(stdin, &mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => p.out(&buf[..n])?,
            Err(_) => return Err(1),
        }
    }
}

/// Copy one file; `Ok(false)` means it could not be read (already reported).
fn copy_file(p: &prog::Prog, path: &[u8]) -> Result<bool, i32> {
    let Some((base, rel)) = p.base_for(path) else {
        p.complain(path, "not found")?;
        return Ok(false);
    };
    // (the connection belongs to the program: borrow it without closing it)
    let mut dir = Fs::new(base);
    let opened = dir.open(rel, 0, rights::READ);
    let mut file = match opened {
        Ok(f) => f,
        Err(e) => {
            p.complain(path, prog::why(e))?;
            return Ok(false);
        }
    };
    let mut offset = 0u64;
    let result = loop {
        match file.read(offset, 8192) {
            Ok(chunk) if chunk.is_empty() => break Ok(true),
            Ok(chunk) => {
                offset += chunk.len() as u64;
                if let Err(c) = p.out(&chunk) {
                    break Err(c);
                }
            }
            Err(e) => {
                let r = p.complain(path, prog::why(e));
                break r.map(|()| false);
            }
        }
    };
    file.close();
    result
}

#[no_mangle]
pub fn app_main() -> i32 {
    let p = match prog::start() {
        Ok(p) => p,
        Err(c) => return c,
    };
    if p.args().is_empty() {
        return match copy_stdin(&p) {
            Ok(()) => 0,
            Err(c) => c,
        };
    }
    let mut code = 0;
    for path in p.args() {
        match copy_file(&p, path) {
            Ok(true) => {}
            Ok(false) => code = 1,
            Err(c) => return c,
        }
    }
    code
}
