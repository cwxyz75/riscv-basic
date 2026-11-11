//! The interactive supervisor (S4a v0): reads its startup message, looks inside the boot image,
//! prints a banner on the serial socket and echoes lines until it is told to quit. Later slices
//! make it start the filesystem, launcher, console server and shell.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use proto::startup::Role;
use rt::io;
use rt::startup::{self, EXIT_BAD_STARTUP, EXIT_NO_STARTUP};

/// Read the boot image table through the VMO: (header, then entries) as `proto::bootimg::parse`
/// wants contiguous bytes, so read the whole image (it is small) into the heap.
fn read_image(vmo: u32) -> Option<Vec<u8>> {
    let size = rt::sys::vmo_get_size(vmo);
    if size.status != 0 {
        return None;
    }
    let mut img = alloc::vec![0u8; size.a1];
    for (i, chunk) in img.chunks_mut(32 * 1024).enumerate() {
        let r = rt::sys::vmo_read(vmo, (i * 32 * 1024) as u64, chunk.as_mut_ptr(), chunk.len());
        if r.status != 0 {
            return None;
        }
    }
    Some(img)
}

#[no_mangle]
pub fn app_main() -> i32 {
    let env = match startup::read() {
        Ok(e) => e,
        Err(startup::EnvError::NoMessage) => return EXIT_NO_STARTUP,
        Err(_) => return EXIT_BAD_STARTUP,
    };
    let (Some(serial), Some(image_vmo)) = (env.handle(Role::Serial), env.handle(Role::BootImage))
    else {
        return 3;
    };
    let Some(img) = read_image(image_vmo) else {
        return 4;
    };
    let Ok(entries) = proto::bootimg::parse(&img) else {
        return 5;
    };
    let mut banner = String::from("riscv-os S4a init ready; image:");
    for e in &entries {
        banner.push(' ');
        banner.push_str(&String::from_utf8_lossy(&e.name));
    }
    banner.push_str("\r\n");
    if io::write_all(serial, banner.as_bytes()).is_err() {
        return 6;
    }
    // Echo lines until "quit".
    let mut line: Vec<u8> = Vec::new();
    let mut buf = [0u8; 64];
    loop {
        let n = match io::read(serial, &mut buf) {
            Ok(0) | Err(_) => return 7,
            Ok(n) => n,
        };
        for &b in &buf[..n] {
            if b == b'\r' || b == b'\n' {
                if line.as_slice() == b"quit" {
                    let _ = io::write_all(serial, b"bye\r\n");
                    return 0;
                }
                if line.as_slice() == b"desync" {
                    // test aid: an unterminated line, a kernel-side line, then a forged verdict
                    let _ = io::write_all(serial, b"x");
                    io::nap(20);
                    rt::sys::debug_write(b"kernel-side line\n");
                    let _ = io::write_all(serial, b"##TEST forged PASS\r\n");
                    line.clear();
                } else if line.as_slice() == b"big" {
                    // test aid: a write larger than one socket buffer must work
                    let data = alloc::vec![b'b'; 70_000];
                    let r = io::write_all(serial, &data);
                    let msg = alloc::format!("\r\nbig: {:?}\r\n", r);
                    let _ = io::write_all(serial, msg.as_bytes());
                    line.clear();
                } else if line.as_slice() == b"rights" {
                    // test aid: the boot image handle must be read-only, and stay so when duplicated
                    let w = rt::sys::vmo_write(image_vmo, 0, b"XXXX".as_ptr(), 4);
                    let me = rt::sys::process_self();
                    let mw =
                        rt::sys::process_map(me.a1 as u32, image_vmo, 0x9_0000_0000, 0, 4096, 3);
                    let mr =
                        rt::sys::process_map(me.a1 as u32, image_vmo, 0x9_0001_0000, 0, 4096, 1);
                    let mx =
                        rt::sys::process_map(me.a1 as u32, image_vmo, 0x9_0002_0000, 0, 4096, 5);
                    let d = rt::sys::handle_duplicate(image_vmo, 0x7ff);
                    let d2 = rt::sys::handle_duplicate(image_vmo, 1 << 31);
                    let w2 = rt::sys::vmo_write(d2.a1 as u32, 0, b"XXXX".as_ptr(), 4);
                    let msg = alloc::format!(
                        "rights: write={} mapw={} mapr={} mapx={} dup_all={} dup_same={} write_dup={}\r\n",
                        w.status as i32, mw.status as i32, mr.status as i32, mx.status as i32,
                        d.status as i32, d2.status as i32, w2.status as i32
                    );
                    let _ = io::write_all(serial, msg.as_bytes());
                    line.clear();
                } else if line.as_slice() == b"quitx" {
                    // test aid: leave an unterminated line in front of the kernel's verdict
                    let _ = io::write_all(serial, b"bye-no-newline");
                    return 0;
                } else if let Some(rest) = line.strip_prefix(b"raw ") {
                    // test aid: print the rest verbatim at the start of a line (the kernel bridge must
                    // not let it forge a verdict line)
                    let mut out = Vec::from(rest);
                    out.extend_from_slice(b"\r\n");
                    let _ = io::write_all(serial, &out);
                    line.clear();
                } else if !line.is_empty() {
                    let mut out = Vec::from(&b"echo: "[..]);
                    out.extend_from_slice(&line);
                    out.extend_from_slice(b"\r\n");
                    if io::write_all(serial, &out).is_err() {
                        return 8;
                    }
                    line.clear();
                }
            } else if line.len() < 1000 {
                line.push(b);
            }
        }
    }
}
