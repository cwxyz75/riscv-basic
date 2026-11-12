//! A program that never ends on its own: it prints `spinning` to STDOUT (when it has one) and
//! then sleeps in a loop. It is the job Ctrl-C must kill (S4d), and later the shell's `spin`.
#![no_std]
#![no_main]

extern crate alloc;

use proto::startup::Role;
use rt::io;
use rt::startup::{self, EXIT_BAD_STARTUP, EXIT_NO_STARTUP};

#[no_mangle]
pub fn app_main() -> i32 {
    let env = match startup::read() {
        Ok(e) => e,
        Err(startup::EnvError::NoMessage) => return EXIT_NO_STARTUP,
        Err(_) => return EXIT_BAD_STARTUP,
    };
    if let Some(out) = env.handle(Role::Stdout) {
        let _ = io::write_all(out, b"spinning\n");
    }
    loop {
        io::nap(100);
    }
}
