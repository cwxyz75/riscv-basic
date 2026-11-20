//! The decoded form of a trap from user mode, the same on every architecture (AR1): the architecture's `TrapFrame::decode_user`
//! produces it, and nothing outside `arch` reads a cause register.

use kcore::aspace::Access;

/// What kind of exception a trap from user mode was (the same kinds on every architecture; EX2 uses these numbers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultKind {
    PageFault,
    Illegal,
    Breakpoint,
    Misaligned,
    Access,
    Other,
}

/// A trap from user mode, decoded: nothing outside `arch` reads a cause register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fault {
    pub kind: FaultKind,
    /// The faulting address (for a page fault, an access fault or a misaligned access).
    pub address: u64,
    /// The program counter of the trapping instruction.
    pub pc: u64,
    /// The access that faulted, for a page fault.
    pub access: Option<Access>,
    /// The architecture's raw cause value (the exception message's raw field, EX2; logged by `fault-log`).
    pub code: u64,
}

/// A trap from user mode: a system call or a fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserTrap {
    Syscall,
    Fault(Fault),
}

impl FaultKind {
    /// The kind in the user-visible numbering (`abi::arch::fault`). An exception the kernel does not name is an ACCESS fault.
    pub const fn abi_kind(self) -> u32 {
        match self {
            FaultKind::PageFault => abi::arch::fault::PAGE_FAULT,
            FaultKind::Illegal => abi::arch::fault::ILLEGAL,
            FaultKind::Breakpoint => abi::arch::fault::BREAKPOINT,
            FaultKind::Misaligned => abi::arch::fault::MISALIGNED,
            FaultKind::Access | FaultKind::Other => abi::arch::fault::ACCESS,
        }
    }
}
