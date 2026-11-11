//! A small program that uses the S4a runtime: it reads its startup message, grows the heap,
//! prints its arguments to STDOUT and copies STDIN to STDOUT until end of stream. The exit
//! code is the number of arguments (or a negative code for a bad or missing startup message).
#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use proto::startup::Role;
use rt::io;
use rt::startup::{self, EnvError, EXIT_BAD_STARTUP, EXIT_NO_STARTUP};

#[no_mangle]
pub fn app_main() -> i32 {
    let env = match startup::read() {
        Ok(e) => e,
        Err(EnvError::NoMessage) => return EXIT_NO_STARTUP,
        Err(_) => return EXIT_BAD_STARTUP,
    };
    let (Some(stdin), Some(stdout)) = (env.handle(Role::Stdin), env.handle(Role::Stdout)) else {
        return 3;
    };
    // Force the heap to grow well past its first chunk, and check what we stored.
    let big: Vec<u8> = (0..524_288u32).map(|i| i as u8).collect();
    let sum: u64 = big.iter().map(|&b| u64::from(b)).sum();
    if sum != 2048 * (255 * 256 / 2) {
        return 90;
    }
    drop(big);
    if env.startup.args.get(1).map(|a| a.as_slice()) == Some(b"hog") {
        // allocate 1 MiB blocks until the heap says no: it must fail gracefully at its cap
        let mut blocks: Vec<Vec<u8>> = Vec::new();
        loop {
            let mut b: Vec<u8> = Vec::new();
            if b.try_reserve_exact(1 << 20).is_err() {
                break;
            }
            // (the memory is reserved from the heap, not touched: the test is about the cap)
            blocks.push(b);
        }
        return blocks.len() as i32;
    }
    let mut line = String::from("args=");
    for (i, a) in env.startup.args.iter().enumerate() {
        if i > 0 {
            line.push(' ');
        }
        line.push_str(&String::from_utf8_lossy(a));
    }
    line.push('\n');
    if io::write_all(stdout, line.as_bytes()).is_err() {
        return 91;
    }
    let mut buf = [0u8; 256];
    loop {
        match io::read(stdin, &mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if io::write_all(stdout, &buf[..n]).is_err() {
                    return 92;
                }
            }
            Err(_) => return 93,
        }
    }
    env.startup.args.len() as i32
}
