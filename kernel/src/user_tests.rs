//! Kernel-side tests of user mode (S3b): the boot-time user suite (`init`) is run as a real
//! process and must exit cleanly; processes that misbehave on purpose (`crash`) must be
//! ended with the right exit code while the kernel carries on; and tearing a process down
//! must give back every frame and every kernel object (INVARIANTS H6).

use crate::{kmem, thread, user};
use abi::status::Status;
use alloc::format;
use alloc::string::String;

type Outcome = Result<(), String>;

/// scause values (RISC-V privileged spec): the exit code of a process ended by a fault
/// is `-1000 - scause` (SYSCALLS.md, "User address space and process start").
const ILLEGAL_INSTRUCTION: i32 = 2;
const INSTRUCTION_PAGE_FAULT: i32 = 12;
const LOAD_PAGE_FAULT: i32 = 13;
const STORE_PAGE_FAULT: i32 = 15;

fn free_frames() -> usize {
    kmem::get().mm.db.free_count()
}

fn run(image: &[u8], arg: u64) -> Result<i32, String> {
    let (pid, tid) =
        user::start(image, arg).map_err(|e| format!("could not start the program: {e:?}"))?;
    Ok(user::finish(pid, tid))
}

/// The kernel's end of init's bootstrap channel, for the helper thread.
static mut HELPER_END: Option<user::KernelEnd> = None;

/// A stand-in server on the kernel side of init's bootstrap channel (see the comment in
/// init.rs above `wait_arguments_are_checked`): the first 4 bytes of a request are a
/// transaction id, the 5th the operation.
fn helper_main(_: usize) {
    // SAFETY: set by `run_all` just before this thread is spawned, taken once here.
    let end =
        unsafe { (*core::ptr::addr_of_mut!(HELPER_END)).take() }.expect("the helper's end was set");
    loop {
        if !end.wait_readable(u64::MAX) {
            continue;
        }
        match end.read() {
            Ok(msg) if msg.len() >= 5 => match msg[4] {
                b'E' => {
                    let _ = end.write(&msg);
                }
                b'D' => {
                    thread::sleep_ticks(3);
                    let _ = end.write(&msg);
                }
                b'N' => {
                    let _ = end.write(&[0xad, 0xde, 0, 0, b'n']);
                    let _ = end.write(&msg);
                }
                b'C' => break,
                b'I' => {
                    // hand over the `crash` program image as a VMO (for the user-level launcher)
                    let _ = end.write_with_vmo(&msg[..5], user::CRASH_ELF);
                }
                b'H' => {
                    // the `hello` program image, for the runtime tests
                    let _ = end.write_with_vmo(&msg[..5], user::HELLO_ELF);
                }
                b'T' => {
                    thread::sleep_ticks(3);
                    break;
                }
                _ => {} // 'S' and anything else: silence
            },
            Ok(_) => {}
            Err(Status::ShouldWait) => {}
            Err(_) => break, // PEER_CLOSED: the process is gone
        }
    }
    end.close();
}

pub struct Report {
    /// (name, outcome) of each kernel-side test.
    pub tests: alloc::vec::Vec<(String, Outcome)>,
    /// `##TEST` lines the user suite reported: (pass, fail), already counted by `user`.
    pub user_reported: (u32, u32),
}

pub fn run_all() -> Report {
    let mut tests: alloc::vec::Vec<(String, Outcome)> = alloc::vec::Vec::new();
    let mut push = |name: &str, r: Outcome| tests.push((String::from(name), r));

    push(
        "user_programs_are_embedded",
        if user::INIT_ELF.len() > 1000 && user::CRASH_ELF.len() > 1000 {
            Ok(())
        } else {
            Err(String::from(
                "the user programs were not built before the kernel (run `make os-user`)",
            ))
        },
    );

    // The user-level suite, as a process.
    let frames = free_frames();
    let vmos_before = kmem::get().vmos.count();
    let threads_before = thread::thread_count();
    let code = (|| -> Result<i32, String> {
        let (pid, tid) = user::start(
            user::INIT_ELF,
            u64::from(cfg!(feature = "sched-round-robin")),
        )
        .map_err(|e| format!("could not start init: {e:?}"))?;
        // SAFETY: the helper thread has not been spawned yet; it takes this once.
        unsafe { *core::ptr::addr_of_mut!(HELPER_END) = user::take_bootstrap(pid) };
        let helper = thread::spawn("helper", 20, helper_main, 0);
        let code = user::finish(pid, tid);
        thread::join(helper);
        Ok(code)
    })();
    // A child that outlived init dies asynchronously (kill flag, then its own exit): give it a
    // bounded time, so one leaking test does not also fail the leak checks below.
    for _ in 0..200 {
        if user::process_count() == 0 {
            break;
        }
        thread::sleep_ticks(1);
    }
    let reported = user::reported();
    push(
        "init_runs_in_user_mode_and_passes",
        match &code {
            Ok(0) if reported.0 > 0 && reported.1 == 0 => Ok(()),
            Ok(c) => Err(format!(
                "init exit code {c}, {} user tests passed, {} failed",
                reported.0, reported.1
            )),
            Err(e) => Err(e.clone()),
        },
    );
    push(
        "process_teardown_returns_every_frame",
        if free_frames() == frames {
            Ok(())
        } else {
            Err(format!("free frames {} -> {}", frames, free_frames()))
        },
    );
    push(
        "no_process_or_vmo_survives_init",
        if user::process_count() == 0 && kmem::get().vmos.count() == vmos_before {
            Ok(())
        } else {
            Err(format!(
                "{} processes and {} VMOs left (wanted 0 and {vmos_before})",
                user::process_count(),
                kmem::get().vmos.count()
            ))
        },
    );
    thread::sleep_ticks(3); // detached user threads and their stacks are freed by the reaper
    push(
        "no_kernel_thread_survives_init",
        if thread::thread_count() == threads_before {
            Ok(())
        } else {
            Err(format!(
                "kernel threads {} -> {}",
                threads_before,
                thread::thread_count()
            ))
        },
    );
    push(
        "process_exit_releases_all_objects",
        if user::live_objects() == 0 {
            Ok(())
        } else {
            Err(format!(
                "{} kernel objects left alive after init exited",
                user::live_objects()
            ))
        },
    );

    // Programs that misbehave: the kernel ends them and keeps running.
    let faults: [(u64, &str, i32); 9] = [
        (
            1,
            "user_store_to_kernel_memory_is_fatal",
            -1000 - STORE_PAGE_FAULT,
        ),
        (
            2,
            "user_load_from_kernel_memory_is_fatal",
            -1000 - LOAD_PAGE_FAULT,
        ),
        (
            3,
            "user_store_to_own_text_is_fatal",
            -1000 - STORE_PAGE_FAULT,
        ),
        (
            4,
            "user_jump_to_data_is_fatal",
            -1000 - INSTRUCTION_PAGE_FAULT,
        ),
        (
            5,
            "user_privileged_instruction_is_fatal",
            -1000 - ILLEGAL_INSTRUCTION,
        ),
        (
            28,
            // the vector unit (mandatory in RVA23) is off in user mode: no vector state is
            // saved per thread, so a `vsetvli` must trap, on any CPU
            "user_vector_instruction_is_fatal",
            -1000 - ILLEGAL_INSTRUCTION,
        ),
        (
            6,
            "user_jump_to_unmapped_memory_is_fatal",
            -1000 - INSTRUCTION_PAGE_FAULT,
        ),
        (
            7,
            "user_null_pointer_load_is_fatal",
            -1000 - LOAD_PAGE_FAULT,
        ),
        (
            10,
            "user_jump_to_kernel_text_is_fatal",
            -1000 - INSTRUCTION_PAGE_FAULT,
        ),
    ];
    for (arg, name, want) in faults {
        let frames = free_frames();
        let got = run(user::CRASH_ELF, arg);
        let leaked = frames.saturating_sub(free_frames());
        push(
            name,
            match got {
                Ok(c) if c == want && leaked == 0 => Ok(()),
                Ok(c) if c == want => Err(format!("ended correctly but leaked {leaked} frames")),
                Ok(c) => Err(format!("exit code {c}, want {want}")),
                Err(e) => Err(e),
            },
        );
    }
    push(
        "a_syscall_with_sp_in_the_kernel_is_harmless",
        match run(user::CRASH_ELF, 11) {
            Ok(7) => Ok(()),
            Ok(c) => Err(format!("exit code {c}, want 7")),
            Err(e) => Err(e),
        },
    );
    push(
        "the_stack_has_a_guard_page",
        match run(user::CRASH_ELF, 12) {
            Ok(c) if c == -1000 - STORE_PAGE_FAULT || c == -1000 - LOAD_PAGE_FAULT => Ok(()),
            Ok(c) => Err(format!("exit code {c}, want a page fault")),
            Err(e) => Err(e),
        },
    );
    push(
        "kernel_top_level_slots_are_stable",
        if kmem::kernel_half_unchanged() {
            Ok(())
        } else {
            Err(String::from(
                "the kernel's top-level page-table slots changed after boot",
            ))
        },
    );
    // Two processes alive at once, each with a private marker at the same stack address.
    let frames = free_frames();
    let two = (|| -> Result<(), String> {
        let a = user::start(user::CRASH_ELF, 9 | (0x11 << 8)).map_err(|e| format!("{e:?}"))?;
        let b = user::start(user::CRASH_ELF, 9 | (0x22 << 8)).map_err(|e| format!("{e:?}"))?;
        let (ca, cb) = (user::finish(a.0, a.1), user::finish(b.0, b.1));
        if (ca, cb) == (0x11, 0x22) {
            Ok(())
        } else {
            Err(format!(
                "exit codes ({ca}, {cb}), want (17, 34): the processes saw each other's memory"
            ))
        }
    })();
    push("two_processes_do_not_share_memory", two);
    // The same, switching through the yield syscall.
    let yielding = (|| -> Result<(), String> {
        let a = user::start(user::CRASH_ELF, 13 | (0x33 << 8)).map_err(|e| format!("{e:?}"))?;
        let b = user::start(user::CRASH_ELF, 13 | (0x44 << 8)).map_err(|e| format!("{e:?}"))?;
        let (ca, cb) = (user::finish(a.0, a.1), user::finish(b.0, b.1));
        if (ca, cb) == (0x33, 0x44) {
            Ok(())
        } else {
            Err(format!("exit codes ({ca}, {cb}), want (51, 68)"))
        }
    })();
    push("yielding_processes_do_not_share_memory", yielding);
    push(
        "two_process_teardown_returns_every_frame",
        if free_frames() == frames {
            Ok(())
        } else {
            Err(format!("free frames {} -> {}", frames, free_frames()))
        },
    );
    push(
        "user_exit_code_is_reported",
        match run(user::CRASH_ELF, 8) {
            Ok(42) => Ok(()),
            Ok(c) => Err(format!("exit code {c}, want 42")),
            Err(e) => Err(e),
        },
    );
    // Bad programs are refused before anything is built.
    let mut corrupt = alloc::vec::Vec::from(user::INIT_ELF);
    corrupt[0] = 0;
    let frames = free_frames();
    push(
        "a_corrupt_program_is_refused_without_leaks",
        match user::start(&corrupt, 0) {
            Err(user::LoadError::Elf(_)) if free_frames() == frames => Ok(()),
            Err(e) => Err(format!(
                "refused with {e:?}, free frames {} -> {}",
                frames,
                free_frames()
            )),
            Ok(_) => Err(String::from("a corrupt ELF was started")),
        },
    );
    Report {
        tests,
        user_reported: reported,
    }
}
