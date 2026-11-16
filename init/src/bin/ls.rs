//! `ls [-l] [dir]`: the entries of a directory (default `.`), sorted bytewise, one per line, directories suffixed with `/`. With
//! `-l` each line is `<type><permissions> <size> <name>` (the advisory mode of USER-VIEW-SPEC section 6; a link also shows
//! ` -> <target>`).
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
    let mut args: alloc::vec::Vec<&[u8]> = p.args().iter().map(|a| &a[..]).collect();
    let long = args.first() == Some(&&b"-l"[..]);
    if long {
        args.remove(0);
    }
    let path: &[u8] = args.first().copied().unwrap_or(b".");
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
    let mut entries = match listing {
        Ok(e) => e,
        Err(e) => {
            dir.close();
            return match p.complain(path, prog::why(e)) {
                Ok(()) => 1,
                Err(c) => c,
            };
        }
    };
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let mut out: Vec<u8> = Vec::new();
    for e in &entries {
        if long {
            // one STAT per entry (a link is described itself, with its target)
            let st = dir.stat_stop(&e.name, flags::NOFOLLOW).ok();
            let (kind, mode, size) = match &st {
                Some(s) => (
                    s.info.kind,
                    if s.info.mode == proto::file::ABSENT {
                        match s.info.kind {
                            2 => 0o755,
                            3 => 0o777,
                            _ => 0o644,
                        }
                    } else {
                        s.info.mode
                    },
                    s.size,
                ),
                None => (1, 0, 0),
            };
            out.push(match kind {
                2 => b'd',
                3 => b'l',
                _ => b'-',
            });
            for (bit, c) in [
                (0o400, b'r'),
                (0o200, b'w'),
                (0o100, b'x'),
                (0o040, b'r'),
                (0o020, b'w'),
                (0o010, b'x'),
                (0o004, b'r'),
                (0o002, b'w'),
                (0o001, b'x'),
            ] {
                out.push(if mode & bit != 0 { c } else { b'-' });
            }
            out.extend_from_slice(alloc::format!(" {size} ").as_bytes());
        }
        out.extend_from_slice(&e.name);
        if e.is_dir {
            out.push(b'/');
        }
        if long && e.is_link {
            if let Ok(t) = dir.readlink(&e.name) {
                out.extend_from_slice(b" -> ");
                out.extend_from_slice(&t);
            }
        }
        out.push(b'\n');
    }
    dir.close();
    match p.out(&out) {
        Ok(()) => 0,
        Err(c) => c,
    }
}
