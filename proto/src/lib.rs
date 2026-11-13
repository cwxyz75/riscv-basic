//! Wire formats of the S4 userland (S4-SPEC.md sections 5-8): pure functions over byte
//! slices, `no_std` + `alloc`, `forbid(unsafe_code)`. Every decoder validates before it
//! allocates and returns an error for any malformed input; none may panic (the tests
//! include a mutation fuzz). Everything is little endian.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod blk;
pub mod bootimg;
pub mod console;
pub mod file;
pub mod launch;
pub mod startup;
pub mod tty;

pub(crate) fn rd_u16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        b.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}
pub(crate) fn rd_u32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        b.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}
pub(crate) fn rd_u64(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        b.get(at..at.checked_add(8)?)?.try_into().ok()?,
    ))
}
