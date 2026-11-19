//! The supervisor timer: Sstc `stimecmp`, one-shot, no periodic tick (L13). QEMU `virt`'s `time` counter runs at 10 MHz.

use core::arch::asm;
use core::sync::atomic::Ordering;

/// The frequency of the `time` counter in Hz: 10 MHz on QEMU `virt`. One place: PL4 reads it from the device tree and changes only
/// this function.
pub fn timebase() -> u64 {
    10_000_000
}

/// The counter value when the first tick was programmed (the clock's zero); 0 until then.
static BOOT_COUNT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Nanoseconds since boot, from the `time` counter (the clock user mode sees: `clock_get`, every deadline).
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: the remainder is below the frequency (10^7 for the 10 MHz counter), so rem * 10^9 stays far below u64::MAX; the quotient times 10^9 holds 584 years
pub fn now_ns() -> u64 {
    let c = now().saturating_sub(BOOT_COUNT.load(Ordering::Relaxed));
    let hz = timebase();
    // (no 128-bit division: it is a slow library call on this target, and this runs on every clock read)
    (c / hz) * 1_000_000_000 + (c % hz) * 1_000_000_000 / hz
}

pub fn now() -> u64 {
    let t: u64;
    // SAFETY: reads the `time` CSR (user-readable counter).
    unsafe { asm!("csrr {}, time", out(reg) t) };
    t
}

/// Write `stimecmp` (Sstc): the supervisor timer interrupt becomes pending when `time >= value` and stays pending until a larger value is
/// written. Needs `menvcfg.STCE`, which the firmware sets when the hart has Sstc; the kernel requires it (KERNEL-V1-SPEC 2.1).
fn write_stimecmp(value: u64) {
    // SAFETY: `stimecmp` (CSR 0x14d) only schedules this hart's own supervisor timer interrupt.
    unsafe { asm!("csrw 0x14d, {}", in(reg) value) };
}

/// Read `stimecmp` back (the boot probe).
fn read_stimecmp() -> u64 {
    let v: u64;
    // SAFETY: reads a CSR with no side effect.
    unsafe { asm!("csrr {}, 0x14d", out(reg) v) };
    v
}

/// Bring this hart's timer up: record the clock's zero (the first hart's first call), check that `stimecmp` works and leave the timer
/// off (no event). The kernel has no periodic tick (KERNEL-V1-SPEC 5.1): `program` arms the next event.
#[allow(clippy::panic)] // INVARIANT: boot-time profile check: a hart without Sstc cannot run this kernel (OD127)
pub fn start() {
    let _ = BOOT_COUNT.compare_exchange(0, now(), Ordering::SeqCst, Ordering::SeqCst);
    write_stimecmp(0x5a5a_5a5a_0000_0001);
    assert!(
        read_stimecmp() == 0x5a5a_5a5a_0000_0001,
        "this hart has no Sstc (`stimecmp` is not writable): the kernel requires it (OD127)"
    );
    write_stimecmp(u64::MAX);
}

/// Arm this hart's one-shot timer for `event` (nanoseconds on the `now_ns` clock; `None`: never). An event already past fires at once.
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: the event is a nanosecond time far below the range where scaling to counter cycles overflows (584 years)
pub fn program(event: Option<u64>) {
    let cycles = match event {
        None => u64::MAX,
        Some(ns) => {
            let hz = timebase();
            // (no 128-bit division, as in `now_ns`)
            BOOT_COUNT.load(Ordering::Relaxed).saturating_add(
                (ns / 1_000_000_000) * hz + ((ns % 1_000_000_000) * hz).div_ceil(1_000_000_000), // (rounded up: never early)
            )
        }
    };
    write_stimecmp(cycles);
}
