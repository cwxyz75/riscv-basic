//! The wall-clock service (`/svc/clock`, FS-SPEC section 7): ask for the time, get UTC nanoseconds since the epoch and how far
//! to trust them. Request: `txid u32 | op u16 | 0 u16`. Reply: `txid u32 | status i32 | nanoseconds u64 | quality u8 | 0 u8 x 7`.
use crate::{rd_u16, rd_u32, rd_u64};
use alloc::vec::Vec;

pub const OP_NOW: u16 = 1;
pub const REQUEST_LEN: usize = 8;
pub const REPLY_LEN: usize = 24;

pub mod status {
    pub const OK: i32 = 0;
    pub const BAD_REQUEST: i32 = 1;
}

/// How far to trust the time (mirrors `clockmodel::Quality`).
pub mod quality {
    /// No source: the time counts from the epoch at boot.
    pub const UNSET: u8 = 0;
    /// Read from a hardware clock.
    pub const FROM_RTC: u8 = 1;
    /// Set by a time-synchronisation client (none yet).
    pub const SYNCED: u8 = 2;
}

pub fn encode_now(txid: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(REQUEST_LEN);
    b.extend_from_slice(&txid.to_le_bytes());
    b.extend_from_slice(&OP_NOW.to_le_bytes());
    b.extend_from_slice(&0u16.to_le_bytes());
    b
}

/// `Ok(txid)` for a well-formed `Now`; `Err(Some(txid))` for a malformed request that had one.
pub fn decode_request(b: &[u8]) -> Result<u32, Option<u32>> {
    let txid = rd_u32(b, 0);
    if b.len() != REQUEST_LEN || rd_u16(b, 4) != Some(OP_NOW) || rd_u16(b, 6) != Some(0) {
        return Err(txid);
    }
    txid.ok_or(None)
}

pub fn encode_reply(txid: u32, status: i32, nanos: u64, quality: u8) -> Vec<u8> {
    let mut b = Vec::with_capacity(REPLY_LEN);
    b.extend_from_slice(&txid.to_le_bytes());
    b.extend_from_slice(&status.to_le_bytes());
    b.extend_from_slice(&nanos.to_le_bytes());
    b.push(quality);
    b.extend_from_slice(&[0; 7]);
    b
}

/// `(txid, status, nanoseconds, quality)`.
pub fn decode_reply(b: &[u8]) -> Option<(u32, i32, u64, u8)> {
    if b.len() != REPLY_LEN || b[17..].iter().any(|&x| x != 0) {
        return None;
    }
    Some((rd_u32(b, 0)?, rd_u32(b, 4)? as i32, rd_u64(b, 8)?, b[16]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_replies_round_trip() {
        assert_eq!(encode_now(7), [7, 0, 0, 0, 1, 0, 0, 0]);
        assert_eq!(decode_request(&encode_now(7)), Ok(7));
        assert_eq!(decode_request(&[5, 0, 0, 0, 9, 0, 0, 0]), Err(Some(5)));
        assert_eq!(decode_request(&[5, 0, 0, 0, 1, 0, 1, 0]), Err(Some(5)));
        assert_eq!(decode_request(&[1, 2]), Err(None));
        let r = encode_reply(9, status::OK, 1_700_000_000_000_000_000, quality::FROM_RTC);
        assert_eq!(r.len(), REPLY_LEN);
        assert_eq!(
            decode_reply(&r),
            Some((9, 0, 1_700_000_000_000_000_000, quality::FROM_RTC))
        );
        assert_eq!(decode_reply(&r[..23]), None);
        let mut bad = r.clone();
        bad[20] = 1;
        assert_eq!(decode_reply(&bad), None);
    }

    #[test]
    fn every_truncation_is_refused_not_misread() {
        let m = encode_now(1);
        for n in 0..m.len() {
            assert!(decode_request(&m[..n]).is_err());
        }
    }
}
