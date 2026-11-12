//! The supervisor timer, through the SBI TIME extension (OpenSBI), at a 1 ms tick.
//! QEMU `virt`'s `time` counter runs at 10 MHz.

use core::arch::asm;
use core::sync::atomic::Ordering;

const EID_TIME: usize = 0x5449_4D45;
/// Counter cycles per tick: 10 MHz / 1000 = 1 ms.
pub const TICK_CYCLES: u64 = 10_000;

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
