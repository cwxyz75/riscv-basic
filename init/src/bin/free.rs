//! `free`: the session's memory, from `/proc/meminfo` (UV6): `total used free` in kB, one line after a header.
#![no_std]
#![no_main]

extern crate alloc;

#[path = "../prog.rs"]
mod prog;

fn field(text: &str, name: &str) -> Option<u64> {
    text.lines().find_map(|l| {
        l.strip_prefix(name)?
            .strip_prefix(':')?
            .trim()
            .strip_suffix("kB")?
            .trim()
            .parse()
            .ok()
    })
}

#[no_mangle]
pub fn app_main() -> i32 {
    let p = match prog::start() {
        Ok(p) => p,
        Err(c) => return c,
    };
    let Ok(data) = p.read_file(b"/proc/meminfo") else {
        return match p.complain(b"/proc/meminfo", "not found") {
            Ok(()) => 1,
            Err(c) => c,
        };
    };
    let text = core::str::from_utf8(&data).unwrap_or("");
    let (Some(total), Some(free)) = (field(text, "MemTotal"), field(text, "MemFree")) else {
        return 1;
    };
    let line = alloc::format!(
        "total used free\n{total} {} {free}\n",
        total.saturating_sub(free)
    );
    match p.out(line.as_bytes()) {
        Ok(()) => 0,
        Err(c) => c,
    }
}
