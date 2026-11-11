//! The OS-track kernel. S0 made the harness; S1 (this) turns on Sv39 with the
//! kernel in the upper half, adds the frame allocator, the kernel heap and the
//! page-fault path with lazily committed memory objects, and proves them with
//! self-tests that report as structured lines (`kcore::testlog`) and end QEMU
//! through the test finisher with a pass/fail exit status.

#![no_std]
#![no_main]

extern crate alloc;

mod boot;
mod console;
mod finisher;
mod heap;
mod irq;
mod kmem;
mod sched_tests;
mod selftest;
mod thread;
mod timer;
mod trap;
mod user;
mod user_tests;

use core::fmt::Write;
use core::panic::PanicInfo;
use kcore::latch::Latch;
use kcore::testlog;

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
        let _ = testlog::test(&mut console::Console, name, result);
        if result.is_ok() {
            self.pass += 1;
        } else {
            self.fail += 1;
        }
    }
}

#[no_mangle]
pub extern "C" fn kmain(hart_id: usize, _dtb: usize) -> ! {
    let mut out = console::Console;
    let _ = testlog::boot(&mut out, "kernel", "s3");
    let _ = writeln!(
        out,
        "hello from the OS track, hart {hart_id}, paging via the early table"
    );

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
    kmem::init();
    let _ = writeln!(
        out,
        "kernel page table active; {} frames free",
        kmem::get().mm.db.free_count()
    );

    for (name, test) in selftest::TESTS {
        match test() {
            Ok(()) => tally.record(name, Ok(())),
            Err(detail) => tally.record(name, Err(&detail)),
        }
    }
    // S2: threads, the timer and the scheduler. The boot thread becomes a real-time
    // thread and interrupts come on.
    thread::init(sched_tests::MAIN_PRIO);
    timer::start();
    irq::enable_timer_interrupt();
    irq::enable();
    let class = thread::class_name();
    let _ = writeln!(out, "scheduler class: {class}");
    tally.record(
        &alloc::format!("scheduler_class_is_{}", class.replace('-', "_")),
        Ok(()),
    );
    for (name, test) in sched_tests::TESTS.iter().chain(sched_tests::PRIORITY_TESTS) {
        match test() {
            Ok(()) => tally.record(name, Ok(())),
            Err(detail) => tally.record(name, Err(&detail)),
        }
    }
    // S3b: user mode. The user suite reports its own `##TEST` lines (counted by the
    // kernel); the kernel-side checks are reported here.
    kmem::snapshot_kernel_half();
    let before = user::reported();
    let report = user_tests::run_all();
    let _ = before;
    tally.add(report.user_reported.0, report.user_reported.1);
    for (name, outcome) in &report.tests {
        match outcome {
            Ok(()) => tally.record(name, Ok(())),
            Err(detail) => tally.record(name, Err(detail)),
        }
    }
    #[cfg(feature = "fail-demo")]
    tally.record(
        "deliberate_failure",
        Err("fail-demo build: failing on purpose"),
    );

    let _ = testlog::done(&mut out, tally.pass, tally.fail);
    finisher::exit(tally.fail == 0)
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    let _ = testlog::panic(&mut console::Console, info);
    finisher::exit(false)
}
