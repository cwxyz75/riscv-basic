//! The OS-track kernel. S0 made the harness; S1 (this) turns on Sv39 with the
//! kernel in the upper half, adds the frame allocator, the kernel heap and the
//! page-fault path with lazily committed memory objects, and proves them with
//! self-tests that report as structured lines (`kcore::testlog`) and end QEMU
//! through the test finisher with a pass/fail exit status.

#![no_std]
#![cfg_attr(feature = "interactive", allow(dead_code))]
#![no_main]
#![deny(clippy::undocumented_unsafe_blocks)] // KT3: every unsafe block says why it holds
#![deny(clippy::large_stack_frames)] // KT13: no frame over 1 KiB (kernel/clippy.toml) on a 16 KiB kernel stack

extern crate alloc;

// The architecture's modules keep their short names at the crate root (`crate::trap::...`).
use arch::{aia, finisher, fp, irq, timer, trap};

mod arch;
mod bkl;
mod chaos;
mod console;
mod dtb;
mod heap;
#[cfg(feature = "interactive")]
mod interactive;
mod kmem;
mod kstack;
mod latency;
mod percpu;
mod sched_tests;
mod selftest;
mod smp;
mod smp_tests;
mod thread;
mod user;
mod user_tests;

use core::fmt::Write;
use core::panic::PanicInfo;
use kcore::latch::Latch;
use kcore::testlog;

/// The release build: no boot self-tests, no `##` lines for passing checks, no banners. It boots
/// straight to the shell (T48). A failing kernel-side check, a panic and the final verdict on a
/// failure still print, so a broken release build is not silent.
const RELEASE: bool = cfg!(feature = "release");

/// A zero-initialised static: it lives in .bss, so this checks `_start` cleared it.
static BSS_PROBE: [u64; 64] = [0; 64];

struct Tally {
    pass: u32,
    fail: u32,
}

impl Tally {
    /// Count results the user suite has already printed as `##TEST` lines.
    fn add(&mut self, pass: u32, fail: u32) {
        self.pass += pass;
        self.fail += fail;
    }

    fn record(&mut self, name: &str, result: Result<(), &str>) {
        if !(RELEASE && result.is_ok()) {
            let _ = testlog::test(&mut console::Console, name, result);
        }
        if result.is_ok() {
            self.pass += 1;
        } else {
            self.fail += 1;
        }
    }
}

#[no_mangle]
#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
pub extern "C" fn kmain(hart_id: usize, dtb: usize) -> ! {
    // First of all, before anything can print: where the tree is (the early table maps all physical space below 64 GiB, and the pointer is
    // bounds-checked), then the console it names and the test device (allocation-free: there is no heap yet).
    dtb::locate(dtb);
    console::init_from_tree();
    finisher::init(dtb::dtb_blob().and_then(kcore::fdt::test_finisher_base));
    // this hart's record in `tp` (the trap entry needs it)
    percpu::bind(0, hart_id);
    percpu::this()
        .online
        .store(true, core::sync::atomic::Ordering::Release);
    let mut out = console::Console;
    if !RELEASE {
        let _ = testlog::boot(&mut out, "kernel", "s3");
        let _ = writeln!(
            out,
            "hello from the OS track, hart {hart_id}, paging via the early table"
        );
    }

    let mut tally = Tally { pass: 0, fail: 0 };
    tally.record(
        "bss_is_zeroed",
        if BSS_PROBE.iter().all(|&w| w == 0) {
            Ok(())
        } else {
            Err("a .bss word was not zero")
        },
    );
    tally.record("latch_works_on_the_target", {
        let latch = Latch::new();
        latch.set();
        if latch.take() && !latch.take() {
            Ok(())
        } else {
            Err("latch semantics broke on riscv64")
        }
    });

    heap::init();
    trap::init();
    if dtb::boot_arg_present("console=sbi") {
        console::use_sbi(); // (the fallback path, exercised on a machine that has a UART: `make os-boot-test-sbi-console`)
    }
    dtb::init_timebase(); // (the clock's rate: before anything reads the time)
    kmem::init();
    heap::init_window(); // (before the kernel half is snapshotted, like the stack window)
    dtb::require_extensions(); // PL1: every hart has the profile's extensions, or the kernel stops here
    kstack::init(); // the stack window's top-level page-table entry must exist before the kernel half is snapshotted
    aia::init();
    if !RELEASE {
        let _ = writeln!(
            out,
            "interrupt controller: {}",
            if aia::present() { "AIA" } else { "none" }
        );
        let _ = writeln!(
            out,
            "kernel page table active; {} frames free",
            kmem::get().mm.db.free_count()
        );
    }
    // S5a: start the other harts (they only idle and count interrupts until S5c).
    chaos::init(); // (KT7: no effect unless built with the `chaos` feature and booted with `chaos=<seed>`)
    let harts = smp::start_secondaries(hart_id);
    if harts > 1 && !RELEASE {
        let _ = writeln!(out, "harts online: {}", smp::online());
    }

    for (name, test) in selftest::TESTS.iter().filter(|_| !RELEASE) {
        match test() {
            Ok(()) => tally.record(name, Ok(())),
            Err(detail) => tally.record(name, Err(&detail)),
        }
    }
    // S2: threads, the timer and the scheduler. The boot thread becomes a real-time
    // thread and interrupts come on.
    thread::init(sched_tests::MAIN_PRIO);
    // From here the big kernel lock is real: entry points assert that it is held.
    bkl::enforce();
    // The other harts join the scheduler (each adopts its boot context as its idle thread).
    {
        let deadline = timer::now() + timer::cycles_from_ns(2_000_000_000);
        while thread::joined() < smp::started() && timer::now() < deadline {
            core::hint::spin_loop();
        }
    }
    timer::start();
    irq::enable_timer_interrupt();
    smp::enable_ipi_on_this_hart();
    irq::enable();
    let class = thread::class_name();
    if !RELEASE {
        let _ = writeln!(out, "scheduler class: {class}");
    }
    tally.record(
        &alloc::format!("scheduler_class_is_{}", class.replace('-', "_")),
        Ok(()),
    );
    for (name, test) in sched_tests::TESTS
        .iter()
        .chain(sched_tests::PRIORITY_TESTS)
        .filter(|_| !RELEASE)
    {
        match test() {
            Ok(()) => tally.record(name, Ok(())),
            Err(detail) => tally.record(name, Err(&detail)),
        }
    }
    // S3b: user mode. The user suite reports its own `##TEST` lines (counted by the
    // kernel); the kernel-side checks are reported here.
    kmem::snapshot_kernel_half();
    #[cfg(feature = "stack-overflow-demo")]
    {
        fn recurse(depth: usize) -> usize {
            let pad = [depth as u8; 512];
            core::hint::black_box(&pad);
            recurse(depth + 1) + usize::from(pad[0])
        }
        let t = thread::spawn(
            "overflow",
            10,
            |n| {
                recurse(n);
            },
            0,
        );
        thread::join(t);
    }
    for (name, test) in smp_tests::TESTS.iter().filter(|_| !RELEASE) {
        match test() {
            Ok(()) => tally.record(name, Ok(())),
            Err(detail) => tally.record(name, Err(&detail)),
        }
    }
    let before = user::reported();
    #[cfg(feature = "interactive")]
    let report = interactive::run();
    #[cfg(not(feature = "interactive"))]
    let report = user_tests::run_all();
    let _ = before;
    tally.add(report.user_reported.0, report.user_reported.1);
    for (name, outcome) in &report.tests {
        match outcome {
            Ok(()) => tally.record(name, Ok(())),
            Err(detail) => tally.record(name, Err(detail)),
        }
    }
    #[cfg(feature = "latency-trace")]
    {
        latency::report(); // (before the check below resets the maximum)
        match latency::the_latency_trace_can_see_a_long_section() {
            Ok(()) => tally.record("the_latency_trace_can_see_a_long_section", Ok(())),
            Err(d) => tally.record("the_latency_trace_can_see_a_long_section", Err(&d)),
        }
    }
    #[cfg(feature = "fail-demo")]
    tally.record(
        "deliberate_failure",
        Err("fail-demo build: failing on purpose"),
    );

    if RELEASE && tally.fail == 0 {
        let _ = writeln!(out, "system halted");
    } else {
        let _ = testlog::done(&mut out, tally.pass, tally.fail);
    }
    finisher::exit(tally.fail == 0)
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    console::break_lock();
    let _ = testlog::panic(&mut console::Console, info);
    finisher::exit(false)
}
