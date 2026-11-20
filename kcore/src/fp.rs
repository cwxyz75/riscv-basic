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
//! state in between) always reloads. Everything here runs under the big kernel lock.

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

    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: a u64 counter that gains one per thread
    fn next(&mut self) -> u64 {
        self.last += 1;
        self.last
    }
}

/// The state of one unit for a thread.
pub struct UnitThread<R> {
    state: Option<(Box<R>, u64)>,
}

impl<R> Default for UnitThread<R> {
    fn default() -> Self {
        UnitThread { state: None }
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

impl UnitThread<FpRegs> {
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
        **regs = unit.save();
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
