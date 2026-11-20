//! RISC-V (Sv39): the user address range and the stack arena.

/// Lowest mappable user address; page 0 is never mapped.
pub const USER_BASE: u64 = 0x1000;
/// First address above the user half.
pub const USER_TOP: u64 = 0x0000_0040_0000_0000;
/// Initial `sp`.
pub const USER_STACK_TOP: u64 = 0x0000_0020_0000_0000;
/// The stack arena: where `thread_spawn` maps the stacks it provisions (32 GiB nothing else is given).
pub const USER_STACK_ARENA_BASE: u64 = 0x30_0000_0000;
pub const USER_STACK_ARENA_END: u64 = 0x38_0000_0000;
