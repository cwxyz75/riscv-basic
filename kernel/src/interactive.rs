//! The interactive boot (S4, feature `interactive`): instead of the user test suite, start the
//! supervisor `init` with the boot image and a serial socket, and bridge the UART to that
//! socket until `init` exits. The bridge is a polling kernel thread (OD52): no user driver
//! exists until S6. Everything it forwards from a user program to the UART has any line-start
//! `##` rewritten to `# #`, so no program can forge a verdict line.

use crate::{console, thread, user};

type Outcome = Result<(), String>;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use proto::startup::{Role, Startup};

/// Forwarded bytes are rewritten so a line never *starts* with `##`.
struct Escaper {
    pos: usize,
    first_hash: bool,
}

impl Escaper {
    fn new() -> Escaper {
        Escaper {
            pos: 0,
            first_hash: false,
        }
    }

    fn feed(&mut self, b: u8, out: &mut Vec<u8>) {
        if self.pos == 1 && self.first_hash && b == b'#' {
            out.push(b' ');
        }
        out.push(b);
        if b == b'\n' || b == b'\r' {
            self.pos = 0;
            self.first_hash = false;
        } else {
            if self.pos == 0 {
                self.first_hash = b == b'#';
            }
            self.pos += 1;
        }
    }
}

static mut BRIDGE_SOCKET: Option<user::KernelSocket> = None;

/// How many UART input bytes were dropped because the process was not reading.
static DROPPED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

fn bridge_main(_: usize) {
    // SAFETY: set by `run` just before this thread is spawned, taken once here.
    let sock = unsafe { (*core::ptr::addr_of_mut!(BRIDGE_SOCKET)).take() }
        .expect("the bridge socket was set");
    let mut esc = Escaper::new();
    let mut rx: Vec<u8> = Vec::new();
    loop {
        // UART -> process
        rx.clear();
        while rx.len() < 64 {
            match console::try_read() {
                Some(b) => rx.push(b),
                None => break,
            }
        }
        if !rx.is_empty() {
            let mut at = 0;
            while at < rx.len() {
                match sock.write(&rx[at..]) {
                    Ok(n) => at += n,
                    Err(_) => {
                        DROPPED.fetch_add(
                            (rx.len() - at) as u32,
                            core::sync::atomic::Ordering::Relaxed,
                        );
                        break;
                    }
                }
            }
        }
        // process -> UART
        match sock.read(256) {
            Ok(bytes) => {
                let mut out = Vec::with_capacity(bytes.len() + 4);
                for b in bytes {
                    esc.feed(b, &mut out);
                }
                // one chunk at a time with interrupts off, so a kernel `##` line cannot split it
                crate::irq::without(|| {
                    let mut c = console::Console;
                    let _ = c.write_str(&String::from_utf8_lossy(&out));
                });
            }
            Err(abi::status::Status::PeerClosed) => return, // init is gone
            Err(_) => {}
        }
        thread::sleep_ticks(1);
    }
}

/// Run the interactive system to the end of `init`; returns the kernel-side checks.
pub fn run() -> crate::user_tests::Report {
    let mut tests: Vec<(String, Outcome)> = Vec::new();
    let result = (|| -> Result<i32, String> {
        let (pid, tid) =
            user::start(user::INIT_ELF, 2).map_err(|e| format!("could not start init: {e:?}"))?;
        let end = user::take_bootstrap(pid).ok_or("no bootstrap end")?;
        let (ksock, user_sock) =
            user::kernel_socket_pair().map_err(|e| format!("socket: {e:?}"))?;
        let image =
            user::ktable_readonly_vmo(user::BOOT_IMAGE).map_err(|e| format!("image: {e:?}"))?;
        let msg = Startup {
            args: alloc::vec![b"init".to_vec()],
            env: Vec::new(),
            namespace: Vec::new(),
            roles: alloc::vec![(Role::BootImage, 0), (Role::Serial, 1)],
        }
        .encode()
        .map_err(|e| format!("startup: {e:?}"))?;
        end.write_handles(&msg, &[image, user_sock])
            .map_err(|e| format!("startup write: {e:?}"))?;
        end.close();
        // SAFETY: the bridge thread has not been spawned yet; it takes this once.
        unsafe { *core::ptr::addr_of_mut!(BRIDGE_SOCKET) = Some(ksock) };
        let bridge = thread::spawn("serial", 20, bridge_main, 0);
        let code = user::finish(pid, tid);
        thread::join(bridge);
        Ok(code)
    })();
    let verdict = match &result {
        Ok(0) => Ok(()),
        Ok(c) => Err(format!("init exited with code {c}")),
        Err(e) => Err(e.clone()),
    };
    tests.push(("interactive_init_exits_cleanly".into(), verdict));
    thread::sleep_ticks(3);
    tests.push((
        "interactive_nothing_survives_init".into(),
        if user::process_count() == 0 && user::live_objects() == 0 {
            Ok(())
        } else {
            Err(format!(
                "{} processes and {} objects left",
                user::process_count(),
                user::live_objects()
            ))
        },
    ));
    crate::user_tests::Report {
        tests,
        user_reported: user::reported(),
    }
}
