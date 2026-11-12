//! `ipchog`: fills sockets with data and then makes events until its job's quotas say no (S4.5c),
//! and reports how far it got. Its job allows 192 KiB of queued data and 256 objects.
#![no_std]
#![no_main]

extern crate alloc;

#[path = "../prog.rs"]
mod prog;

use alloc::format;
use rt::sys;

#[no_mangle]
pub fn app_main() -> i32 {
    let p = match prog::start() {
        Ok(p) => p,
        Err(c) => return c,
    };
    let mut queued = 0usize;
    let block = [3u8; 1024];
    'fill: loop {
        let pair = sys::socket_create();
        if pair.status != 0 {
            break;
        }
        for _ in 0..32 {
            let w = sys::socket_write(pair.a1 as u32, block.as_ptr(), block.len());
            if w.status != 0 {
                break 'fill;
            }
            queued += w.a1;
        }
    }
    if let Err(c) = p.out(format!("ipchog: queued {} KiB\n", queued / 1024).as_bytes()) {
        return c;
    }
    let mut events = 0;
    while sys::event_create().status == 0 {
        events += 1;
    }
    match p.out(format!("ipchog: made {} events\n", events).as_bytes()) {
        Ok(()) => 0,
        Err(c) => c,
    }
}
