//! Floating-point and vector state for user threads (slice A, OD76; PL2): who owns a hart's registers, when they
//! are saved and when they are loaded. Pure logic over an abstract register unit; the kernel supplies
//! the real ones (`fsd`/`fld` and the `fcsr`; whole-register vector stores and loads), the tests supply an array. The
//! protocol is one model with two instances: the FP unit (`FpThread`, `FpRegs`) and the vector unit (`VecThread`, `VecRegs`), each
//! with its own hart bookkeeping and tokens. The text below says "FP"; the vector unit follows it with `sstatus.VS` for `sstatus.FS`.
//!
//! The protocol, per thread and per hart:
//!
//! * A thread starts with the FP unit **off** (`sstatus.FS = Off`): it cannot read any FP register, so
//!   it cannot see another thread's values, and a thread that never uses FP pays nothing.
//! * The first FP instruction traps as an illegal instruction; the kernel then **enables** the thread
//!   (a zeroed register file) and retries the instruction.
//! * On **every trap entry from user mode**, if the unit says its state is *dirty*, the registers are
//!   saved into the thread, and the thread gets a new *token* (a unique id of this exact state).
//! * On **every return to user mode**, a thread with FP state gets it loaded unless this hart already
//!   holds exactly that state (the hart's `owner` token equals the thread's): a thread that is
//!   alone on a hart, or whose last switch was to a non-FP thread, loads nothing. A thread without FP
//!   state returns with the unit off.
//!
//! Tokens are unique and never reused, so a stale hart (the thread ran elsewhere and changed its
//! state in between) always reloads. The model runs under the big kernel lock; the kernel's lock-free paths (claims, `kernel/src/arch/riscv64/fp.rs`) only skip a load or save this model would have done as a no-op or do the save into the same buffer.

use alloc::boxed::Box;

/// The architectural FP state of one thread: `f0`-`f31` (as raw 64-bit patterns) and `fcsr`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FpRegs {
    pub f: [u64; 32],
    pub fcsr: u32,
}

impl FpRegs {
    pub const ZERO: FpRegs = FpRegs {
        f: [0; 32],
        fcsr: 0,
    };
}

/// The architectural vector state of one thread: `v0`-`v31` (`32 * vlenb` bytes, register after register), `vstart`, `vcsr`, `vl` and
/// `vtype` (whose top bit, `vill`, says the last `vsetvl` was illegal).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct VecRegs {
    pub v: alloc::vec::Vec<u8>,
    pub vstart: u64,
    pub vcsr: u64,
    pub vl: u64,
    pub vtype: u64,
}

impl VecRegs {
    /// A zeroed state for a unit whose registers are `vlenb` bytes long.
    pub fn zero(vlenb: usize) -> VecRegs {
        VecRegs {
            v: alloc::vec![0; vlenb.saturating_mul(32)],
            vstart: 0,
            vcsr: 0,
            vl: 0,
            vtype: 0,
        }
    }
}

/// A hart's register file (FP or vector), as the kernel can reach it.
pub trait StateUnit {
    type Regs: Clone;
    /// Read every register.
    fn save(&mut self) -> Self::Regs;
    /// Write every register.
    fn load(&mut self, regs: &Self::Regs);
    /// Read every register into `regs`, which already has the right shape (a unit whose registers are large saves in place: the trap path
    /// allocates nothing).
    fn save_into(&mut self, regs: &mut Self::Regs) {
        *regs = self.save();
    }
}

/// A hart's FP register file.
pub trait FpUnit: StateUnit<Regs = FpRegs> {}
impl<T: StateUnit<Regs = FpRegs>> FpUnit for T {}

/// A hart's vector register file.
pub trait VecUnit: StateUnit<Regs = VecRegs> {}
impl<T: StateUnit<Regs = VecRegs>> VecUnit for T {}

/// Is `insn` (the faulting instruction as the trap reported it in `stval`) one that only a vector unit executes? The OP-V major opcode
/// (`vsetvli`, `vadd`, ...), or a load or store whose width field is a vector element width (0, 5, 6, 7; the FP loads and stores use 1 to 4).
/// A CSR instruction naming a vector CSR (the program reading `vlenb` first) counts too. 0 (the trap gave no instruction) and compressed instructions are not vector instructions.
pub fn is_vector_insn(insn: u64) -> bool {
    if insn & 3 != 3 {
        return false;
    }
    match insn & 0x7f {
        0x57 => true,
        0x07 | 0x27 => matches!((insn >> 12) & 7, 0 | 5 | 6 | 7),
        // a CSR instruction on one of the vector CSRs: vstart, vxsat, vxrm, vcsr, vl, vtype, vlenb
        0x73 => {
            (insn >> 12) & 7 != 0
                && matches!(
                    (insn >> 20) & 0xfff,
                    0x008 | 0x009 | 0x00a | 0x00f | 0xc20 | 0xc21 | 0xc22
                )
        }
        _ => false,
    }
}

/// What the kernel must put in the returning thread's `sstatus.FS`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fs {
    /// The unit is off: the thread has no FP state and any FP instruction traps.
    Off,
    /// The registers hold the thread's state and nothing has changed since it was saved or loaded.
    Clean,
}

/// A source of unique state tokens (never zero, never reused).
#[derive(Default)]
pub struct Tokens {
    last: u64,
}

impl Tokens {
    pub fn new() -> Tokens {
        Tokens::default()
    }

    /// For test builds (`ff=fptoken:<n>`): the next token is `n + 1`, so the counter's far end is reached in a few steps.
    pub const fn starting_at(n: u64) -> Tokens {
        Tokens { last: n }
    }

    /// The next token (a u64 counter: 2^64 states is not reachable; at the end it would stop, never wrap).
    fn next(&mut self) -> u64 {
        self.last = self.last.saturating_add(1);
        self.last
    }
}

/// The state of one unit for a thread.
pub struct UnitThread<R> {
    state: Option<(Box<R>, u64)>,
    /// The hart whose register file holds this thread's latest state, if any (the kernel's lock-free return path keeps a claim on that hart).
    holder: Option<usize>,
}

impl<R> Default for UnitThread<R> {
    fn default() -> Self {
        UnitThread {
            state: None,
            holder: None,
        }
    }
}

/// The FP state of a thread.
pub type FpThread = UnitThread<FpRegs>;
/// The vector state of a thread.
pub type VecThread = UnitThread<VecRegs>;

impl<R> UnitThread<R> {
    pub fn new() -> UnitThread<R> {
        UnitThread::default()
    }

    /// The hart that holds the thread's latest state in its registers, as the kernel last recorded it.
    pub fn holder(&self) -> Option<usize> {
        self.holder
    }

    pub fn set_holder(&mut self, hart: Option<usize>) {
        self.holder = hart;
    }

    /// Where the saved registers live (the address of the box; stable until the thread's state is dropped), for a save made without a table lookup.
    pub fn regs_ptr(&mut self) -> Option<*mut R> {
        self.state.as_mut().map(|(r, _)| &mut **r as *mut R)
    }

    /// Does the thread use the FP unit (has it been enabled)?
    pub fn enabled(&self) -> bool {
        self.state.is_some()
    }

    /// The saved registers, if enabled (for tests and for the kernel's checks).
    pub fn regs(&self) -> Option<&R> {
        self.state.as_ref().map(|(r, _)| &**r)
    }

    /// The first use of the unit: start from `zero`, a zeroed register file. Returns false if already enabled (then
    /// an illegal instruction with the unit enabled is a real one).
    pub fn enable_with(&mut self, tokens: &mut Tokens, zero: R) -> bool {
        if self.state.is_some() {
            return false;
        }
        self.state = Some((Box::new(zero), tokens.next()));
        true
    }
}

impl<R> UnitThread<R> {
    /// Replace the saved registers (a debugger's write, EX1): the thread has state from now on, under a new token (so every hart that holds the
    /// old copy reloads) and with no holder.
    pub fn replace_regs(&mut self, tokens: &mut Tokens, regs: R) {
        self.state = Some((Box::new(regs), tokens.next()));
        self.holder = None;
    }
}

impl UnitThread<FpRegs> {
    /// `replace_regs` with the registers written in their box by `fill` (from zeros): no copy of the register file on the caller's stack (KT13).
    pub fn replace_regs_with(&mut self, tokens: &mut Tokens, fill: impl FnOnce(&mut FpRegs)) {
        let mut regs = Box::new(FpRegs::ZERO);
        fill(&mut regs);
        self.state = Some((regs, tokens.next()));
        self.holder = None;
    }

    /// The first FP use: start from a zeroed register file.
    pub fn enable(&mut self, tokens: &mut Tokens) -> bool {
        self.enable_with(tokens, FpRegs::ZERO)
    }
}

/// One hart's FP bookkeeping: the token of the state its registers currently hold (0: none known).
#[derive(Default)]
pub struct FpHart {
    owner: u64,
}

impl FpHart {
    pub const fn new() -> FpHart {
        FpHart { owner: 0 }
    }

    /// This hart no longer holds anything known: the next return to user mode loads (another hart took the thread's state over).
    pub fn forget(&mut self) {
        self.owner = 0;
    }

    /// Does this hart's register file hold exactly `thread`'s current state?
    pub fn holds<R>(&self, thread: &UnitThread<R>) -> bool {
        thread
            .state
            .as_ref()
            .is_some_and(|(_, token)| *token == self.owner)
    }

    /// A trap from user mode: if the thread dirtied the registers, save them into the thread. Returns
    /// false if the unit claims dirty state for a thread that was never enabled (the hardware cannot
    /// do that; the kernel treats it as a bug).
    pub fn trap_entry<U: StateUnit>(
        &mut self,
        thread: &mut UnitThread<U::Regs>,
        dirty: bool,
        tokens: &mut Tokens,
        unit: &mut U,
    ) -> bool {
        if !dirty {
            return true;
        }
        let Some((regs, token)) = thread.state.as_mut() else {
            return false;
        };
        unit.save_into(regs);
        *token = tokens.next();
        self.owner = *token;
        true
    }

    /// Return to user mode: make the unit hold the thread's state (if it has any) and say what
    /// `sstatus.FS` must be.
    pub fn return_to_user<U: StateUnit>(
        &mut self,
        thread: &UnitThread<U::Regs>,
        unit: &mut U,
    ) -> Fs {
        let Some((regs, token)) = thread.state.as_ref() else {
            return Fs::Off;
        };
        if self.owner != *token {
            unit.load(regs);
            self.owner = *token;
        }
        Fs::Clean
    }
}

/// What a thread's frame says about a unit (`sstatus.FS` or `.VS`), as the lock-free return path reads it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lazy {
    Off,
    Initial,
    Clean,
    Dirty,
}

/// The per-hart claim cells of one unit (T188): the thread (0: none) whose latest state this hart's register file holds, and the address of that
/// thread's saved copy. The kernel backs them with atomics in the hart's record; the model with plain cells. Every write is under the big lock; the
/// lock-free paths read their own hart's cells with interrupts off.
pub trait ClaimCells {
    /// The thread this hart claims (0: none).
    fn claim(&self, hart: usize) -> u64;
    /// The address of the claimed thread's saved copy (0: none).
    fn buf(&self, hart: usize) -> usize;
    /// Claim: store `buf` first, then `claim` (a reader that sees the claim sees the address).
    fn set(&self, hart: usize, tid: u64, buf: usize);
    /// Drop the claim: store `claim` 0 first, then `buf` 0.
    fn clear(&self, hart: usize);
}

/// This hart's register file now holds `tid`'s latest state (`claim` follows the save or the load that made it so): record the claim, and drop the one
/// the thread's previous holder hart had (that copy is stale now). Under the big lock.
pub fn claim<R, C: ClaimCells>(
    cells: &C,
    t: &mut UnitThread<R>,
    harts: &mut [FpHart],
    tid: u64,
    h: usize,
) {
    if let Some(old) = t.holder() {
        // (only if the old hart still holds this thread: another thread's load there has replaced the claim already)
        if old != h && cells.claim(old) == tid {
            cells.clear(old);
            // (a save that takes no lock keeps the token, so the old hart's token must stop matching)
            if let Some(f) = harts.get_mut(old) {
                f.forget();
            }
        }
    }
    debug_assert!(
        harts.get(h).is_some_and(|f| f.holds(t)),
        "a claim for thread {tid} on hart {h} that does not hold its state"
    );
    t.set_holder(Some(h));
    cells.set(h, tid, t.regs_ptr().map_or(0, |p| p as usize));
}

/// The thread's state is going away or being replaced (it ended, or a debugger wrote its registers): a claim must not outlive the state it names. Only for
/// a thread that is on no hart. `forget_hart`: also make that hart's token stop matching (a replacement keeps the thread; the new state has a new token
/// anyway, so this is for the debugger's write, as the kernel did). Under the big lock.
pub fn drop_claim<R, C: ClaimCells>(
    cells: &C,
    t: &UnitThread<R>,
    harts: &mut [FpHart],
    tid: u64,
    forget_hart: bool,
) {
    if let Some(h) = t.holder() {
        // (only if the hart still holds this thread: another thread's load there has replaced the claim already)
        if cells.claim(h) == tid {
            cells.clear(h);
            if forget_hart {
                if let Some(f) = harts.get_mut(h) {
                    f.forget();
                }
            }
        }
    }
}

/// Where a trap that takes no lock may save the dirty unit: the address of `tid`'s saved copy, when this hart claims `tid` (the buffer is read first,
/// then the claim, as the cells were written).
pub fn light_target<C: ClaimCells>(cells: &C, h: usize, tid: u64) -> Option<usize> {
    let buf = cells.buf(h);
    if cells.claim(h) != tid || buf == 0 {
        return None;
    }
    Some(buf)
}

/// May the return to user mode skip the lock for this unit? Off: nothing is owed. Clean or Dirty: when this hart's claim names the thread (its registers
/// are the thread's). Initial never: the zero state is still to be loaded.
pub fn quiet_return<C: ClaimCells>(cells: &C, h: usize, tid: u64, frame: Lazy) -> bool {
    match frame {
        Lazy::Off => true,
        Lazy::Initial => false,
        Lazy::Clean | Lazy::Dirty => cells.claim(h) == tid,
    }
}
