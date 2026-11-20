//! What differs by processor architecture in the user-visible contract (AR1d, KERNEL-V1-SPEC 2.6): the user address range, the
//! stack arena, and what an exception reports. Everything else in `abi` is the same on every architecture. RISC-V is the only
//! implementation; a second architecture adds a sibling module and selects it here.

mod riscv64;
pub use riscv64::*;

/// The kinds of exception a user thread can take, the same on every architecture. The exit code of a process ended by an
/// unhandled fault is `-1000 - kind` (INVARIANTS P3), and an exception message carries the kind (EX2).
pub mod fault {
    /// A load, store or instruction fetch the address space would not satisfy.
    pub const PAGE_FAULT: u32 = 1;
    /// An illegal or unimplemented instruction.
    pub const ILLEGAL: u32 = 2;
    /// A breakpoint.
    pub const BREAKPOINT: u32 = 3;
    /// A misaligned access the hardware does not perform.
    pub const MISALIGNED: u32 = 4;
    /// An access the memory system refused (not a page fault), or an exception of a kind the kernel does not name.
    pub const ACCESS: u32 = 5;
    /// A pager failure (EX2).
    pub const PAGER: u32 = 6;

    /// Base of the exit codes of faults: a process ended by a fault of `kind` exits with `EXIT_BASE - kind`.
    pub const EXIT_BASE: i32 = -1000;

    /// The exit code of a process ended by a fault of `kind`.
    pub const fn exit_code(kind: u32) -> i32 {
        EXIT_BASE.wrapping_sub(kind as i32)
    }

    /// The kind an exit code stands for, if it is a fault exit.
    pub const fn kind_of_exit(code: i32) -> Option<u32> {
        if code < EXIT_BASE && code >= EXIT_BASE.wrapping_sub(99) {
            Some(EXIT_BASE.wrapping_sub(code) as u32)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fault::*;

    #[test]
    fn exit_codes_round_trip() {
        for k in [PAGE_FAULT, ILLEGAL, BREAKPOINT, MISALIGNED, ACCESS, PAGER] {
            assert_eq!(kind_of_exit(exit_code(k)), Some(k));
        }
        assert_eq!(exit_code(ILLEGAL), -1002);
        assert_eq!(kind_of_exit(0), None);
        assert_eq!(kind_of_exit(-1000), None);
        assert_eq!(kind_of_exit(-2009), None);
    }
}
