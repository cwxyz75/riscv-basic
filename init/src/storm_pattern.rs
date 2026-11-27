//! T266: the patterns of the kill storms (`stream_proto.rs`), in one file that both sides include with `#[path]`: the socket hand in `crash.rs`
//! writes them, the suite computes them from the offset or the index to check what it read (an oracle that is not echoed), so they cannot drift.
#![allow(dead_code)] // (each side uses its own part)

/// The byte at `offset` of a pattern-checked stream: not periodic at 256 bytes, 1 KiB or 4 KiB, so a chunk copied to the wrong place or not at
/// all shows.
pub fn stream_byte(offset: u64) -> u8 {
    (offset ^ (offset >> 8) ^ (offset >> 16)) as u8
}

/// The length of message `i` of a channel kill storm: 1 to 64 KiB, the big copies of T215 (1 KiB and more go outside the object lock).
pub fn msg_len(i: u32) -> usize {
    1024 * (1 + (i as usize * 37 + 11) % 64)
}

/// The byte at `offset` of message `i`.
pub fn msg_byte(i: u32, offset: usize) -> u8 {
    (offset ^ (offset >> 8) ^ (i as usize).wrapping_mul(31)) as u8
}
