//! `nproc`: the number of processors, counted from `/proc/cpuinfo` (UV6).
#![no_std]
#![no_main]

extern crate alloc;

#[path = "../prog.rs"]
mod prog;

#[no_mangle]
pub fn app_main() -> i32 {
    let p = match prog::start() {
        Ok(p) => p,
        Err(c) => return c,
    };
    let Ok(data) = p.read_file(b"/proc/cpuinfo") else {
        return match p.complain(b"/proc/cpuinfo", "not found") {
            Ok(()) => 1,
            Err(c) => c,
        };
    };
    let n = data
        .split(|&b| b == b'\n')
        .filter(|l| l.starts_with(b"processor"))
        .count();
    match p.out(alloc::format!("{n}\n").as_bytes()) {
        Ok(()) => 0,
        Err(c) => c,
    }
}
