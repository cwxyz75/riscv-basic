//! Per-hart state (S5, spec section 3). `tp` holds `&PerHart` in kernel code and is never
//! restored from a trap frame on a return to the kernel; the user's `tp` lives in the trap frame,
//! and `sscratch` holds `&PerHart` while a hart is in user mode (0 in the kernel). A `PerHart` is
//! read only with interrupts off (or by its own hart in the trap path), so a thread that
//! migrates between harts never sees another hart's record through a stale pointer.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// Most harts the kernel manages (QEMU `-smp` up to this).
pub const MAX_HARTS: usize = 8;

/// Why a hart was sent an IPI (bits of `PerHart::pending`).
pub const IPI_RESCHED: u32 = 1;
/// Run the hart's `work_kind` job in its IPI handler (tests of the lock).
pub const IPI_WORK: u32 = 2;

/// The layout of the first two fields is used by the trap assembly (`trap.rs`, `user.rs`).
#[repr(C)]
pub struct PerHart {
    /// Offset 0: the top of the kernel stack the next trap from user mode will use; written on
    /// every return to user mode.
    pub kstack_top: AtomicU64,
    /// Offset 8: scratch for the user's `sp` while the trap entry switches stacks.
    pub scratch_sp: AtomicU64,
    /// Dense index of this hart (0 is the boot hart).
    pub index: AtomicUsize,
    /// The SBI hart id.
    pub hart_id: AtomicUsize,
    /// Set (release) once the hart is running kernel code.
    pub online: AtomicBool,
    /// Timer interrupts this hart has taken.
    pub timer_ticks: AtomicU64,
    /// Software interrupts (IPIs) this hart has taken.
    pub ipis: AtomicU64,
    /// Is the hart in (or entering) user mode right now?
    pub in_user: AtomicBool,
    /// Set when this hart is part of the scheduler (its idle thread exists).
    pub in_sched: AtomicBool,
    /// Reschedule IPIs this hart has handled.
    pub resched_ipis: AtomicU64,
    /// Pending IPI reasons.
    pub pending: AtomicU32,
    /// The next timer deadline of this hart (`time` counter value).
    pub deadline: AtomicU64,
    /// The value of `tp` this hart saw when it bound itself (must equal this record's address).
    pub tp_seen: AtomicUsize,
    /// The nesting depth of the big kernel lock held on this hart (the lock follows the thread:
    /// `bkl::swap_depth`).
    pub bkl_depth: AtomicU32,
    /// The lock classes this hart holds (bit per `bkl::Class`), for the lock-order checker.
    pub locks_held: AtomicU32,
    /// Work for the hart's IPI handler (S5b tests): kind, argument, result and a done flag.
    pub work_kind: AtomicU32,
    pub work_arg: AtomicU64,
    pub work_result: AtomicU64,
    pub work_done: AtomicBool,
    /// The `satp` last written for user mode on this hart (so a switch is made only when needed).
    pub active_satp: AtomicU64,
}

impl PerHart {
    const fn new() -> PerHart {
        PerHart {
            kstack_top: AtomicU64::new(0),
            scratch_sp: AtomicU64::new(0),
            index: AtomicUsize::new(0),
            hart_id: AtomicUsize::new(0),
            online: AtomicBool::new(false),
            timer_ticks: AtomicU64::new(0),
            ipis: AtomicU64::new(0),
            in_user: AtomicBool::new(false),
            in_sched: AtomicBool::new(false),
            resched_ipis: AtomicU64::new(0),
            pending: AtomicU32::new(0),
            deadline: AtomicU64::new(0),
            tp_seen: AtomicUsize::new(0),
            bkl_depth: AtomicU32::new(0),
            locks_held: AtomicU32::new(0),
            work_kind: AtomicU32::new(0),
            work_arg: AtomicU64::new(0),
            work_result: AtomicU64::new(0),
            work_done: AtomicBool::new(false),
            active_satp: AtomicU64::new(0),
        }
    }
}

static HARTS: [PerHart; MAX_HARTS] = [const { PerHart::new() }; MAX_HARTS];

/// The record of hart `index`.
pub fn hart(index: usize) -> &'static PerHart {
    &HARTS[index]
}

/// The calling hart's record, through `tp`.
pub fn this() -> &'static PerHart {
    let p: usize;
    // SAFETY: reads `tp`, which holds `&PerHart` in kernel code (`bind`); no memory is touched.
    unsafe { core::arch::asm!("mv {}, tp", out(reg) p, options(nomem, nostack, preserves_flags)) };
    debug_assert!(
        HARTS.as_ptr_range().contains(&(p as *const PerHart)),
        "tp does not point at a PerHart"
    );
    // SAFETY: `tp` was set by `bind` to one of the `HARTS` entries, which live for ever.
    unsafe { &*(p as *const PerHart) }
}

/// Make the calling hart's `tp` point at its record and fill the record in. Called first thing on
/// every hart, before it can take a trap.
pub fn bind(index: usize, hart_id: usize) {
    let h = &HARTS[index];
    h.index.store(index, Ordering::Relaxed);
    h.hart_id.store(hart_id, Ordering::Relaxed);
    // SAFETY: sets `tp`, which kernel code reserves for this purpose (LLVM never allocates it).
    unsafe {
        core::arch::asm!("mv tp, {}", in(reg) h as *const PerHart as usize, options(nomem, nostack, preserves_flags))
    };
}
