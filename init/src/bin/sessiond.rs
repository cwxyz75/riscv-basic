//! `sessiond`: the session manager (USER-VIEW-SPEC section 10, T128). It builds each session (a job, a namespace table from a
//! profile, the services, a shell), decides who gets one (the boot policy and the login, through `authd`, which it starts) and
//! supervises it. `init` gives it handles to the root job and the root resource (to make jobs and to start the device manager),
//! the boot image and the serial line, and nothing else: `init` holds no profile, credential or password.
//!
//! It ends by exiting 0 when a shell asks to `halt` (which lets the kernel power off) and with a non-zero code on a failure.
//!
//! Handles: `BootImage`, `Serial`, `Job` (the root job), `Resource` (the root resource). Environment: `RECOVERY=1`.
#![no_std]
#![no_main]

extern crate alloc;

use proto::startup::Role;
use rt::startup::{self, EXIT_BAD_STARTUP, EXIT_NO_STARTUP};

#[path = "../session.rs"]
mod session;

fn read_image(vmo: u64) -> Option<alloc::vec::Vec<u8>> {
    let size = rt::sys::vmo_get_size(vmo);
    if size.status != 0 {
        return None;
    }
    let mut img = alloc::vec![0u8; size.a1];
    for (i, chunk) in img.chunks_mut(32 * 1024).enumerate() {
        if rt::sys::vmo_read(vmo, (i * 32 * 1024) as u64, chunk.as_mut_ptr(), chunk.len()).status
            != 0
        {
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
    let (Some(image_vmo), Some(serial), Some(root_job), Some(resource)) = (
        env.handle(Role::BootImage),
        env.handle(Role::Serial),
        env.handle(Role::Job),
        env.handle(Role::Resource),
    ) else {
        return 3;
    };
    let Some(img) = read_image(image_vmo) else {
        return 4;
    };
    let Ok(entries) = proto::bootimg::parse(&img) else {
        return 5;
    };
    let powers = session::Powers { root_job, resource };
    let recovery = env.var(b"RECOVERY") == Some(b"1");
    session::supervise(&powers, serial, image_vmo, &img, &entries, recovery)
}
