//! Ethernet II.
use crate::{arr, be16, out, Mac, WireError};

pub const HEADER: usize = 14;
pub const ETHERTYPE_IPV4: u16 = 0x0800;
pub const ETHERTYPE_ARP: u16 = 0x0806;
/// The largest frame without VLAN tags.
pub const FRAME_MAX: usize = 1514;

#[derive(Clone, Copy, Debug)]
pub struct Eth<'a> {
    pub dst: Mac,
    pub src: Mac,
    pub ethertype: u16,
    pub payload: &'a [u8],
}

/// Parse a frame. `Truncated` if it is shorter than the header.
pub fn parse(frame: &[u8]) -> Result<Eth<'_>, WireError> {
    Ok(Eth {
        dst: arr(frame, 0)?,
        src: arr(frame, 6)?,
        ethertype: be16(frame, 12)?,
        payload: frame.get(HEADER..).ok_or(WireError::Truncated)?,
    })
}

/// Write the header at the start of `buf`; returns its length.
pub fn write_header(
    buf: &mut [u8],
    dst: Mac,
    src: Mac,
    ethertype: u16,
) -> Result<usize, WireError> {
    let o = out(buf, 0, HEADER)?;
    o[0..6].copy_from_slice(&dst);
    o[6..12].copy_from_slice(&src);
    o[12..14].copy_from_slice(&ethertype.to_be_bytes());
    Ok(HEADER)
}
