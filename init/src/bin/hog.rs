//! `hog`: takes memory until the job it runs in says no (S4.5). It prints how many MiB it holds
//! after each MiB, so the shell shows how far it got. When the job refuses more memory the heap
//! cannot grow, the allocation fails, and `hog` says so and exits 7 (`[exit 7]`).
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
    let mut held: Vec<Vec<u8>> = Vec::new();
    loop {
        let mut chunk: Vec<u8> = Vec::new();
        if chunk.try_reserve_exact(1 << 20).is_err() {
            let line = alloc::format!("hog: refused at {} MiB\n", held.len());
            let _ = p.out(line.as_bytes());
            return 7;
        }
        // (filled, so every page is really committed)
        chunk.resize(1 << 20, 0xa5);
        held.push(chunk);
        let line = alloc::format!("hog: {} MiB\n", held.len());
        if let Err(c) = p.out(line.as_bytes()) {
            return c;
        }
    }
}
