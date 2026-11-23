//! The supervisor timer: Sstc `stimecmp`, one-shot, no periodic tick (L13). The `time` counter's rate comes from the device tree (`set_frequency`).

use core::arch::asm;
use core::sync::atomic::Ordering;

/// The `time` counter's frequency in Hz and the fixed-point multiplier derived from it (`kcore::timebase`), set once at boot from the device
/// tree's `timebase-frequency` (`set_frequency`, PL4b). Both are 0 until then: the clock reads 0 and nothing is armed.
static HZ: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static MULT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Record the counter's frequency (once, on the boot hart, before the clock is used). `false` for a rate the kernel cannot convert.
pub fn set_frequency(hz: u64) -> bool {
    let Some(tb) = kcore::timebase::Timebase::new(hz) else {
        return false;
    };
    MULT.store(tb.mult(), Ordering::Relaxed);
    HZ.store(tb.hz(), Ordering::Relaxed);
    // the clock's zero is here, once, on the boot hart, before any reader: the clock never steps back when the first tick is armed
    let _ = BOOT_COUNT.compare_exchange(0, now(), Ordering::SeqCst, Ordering::SeqCst);
    true
}

/// The counter's frequency in Hz (0 before `set_frequency`).
pub fn timebase() -> u64 {
    HZ.load(Ordering::Relaxed)
}

fn conversion() -> kcore::timebase::Timebase {
    kcore::timebase::Timebase::from_parts(HZ.load(Ordering::Relaxed), MULT.load(Ordering::Relaxed))
}

/// Counter cycles in `ns` nanoseconds, rounded up (a timer armed for the result never fires early).
pub fn cycles_from_ns(ns: u64) -> u64 {
    conversion().ns_to_cycles(ns)
}

/// The counter value when the first tick was programmed (the clock's zero); 0 until then.
static BOOT_COUNT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Nanoseconds since boot, from the `time` counter (the clock user mode sees: `clock_get`, every deadline): one multiplication and one shift.
pub fn now_ns() -> u64 {
    let c = now().saturating_sub(BOOT_COUNT.load(Ordering::Relaxed));
    conversion().cycles_to_ns(c)
}

/// What user mode needs to compute the clock itself (the system library's data page, AB7): the multiplier and shift of `cycles_to_ns`, and the counter's
/// value at the clock's zero.
pub fn clock_params() -> (u64, u64, u64) {
    (
        MULT.load(Ordering::Relaxed),
        u64::from(kcore::timebase::SHIFT),
        BOOT_COUNT.load(Ordering::Relaxed),
    )
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
pub fn program(event: Option<u64>) {
    debug_assert!(
        MULT.load(Ordering::Relaxed) != 0,
        "the timer is armed before the timebase is set"
    );
    let cycles = match event {
        None => u64::MAX,
        // (rounded up: never early)
        Some(ns) => BOOT_COUNT
            .load(Ordering::Relaxed)
            .saturating_add(cycles_from_ns(ns)),
    };
    write_stimecmp(cycles);
}
