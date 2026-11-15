//! Wire formats for the S8 network stack (NET-SPEC 6.1): zero-allocation views over `&[u8]` and builders into
//! `&mut [u8]`. `no_std`, no `unsafe`, **total**: no function panics, indexes out of bounds or overflows on any input
//! (the fuzz-style tests in `tests/` enforce it); every length that comes from a packet is checked against the bytes
//! actually present before it is used.
#![no_std]
#![forbid(unsafe_code)]

pub mod arp;
pub mod checksum;
pub mod dhcp;
pub mod eth;
pub mod icmp;
pub mod ipv4;
pub mod udp;

/// Why a parse or a build failed. Each parser documents which it returns; the stack counts them by kind.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WireError {
    /// Fewer bytes than the format needs.
    Truncated,
    /// A header field has a value the format forbids (version, header length, hardware type, ...).
    Malformed,
    /// A length field disagrees with the bytes present.
    BadLength,
    /// The checksum does not verify.
    BadChecksum,
    /// An IPv4 fragment (offset non-zero or more-fragments set): not supported (no reassembly in S8).
    Fragment,
    /// A well-formed message of a kind this stack does not handle.
    Unsupported,
    /// The output buffer is too small.
    NoSpace,
}

pub type Mac = [u8; 6];
pub type Ip = [u8; 4];

pub const BROADCAST_MAC: Mac = [0xff; 6];
pub const BROADCAST_IP: Ip = [255; 4];
pub const UNSPECIFIED_IP: Ip = [0; 4];

pub(crate) fn be16(b: &[u8], at: usize) -> Result<u16, WireError> {
    match b.get(at..at.checked_add(2).ok_or(WireError::Truncated)?) {
        Some(&[a, c]) => Ok(u16::from_be_bytes([a, c])),
        _ => Err(WireError::Truncated),
    }
}

pub(crate) fn be32(b: &[u8], at: usize) -> Result<u32, WireError> {
    match b.get(at..at.checked_add(4).ok_or(WireError::Truncated)?) {
        Some(&[a, c, d, e]) => Ok(u32::from_be_bytes([a, c, d, e])),
        _ => Err(WireError::Truncated),
    }
}

pub(crate) fn arr<const N: usize>(b: &[u8], at: usize) -> Result<[u8; N], WireError> {
    let s = b.get(at..at.checked_add(N).ok_or(WireError::Truncated)?);
    let mut out = [0u8; N];
    out.copy_from_slice(s.ok_or(WireError::Truncated)?);
    Ok(out)
}

/// The output window `buf[at..at + n]`, or `NoSpace`.
pub(crate) fn out(buf: &mut [u8], at: usize, n: usize) -> Result<&mut [u8], WireError> {
    buf.get_mut(at..at.checked_add(n).ok_or(WireError::NoSpace)?)
        .ok_or(WireError::NoSpace)
}
