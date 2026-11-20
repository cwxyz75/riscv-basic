//! Floating-point and vector state of user threads (slice A, OD76; PL2): the real FP and vector units behind `kcore::fp`'s protocol.
//!
//! The kernel itself does no floating point or vector work and runs with `sstatus.FS` and `sstatus.VS` Off (INVARIANTS C7, FP3): a unit is
//! switched on only inside `HartUnit`'s and `VecHartUnit`'s save and load, and every trap from user mode switches both off
//! again after saving what the user left in them. A user thread starts with both off; its first FP or vector
//! instruction traps as an illegal instruction and `first_use` enables the right unit (told apart by the instruction `stval` holds, or tried in
//! turn when it does not). All of this runs under the big kernel lock.

use crate::trap::TrapFrame;
use alloc::collections::BTreeMap;
use core::cell::UnsafeCell;
use kcore::fp::{FpHart, FpRegs, FpThread, Fs, StateUnit, Tokens, VecRegs, VecThread};

/// `sstatus.FS` (bits 14:13): Off 0, Initial 1, Clean 2, Dirty 3.
const FS_MASK: u64 = 3 << 13;
const FS_CLEAN: u64 = 2 << 13;
const FS_DIRTY: u64 = 3 << 13;
/// `sstatus.VS` (bits 10:9): the same four values.
const VS_MASK: u64 = 3 << 9;
const VS_CLEAN: u64 = 2 << 9;
const VS_DIRTY: u64 = 3 << 9;

/// The length of a vector register in bytes, read from `vlenb` on the boot hart (every other hart must agree: `check_hart`).
static VLENB: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

fn vlenb() -> usize {
    VLENB.load(core::sync::atomic::Ordering::Relaxed)
}

/// Read `vlenb` on the boot hart and keep it (boot, once). Panics for a vector length that is zero or not a power of two.
pub fn init_boot_hart() {
    let n = crate::arch::vlenb_read();
    assert!(
        n.is_power_of_two() && n <= 4096,
        "vlenb {n} is not a vector register length"
    );
    VLENB.store(n, core::sync::atomic::Ordering::Relaxed);
}

/// A secondary hart: its vector registers must be as long as the boot hart's, or the saved state of a thread would not fit.
pub fn check_hart() {
    let n = crate::arch::vlenb_read();
    assert!(
        n == vlenb(),
        "harts disagree on the vector register length: vlenb {n} here, {} on the boot hart",
        vlenb()
    );
}

struct FpState {
    tokens: Tokens,
    threads: BTreeMap<u32, FpThread>,
    harts: [FpHart; crate::percpu::MAX_HARTS],
    /// The vector unit's twin of the three fields above.
    vtokens: Tokens,
    vthreads: BTreeMap<u32, VecThread>,
    vharts: [FpHart; crate::percpu::MAX_HARTS],
}

struct Cell(UnsafeCell<Option<FpState>>);
// SAFETY: every access holds the big kernel lock (asserted in `state`).
unsafe impl Sync for Cell {}
static STATE: Cell = Cell(UnsafeCell::new(None));

/// Build the FP bookkeeping once into `slot` (its per-hart array is MAX_HARTS entries: out of line, so `state` has a small frame).
#[inline(never)]
#[allow(clippy::large_stack_frames)] // KT13: audited, once, at the first use of the FP state
fn init_state(slot: &mut Option<FpState>) {
    *slot = Some(FpState {
        tokens: Tokens::new(),
        threads: BTreeMap::new(),
        harts: [const { FpHart::new() }; crate::percpu::MAX_HARTS],
        vtokens: Tokens::new(),
        vthreads: BTreeMap::new(),
        vharts: [const { FpHart::new() }; crate::percpu::MAX_HARTS],
    });
}

fn state() -> &'static mut FpState {
    debug_assert!(
        crate::bkl::held(),
        "FP state touched without the big kernel lock"
    );
    // SAFETY: the big kernel lock is held, and no caller keeps the reference across a context switch.
    unsafe {
        let cell = &mut *STATE.0.get();
        if cell.is_none() {
            init_state(cell);
        }
        match cell {
            Some(s) => s,
            None => unreachable!(),
        }
    }
}

/// This hart's FP registers.
struct HartUnit;

impl StateUnit for HartUnit {
    type Regs = FpRegs;
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
        // SAFETY: as `save`, reading instead of writing. The FP registers are deliberately *not*
        // declared clobbered: the kernel executes no FP instruction of its own (it runs with the unit
        // off, INVARIANTS FP3), so the compiler holds nothing in them; declaring them would make it
        // save and restore the callee-saved ones with FP instructions in this function's prologue,
        // which trap with the unit off (found the hard way).
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

/// This hart's vector registers (`vlenb` bytes each). The kernel is compiled without V, so the instructions are assembled with the extension on for
/// these blocks only.
struct VecHartUnit;

impl StateUnit for VecHartUnit {
    type Regs = VecRegs;

    fn save(&mut self) -> VecRegs {
        let n = vlenb();
        let mut r = VecRegs::zero(n);
        let mut csrs = [0u64; 4]; // vstart, vcsr, vl, vtype
        let stride = n.saturating_mul(8);
        // SAFETY: the unit is switched on for exactly these instructions. `r.v` is 32 * vlenb bytes and each `vs8r.v` stores eight registers
        // (8 * vlenb bytes) at the pointer, which steps by that stride; `csrs` is four u64s. The registers are deliberately not declared
        // clobbered (the kernel holds nothing in them, as for FP).
        unsafe {
            core::arch::asm!(
                "csrs sstatus, {on}",
                ".option push",
                ".option arch, +v",
                "vs8r.v v0, ({p})",
                "add {p}, {p}, {s}",
                "vs8r.v v8, ({p})",
                "add {p}, {p}, {s}",
                "vs8r.v v16, ({p})",
                "add {p}, {p}, {s}",
                "vs8r.v v24, ({p})",
                "csrr {t}, vstart",
                "sd {t}, 0({c})",
                "csrr {t}, vcsr",
                "sd {t}, 8({c})",
                "csrr {t}, vl",
                "sd {t}, 16({c})",
                "csrr {t}, vtype",
                "sd {t}, 24({c})",
                ".option pop",
                "csrc sstatus, {off}",
                p = inout(reg) r.v.as_mut_ptr() => _,
                s = in(reg) stride,
                c = in(reg) csrs.as_mut_ptr(),
                t = out(reg) _,
                on = in(reg) 1u64 << 9,
                off = in(reg) VS_MASK,
                options(nostack),
            );
        }
        [r.vstart, r.vcsr, r.vl, r.vtype] = csrs;
        r
    }

    fn load(&mut self, r: &VecRegs) {
        let n = vlenb();
        let stride = n.saturating_mul(8);
        // a saved `vill` (the user's last vsetvl was illegal) is restored by an illegal vtype again: a reserved bit set
        let vtype = if r.vtype >> 63 != 0 { 1 << 8 } else { r.vtype };
        // SAFETY: as `save`, reading instead of writing. `vstart` is cleared first (a whole-register load skips elements below it), the length
        // and type are restored by `vsetvl` (it keeps the saved `vl`: that was already at most the maximum for this type), the registers by
        // `vl8re8.v` and the user's `vcsr` and `vstart` last.
        unsafe {
            core::arch::asm!(
                "csrs sstatus, {on}",
                ".option push",
                ".option arch, +v",
                "csrw vstart, zero",
                "vsetvl zero, {vl}, {vt}",
                "vl8re8.v v0, ({p})",
                "add {p}, {p}, {s}",
                "vl8re8.v v8, ({p})",
                "add {p}, {p}, {s}",
                "vl8re8.v v16, ({p})",
                "add {p}, {p}, {s}",
                "vl8re8.v v24, ({p})",
                "csrw vcsr, {cs}",
                "csrw vstart, {st}",
                ".option pop",
                "csrc sstatus, {off}",
                p = inout(reg) r.v.as_ptr() => _,
                s = in(reg) stride,
                vl = in(reg) r.vl,
                vt = in(reg) vtype,
                cs = in(reg) r.vcsr,
                st = in(reg) r.vstart,
                on = in(reg) 1u64 << 9,
                off = in(reg) VS_MASK,
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
    let vdirty = frame.sstatus & VS_MASK == VS_DIRTY;
    // (the negative-control kernel forgets to save: the test that guards the save must then fail)
    if vdirty && !cfg!(feature = "fault-skip-fp-save") {
        let s = state();
        let tid = crate::thread::current();
        let FpState {
            vtokens,
            vthreads,
            vharts,
            ..
        } = s;
        let ok = match vthreads.get_mut(&tid) {
            Some(t) => vharts[hart()].trap_entry(t, true, vtokens, &mut VecHartUnit),
            None => false,
        };
        assert!(
            ok,
            "the vector unit was dirty for a thread that never enabled it"
        );
        frame.sstatus = (frame.sstatus & !VS_MASK) | VS_CLEAN;
    }
    if dirty && !cfg!(feature = "fault-skip-fp-save") {
        let s = state();
        let tid = crate::thread::current();
        let FpState {
            tokens,
            threads,
            harts,
            ..
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
    // SAFETY: clears sstatus.FS and sstatus.VS only: the kernel runs with the FP and vector units off.
    unsafe { core::arch::asm!("csrc sstatus, {}", in(reg) FS_MASK | VS_MASK) };
}

/// About to return to user mode: put the thread's FP and vector state in the units (if it has any) and set the
/// returned `sstatus.FS` and `sstatus.VS` accordingly.
pub fn return_to_user(frame: &mut TrapFrame) {
    if frame.sstatus & (1 << 8) != 0 {
        return;
    }
    // A thread with no FP or vector state has `FS` and `VS` Off in its frame (a thread that has state always returns
    // with Clean or Dirty, and `first_use` sets Initial), so the common case takes no lock at all:
    // taking the big lock on every return to user mode made syscalls with many live processes at four
    // harts several times slower (found by `syscalls_stay_fast_with_many_live_processes` in CI).
    if frame.sstatus & (FS_MASK | VS_MASK) == 0 {
        return;
    }
    crate::bkl::lock();
    let s = state();
    let tid = crate::thread::current();
    let fs = if frame.sstatus & FS_MASK == 0 {
        None
    } else {
        Some(match s.threads.get(&tid) {
            Some(t) => {
                let fs = s.harts[hart()].return_to_user(t, &mut HartUnit);
                // dirty state still in the unit (a trap that skipped `trap_entry`) is the thread's own
                debug_assert!(
                    frame.sstatus & FS_MASK != FS_DIRTY || s.harts[hart()].holds(t),
                    "returning to a thread with unsaved dirty FP state that is not in this hart's unit"
                );
                fs
            }
            None => Fs::Off,
        })
    };
    let vs = if frame.sstatus & VS_MASK == 0 {
        None
    } else {
        Some(match s.vthreads.get(&tid) {
            Some(t) => {
                let vs = s.vharts[hart()].return_to_user(t, &mut VecHartUnit);
                debug_assert!(
                    frame.sstatus & VS_MASK != VS_DIRTY || s.vharts[hart()].holds(t),
                    "returning to a thread with unsaved dirty vector state that is not in this hart's unit"
                );
                vs
            }
            None => Fs::Off,
        })
    };
    crate::bkl::unlock();
    match fs {
        None => {}
        Some(Fs::Off) => frame.sstatus &= !FS_MASK,
        // (a trap that did not go through `trap_entry`, a light IPI, leaves dirty state in the unit:
        // it stays dirty so the next trap saves it)
        Some(Fs::Clean) if frame.sstatus & FS_MASK != FS_DIRTY => {
            frame.sstatus = (frame.sstatus & !FS_MASK) | FS_CLEAN
        }
        Some(Fs::Clean) => {}
    }
    match vs {
        None => {}
        Some(Fs::Off) => frame.sstatus &= !VS_MASK,
        Some(Fs::Clean) if frame.sstatus & VS_MASK != VS_DIRTY => {
            frame.sstatus = (frame.sstatus & !VS_MASK) | VS_CLEAN
        }
        Some(Fs::Clean) => {}
    }
}

/// An illegal instruction from user mode: this may be the first use of the FP unit or of the vector unit by this thread. The instruction the
/// trap reports (`stval`) says which; when it says nothing (0) the units are tried in turn. The unit is enabled, from zeros, and
/// true is returned (the instruction is then retried; if it traps again its unit is on and the trap is a real illegal instruction).
pub fn first_use(frame: &mut TrapFrame) -> bool {
    if kcore::fp::is_vector_insn(frame.stval) {
        return vec_first_use(frame);
    }
    fp_first_use(frame) || (frame.stval == 0 && vec_first_use(frame))
}

fn fp_first_use(frame: &mut TrapFrame) -> bool {
    if frame.sstatus & FS_MASK != 0 {
        return false;
    }
    let s = state();
    let tid = crate::thread::current();
    let FpState {
        tokens, threads, ..
    } = s;
    let enabled = threads.entry(tid).or_default().enable(tokens);
    if enabled {
        // not Off any more: the return path then loads its (zero) state and sets Clean
        frame.sstatus |= 1 << 13;
    }
    enabled
}

fn vec_first_use(frame: &mut TrapFrame) -> bool {
    if frame.sstatus & VS_MASK != 0 {
        return false;
    }
    let s = state();
    let tid = crate::thread::current();
    let FpState {
        vtokens, vthreads, ..
    } = s;
    let enabled = vthreads
        .entry(tid)
        .or_default()
        .enable_with(vtokens, VecRegs::zero(vlenb()));
    if enabled {
        frame.sstatus |= 1 << 9; // Initial: the return path loads the zero state and sets Clean
    }
    enabled
}

/// A thread has ended: forget its FP and vector state.
pub fn thread_gone(tid: u32) {
    let s = state();
    s.threads.remove(&tid);
    s.vthreads.remove(&tid);
}

/// How many threads hold FP or vector state (the end-of-suite leak check: none may survive their process).
pub fn entry_count() -> usize {
    crate::irq::without(|| {
        crate::bkl::lock();
        let s = state();
        let n = s.threads.len() + s.vthreads.len();
        crate::bkl::unlock();
        n
    })
}
