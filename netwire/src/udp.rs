//! UDP over IPv4.
use crate::checksum;
use crate::{be16, out, Ip, WireError};

pub const HEADER: usize = 8;

#[derive(Clone, Copy, Debug)]
pub struct Udp<'a> {
    pub src_port: u16,
    pub dst_port: u16,
    /// Exactly the UDP length less the header (Ethernet padding and IP trailer bytes are not included).
    pub payload: &'a [u8],
    /// Whether the datagram carried a checksum (zero means none; accepted on receive).
    pub had_checksum: bool,
}

/// Parse a UDP datagram (the IP payload) sent from `src` to `dst`. `Truncated` under 8 bytes; `Malformed` when the
/// length field is below 8; `BadLength` when it exceeds the bytes present; `BadChecksum` when a non-zero checksum fails.
pub fn parse<'a>(d: &'a [u8], src: Ip, dst: Ip) -> Result<Udp<'a>, WireError> {
    if d.len() < HEADER {
        return Err(WireError::Truncated);
    }
    let len = usize::from(be16(d, 4)?);
    if len < HEADER {
        return Err(WireError::Malformed);
    }
    let body = d.get(..len).ok_or(WireError::BadLength)?;
    let ck = be16(d, 6)?;
    if ck != 0 {
        let init = checksum::pseudo(src, dst, 17, len as u16);
        if !checksum::verifies(body, init) {
            return Err(WireError::BadChecksum);
        }
    }
    Ok(Udp {
        src_port: be16(d, 0)?,
        dst_port: be16(d, 2)?,
        payload: body.get(HEADER..).ok_or(WireError::BadLength)?,
        had_checksum: ck != 0,
    })
}

/// Write a datagram with its checksum (never zero on the wire: a computed zero is sent as 0xffff). Returns its length.
pub fn write(
    buf: &mut [u8],
    src: Ip,
    dst: Ip,
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
) -> Result<usize, WireError> {
    let n = HEADER
        .checked_add(payload.len())
        .ok_or(WireError::NoSpace)?;
    let n16 = u16::try_from(n).map_err(|_| WireError::NoSpace)?;
    let o = out(buf, 0, n)?;
    o[0..2].copy_from_slice(&src_port.to_be_bytes());
    o[2..4].copy_from_slice(&dst_port.to_be_bytes());
    o[4..6].copy_from_slice(&n16.to_be_bytes());
    o[6..8].copy_from_slice(&[0, 0]);
    o[HEADER..].copy_from_slice(payload);
    let init = checksum::pseudo(src, dst, 17, n16);
    let c = !checksum::fold(checksum::sum16(o, init));
    let c = if c == 0 { 0xffff } else { c };
    o[6..8].copy_from_slice(&c.to_be_bytes());
    Ok(n)
}
