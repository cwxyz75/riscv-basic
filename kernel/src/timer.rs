//! The supervisor timer, through the SBI TIME extension (OpenSBI), at a 1 ms tick.
//! QEMU `virt`'s `time` counter runs at 10 MHz.

use core::arch::asm;
use core::sync::atomic::Ordering;

const EID_TIME: usize = 0x5449_4D45;
/// Counter cycles per tick: 10 MHz / 1000 = 1 ms.
pub const TICK_CYCLES: u64 = 10_000;

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

fn set(deadline: u64) {
    // SAFETY: `ecall` from S-mode into OpenSBI's TIME extension (a7 = extension,
    // a6 = function 0 = set_timer, a0 = deadline); it also clears a pending timer
    // interrupt. Standard SBI calling convention.
    unsafe {
        asm!(
            "ecall",
            in("a7") EID_TIME,
            in("a6") 0usize,
            inout("a0") deadline as usize => _,
            out("a1") _,
        );
    }
}

/// Program this hart's first tick (each hart has its own deadline and its own timer).
pub fn start() {
    let _ = BOOT_COUNT.compare_exchange(0, now(), Ordering::SeqCst, Ordering::SeqCst);
    let first = now() + TICK_CYCLES;
    crate::percpu::this()
        .deadline
        .store(first, Ordering::SeqCst);
    set(first);
}

/// Program the next tick (called from the timer interrupt). Deadlines advance by
/// exact multiples, so ticks do not drift; if we fell behind (the host stalled the
/// emulator) the next one is a full tick away rather than a burst.
pub fn next_tick() {
    let hart = crate::percpu::this();
    let mut next = hart.deadline.load(Ordering::SeqCst) + TICK_CYCLES;
    let t = now();
    if next <= t {
        next = t + TICK_CYCLES;
    }
    hart.deadline.store(next, Ordering::SeqCst);
    set(next);
}
