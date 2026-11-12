//! `ls [dir]`: the entries of a directory (default `.`), sorted bytewise, one per line,
//! directories suffixed with `/`.
#![no_std]
#![no_main]

extern crate alloc;

#[path = "../prog.rs"]
mod prog;

use alloc::vec::Vec;
use proto::file::{flags, rights};
use rt::fs::Fs;

#[no_mangle]
pub fn app_main() -> i32 {
    let p = match prog::start() {
        Ok(p) => p,
        Err(c) => return c,
    };
    let path: &[u8] = p.args().first().map_or(&b"."[..], |a| &a[..]);
    let want = rights::READ | rights::ENUMERATE;
    let Some((base, rel)) = p.base_for(path) else {
        return match p.complain(path, "not found") {
            Ok(()) => 1,
            Err(c) => c,
        };
    };
    let mut start = Fs::new(base); // borrowed: the program's own connection stays open
    let dir = if rel.is_empty() || rel == b"." {
        start.clone_conn(want)
    } else {
        start.open(rel, flags::DIRECTORY, want)
    };
    let mut dir = match dir {
        Ok(d) => d,
        Err(e) => {
            return match p.complain(path, prog::why(e)) {
                Ok(()) => 1,
                Err(c) => c,
            };
        }
    };
    let listing = dir.read_dir();
    dir.close();
    let mut entries = match listing {
        Ok(e) => e,
        Err(e) => {
            return match p.complain(path, prog::why(e)) {
                Ok(()) => 1,
                Err(c) => c,
            };
        }
    };
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let mut out: Vec<u8> = Vec::new();
    for e in &entries {
        out.extend_from_slice(&e.name);
        if e.is_dir {
            out.push(b'/');
        }
        out.push(b'\n');
    }
    match p.out(&out) {
        Ok(()) => 0,
        Err(c) => c,
    }
}
