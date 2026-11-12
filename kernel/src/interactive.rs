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
use proto::startup::{Role, Startup};

static mut BRIDGE_SOCKET: Option<user::KernelSocket> = None;

/// How many UART input bytes were dropped because the process was not reading.
static DROPPED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Set by `run` when `init` is gone, so the bridge stops even if some process still holds a
/// duplicate of the serial socket.
static STOP: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Input the process has not taken yet is kept (bounded) and retried every tick: a full socket
/// or an exhausted IPC budget delays input, it does not lose it, until this many bytes pile up.
const PENDING_MAX: usize = 4096;

/// Forward whatever the process wrote that the bridge has not yet printed. Called when the bridge
/// stops (init is gone), so output written just before exit is not lost.
fn flush_output(sock: &user::KernelSocket) {
    while let Ok(bytes) = sock.read(256) {
        if bytes.is_empty() {
            break;
        }
        console::write_user(&bytes);
    }
}

fn bridge_main(_: usize) {
    use core::sync::atomic::Ordering;
    // SAFETY: set by `run` just before this thread is spawned, taken once here.
    let sock = unsafe { (*core::ptr::addr_of_mut!(BRIDGE_SOCKET)).take() }
        .expect("the bridge socket was set");
    let mut pending: Vec<u8> = Vec::new();
    while !STOP.load(Ordering::Relaxed) {
        // UART -> process
        while pending.len() < PENDING_MAX {
            match console::try_read() {
                Some(b) => pending.push(b),
                None => break,
            }
        }
        if !pending.is_empty() {
            match sock.write(&pending) {
                Ok(n) => {
                    pending.drain(..n);
                }
                Err(abi::status::Status::PeerClosed) => return flush_output(&sock),
                Err(_) => {} // full or out of budget: try again next tick
            }
            if pending.len() >= PENDING_MAX {
                // the UART side has more than we can hold: count what we will not read
                let mut lost = 0u32;
                while console::try_read().is_some() {
                    lost += 1;
                }
                DROPPED.fetch_add(lost, Ordering::Relaxed);
            }
        }
        // process -> UART
        match sock.read(256) {
            Ok(bytes) => console::write_user(&bytes),
            Err(abi::status::Status::PeerClosed) => return flush_output(&sock), // init is gone
            Err(_) => {}
        }
        thread::sleep_ticks(1);
    }
    flush_output(&sock);
}

/// Run the interactive system to the end of `init`; returns the kernel-side checks.
pub fn run() -> crate::user_tests::Report {
    let mut tests: Vec<(String, Outcome)> = Vec::new();
    let threads_before = thread::thread_count();
    let result = (|| -> Result<i32, String> {
        let (pid, tid) =
            user::start(user::INIT_ELF, 2).map_err(|e| format!("could not start init: {e:?}"))?;
        let end = user::take_bootstrap(pid).ok_or("no bootstrap end")?;
        let (ksock, user_sock) =
            user::kernel_socket_pair().map_err(|e| format!("socket: {e:?}"))?;
        let image =
            user::ktable_readonly_vmo(user::BOOT_IMAGE).map_err(|e| format!("image: {e:?}"))?;
        let msg = Startup {
            args: if cfg!(feature = "repl") {
                alloc::vec![b"init".to_vec(), b"repl".to_vec()]
            } else {
                alloc::vec![b"init".to_vec()]
            },
            env: Vec::new(),
            namespace: Vec::new(),
            roles: alloc::vec![(Role::BootImage, 0), (Role::Serial, 1)],
        }
        .encode()
        .map_err(|e| format!("startup: {e:?}"))?;
        if let Err(e) = end.write_handles(&msg, &[image, user_sock]) {
            user::ktable_close(image);
            user::ktable_close(user_sock);
            return Err(format!("startup write: {e:?}"));
        }
        end.close();
        // SAFETY: the bridge thread has not been spawned yet; it takes this once.
        unsafe { *core::ptr::addr_of_mut!(BRIDGE_SOCKET) = Some(ksock) };
        let bridge = thread::spawn("serial", 20, bridge_main, 0);
        let code = user::finish(pid, tid);
        STOP.store(true, core::sync::atomic::Ordering::Relaxed);
        thread::join(bridge);
        Ok(code)
    })();
    let verdict = match &result {
        Ok(0) => Ok(()),
        Ok(c) => Err(format!("init exited with code {c}")),
        Err(e) => Err(e.clone()),
    };
    tests.push(("interactive_init_exits_cleanly".into(), verdict));
    // The services `init` started (launcher, filesystem) end when their last client, `init`,
    // is gone; give them a bounded time (a loaded machine needs more than a few ticks).
    for _ in 0..2000 {
        if user::process_count() == 0 {
            break;
        }
        thread::sleep_ticks(1);
    }
    thread::sleep_ticks(3);
    let dropped = DROPPED.load(core::sync::atomic::Ordering::Relaxed);
    tests.push((
        "interactive_no_input_was_dropped".into(),
        if dropped == 0 {
            Ok(())
        } else {
            Err(format!("{dropped} UART input bytes were dropped"))
        },
    ));
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
    tests.push((
        "interactive_no_kernel_thread_survives".into(),
        if thread::thread_count() == threads_before {
            Ok(())
        } else {
            Err(format!(
                "kernel threads {} -> {}",
                threads_before,
                thread::thread_count()
            ))
        },
    ));
    crate::user_tests::Report {
        tests,
        user_reported: user::reported(),
    }
}
