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
    // SAFETY: the object lock is held (every caller does; it is checked in debug builds only), so no other hart has the tables; no caller keeps the reference across a context switch, and a function
    // that holds one passes it down (`drop_claim` takes `s`) instead of calling `state()` again, so no two live `&mut` alias on this hart (T239).
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
        // SAFETY: `f` points at the 32 u64s of the live `&mut FpRegs` and `fcsr` at its u32, and only those are written; the unit is switched on for exactly these instructions and goes
        // back to Off; interrupts are off across the block (the trap context, or the object lock, which asserts it), so no switch can save another thread's registers between the `csrs` and the `csrc`, and this hart's registers hold the thread's state (T242).
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
        // SAFETY: as `save`: `f` points at the 32 u64s of the live `&FpRegs`, `c` holds its fcsr value, they are only read, and interrupts are off across the block (the trap context, or the object lock, which asserts it), so no switch can save another thread's registers between the `csrs` and the `csrc`. The FP registers are deliberately *not*
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
        // SAFETY: `r.v` is 32 * vlenb bytes (checked above; `vlenb` is the same on every hart, `check_hart`) and each `vs8r.v` stores eight registers
        // (8 * vlenb bytes) at the pointer, which steps by that stride; `csrs` is four u64s. The registers are deliberately not declared
        // clobbered (the kernel holds nothing in them, as for FP). `vstart` is saved, zeroed and restored, the vector registers are only read, whole-register stores do not
        // depend on `vtype`/`vill`, the unit goes back to Off, and interrupts are off across the block (the trap context, or the object lock, which asserts it), so no switch can save another thread's registers between the `csrs` and the `csrc` (T242).
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
        if r.v.len() != n.saturating_mul(32) {
            return; // (as in `save_into`: the four `vl8re8.v` read 32 * vlenb bytes; every `VecRegs` has that length, and the length is now checked here too, T242)
        }
        let stride = n.saturating_mul(8);
        // a saved `vill` (the user's last vsetvl was illegal) is restored by an illegal vtype again: a reserved bit set
        let vtype = if r.vtype >> 63 != 0 { 1 << 8 } else { r.vtype };
        // SAFETY: as `save`, reading instead of writing. `vstart` is cleared first (a whole-register load skips elements below it), the length
        // and type are restored by `vsetvl` (it keeps the saved `vl`: that was already at most the maximum for this type), the registers by
        // `vl8re8.v` and the user's `vcsr` and `vstart` last. `r.v` is 32 * vlenb bytes (checked above); the user-written `vl`/`vtype`/`vcsr`/`vstart` go through `vsetvl` and WARL CSR
        // writes, which cannot trap in S-mode (a bad `vtype` only sets `vill`); interrupts are off across the block (the trap context, or the object lock, which asserts it), so no switch can save another thread's registers between the `csrs` and the `csrc` (T242).
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
    if fp {
        kcore::fp::claim(&FpCells, t, harts, tid, h);
    } else {
        kcore::fp::claim(&VecCells, t, harts, tid, h);
    }
}

/// The per-hart claim cells of the FP unit, in the hart's record (`kcore::fp::ClaimCells`; the protocol is modelled in `kcore/tests/fp_claim_model.rs`).
struct FpCells;
/// The same for the vector unit.
struct VecCells;

impl kcore::fp::ClaimCells for FpCells {
    fn claim(&self, hart: usize) -> u64 {
        crate::percpu::hart(hart)
            .fs_claim
            .load(core::sync::atomic::Ordering::Acquire)
    }
    fn buf(&self, hart: usize) -> usize {
        crate::percpu::hart(hart)
            .fs_buf
            .load(core::sync::atomic::Ordering::Acquire)
    }
    fn set(&self, hart: usize, tid: u64, buf: usize) {
        let p = crate::percpu::hart(hart);
        p.fs_buf.store(buf, core::sync::atomic::Ordering::Release);
        p.fs_claim.store(tid, core::sync::atomic::Ordering::Release);
    }
    fn clear(&self, hart: usize) {
        let p = crate::percpu::hart(hart);
        p.fs_claim.store(0, core::sync::atomic::Ordering::Release);
        p.fs_buf.store(0, core::sync::atomic::Ordering::Release);
    }
}

impl kcore::fp::ClaimCells for VecCells {
    fn claim(&self, hart: usize) -> u64 {
        crate::percpu::hart(hart)
            .vs_claim
            .load(core::sync::atomic::Ordering::Acquire)
    }
    fn buf(&self, hart: usize) -> usize {
        crate::percpu::hart(hart)
            .vs_buf
            .load(core::sync::atomic::Ordering::Acquire)
    }
    fn set(&self, hart: usize, tid: u64, buf: usize) {
        let p = crate::percpu::hart(hart);
        p.vs_buf.store(buf, core::sync::atomic::Ordering::Release);
        p.vs_claim.store(tid, core::sync::atomic::Ordering::Release);
    }
    fn clear(&self, hart: usize) {
        let p = crate::percpu::hart(hart);
        p.vs_claim.store(0, core::sync::atomic::Ordering::Release);
        p.vs_buf.store(0, core::sync::atomic::Ordering::Release);
    }
}

/// What a frame's `sstatus` field says about a unit.
fn lazy_of(bits: u64, clean: u64, dirty: u64) -> kcore::fp::Lazy {
    if bits == 0 {
        kcore::fp::Lazy::Off
    } else if bits == dirty {
        kcore::fp::Lazy::Dirty
    } else if bits == clean {
        kcore::fp::Lazy::Clean
    } else {
        kcore::fp::Lazy::Initial
    }
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
        let Some(buf) = kcore::fp::light_target(&VecCells, me.index.load(Acquire), tid) else {
            return false;
        };
        // SAFETY: the claim names the thread running here (set only under the object lock together with the holder and `vs_buf`; another hart clears it only when it
        // loads or saves the same thread, which cannot run in two places, and thread ids are never reused), so its registers are in this hart's unit and nothing else
        // touches its saved copy. `buf` is the address of that copy's box, which is freed or replaced only by `thread_gone` and `write_vec`'s replacement, each of which drops
        // the claim first under the object lock and runs only for a thread that is not running (T242).
        unsafe { VecHartUnit.save_into(&mut *(buf as *mut VecRegs)) };
        frame.sstatus = (frame.sstatus & !VS_MASK) | VS_CLEAN;
    }
    if fsb == FS_DIRTY {
        let Some(buf) = kcore::fp::light_target(&FpCells, me.index.load(Acquire), tid) else {
            return false;
        };
        // SAFETY: as above, for the FP unit's copy (`fs_claim`, `fs_buf`; `write_fp`'s replacement drops the claim first, for a parked target) (T242).
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
    return_to_user_with(frame, false);
}

/// `return_to_user` for a caller that holds the object lock already (the first entry of a thread).
pub fn return_to_user_held(frame: &mut TrapFrame) {
    return_to_user_with(frame, true);
}

fn return_to_user_with(frame: &mut TrapFrame, held: bool) {
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
    let h = me.index.load(core::sync::atomic::Ordering::Relaxed);
    let fs_ok = kcore::fp::quiet_return(&FpCells, h, tid, lazy_of(fsb, FS_CLEAN, FS_DIRTY));
    let vs_ok = kcore::fp::quiet_return(&VecCells, h, tid, lazy_of(vsb, VS_CLEAN, VS_DIRTY));
    if fs_ok && vs_ok {
        return;
    }
    if !held {
        crate::bkl::lock();
    }
    let s = state();
    let fs = if fsb == 0 {
        None
    } else {
        Some(match s.threads.get_mut(&tid) {
            Some(t) => {
                // (asked before the load: afterwards the hart holds the thread's state whatever it held)
                let was_held = s.harts[hart()].holds(t);
                let fs = s.harts[hart()].return_to_user(t, &mut HartUnit);
                claim(t, &mut s.harts, tid, hart(), true);
                // dirty state still in the unit (a trap that skipped `trap_entry`) is the thread's own
                debug_assert!(
                    fsb != FS_DIRTY || was_held,
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
                let was_held = s.vharts[hart()].holds(t);
                let vs = s.vharts[hart()].return_to_user(t, &mut VecHartUnit);
                claim(t, &mut s.vharts, tid, hart(), false);
                debug_assert!(
                    vsb != VS_DIRTY || was_held,
                    "returning to a thread with unsaved dirty vector state that is not in this hart's unit"
                );
                vs
            }
            None => Fs::Off,
        })
    };
    if !held {
        crate::bkl::unlock();
    }
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
        return match vec_first_use(frame) {
            Some(on) => on || fp_first_use(frame),
            None => false, // (the job is at its kernel-memory limit: the instruction faults; the FP unit is not tried)
        };
    }
    fp_first_use(frame) || (frame.stval == 0 && vec_first_use(frame).unwrap_or(false))
}

fn fp_first_use(frame: &mut TrapFrame) -> bool {
    if frame.sstatus & FS_MASK != 0 {
        return false;
    }
    let tid = crate::thread::current();
    let job = crate::user::job_of_thread(tid);
    let s = state();
    // (the state is the job's kernel memory: a job at its limit cannot give a thread more; the instruction then faults like any illegal one)
    if !charge_state(
        &mut s.fp_charge,
        tid,
        job,
        (core::mem::size_of::<FpThread>() + core::mem::size_of::<FpRegs>()) as u64,
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

/// `Some(enabled)`, or `None` when the job could not be charged for the state.
fn vec_first_use(frame: &mut TrapFrame) -> Option<bool> {
    if frame.sstatus & VS_MASK != 0 {
        return Some(false);
    }
    let tid = crate::thread::current();
    let job = crate::user::job_of_thread(tid);
    let s = state();
    let bytes =
        (core::mem::size_of::<VecThread>() + core::mem::size_of::<VecRegs>() + vlenb() * 32) as u64;
    if !charge_state(&mut s.vec_charge, tid, job, bytes) {
        return None;
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
    Some(enabled)
}

/// Charge `bytes` of register state to the job of user thread `tid`, once (a second call for a thread already charged is free); `false`
/// when the job is at its kernel-memory limit. Kernel threads are not charged.
///
/// `job` is the thread's job (`None` for a kernel thread), named by the caller: a caller inside a user-state closure has it from its own `s`, and this
/// function does not open the state again (KT14: that aliased the caller's `&mut UserState`).
fn charge_state(
    book: &mut BTreeMap<u64, (kcore::job::JobId, u64)>,
    tid: u64,
    job: Option<kcore::job::JobId>,
    bytes: u64,
) -> bool {
    if book.contains_key(&tid) {
        return true;
    }
    let Some(job) = job else {
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
    if let Some(t) = s.threads.get(&tid) {
        kcore::fp::drop_claim(&FpCells, t, &mut [], tid, false);
    }
    if let Some(t) = s.vthreads.get(&tid) {
        kcore::fp::drop_claim(&VecCells, t, &mut [], tid, false);
    }
    s.threads.remove(&tid);
    s.vthreads.remove(&tid);
    credit_state(&mut s.fp_charge, tid);
    credit_state(&mut s.vec_charge, tid);
}

/// How many threads hold FP or vector state (the end-of-suite leak check: none may survive their process).
pub fn entry_count() -> usize {
    crate::irq::without(|| {
        let s = state();
        s.threads.len() + s.vthreads.len()
    })
}

// ---- EX1: reading and writing a stopped thread's FP and vector state ----

/// The vector register length in bytes, or `None` when this machine has no vector unit.
pub fn vector_vlenb() -> Option<usize> {
    let n = vlenb();
    (n > 0).then_some(n)
}

/// The thread's FP state in the layout of `abi::thread_state::FP` (f0 to f31, then fcsr), zeros for a thread that never used the unit. The
/// thread is stopped: its last trap saved what it had in the unit.
pub fn read_fp(tid: u64) -> [u64; 33] {
    let s = state();
    let mut w = [0u64; 33];
    if let Some(r) = s.threads.get(&tid).and_then(|t| t.regs()) {
        w[..32].copy_from_slice(&r.f);
        w[32] = u64::from(r.fcsr);
    }
    w
}

/// Make the registers of the hart that holds `tid`'s state stale (its claim is dropped, so the return to user mode loads from the saved copy).
fn drop_claim(s: &mut FpState, tid: u64, fp: bool) {
    if fp {
        if let Some(t) = s.threads.get(&tid) {
            kcore::fp::drop_claim(&FpCells, t, &mut s.harts, tid, true);
        }
    } else if let Some(t) = s.vthreads.get(&tid) {
        kcore::fp::drop_claim(&VecCells, t, &mut s.vharts, tid, true);
    }
}

/// Write the thread's FP state (the layout of `read_fp`; `fcsr` keeps its 8 defined bits). Allocates the state for a thread that has none,
/// charged to its job. `frame` is the thread's saved user frame if it has run (its `sstatus.FS` is set so the return to user mode loads the
/// state); a thread that has not run gets it at its first entry (`mark_present`). False when the job cannot be charged.
#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB (the register image is built on the stack and moved into its box); T172 shrinks them
pub fn write_fp(tid: u64, job: Option<kcore::job::JobId>, w: &[u64; 33]) -> bool {
    let s = state();
    if !charge_state(
        &mut s.fp_charge,
        tid,
        job,
        (core::mem::size_of::<FpThread>() + core::mem::size_of::<FpRegs>()) as u64,
    ) {
        return false;
    }
    drop_claim(s, tid, true);
    let mut f = [0u64; 32];
    f.copy_from_slice(&w[..32]);
    let regs = FpRegs {
        f,
        fcsr: (w[32] & 0xff) as u32,
    };
    let FpState {
        tokens, threads, ..
    } = s;
    threads.entry(tid).or_default().replace_regs(tokens, regs);
    true
}

/// The thread's vector state in the layout of `abi::thread_state::VECTOR`: vtype, vl, vstart, vcsr, then v0 to v31 (`vlenb` bytes each).
pub fn read_vec(tid: u64) -> Option<alloc::vec::Vec<u8>> {
    let s = state();
    let n = vlenb();
    let mut out = alloc::vec::Vec::new();
    out.try_reserve_exact(32usize.saturating_add(n.saturating_mul(32)))
        .ok()?;
    match s.vthreads.get(&tid).and_then(|t| t.regs()) {
        Some(r) => {
            for w in [r.vtype, r.vl, r.vstart, r.vcsr] {
                out.extend_from_slice(&w.to_le_bytes());
            }
            out.extend_from_slice(&r.v);
        }
        None => out.resize(32usize.saturating_add(n.saturating_mul(32)), 0),
    }
    Some(out)
}

/// Write the thread's vector state (`bytes` in the layout of `read_vec`, of exactly `32 + 32 * vlenb` bytes). As `write_fp`.
pub fn write_vec(tid: u64, job: Option<kcore::job::JobId>, bytes: &[u8]) -> bool {
    let n = vlenb();
    let s = state();
    if !charge_state(
        &mut s.vec_charge,
        tid,
        job,
        (core::mem::size_of::<VecThread>() + core::mem::size_of::<VecRegs>() + n * 32) as u64,
    ) {
        return false;
    }
    drop_claim(s, tid, false);
    let word = |i: usize| {
        let mut b = [0u8; 8];
        b.copy_from_slice(&bytes[i * 8..i * 8 + 8]);
        u64::from_le_bytes(b)
    };
    let regs = VecRegs {
        v: bytes[32..].to_vec(),
        vtype: word(0),
        vl: word(1),
        vstart: word(2),
        vcsr: word(3),
    };
    let FpState {
        vtokens, vthreads, ..
    } = s;
    vthreads.entry(tid).or_default().replace_regs(vtokens, regs);
    true
}

/// A thread's FP or vector state was written while it was parked: its frame says Initial so the return to user mode loads it (the thread does
/// this to its own frame when it wakes).
pub fn apply_written(tid: u64, frame: &mut TrapFrame, fp: bool, vec: bool) {
    let _ = tid;
    if fp {
        frame.sstatus = (frame.sstatus & !FS_MASK) | (1 << 13);
    }
    if vec {
        frame.sstatus = (frame.sstatus & !VS_MASK) | (1 << 9);
    }
}

/// A thread is about to run for the first time from a register image: if state was written for it beforehand, the units are to be loaded.
pub fn mark_present(tid: u64, frame: &mut TrapFrame) {
    let s = state();
    if s.threads.get(&tid).is_some_and(|t| t.enabled()) {
        frame.sstatus = (frame.sstatus & !FS_MASK) | (1 << 13);
    }
    if s.vthreads.get(&tid).is_some_and(|t| t.enabled()) {
        frame.sstatus = (frame.sstatus & !VS_MASK) | (1 << 9);
    }
}
