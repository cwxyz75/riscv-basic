//! `ps`: the session's programs, read from `/proc` (UV6): a header, then `PID STATE COMMAND` per process, in pid order. The
//! state is `R` (running) or `Z` (ended, not yet waited for); the command is the program's arguments joined by spaces.
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
    let Ok(entries) = p.read_dir(b"/proc") else {
        return match p.complain(b"/proc", "not found") {
            Ok(()) => 1,
            Err(c) => c,
        };
    };
    let mut pids: Vec<u32> = entries
        .iter()
        .filter(|e| e.is_dir)
        .filter_map(|e| core::str::from_utf8(&e.name).ok()?.parse().ok())
        .collect();
    pids.sort_unstable();
    let mut out: Vec<u8> = b"PID STATE COMMAND\n".to_vec();
    for pid in pids {
        // (a process that went away between the listing and the read is not shown)
        let Ok(stat) = p.read_file(alloc::format!("/proc/{pid}/stat").as_bytes()) else {
            continue;
        };
        let Ok(cmd) = p.read_file(alloc::format!("/proc/{pid}/cmdline").as_bytes()) else {
            continue;
        };
        let state = stat
            .rsplit(|&b| b == b')')
            .next()
            .map(|s| s.trim_ascii())
            .unwrap_or(b"?");
        out.extend_from_slice(alloc::format!("{pid} ").as_bytes());
        out.extend_from_slice(state);
        out.push(b' ');
        let words: Vec<&[u8]> = cmd.split(|&b| b == 0).filter(|w| !w.is_empty()).collect();
        out.extend_from_slice(&words.join(&b' '));
        out.push(b'\n');
    }
    match p.out(&out) {
        Ok(()) => 0,
        Err(c) => c,
    }
}
