//! Floating-point and vector state of user threads (slice A, OD76; PL2): the real FP and vector units behind `kcore::fp`'s protocol.
//!
//! The kernel itself does no floating point or vector work and runs with `sstatus.FS` and `sstatus.VS` Off (INVARIANTS C7, FP3): a unit is
//! switched on only inside `HartUnit`'s and `VecHartUnit`'s save and load, and every trap from user mode switches both off
//! again after saving what the user left in them. A user thread starts with both off; its first FP or vector
//! instruction traps as an illegal instruction and `first_use` enables the right unit (told apart by the instruction `stval` holds, or tried in
//! turn when it does not). The saves and loads run under the big kernel lock, except that a thread whose state is already in this hart's registers
//! (the hart's *claim*, T166) returns to user mode with no lock and `light_save` saves dirty state on the lock-free trap paths.

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
        n.is_power_of_two() && n <= 8192,
        "vlenb {n} is not a vector register length"
    );
    VLENB.store(n, core::sync::atomic::Ordering::Relaxed);
}

/// Enable `cbo.zero`, `cbo.clean`, `cbo.flush` and `cbo.inval` for user mode on this hart; the boot stops if a bit does not stick (PL3).
pub fn enable_cache_block_ops_or_stop() {
    assert!(
        crate::arch::enable_cache_block_ops(),
        "senvcfg: the cache-block bits (CBZE, CBCFE, CBIE) did not stick: M-mode must enable them (menvcfg) and the hart needs Zicbom and Zicboz"
    );
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
    threads: BTreeMap<u64, FpThread>,
    harts: [FpHart; crate::percpu::MAX_HARTS],
    /// The vector unit's twin of the three fields above.
    vtokens: Tokens,
    vthreads: BTreeMap<u64, VecThread>,
    vharts: [FpHart; crate::percpu::MAX_HARTS],
    /// What each user thread's FP and vector state was charged to its job's kernel bytes (MM5): credited when the thread ends.
    fp_charge: BTreeMap<u64, (kcore::job::JobId, u64)>,
    vec_charge: BTreeMap<u64, (kcore::job::JobId, u64)>,
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
        tokens: Tokens::starting_at(crate::dtb::ff_value("fptoken").unwrap_or(0)),
        threads: BTreeMap::new(),
        harts: [const { FpHart::new() }; crate::percpu::MAX_HARTS],
        vtokens: Tokens::starting_at(crate::dtb::ff_value("fptoken").unwrap_or(0)),
        vthreads: BTreeMap::new(),
        vharts: [const { FpHart::new() }; crate::percpu::MAX_HARTS],
        fp_charge: BTreeMap::new(),
        vec_charge: BTreeMap::new(),
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
        self.save_into(&mut r);
        r
    }

    /// In place: the trap paths save into the thread's own copy and build no 264-byte temporary (the kernel's stack budget, KT13).
    fn save_into(&mut self, r: &mut FpRegs) {
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
        let mut r = VecRegs::zero(vlenb());
        self.save_into(&mut r);
        r
    }

    /// Save into the thread's existing buffer: nothing is allocated in the trap path.
    fn save_into(&mut self, r: &mut VecRegs) {
        let n = vlenb();
        if r.v.len() != n.saturating_mul(32) {
            return; // (a buffer of another length would be written out of bounds: never made by `first_use`)
        }
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
                // `vstart` first, then cleared: the whole-register stores skip the elements below it
                "csrr {vs}, vstart",
                "sd {vs}, 0({c})",
                "csrw vstart, zero",
                "vs8r.v v0, ({p})",
                "add {p}, {p}, {s}",
                "vs8r.v v8, ({p})",
                "add {p}, {p}, {s}",
                "vs8r.v v16, ({p})",
                "add {p}, {p}, {s}",
                "vs8r.v v24, ({p})",
                "csrr {t}, vcsr",
                "sd {t}, 8({c})",
                "csrr {t}, vl",
                "sd {t}, 16({c})",
                "csrr {t}, vtype",
                "sd {t}, 24({c})",
                // the unit goes on holding the thread's state (the hart keeps its token): `vstart` as it was
                "csrw vstart, {vs}",
                ".option pop",
                "csrc sstatus, {off}",
                p = inout(reg) r.v.as_mut_ptr() => _,
                s = in(reg) stride,
                c = in(reg) csrs.as_mut_ptr(),
                t = out(reg) _,
                vs = out(reg) _,
                on = in(reg) 1u64 << 9,
                off = in(reg) VS_MASK,
                options(nostack),
            );
        }
        [r.vstart, r.vcsr, r.vl, r.vtype] = csrs;
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

/// This hart's register file now holds `tid`'s latest FP (`fp`) or vector state: record the claim, and drop the one the thread's previous holder
/// hart had (that copy is stale now). Under the big lock.
fn claim<R>(t: &mut kcore::fp::UnitThread<R>, harts: &mut [FpHart], tid: u64, h: usize, fp: bool) {
    use core::sync::atomic::Ordering::Release;
    let cells = |hart: usize| {
        let p = crate::percpu::hart(hart);
        if fp {
            (&p.fs_claim, &p.fs_buf)
        } else {
            (&p.vs_claim, &p.vs_buf)
        }
    };
    if let Some(old) = t.holder() {
        if old != h {
            let (c, b) = cells(old);
            // (only if the old hart still holds this thread: another thread's load there has replaced the claim already)
            if c.load(core::sync::atomic::Ordering::Acquire) == tid {
                c.store(0, Release);
                b.store(0, Release);
                // (a save that takes no lock keeps the token, so the old hart's token must stop matching)
                if let Some(f) = harts.get_mut(old) {
                    f.forget();
                }
            }
        }
    }
    t.set_holder(Some(h));
    let (c, b) = cells(h);
    b.store(t.regs_ptr().map_or(0, |p| p as usize), Release);
    c.store(tid, Release);
}

/// Save what the user left dirty in the FP and vector units without the big lock, when this hart's claim names the running thread (so its
/// registers are in the unit and its saved copy is at a known address): the trap paths that take no lock (L12b, L12c) use it so that a thread
/// whose compiled code touches vector registers (the whole user build is RVA23U64) still gets the quiet path. Returns false if state is
/// dirty that it could not save: the caller takes the full path, whose `trap_entry` saves under the lock.
pub fn light_save(frame: &mut TrapFrame) -> bool {
    use core::sync::atomic::Ordering::Acquire;
    if frame.sstatus & (1 << 8) != 0 {
        return true;
    }
    let (fsb, vsb) = (frame.sstatus & FS_MASK, frame.sstatus & VS_MASK);
    if fsb != FS_DIRTY && vsb != VS_DIRTY {
        return true;
    }
    if cfg!(feature = "fault-skip-fp-save") {
        return false;
    }
    let tid = crate::thread::current();
    let me = crate::percpu::this();
    if vsb == VS_DIRTY {
        let buf = me.vs_buf.load(Acquire);
        if me.vs_claim.load(Acquire) != tid || buf == 0 {
            return false;
        }
        // SAFETY: the claim names the thread running here, so its registers are in this hart's unit and nothing else touches its saved copy;
        // `buf` is the address of that copy's box, which lives until `thread_gone` (which clears the claim first and runs only for a thread that
        // is not running).
        unsafe { VecHartUnit.save_into(&mut *(buf as *mut VecRegs)) };
        frame.sstatus = (frame.sstatus & !VS_MASK) | VS_CLEAN;
    }
    if fsb == FS_DIRTY {
        let buf = me.fs_buf.load(Acquire);
        if me.fs_claim.load(Acquire) != tid || buf == 0 {
            return false;
        }
        // SAFETY: as above, for the FP unit's copy.
        unsafe { HartUnit.save_into(&mut *(buf as *mut FpRegs)) };
        frame.sstatus = (frame.sstatus & !FS_MASK) | FS_CLEAN;
    }
    true
}

/// A trap from user mode, before anything else can use this hart's FP registers: save what the user
/// dirtied, and switch the unit off for the kernel. (Idempotent for one trap: the frame says Clean
/// afterwards.)
pub fn trap_entry(frame: &mut TrapFrame) {
    if frame.sstatus & (1 << 8) != 0 {
        return; // from the kernel
    }
    // The cheap way first (no table lookup, no new token): the hart's claim names the running thread, so its dirty state is saved in place.
    // Only what that cannot do (no claim yet: the first save of a thread, or the state came from another hart) takes the slow way below.
    if light_save(frame) {
        // SAFETY: clears sstatus.FS and sstatus.VS only: the kernel runs with the FP and vector units off.
        unsafe { core::arch::asm!("csrc sstatus, {}", in(reg) FS_MASK | VS_MASK) };
        return;
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
            Some(t) => {
                let ok = vharts[hart()].trap_entry(t, true, vtokens, &mut VecHartUnit);
                claim(t, vharts, tid, hart(), false);
                ok
            }
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
            Some(t) => {
                let ok = harts[hart()].trap_entry(t, true, tokens, &mut HartUnit);
                claim(t, harts, tid, hart(), true);
                ok
            }
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
    let (fsb, vsb) = (frame.sstatus & FS_MASK, frame.sstatus & VS_MASK);
    if fsb | vsb == 0 {
        return;
    }
    // A thread that has used a unit takes no lock either while this hart's registers still hold its latest state (T166; with the user build on
    // RVA23U64 nearly every thread has used the vector unit): the frame says Clean or Dirty (not Initial: no load is owed) and this hart's claim
    // names the thread. The claim is set where the state is saved or loaded and is cleared when another hart saves or loads the thread's state.
    let tid = crate::thread::current();
    let me = crate::percpu::this();
    let fs_ok = fsb == 0
        || (fsb >= FS_CLEAN && me.fs_claim.load(core::sync::atomic::Ordering::Acquire) == tid);
    let vs_ok = vsb == 0
        || (vsb >= VS_CLEAN && me.vs_claim.load(core::sync::atomic::Ordering::Acquire) == tid);
    if fs_ok && vs_ok {
        return;
    }
    crate::bkl::lock();
    let s = state();
    let fs = if fsb == 0 {
        None
    } else {
        Some(match s.threads.get_mut(&tid) {
            Some(t) => {
                let fs = s.harts[hart()].return_to_user(t, &mut HartUnit);
                claim(t, &mut s.harts, tid, hart(), true);
                // dirty state still in the unit (a trap that skipped `trap_entry`) is the thread's own
                debug_assert!(
                    fsb != FS_DIRTY || s.harts[hart()].holds(t),
                    "returning to a thread with unsaved dirty FP state that is not in this hart's unit"
                );
                fs
            }
            None => Fs::Off,
        })
    };
    let vs = if vsb == 0 {
        None
    } else {
        Some(match s.vthreads.get_mut(&tid) {
            Some(t) => {
                let vs = s.vharts[hart()].return_to_user(t, &mut VecHartUnit);
                claim(t, &mut s.vharts, tid, hart(), false);
                debug_assert!(
                    vsb != VS_DIRTY || s.vharts[hart()].holds(t),
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
        // (a vector-FP instruction such as `vfadd.vv` is a vector instruction that is also the thread's first FP use, with the vector unit already on)
        return vec_first_use(frame) || fp_first_use(frame);
    }
    fp_first_use(frame) || (frame.stval == 0 && vec_first_use(frame))
}

fn fp_first_use(frame: &mut TrapFrame) -> bool {
    if frame.sstatus & FS_MASK != 0 {
        return false;
    }
    let s = state();
    let tid = crate::thread::current();
    // (the state is the job's kernel memory: a job at its limit cannot give a thread more; the instruction then faults like any illegal one)
    if !charge_state(
        &mut s.fp_charge,
        tid,
        core::mem::size_of::<FpThread>() as u64,
    ) {
        return false;
    }
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
    let bytes = (core::mem::size_of::<VecThread>() + crate::arch::vlenb_read() * 32) as u64;
    if !charge_state(&mut s.vec_charge, tid, bytes) {
        return false;
    }
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

/// Charge `bytes` of register state to the job of user thread `tid`, once (a second call for a thread already charged is free); `false`
/// when the job is at its kernel-memory limit. Kernel threads are not charged.
fn charge_state(book: &mut BTreeMap<u64, (kcore::job::JobId, u64)>, tid: u64, bytes: u64) -> bool {
    if book.contains_key(&tid) {
        return true;
    }
    let Some(job) = crate::user::job_of_thread(tid) else {
        return true;
    };
    let j = crate::kmem::jobs();
    if j.jobs
        .charge(job, kcore::job::Resource::KernelBytes, bytes)
        .is_err()
    {
        return false;
    }
    j.jobs.retain(job);
    book.insert(tid, (job, bytes));
    true
}

/// Give back what `charge_state` charged for `tid` in `book`.
fn credit_state(book: &mut BTreeMap<u64, (kcore::job::JobId, u64)>, tid: u64) {
    if let Some((job, bytes)) = book.remove(&tid) {
        let j = crate::kmem::jobs();
        j.jobs.credit(job, kcore::job::Resource::KernelBytes, bytes);
        j.jobs.release(job);
    }
}

/// A thread has ended: forget its FP and vector state.
pub fn thread_gone(tid: u64) {
    let s = state();
    // (a claim must not outlive the state it names)
    for (t, fp) in [
        (s.threads.get(&tid).and_then(|t| t.holder()), true),
        (s.vthreads.get(&tid).and_then(|t| t.holder()), false),
    ] {
        if let Some(h) = t {
            let p = crate::percpu::hart(h);
            let (cell, buf) = if fp {
                (&p.fs_claim, &p.fs_buf)
            } else {
                (&p.vs_claim, &p.vs_buf)
            };
            if cell.load(core::sync::atomic::Ordering::Acquire) == tid {
                cell.store(0, core::sync::atomic::Ordering::Release);
                buf.store(0, core::sync::atomic::Ordering::Release);
            }
        }
    }
    s.threads.remove(&tid);
    s.vthreads.remove(&tid);
    credit_state(&mut s.fp_charge, tid);
    credit_state(&mut s.vec_charge, tid);
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
