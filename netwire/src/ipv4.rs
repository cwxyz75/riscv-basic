//! IPv4. Options are skipped, not interpreted; fragments are refused.
use crate::checksum;
use crate::{arr, be16, out, Ip, WireError};

pub const HEADER: usize = 20;
pub const PROTO_ICMP: u8 = 1;
pub const PROTO_UDP: u8 = 17;
/// "Don't fragment".
pub const FLAG_DF: u16 = 0x4000;

/// The fields of a header that the stack reads or sets (everything a rebuild needs to reproduce the bytes).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Header {
    pub tos: u8,
    pub id: u16,
    /// Flags and fragment offset exactly as on the wire.
    pub flags_frag: u16,
    pub ttl: u8,
    pub proto: u8,
    pub src: Ip,
    pub dst: Ip,
}

#[derive(Clone, Copy, Debug)]
pub struct Ipv4<'a> {
    pub header: Header,
    /// The header length in bytes (20 to 60).
    pub header_len: usize,
    /// The payload: exactly the IP total length less the header, **not** the frame length (Ethernet pads short frames).
    pub payload: &'a [u8],
}

/// Parse a datagram. Errors, in order: `Truncated` (under 20 bytes), `Malformed` (version, IHL, total length below the
/// header), `BadLength` (total length beyond the bytes present), `BadChecksum`, `Fragment`.
pub fn parse(d: &[u8]) -> Result<Ipv4<'_>, WireError> {
    let vihl = *d.first().ok_or(WireError::Truncated)?;
    if d.len() < HEADER {
        return Err(WireError::Truncated);
    }
    let header_len = usize::from(vihl & 15) * 4;
    if vihl >> 4 != 4 || header_len < HEADER {
        return Err(WireError::Malformed);
    }
    let total = usize::from(be16(d, 2)?);
    if total < header_len {
        return Err(WireError::Malformed);
    }
    let hdr = d.get(..header_len).ok_or(WireError::Truncated)?;
    if total > d.len() {
        return Err(WireError::BadLength);
    }
    if !checksum::verifies(hdr, 0) {
        return Err(WireError::BadChecksum);
    }
    let flags_frag = be16(d, 6)?;
    if flags_frag & 0x3fff != 0 {
        return Err(WireError::Fragment);
    }
    Ok(Ipv4 {
        header: Header {
            tos: d[1],
            id: be16(d, 4)?,
            flags_frag,
            ttl: d[8],
            proto: d[9],
            src: arr(d, 12)?,
            dst: arr(d, 16)?,
        },
        header_len,
        payload: d.get(header_len..total).ok_or(WireError::BadLength)?,
    })
}

/// Write a 20-byte header (IHL 5) for a payload of `payload_len` bytes, checksum included. Returns 20.
pub fn write_header(buf: &mut [u8], h: &Header, payload_len: usize) -> Result<usize, WireError> {
    let total = HEADER.checked_add(payload_len).ok_or(WireError::NoSpace)?;
    let total16 = u16::try_from(total).map_err(|_| WireError::NoSpace)?;
    let o = out(buf, 0, HEADER)?;
    o[0] = 0x45;
    o[1] = h.tos;
    o[2..4].copy_from_slice(&total16.to_be_bytes());
    o[4..6].copy_from_slice(&h.id.to_be_bytes());
    o[6..8].copy_from_slice(&h.flags_frag.to_be_bytes());
    o[8] = h.ttl;
    o[9] = h.proto;
    o[10..12].copy_from_slice(&[0, 0]);
    o[12..16].copy_from_slice(&h.src);
    o[16..20].copy_from_slice(&h.dst);
    let c = checksum::checksum(o);
    o[10..12].copy_from_slice(&c.to_be_bytes());
    Ok(HEADER)
}
