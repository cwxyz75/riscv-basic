//! Kernel-side tests of user mode (S3b): the boot-time user suite (`init`) is run as a real
//! process and must exit cleanly; processes that misbehave on purpose (`crash`) must be
//! ended with the right exit code while the kernel carries on; and tearing a process down
//! must give back every frame and every kernel object (INVARIANTS H6).

use crate::{kmem, thread, user};
use abi::status::Status;
use alloc::format;
use alloc::string::String;

use crate::report::Outcome;

/// The exit code of a process ended by a fault is `-1000 - kind` (SYSCALLS.md, "User address space and process start";
/// `abi::arch::fault`): every page fault is one kind, whatever the access.
use abi::arch::fault::{exit_code, ILLEGAL, PAGE_FAULT};

/// Free frames, plus the device directory's (IOMMU6): the first domain of a device id grows the directory for good (its device stays
/// quarantined after the domain goes), which is the kernel's structure, not a leak.
fn free_frames() -> usize {
    crate::irq::without(kmem::free_frames_accounted) + crate::iommu::directory_frames()
}

/// The four terms of `free_frames_accounted`, for a leak report: (database, per-hart caches, kernel stacks, heap).
fn frame_terms() -> (usize, usize, usize, usize) {
    (
        crate::irq::without(|| kmem::get().mm.db.free_count()),
        kmem::cached_frames(),
        crate::kstack::frames_held(),
        crate::heap::frames_held(),
    )
}

/// What else a frame leak report says (T227): the per-hart frame-cache fill, a frame the heap's pool has in hand, the kernel stacks in use,
/// the threads that have exited and still hold a stack, the FP and vector state records, and the walk oracle of both windows
/// ([`windows_walked`]). Read after the terms, so a moving count is seen as such.
fn frame_extras() -> String {
    let fill: alloc::vec::Vec<usize> = (0..crate::smp::started().max(1))
        .map(|i| crate::percpu::hart(i).frame_cache.len())
        .collect();
    let (stacks, heap) = windows_walked();
    format!(
        "per-hart frame caches {fill:?}, in transit {}, kernel stacks live {}, exited threads not reaped {}, FP and vector records {}; \
         walked and formula: stack window {stacks:?}, heap window {heap:?}",
        crate::heap::in_transit(),
        crate::kstack::LIVE.load(core::sync::atomic::Ordering::Relaxed),
        thread::exited_unreaped(),
        crate::fp::entry_count(),
    )
}

/// T227's walk oracle: `(walked, formula)` for the kernel-stack window and the heap window, with interrupts off (`kstack::frames_walked`,
/// `heap::window_frames_walked`).
pub fn windows_walked() -> ((usize, usize), (usize, usize)) {
    crate::irq::without(|| {
        (
            crate::kstack::frames_walked(),
            crate::heap::window_frames_walked(),
        )
    })
}

/// The walk oracle as a check: both windows hold exactly what their formulas say (the leak oracle counts the formulas, so a frame a path
/// leaves in a window uncounted would read as a leak, or hide one).
pub fn windows_hold_what_their_formulas_say() -> Outcome {
    let (stacks, heap) = windows_walked();
    if stacks.0 == stacks.1 && heap.0 == heap.1 {
        Ok(())
    } else {
        Err(format!(
            "walked and formula: stack window {stacks:?}, heap window {heap:?}"
        ))
    }
}

fn run(image: &[u8], arg: u64) -> Result<i32, String> {
    let (pid, tid) =
        user::start(image, arg).map_err(|e| format!("could not start the program: {e:?}"))?;
    Ok(user::finish(pid, tid))
}

/// How long a failed `no_process_or_vmo_survives_init` goes on watching the survivors before it says what is left (T278: slow or stuck),
/// in ticks (milliseconds). Spent only on a failure.
const LEAK_RESAMPLE_TICKS: u64 = 5000;

/// The kernel's end of init's bootstrap channel, for the helper thread.
static mut HELPER_END: Option<user::KernelEnd> = None;

/// A stand-in server on the kernel side of init's bootstrap channel (see the comment in
/// suite.rs above `wait_arguments_are_checked`): the first 4 bytes of a request are a
/// transaction id, the 5th the operation.
#[allow(clippy::large_stack_frames)] // KT13: audited, a test-only kernel thread whose frame holds the arms of the helper's requests; the boot-time stack budget check bounds it
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
                b'B' => {
                    // the boot image as a read-only VMO (for the launcher tests)
                    if let Ok(h) = user::ktable_readonly_vmo(user::BOOT_IMAGE) {
                        if end.write_handles(&msg[..5], &[h]).is_err() {
                            user::ktable_close(h);
                        }
                    }
                }
                b'A' => {
                    // the conformance program image (AB9)
                    let _ = end.write_with_vmo(&msg[..5], user::ABI_CONFORMANCE_ELF);
                }
                b'F' => {
                    // the guest fuzzer's program image (KT8)
                    let _ = end.write_with_vmo(&msg[..5], user::KFUZZ_ELF);
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

/// While user code runs on the boot hart, and while it idles, another hart must be able to take
/// the big kernel lock (the lock is released on every path that leaves kernel code). With one
/// hart there is nobody to ask, and the check passes without observing anything.
fn bkl_is_released() -> Outcome {
    use crate::smp;
    if smp::started() < 2 {
        return Ok(());
    }
    let probe_cycles = crate::timer::cycles_from_ns(40_000_000); // 40 ms of the counter
    let probe = |phase: &str| -> Outcome {
        smp::start_work(1, smp::WORK_BKL_PROBE, probe_cycles);
        thread::sleep_ticks(60);
        let r = smp::wait_work(1, 10 * crate::timer::timebase())
            .ok_or_else(|| format!("{phase}: no answer"))?;
        let (ok, fail) = (r & 0xffff_ffff, r >> 32);
        // A lock held by user-mode execution would be found held almost always (ok near 0). Timer
        // ticks and a host that preempts a vCPU inside a locked region (CI, a busy laptop) make
        // "held" common without being a bug, so the bar is only that it is free at least as often
        // as it is held.
        if ok < 100 || ok < fail {
            return Err(format!(
                "{phase}: the other hart took the lock {ok} times and found it held {fail} times"
            ));
        }
        Ok(())
    };
    // phase 1: a user thread spinning in U-mode on this hart
    let (pid, tid) = user::start(user::CRASH_ELF, 15).map_err(|e| format!("{e:?}"))?;
    let a = probe("user mode");
    user::end_process(pid, -7);
    user::finish(pid, tid);
    a?;
    // phase 2: this hart idle (every thread blocked)
    probe("idle")
}

/// SCHED-SMP-SPEC task 12, acceptance: a hart whose only events are its timer ticks does not take the big kernel lock for
/// them. Hart 1 is idle while this thread sleeps on hart 0 (the sleep takes the lock, on hart 0); hart 1's count of
/// big-lock takes must hardly move over 80 ticks (it took one per tick when every trap took the lock).
fn idle_harts_take_no_big_lock_for_their_ticks() -> Outcome {
    use crate::smp;
    if smp::started() < 2 {
        return Ok(());
    }
    thread::sleep_ticks(20); // let the previous test's work on hart 1 drain
    let before = thread::wake_stat(17);
    let ticks0 = crate::percpu::hart(1)
        .timer_ticks
        .load(core::sync::atomic::Ordering::Relaxed);
    // (L13, acceptance: an idle hart takes no timer interrupt over a second: there is no tick, and nothing is armed on it)
    thread::sleep_ticks(1000);
    let takes = thread::wake_stat(17) - before;
    let ticks = crate::percpu::hart(1)
        .timer_ticks
        .load(core::sync::atomic::Ordering::Relaxed)
        - ticks0;
    if ticks > 2 {
        return Err(format!(
            "idle hart 1 took {ticks} timer interrupts in a second: it should take none"
        ));
    }
    if takes > 4 {
        return Err(format!(
            "hart 1 took the big kernel lock {takes} times for {ticks} timer interrupts in a second of nothing"
        ));
    }
    Ok(())
}

pub use crate::report::Report;

#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
pub fn run_all() -> Report {
    let mut tests: alloc::vec::Vec<(String, Outcome)> = alloc::vec::Vec::new();
    let mut push = |name: &str, r: Outcome| tests.push((String::from(name), r));

    push(
        "user_programs_are_embedded",
        if user::SUITE_ELF.len() > 1000 && user::CRASH_ELF.len() > 1000 {
            Ok(())
        } else {
            Err(String::from(
                "the user programs were not built before the kernel (run `make os-user`)",
            ))
        },
    );

    push("bkl_is_released_on_user_entry_and_idle", bkl_is_released());
    push(
        "idle_harts_take_no_big_lock_for_their_ticks",
        idle_harts_take_no_big_lock_for_their_ticks(),
    );

    // The user-level suite, as a process.
    let frames = free_frames();
    let frames_terms = frame_terms();
    let frames_extras = frame_extras();
    let vmos_before = crate::irq::without(|| kmem::get().vmos.count());
    let threads_before = thread::thread_count();
    let tids_before = thread::tids();
    // (T278: whether a failing leak check below has printed the leak report already)
    let mut leak_reported = false;
    let code = (|| -> Result<i32, String> {
        let (pid, tid) = user::start(
            user::SUITE_ELF,
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
    // (T265, T278: init's end kills every child it left, some fifty at four harts, and their teardowns run at once; that took about 0.3 s
    // on a quiet host and longer on a loaded one, past the 0.2 s this wait had, which failed the checks below on processes that were only
    // slow. The wait is now for every process to be gone from the table and from the record of those on their way out, up to the 5 s of
    // the leak watch: a stuck teardown still fails, a slow one does not, and how long it took is printed for every boot as one `leak:
    // settle` line, with how many processes there were and how many sweeps ran meanwhile.)
    let (t0, left, sweeps0) = (thread::ticks(), user::process_count(), user::sweep_epoch());
    while (user::process_count() != 0 || user::dying::count() != 0)
        && thread::ticks().wrapping_sub(t0) < LEAK_RESAMPLE_TICKS
    {
        thread::sleep_ticks(1);
    }
    let _ = core::fmt::Write::write_fmt(
        &mut crate::console::Console,
        format_args!(
            "leak: settle {} ms: {left} processes in the table when init ended, {} on their way out now, {} sweeps begun meanwhile\n",
            thread::ticks().wrapping_sub(t0),
            user::process_count().saturating_add(user::dying::count()),
            user::sweep_epoch().wrapping_sub(sweeps0)
        ),
    );
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
    // (frames in flight settle: a dead thread's stack goes back as its hart switches away, a heap service moves frames between the terms; a leak does not
    // settle, so the check waits a bounded time for the count to come back before it fails; T227)
    // (T207: a dying VMO's pages go back at the tail of the next call, and a system that has stopped calling has nothing to run it: the check is that
    // memory is returned once something does)
    for _ in 0..200 {
        crate::user::drain_dying();
        if free_frames() == frames {
            break;
        }
        thread::sleep_ticks(1);
    }
    push(
        "process_teardown_returns_every_frame",
        if free_frames() == frames {
            Ok(())
        } else {
            let pending = kcore::aspace::DRAIN_PENDING.load(core::sync::atomic::Ordering::Acquire);
            let at = free_frames();
            crate::user::drain_dying();
            let terms = frame_terms();
            Err(format!(
                "free frames {} -> {} (database, caches, stacks, heap: {:?} -> {:?}; a dying VMO still to drain: {pending}; after one more drain: {}; \
                 before init: {frames_extras}; now: {})",
                frames,
                at,
                frames_terms,
                terms,
                free_frames(),
                frame_extras()
            ))
        },
    );
    push(
        "kernel_windows_hold_what_their_formulas_say_after_init",
        windows_hold_what_their_formulas_say(),
    );
    push(
        "no_process_or_vmo_survives_init",
        if user::process_count() == 0
            && user::futex_sleepers() == 0
            && user::pager_leftovers() == 0
            && crate::fp::entry_count() == 0
            && crate::irq::without(|| kmem::get().vmos.count()) == vmos_before
            && user::job_ledger() == (1, 0, 0, 0, 0, 0)
            // (IOMMU6) no device domain, no I/O-mapped page and no leaked mapping record is left
            && (159..=161).all(|w| crate::iommu::stat(w) == 0)
            // (IOMMU7) no live vector and none leaked
            && crate::aia::stat(146) == 0
            && crate::aia::stat(149) == 0
        {
            Ok(())
        } else {
            // (T278: the survivors name themselves, and each thread says where it waits, before the summary)
            user::report_processes();
            thread::report_threads(&tids_before);
            leak_reported = true;
            let summary = format!(
                "{} processes and {} VMOs left (wanted 0 and {vmos_before}), {} futex sleepers, {} pager waiters or pagers; (jobs, \
                 processes, threads) {:?}, wanted (1, 0, 0, 0, 0, 0); device domains, I/O-mapped pages, leaked holds {:?}; vectors live and                  leaked {:?}",
                user::process_count(),
                crate::irq::without(|| kmem::get().vmos.count()),
                user::futex_sleepers(),
                user::pager_leftovers(),
                user::job_ledger(),
                [159, 160, 161].map(crate::iommu::stat),
                [146, 149].map(crate::aia::stat)
            );
            // (T278: then wait a few seconds more and say what is left, so a teardown that is only slow is told from one that is stuck;
            // the check has failed already, on the counts above)
            let t0 = thread::ticks();
            while (user::process_count() != 0 || user::dying::count() != 0)
                && thread::ticks().wrapping_sub(t0) < LEAK_RESAMPLE_TICKS
            {
                thread::sleep_ticks(10);
            }
            let _ = core::fmt::Write::write_fmt(
                &mut crate::console::Console,
                format_args!(
                    "leak: {} ms later: {} processes ({} on their way out), {} kernel threads, (jobs, processes, threads) {:?}, {} objects, {} free frames\n",
                    thread::ticks().wrapping_sub(t0),
                    user::process_count(),
                    user::dying::count(),
                    thread::thread_count(),
                    user::job_ledger(),
                    user::live_objects(),
                    free_frames()
                ),
            );
            Err(summary)
        },
    );
    thread::sleep_ticks(3); // detached user threads and their stacks are freed as the hart switches away from them
    push(
        "no_kernel_thread_survives_init",
        if thread::thread_count() == threads_before {
            Ok(())
        } else {
            if !leak_reported {
                thread::report_threads(&tids_before);
            }
            Err(format!(
                "kernel threads {} -> {}",
                threads_before,
                thread::thread_count()
            ))
        },
    );
    // KT13: how deep the kernel stacks of the threads that ended went, against the budget (half a stack). The offender, if any,
    // is named, and the perturbation, fuzz and allocation-failure runs inherit the check.
    {
        let used = thread::stack_high_water();
        let _ = core::fmt::Write::write_fmt(
            &mut crate::console::Console,
            format_args!(
                "kernel: deepest kernel stack use among ended threads: {used} of {} bytes (budget {})\n",
                thread::STACK_BYTES,
                thread::STACK_BUDGET
            ),
        );
        push(
            "kernel_stack_budget",
            match thread::stack_budget_violation() {
                None => Ok(()),
                Some((name, tid, used)) => Err(format!(
                    "kernel thread {name} (tid {tid}) used {used} of its {} byte stack, over the {} byte budget",
                    thread::STACK_BYTES,
                    thread::STACK_BUDGET
                )),
            },
        );
    }
    push(
        "process_exit_releases_all_objects",
        if user::live_objects() == 0 {
            Ok(())
        } else {
            if !leak_reported {
                user::report_processes(); // (T278: the live objects by kind, and what holds them)
            }
            Err(format!(
                "{} kernel objects left alive after init exited",
                user::live_objects()
            ))
        },
    );

    // Programs that misbehave: the kernel ends them and keeps running.
    let faults: [(u64, &str, i32); 8] = [
        (
            1,
            "user_store_to_kernel_memory_is_fatal",
            exit_code(PAGE_FAULT),
        ),
        (
            2,
            "user_load_from_kernel_memory_is_fatal",
            exit_code(PAGE_FAULT),
        ),
        (3, "user_store_to_own_text_is_fatal", exit_code(PAGE_FAULT)),
        (4, "user_jump_to_data_is_fatal", exit_code(PAGE_FAULT)),
        (
            5,
            "user_privileged_instruction_is_fatal",
            exit_code(ILLEGAL),
        ),
        (
            6,
            "user_jump_to_unmapped_memory_is_fatal",
            exit_code(PAGE_FAULT),
        ),
        (7, "user_null_pointer_load_is_fatal", exit_code(PAGE_FAULT)),
        (
            10,
            "user_jump_to_kernel_text_is_fatal",
            exit_code(PAGE_FAULT),
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
            Ok(c) if c == exit_code(PAGE_FAULT) => Ok(()),
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
    let mut corrupt = alloc::vec::Vec::from(user::SUITE_ELF);
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
