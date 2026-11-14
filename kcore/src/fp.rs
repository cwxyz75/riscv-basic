//! Floating-point state for user threads (slice A, OD76): who owns a hart's FP registers, when they
//! are saved and when they are loaded. Pure logic over an abstract register unit; the kernel supplies
//! the real one (`fsd`/`fld` and the `fcsr`), the tests supply an array.
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

/// A hart's FP register file, as the kernel can reach it.
pub trait FpUnit {
    /// Read every FP register and `fcsr`.
    fn save(&mut self) -> FpRegs;
    /// Write every FP register and `fcsr`.
    fn load(&mut self, regs: &FpRegs);
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

    fn next(&mut self) -> u64 {
        self.last += 1;
        self.last
    }
}

/// The FP state of a thread.
#[derive(Default)]
pub struct FpThread {
    state: Option<(Box<FpRegs>, u64)>,
}

impl FpThread {
    pub fn new() -> FpThread {
        FpThread::default()
    }

    /// Does the thread use the FP unit (has it been enabled)?
    pub fn enabled(&self) -> bool {
        self.state.is_some()
    }

    /// The saved registers, if enabled (for tests and for the kernel's checks).
    pub fn regs(&self) -> Option<&FpRegs> {
        self.state.as_ref().map(|(r, _)| &**r)
    }

    /// The first FP use: start from a zeroed register file. Returns false if already enabled (then
    /// an illegal instruction with the unit enabled is a real one).
    pub fn enable(&mut self, tokens: &mut Tokens) -> bool {
        if self.state.is_some() {
            return false;
        }
        self.state = Some((Box::new(FpRegs::ZERO), tokens.next()));
        true
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
    pub fn holds(&self, thread: &FpThread) -> bool {
        thread
            .state
            .as_ref()
            .is_some_and(|(_, token)| *token == self.owner)
    }

    /// A trap from user mode: if the thread dirtied the registers, save them into the thread. Returns
    /// false if the unit claims dirty state for a thread that was never enabled (the hardware cannot
    /// do that; the kernel treats it as a bug).
    pub fn trap_entry<U: FpUnit>(
        &mut self,
        thread: &mut FpThread,
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
    pub fn return_to_user<U: FpUnit>(&mut self, thread: &FpThread, unit: &mut U) -> Fs {
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
