//! ARP for Ethernet and IPv4 only.
use crate::{arr, be16, out, Ip, Mac, WireError};

pub const LEN: usize = 28;
pub const OP_REQUEST: u16 = 1;
pub const OP_REPLY: u16 = 2;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Arp {
    pub op: u16,
    pub sha: Mac,
    pub spa: Ip,
    pub tha: Mac,
    pub tpa: Ip,
}

/// Parse an ARP payload (the bytes after the Ethernet header; Ethernet padding after the 28 bytes is ignored).
/// `Malformed` unless hardware type 1, protocol 0x0800, lengths 6 and 4 and operation 1 or 2.
pub fn parse(p: &[u8]) -> Result<Arp, WireError> {
    if be16(p, 0)? != 1 || be16(p, 2)? != 0x0800 || p.get(4) != Some(&6) || p.get(5) != Some(&4) {
        return if p.len() < LEN {
            Err(WireError::Truncated)
        } else {
            Err(WireError::Malformed)
        };
    }
    let op = be16(p, 6)?;
    if op != OP_REQUEST && op != OP_REPLY {
        return Err(WireError::Malformed);
    }
    Ok(Arp {
        op,
        sha: arr(p, 8)?,
        spa: arr(p, 14)?,
        tha: arr(p, 18)?,
        tpa: arr(p, 24)?,
    })
}

/// Write the 28-byte payload at the start of `buf`.
pub fn write(buf: &mut [u8], a: &Arp) -> Result<usize, WireError> {
    let o = out(buf, 0, LEN)?;
    o[0..2].copy_from_slice(&1u16.to_be_bytes());
    o[2..4].copy_from_slice(&0x0800u16.to_be_bytes());
    o[4] = 6;
    o[5] = 4;
    o[6..8].copy_from_slice(&a.op.to_be_bytes());
    o[8..14].copy_from_slice(&a.sha);
    o[14..18].copy_from_slice(&a.spa);
    o[18..24].copy_from_slice(&a.tha);
    o[24..28].copy_from_slice(&a.tpa);
    Ok(LEN)
}
