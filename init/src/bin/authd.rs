//! `authd` (UV5a, USER-VIEW-SPEC section 10.1): the one process that holds credential verifiers. It answers the session manager
//! on a private channel, in text, one request and one reply each:
//!
//! * `check <name> <password>` -> `ok` or `no`: may this password obtain a session for that profile?
//! * `set <name> <password>` -> `ok` or `refused`: set a credential. Allowed while the store is empty (the setup session
//!   making the first profile's credential, which makes it non-empty: once) and, after that, only when the physical console asked for recovery at this boot.
//! * `reset-allowed` -> `ok` or `no`: would a `set` be honoured now (the manager asks before prompting for a new password).
//! * `count` -> the number of credentials held.
//!
//! The reply never carries a verifier, a hash or a salt. The seed credentials come from the boot image (`etc/credentials`,
//! read here, never by the manager) and the cost from the manifest (`etc/boot`, line `cost=test` for development images).
//! Salts are unique, not random, until the session stack has the random service (work item T130).
//!
//! Handles: `ServiceAuth` (the channel), `BootImage`. Environment: `RECOVERY=1` when the console asked for recovery.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use auth::{Params, Store, Verifier, SALT_LEN};
use proto::startup::Role;
use rt::abi::signals;
use rt::startup::{self, EXIT_BAD_STARTUP, EXIT_NO_STARTUP};
use rt::sys;

fn read_image(vmo: u64) -> Option<Vec<u8>> {
    let size = sys::vmo_get_size(vmo);
    if size.status != 0 {
        return None;
    }
    let mut img = alloc::vec![0u8; size.a1];
    for (i, chunk) in img.chunks_mut(32 * 1024).enumerate() {
        if sys::vmo_read(vmo, (i * 32 * 1024) as u64, chunk.as_mut_ptr(), chunk.len()).status != 0 {
            return None;
        }
    }
    Some(img)
}

fn entry<'a>(img: &'a [u8], entries: &[proto::bootimg::Entry], name: &[u8]) -> Option<&'a [u8]> {
    let e = entries.iter().find(|e| e.name == name)?;
    img.get(e.offset as usize..(e.offset + e.size) as usize)
}

fn reply(ch: u64, text: &str) {
    sys::channel_write(ch, text.as_ptr(), text.len(), core::ptr::null(), 0);
}

struct State {
    store: Store,
    params: Params,
    recovery: bool,
    salts: u64,
}

impl State {
    fn may_set(&self) -> bool {
        auth::boot::may_reset_credential(self.recovery) || self.store.is_empty()
    }

    fn salt(&mut self, name: &str) -> [u8; SALT_LEN] {
        // unique per credential: the clock, a counter and the name, mixed (not secret, not random)
        self.salts += 1;
        let mut s = [0u8; SALT_LEN];
        s[..8].copy_from_slice(&sys::clock_get().to_le_bytes());
        s[8..].copy_from_slice(&self.salts.to_le_bytes());
        for (i, b) in name.bytes().enumerate() {
            s[i % SALT_LEN] = s[i % SALT_LEN].wrapping_mul(31).wrapping_add(b);
        }
        s
    }

    fn handle(&mut self, req: &str) -> String {
        let mut it = req.splitn(3, ' ');
        match (it.next(), it.next(), it.next()) {
            (Some("check"), Some(name), Some(pw)) => {
                if self.store.check(name, pw.as_bytes()) {
                    "ok".into()
                } else {
                    "no".into()
                }
            }
            (Some("set"), Some(name), Some(pw)) => {
                if !self.may_set() {
                    return "refused".into();
                }
                let salt = self.salt(name);
                match Verifier::new(pw.as_bytes(), salt, self.params) {
                    Ok(v) => {
                        self.store.set(name, v);
                        "ok".into()
                    }
                    Err(_) => "refused".into(),
                }
            }
            (Some("reset-allowed"), None, None) => {
                if self.may_set() {
                    "ok".into()
                } else {
                    "no".into()
                }
            }
            (Some("count"), None, None) => format!("{}", self.store.names().count()),
            _ => "bad request".into(),
        }
    }
}

#[no_mangle]
pub fn app_main() -> i32 {
    let env = match startup::read() {
        Ok(e) => e,
        Err(startup::EnvError::NoMessage) => return EXIT_NO_STARTUP,
        Err(_) => return EXIT_BAD_STARTUP,
    };
    let (Some(ch), Some(image_vmo)) = (env.handle(Role::ServiceAuth), env.handle(Role::BootImage))
    else {
        return 3;
    };
    let Some(img) = read_image(image_vmo) else {
        return 4;
    };
    let Ok(entries) = proto::bootimg::parse(&img) else {
        return 5;
    };
    let manifest = entry(&img, &entries, b"etc/boot").unwrap_or(b"");
    let manifest = core::str::from_utf8(manifest).unwrap_or("");
    let params = if manifest.lines().any(|l| l.trim() == "cost=test") {
        Params::TEST
    } else {
        Params::STRONG
    };
    let store = match entry(&img, &entries, b"etc/credentials") {
        None => Store::default(),
        Some(b) => match core::str::from_utf8(b)
            .ok()
            .and_then(|t| Store::parse(t).ok())
        {
            Some(s) => s,
            None => {
                sys::debug_write(b"authd: etc/credentials does not parse\n");
                return 6;
            }
        },
    };
    let mut st = State {
        store,
        params,
        recovery: env.var(b"RECOVERY") == Some(b"1"),
        salts: 0,
    };
    loop {
        let w = sys::object_wait(ch, signals::READABLE | signals::PEER_CLOSED, u64::MAX);
        if w.status != 0 {
            return 0;
        }
        let mut buf = [0u8; 512];
        let r = sys::channel_read(ch, buf.as_mut_ptr(), buf.len(), core::ptr::null_mut(), 0);
        if r.status != 0 {
            // empty (a spurious wakeup) or the peer is gone
            if sys::object_wait(ch, signals::READABLE, 0).status == 0 {
                continue;
            }
            return 0;
        }
        let req = core::str::from_utf8(&buf[..r.a1]).unwrap_or("");
        let out = st.handle(req);
        reply(ch, &out);
    }
}
