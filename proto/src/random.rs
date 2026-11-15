//! The random service (`/svc/random`, S9 step 5): ask for bytes, get bytes from a generator seeded by the entropy device.
//! Request: `txid u32 | op u16 | n u16`. Reply: `txid u32 | status i32 | bytes`.
use crate::{rd_u16, rd_u32};
use alloc::vec::Vec;

pub const OP_READ: u16 = 1;
/// The most one request may ask for.
pub const MAX_READ: usize = 4096;
pub const REQUEST_LEN: usize = 8;

pub mod status {
    pub const OK: i32 = 0;
    /// The generator has not been seeded yet (no entropy device, or it has not answered): nothing is returned, never a
    /// weak value.
    pub const NOT_SEEDED: i32 = 1;
    pub const BAD_REQUEST: i32 = 2;
}

pub fn encode_read(txid: u32, n: u16) -> Vec<u8> {
    let mut b = Vec::with_capacity(REQUEST_LEN);
    b.extend_from_slice(&txid.to_le_bytes());
    b.extend_from_slice(&OP_READ.to_le_bytes());
    b.extend_from_slice(&n.to_le_bytes());
    b
}

/// `Ok((txid, n))`; `Err(Some(txid))` for a malformed request that had one.
pub fn decode_request(b: &[u8]) -> Result<(u32, usize), Option<u32>> {
    let txid = rd_u32(b, 0);
    if b.len() != REQUEST_LEN || rd_u16(b, 4) != Some(OP_READ) {
        return Err(txid);
    }
    match (txid, rd_u16(b, 6)) {
        (Some(t), Some(n)) if (1..=MAX_READ as u16).contains(&n) => Ok((t, usize::from(n))),
        _ => Err(txid),
    }
}

pub fn encode_reply(txid: u32, status: i32, bytes: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(8 + bytes.len());
    b.extend_from_slice(&txid.to_le_bytes());
    b.extend_from_slice(&status.to_le_bytes());
    b.extend_from_slice(bytes);
    b
}

pub fn decode_reply(b: &[u8]) -> Option<(u32, i32, &[u8])> {
    Some((rd_u32(b, 0)?, rd_u32(b, 4)? as i32, b.get(8..)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_replies_round_trip() {
        assert_eq!(encode_read(7, 32), [7, 0, 0, 0, 1, 0, 32, 0]);
        assert_eq!(decode_request(&encode_read(7, 32)), Ok((7, 32)));
        assert_eq!(decode_request(&encode_read(7, 0)), Err(Some(7)));
        assert_eq!(decode_request(&encode_read(7, 4097)), Err(Some(7)));
        assert_eq!(decode_request(&encode_read(7, 4096)), Ok((7, 4096)));
        assert_eq!(decode_request(&[1, 2]), Err(None));
        assert_eq!(decode_request(&[5, 0, 0, 0, 9, 0, 1, 0]), Err(Some(5)));
        let r = encode_reply(9, status::OK, b"abc");
        assert_eq!(decode_reply(&r), Some((9, 0, &b"abc"[..])));
        assert_eq!(decode_reply(&r[..7]), None);
    }

    #[test]
    fn every_truncation_is_refused_not_misread() {
        let m = encode_read(1, 10);
        for n in 0..m.len() {
            assert!(decode_request(&m[..n]).is_err());
        }
    }
}
