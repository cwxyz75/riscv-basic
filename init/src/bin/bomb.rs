//! `bomb`: tries to make more processes and threads than its job allows (S4.5) and says what
//! happened. Its job lets it be the only process and run four threads in all.
#![no_std]
#![no_main]

extern crate alloc;

#[path = "../prog.rs"]
mod prog;

use alloc::format;
use alloc::vec::Vec;
use rt::sys;

extern "C" fn idle(_: usize) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

#[no_mangle]
pub fn app_main() -> i32 {
    let p = match prog::start() {
        Ok(p) => p,
        Err(c) => return c,
    };
    let r = sys::process_create();
    if let Err(c) = p.out(format!("bomb: process_create: {}\n", r.status).as_bytes()) {
        return c;
    }
    // threads on stacks from the heap, until one is refused
    let me = sys::process_self().a1 as u32;
    let mut stacks: Vec<Vec<u8>> = Vec::new();
    let mut started = 0;
    let mut last = 0;
    for _ in 0..8 {
        let mut stack = alloc::vec![0u8; 8192];
        let top = (stack.as_mut_ptr() as u64 + 8192) & !15;
        let t = sys::thread_create(me, idle as *const () as usize as u64, top, 0);
        stacks.push(stack);
        if t.status == 0 {
            started += 1;
        } else {
            last = t.status;
            break;
        }
    }
    match p.out(format!("bomb: threads: {started} then {last}\n").as_bytes()) {
        Ok(()) => 0,
        Err(c) => c,
    }
}
