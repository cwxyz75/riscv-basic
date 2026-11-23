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
mod entropy;
mod heap;
#[cfg(feature = "interactive")]
mod interactive;
mod kmem;
mod kstack;
mod latency;
mod percpu;
mod report;
#[cfg(not(feature = "release"))]
mod sched_tests;
#[cfg(not(feature = "release"))]
mod selftest;
mod smp;
#[cfg(not(feature = "release"))]
mod smp_tests;
mod thread;
mod user;
#[cfg(not(feature = "release"))]
mod user_tests;

use core::fmt::Write;
use core::panic::PanicInfo;
use kcore::latch::Latch;
use kcore::testlog;

/// The release build: no boot self-tests, no `##` lines for passing checks, no banners. It boots
/// straight to the shell (T48). A failing kernel-side check, a panic and the final verdict on a
/// failure still print, so a broken release build is not silent.
/// The boot thread's priority (the test modules build theirs from it).
const MAIN_PRIO: u8 = 40;

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
    // First of all: this hart's record in `tp` (the trap entry and the console's lock need it, so a panic in what follows can still print).
    percpu::bind(0, hart_id);
    // Then, before anything prints: where the tree is (the early table maps all physical space below 64 GiB, and the pointer is
    // bounds-checked), the console it names, the boot word that forces the firmware's console (the fallback path, exercised on a machine
    // that has a UART: `make os-boot-test-sbi-console`) and the test device. All allocation-free: there is no heap yet.
    dtb::locate(dtb);
    console::init_from_tree();
    if dtb::dtb_blob().is_some_and(|b| kcore::fdt::bootarg_present(b, "console=sbi")) {
        console::use_sbi();
    }
    finisher::init(dtb::dtb_blob().and_then(kcore::fdt::test_finisher_base));
    percpu::this()
        .online
        .store(true, core::sync::atomic::Ordering::Release);
    percpu::this()
        .hart_state
        .store(percpu::HART_ONLINE, core::sync::atomic::Ordering::Release);
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
    dtb::init_timebase(); // (the clock's rate: before anything reads the time)
    entropy::init(hart_id); // (HD1: the pool is seeded before any address is chosen)
    kmem::init();
    heap::init_window(); // (before the kernel half is snapshotted, like the stack window)
    dtb::require_extensions(); // PL1: every hart has the profile's extensions, or the kernel stops here
    if let Some(line) = dtb::ff_report() {
        let _ = writeln!(out, "{line}"); // (T189: what `ff=` asked for, so the harness can insist on it)
    }
    fp::init_boot_hart(); // PL2: the vector register length (every other hart must agree)
    fp::enable_cache_block_ops_or_stop(); // PL3: cbo.zero and friends for user mode
    kstack::init(); // the stack window's top-level page-table entry must exist before the kernel half is snapshotted
    aia::init();
    // PL6: the AIA is required (the inter-hart interrupt is an IMSIC message, and there is no polling mode)
    assert!(
        aia::present(),
        "no usable AIA (APLIC in MSI mode with an IMSIC): this kernel needs `-machine virt,aia=aplic-imsic`"
    );
    if !RELEASE {
        let _ = writeln!(out, "interrupt controller: AIA");
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

    #[cfg(not(feature = "release"))]
    for (name, test) in selftest::TESTS.iter() {
        match test() {
            Ok(()) => tally.record(name, Ok(())),
            Err(detail) => tally.record(name, Err(&detail)),
        }
    }
    // S2: threads, the timer and the scheduler. The boot thread becomes a real-time
    // thread and interrupts come on.
    thread::init(MAIN_PRIO);
    // From here the big kernel lock is real: entry points assert that it is held.
    user::install_hooks(); // (the address-space locks run on this machine's interrupt state and order checker)
    bkl::enforce();
    // The other harts join the scheduler (each adopts its boot context as its idle thread).
    {
        let deadline = timer::now()
            + timer::cycles_from_ns(2_000_000_000 + 100_000_000 * smp::started() as u64);
        while thread::joined() < smp::started() && timer::now() < deadline {
            core::hint::spin_loop();
        }
    }
    timer::start();
    irq::enable_timer_interrupt();
    irq::enable();
    let class = thread::class_name();
    if !RELEASE {
        let _ = writeln!(out, "scheduler class: {class}");
    }
    tally.record(
        &alloc::format!("scheduler_class_is_{}", class.replace('-', "_")),
        Ok(()),
    );
    #[cfg(not(feature = "release"))]
    for (name, test) in sched_tests::TESTS.iter().chain(sched_tests::PRIORITY_TESTS) {
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
    #[cfg(not(feature = "release"))]
    for (name, test) in smp_tests::TESTS.iter() {
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

/// The words on the panicking thread's kernel stack that point into the kernel's code, innermost first: the return addresses of the calls that led here, among a few
/// stale ones (T233: a panic whose location is the standard library's says nothing of its caller; resolve these with `llvm-addr2line -e <kernel ELF>`). Only a thread
/// that entered from user mode has a known stack top (`PerHart.kstack_top`); on any other the list is empty. Reads nothing outside `[sp, kstack_top)`.
struct Trace;

impl core::fmt::Display for Trace {
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: addresses below 2^56; the loop stays inside [sp, top) and counts to at most 32
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let sp = arch::stack_pointer() & !7;
        let top = percpu::this()
            .kstack_top
            .load(core::sync::atomic::Ordering::Relaxed) as usize;
        let text = kmem::sections().text;
        // (a stack is at most 64 KiB here; anything else is not the stack this hart is on)
        if top == 0 || top <= sp || top - sp > 64 * 1024 {
            return Ok(());
        }
        write!(f, " [stack")?;
        let (mut at, mut shown) = (sp, 0);
        while at + 8 <= top && shown < 32 {
            // SAFETY: `at` is 8-aligned and in `[sp, top)`, the live part of this hart's kernel stack, which is mapped and 8-byte readable.
            let w = unsafe { core::ptr::read_volatile(at as *const u64) };
            if w >= text.0 && w < text.1 {
                write!(f, " {w:#x}")?;
                shown += 1;
            }
            at += 8;
        }
        write!(f, "]")
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    console::break_lock();
    let _ = testlog::panic(&mut console::Console, &format_args!("{info}{Trace}"));
    finisher::exit(false)
}
