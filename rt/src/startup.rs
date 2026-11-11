//! Reading the startup message (S4-SPEC section 5) from the bootstrap channel.

extern crate alloc;

use crate::{sys, BOOTSTRAP};
use abi::signals;
use abi::status::Status;
use alloc::vec::Vec;
use proto::startup::{Role, Startup, StartupError, MAX_BYTES, MAX_HANDLES};

/// What a program was started with.
#[derive(Debug)]
pub struct Env {
    pub startup: Startup,
    /// The handles that arrived with the message; the startup message's indices refer here.
    pub handles: Vec<u32>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum EnvError {
    /// The bootstrap channel closed before a message arrived.
    NoMessage,
    /// The message could not be read (too large for the buffers, or a syscall error).
    Read(i32),
    Bad(StartupError),
}

/// Exit code of a program whose startup message was malformed (S4-SPEC section 5).
pub const EXIT_BAD_STARTUP: i32 = -2001;
/// Exit code of a program that never got one.
pub const EXIT_NO_STARTUP: i32 = -2002;

impl Env {
    /// The handle with `role`.
    pub fn handle(&self, role: Role) -> Option<u32> {
        let i = self.startup.handle_of(role)?;
        self.handles.get(i as usize).copied()
    }

    /// The handle of the namespace entry with this prefix (`b"."`, `b"/"`).
    pub fn namespace(&self, prefix: &[u8]) -> Option<u32> {
        let (_, i) = self.startup.namespace.iter().find(|(p, _)| p == prefix)?;
        self.handles.get(*i as usize).copied()
    }
}

/// Block until the startup message arrives, read it, and validate it. Closes every handle
/// that came with a message it refuses, so a bad parent cannot leak handles into us.
pub fn read() -> Result<Env, EnvError> {
    loop {
        let mut bytes = alloc::vec![0u8; MAX_BYTES];
        let mut handles = [0u32; MAX_HANDLES];
        let r = sys::channel_read(
            BOOTSTRAP,
            bytes.as_mut_ptr(),
            bytes.len(),
            handles.as_mut_ptr(),
            handles.len(),
        );
        if r.status == 0 {
            let (nb, nh) = (r.a1, r.a2);
            let hs: Vec<u32> = handles[..nh].to_vec();
            return match Startup::decode(&bytes[..nb], nh) {
                Ok(startup) => Ok(Env {
                    startup,
                    handles: hs,
                }),
                Err(e) => {
                    for h in hs {
                        sys::handle_close(h);
                    }
                    Err(EnvError::Bad(e))
                }
            };
        }
        match r.status() {
            Some(Status::ShouldWait) => {
                let w = sys::object_wait(
                    BOOTSTRAP,
                    signals::READABLE | signals::PEER_CLOSED,
                    u64::MAX,
                );
                let seen = w.a1 as u32;
                if w.status == 0
                    && seen & signals::READABLE == 0
                    && seen & signals::PEER_CLOSED != 0
                {
                    return Err(EnvError::NoMessage);
                }
            }
            Some(Status::PeerClosed) => return Err(EnvError::NoMessage),
            _ => return Err(EnvError::Read(r.status as i32)),
        }
    }
}
