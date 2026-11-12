//! `echo args...`: the arguments joined by one space, and a newline.
#![no_std]
#![no_main]

extern crate alloc;

#[path = "../prog.rs"]
mod prog;

use alloc::vec::Vec;

#[no_mangle]
pub fn app_main() -> i32 {
    let p = match prog::start() {
        Ok(p) => p,
        Err(c) => return c,
    };
    let mut line: Vec<u8> = Vec::new();
    for (i, a) in p.args().iter().enumerate() {
        if i > 0 {
            line.push(b' ');
        }
        line.extend_from_slice(a);
    }
    line.push(b'\n');
    match p.out(&line) {
        Ok(()) => 0,
        Err(c) => c,
    }
}
