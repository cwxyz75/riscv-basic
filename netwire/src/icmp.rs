//! ICMPv4: echo, and destination unreachable for transmit.
use crate::checksum;
use crate::{be16, out, WireError};

pub const HEADER: usize = 8;
pub const TYPE_ECHO_REPLY: u8 = 0;
pub const TYPE_UNREACHABLE: u8 = 3;
pub const TYPE_ECHO_REQUEST: u8 = 8;
pub const CODE_PORT_UNREACHABLE: u8 = 3;

#[derive(Clone, Copy, Debug)]
pub struct Icmp<'a> {
    pub kind: u8,
    pub code: u8,
    /// Bytes 4 to 7: identifier and sequence for echo.
    pub id: u16,
    pub seq: u16,
    pub body: &'a [u8],
}

/// Parse an ICMP message (the IP payload). `Truncated` under 8 bytes; `BadChecksum` when it does not verify.
pub fn parse(m: &[u8]) -> Result<Icmp<'_>, WireError> {
    if m.len() < HEADER {
        return Err(WireError::Truncated);
    }
    if !checksum::verifies(m, 0) {
        return Err(WireError::BadChecksum);
    }
    Ok(Icmp {
        kind: m[0],
        code: m[1],
        id: be16(m, 4)?,
        seq: be16(m, 6)?,
        body: m.get(HEADER..).ok_or(WireError::Truncated)?,
    })
}

fn finish(o: &mut [u8]) {
    o[2..4].copy_from_slice(&[0, 0]);
    let c = checksum::checksum(o);
    o[2..4].copy_from_slice(&c.to_be_bytes());
}

/// Write an echo request or reply with `data`. Returns the message length.
pub fn write_echo(
    buf: &mut [u8],
    reply: bool,
    id: u16,
    seq: u16,
    data: &[u8],
) -> Result<usize, WireError> {
    let n = HEADER.checked_add(data.len()).ok_or(WireError::NoSpace)?;
    let o = out(buf, 0, n)?;
    o[0] = if reply {
        TYPE_ECHO_REPLY
    } else {
        TYPE_ECHO_REQUEST
    };
    o[1] = 0;
    o[4..6].copy_from_slice(&id.to_be_bytes());
    o[6..8].copy_from_slice(&seq.to_be_bytes());
    o[HEADER..].copy_from_slice(data);
    finish(o);
    Ok(n)
}

/// Write a destination-unreachable message quoting `original` (the offending IP header and the first 8 bytes of its
/// payload; at most 28 bytes are quoted).
pub fn write_unreachable(buf: &mut [u8], code: u8, original: &[u8]) -> Result<usize, WireError> {
    let q = original.get(..original.len().min(28)).unwrap_or(&[]);
    let n = HEADER + q.len();
    let o = out(buf, 0, n)?;
    o[0] = TYPE_UNREACHABLE;
    o[1] = code;
    o[4..8].copy_from_slice(&[0, 0, 0, 0]);
    o[HEADER..].copy_from_slice(q);
    finish(o);
    Ok(n)
}
