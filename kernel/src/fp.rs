//! Floating-point state of user threads (slice A, OD76): the real FP unit behind `kcore::fp`'s protocol.
//!
//! The kernel itself does no floating point and runs with `sstatus.FS` Off (INVARIANTS C7): the unit is
//! switched on only inside `HartUnit`'s save and load, and every trap from user mode switches it off
//! again after saving what the user left in it. A user thread starts with the unit off; its first FP
//! instruction traps as an illegal instruction and `first_use` enables it. All of this runs under the
//! big kernel lock.

use crate::trap::TrapFrame;
use alloc::collections::BTreeMap;
use core::cell::UnsafeCell;
use kcore::fp::{FpHart, FpRegs, FpThread, FpUnit, Fs, Tokens};

/// `sstatus.FS` (bits 14:13): Off 0, Initial 1, Clean 2, Dirty 3.
const FS_MASK: u64 = 3 << 13;
const FS_CLEAN: u64 = 2 << 13;
const FS_DIRTY: u64 = 3 << 13;

struct FpState {
    tokens: Tokens,
    threads: BTreeMap<u32, FpThread>,
    harts: [FpHart; crate::percpu::MAX_HARTS],
}

struct Cell(UnsafeCell<Option<FpState>>);
// SAFETY: every access holds the big kernel lock (asserted in `state`).
unsafe impl Sync for Cell {}
static STATE: Cell = Cell(UnsafeCell::new(None));

fn state() -> &'static mut FpState {
    debug_assert!(
        crate::bkl::held(),
        "FP state touched without the big kernel lock"
    );
    // SAFETY: the big kernel lock is held, and no caller keeps the reference across a context switch.
    unsafe {
        (*STATE.0.get()).get_or_insert_with(|| FpState {
            tokens: Tokens::new(),
            threads: BTreeMap::new(),
            harts: [const { FpHart::new() }; crate::percpu::MAX_HARTS],
        })
    }
}

/// This hart's FP registers.
struct HartUnit;

impl FpUnit for HartUnit {
    fn save(&mut self) -> FpRegs {
        let mut r = FpRegs::ZERO;
        let (f, fcsr) = (r.f.as_mut_ptr(), &mut r.fcsr as *mut u32);
        // SAFETY: the unit is switched on for exactly these instructions; `f` points at 32 u64s.
        unsafe {
            core::arch::asm!(
                "csrs sstatus, {on}",
                "fsd f0, 0({p})", "fsd f1, 8({p})", "fsd f2, 16({p})", "fsd f3, 24({p})",
                "fsd f4, 32({p})", "fsd f5, 40({p})", "fsd f6, 48({p})", "fsd f7, 56({p})",
                "fsd f8, 64({p})", "fsd f9, 72({p})", "fsd f10, 80({p})", "fsd f11, 88({p})",
                "fsd f12, 96({p})", "fsd f13, 104({p})", "fsd f14, 112({p})", "fsd f15, 120({p})",
                "fsd f16, 128({p})", "fsd f17, 136({p})", "fsd f18, 144({p})", "fsd f19, 152({p})",
                "fsd f20, 160({p})", "fsd f21, 168({p})", "fsd f22, 176({p})", "fsd f23, 184({p})",
                "fsd f24, 192({p})", "fsd f25, 200({p})", "fsd f26, 208({p})", "fsd f27, 216({p})",
                "fsd f28, 224({p})", "fsd f29, 232({p})", "fsd f30, 240({p})", "fsd f31, 248({p})",
                "frcsr {t}",
                "csrc sstatus, {off}",
                "sw {t}, 0({c})",
                p = in(reg) f,
                c = in(reg) fcsr,
                t = out(reg) _,
                on = in(reg) 1u64 << 13,
                off = in(reg) FS_MASK,
                options(nostack),
            );
        }
        r
    }

    fn load(&mut self, r: &FpRegs) {
        let (f, fcsr) = (r.f.as_ptr(), u64::from(r.fcsr));
        // SAFETY: as `save`, reading instead of writing.
        unsafe {
            core::arch::asm!(
                "csrs sstatus, {on}",
                "fld f0, 0({p})", "fld f1, 8({p})", "fld f2, 16({p})", "fld f3, 24({p})",
                "fld f4, 32({p})", "fld f5, 40({p})", "fld f6, 48({p})", "fld f7, 56({p})",
                "fld f8, 64({p})", "fld f9, 72({p})", "fld f10, 80({p})", "fld f11, 88({p})",
                "fld f12, 96({p})", "fld f13, 104({p})", "fld f14, 112({p})", "fld f15, 120({p})",
                "fld f16, 128({p})", "fld f17, 136({p})", "fld f18, 144({p})", "fld f19, 152({p})",
                "fld f20, 160({p})", "fld f21, 168({p})", "fld f22, 176({p})", "fld f23, 184({p})",
                "fld f24, 192({p})", "fld f25, 200({p})", "fld f26, 208({p})", "fld f27, 216({p})",
                "fld f28, 224({p})", "fld f29, 232({p})", "fld f30, 240({p})", "fld f31, 248({p})",
                "fscsr {c}",
                "csrc sstatus, {off}",
                p = in(reg) f,
                c = in(reg) fcsr,
                on = in(reg) 1u64 << 13,
                off = in(reg) FS_MASK,
                options(nostack),
            );
        }
    }
}

fn hart() -> usize {
    crate::percpu::this()
        .index
        .load(core::sync::atomic::Ordering::Relaxed)
}

/// A trap from user mode, before anything else can use this hart's FP registers: save what the user
/// dirtied, and switch the unit off for the kernel. (Idempotent for one trap: the frame says Clean
/// afterwards.)
pub fn trap_entry(frame: &mut TrapFrame) {
    if frame.sstatus & (1 << 8) != 0 {
        return; // from the kernel
    }
    let dirty = frame.sstatus & FS_MASK == FS_DIRTY;
    if dirty {
        let s = state();
        let tid = crate::thread::current();
        let FpState {
            tokens,
            threads,
            harts,
        } = s;
        let ok = match threads.get_mut(&tid) {
            Some(t) => harts[hart()].trap_entry(t, true, tokens, &mut HartUnit),
            None => false,
        };
        assert!(
            ok,
            "the FP unit was dirty for a thread that never enabled it"
        );
        frame.sstatus = (frame.sstatus & !FS_MASK) | FS_CLEAN;
    }
    // SAFETY: clears sstatus.FS only: the kernel runs with the FP unit off.
    unsafe { core::arch::asm!("csrc sstatus, {}", in(reg) FS_MASK) };
}

/// About to return to user mode: put the thread's FP state in the unit (if it has any) and set the
/// returned `sstatus.FS` accordingly.
pub fn return_to_user(frame: &mut TrapFrame) {
    if frame.sstatus & (1 << 8) != 0 {
        return;
    }
    crate::bkl::lock();
    let s = state();
    let tid = crate::thread::current();
    let fs = match s.threads.get(&tid) {
        Some(t) => s.harts[hart()].return_to_user(t, &mut HartUnit),
        None => Fs::Off,
    };
    crate::bkl::unlock();
    match fs {
        Fs::Off => frame.sstatus &= !FS_MASK,
        // (a trap that did not go through `trap_entry`, a light IPI, leaves dirty state in the unit:
        // it stays dirty so the next trap saves it)
        Fs::Clean if frame.sstatus & FS_MASK != FS_DIRTY => {
            frame.sstatus = (frame.sstatus & !FS_MASK) | FS_CLEAN
        }
        Fs::Clean => {}
    }
}

/// An illegal instruction from user mode: if the unit is off for a thread that has not used it, this
/// may be its first FP instruction. Enable it and say so (the instruction is then retried; if it traps
/// again the unit is on and the trap is a real illegal instruction).
pub fn first_use(frame: &TrapFrame) -> bool {
    if frame.sstatus & FS_MASK != 0 {
        return false;
    }
    let s = state();
    let tid = crate::thread::current();
    let FpState {
        tokens, threads, ..
    } = s;
    threads.entry(tid).or_default().enable(tokens)
}

/// A thread has ended: forget its FP state.
pub fn thread_gone(tid: u32) {
    state().threads.remove(&tid);
}
