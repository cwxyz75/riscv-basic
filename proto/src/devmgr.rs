//! The device manager's protocol (S6c): a client asks for a block device and gets a channel to a
//! driver for it.
//!
//! Request: `txid u32 | op u16 | index u16`. `index` names the block device in ascending address order of its
//! virtio-mmio slot (0 is the lowest; slice DM: the manager serves every block device it finds, one
//! driver each). Reply: `txid u32 | status i32`, and for `Ok` the message carries one handle: the
//! client's end of the block protocol channel (`proto::blk`).

use crate::{rd_u16, rd_u32};
use alloc::vec::Vec;

pub const OP_OPEN_BLOCK: u16 = 1;
pub const REQUEST_LEN: usize = 8;
pub const REPLY_LEN: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum Status {
    Ok = 0,
    /// The device tree lists no block device (or none could be probed).
    NoDevice = 1,
    /// The device's driver has not ended yet: one is in use, or its client just let go and it is
    /// winding down (poll). Closing the manager's channel ends the driver too.
    Busy = 2,
    /// The driver could not be started.
    Failed = 3,
    BadRequest = 4,
}

impl Status {
    pub fn from_i32(v: i32) -> Option<Status> {
        Some(match v {
            0 => Status::Ok,
            1 => Status::NoDevice,
            2 => Status::Busy,
            3 => Status::Failed,
            4 => Status::BadRequest,
            _ => return None,
        })
    }
}

/// Ask for the first block device.
pub fn encode_open_block(txid: u32) -> Vec<u8> {
    encode_open_block_at(txid, 0)
}

/// Ask for the block device number `index` (lowest slot address first).
pub fn encode_open_block_at(txid: u32, index: u16) -> Vec<u8> {
    let mut b = Vec::with_capacity(REQUEST_LEN);
    b.extend_from_slice(&txid.to_le_bytes());
    b.extend_from_slice(&OP_OPEN_BLOCK.to_le_bytes());
    b.extend_from_slice(&index.to_le_bytes());
    b
}

/// `Ok((txid, index))` for an OpenBlock request; `Err(txid if there was one)` otherwise.
pub fn decode_request(b: &[u8]) -> Result<(u32, u16), Option<u32>> {
    let txid = rd_u32(b, 0);
    if b.len() != REQUEST_LEN || rd_u16(b, 4) != Some(OP_OPEN_BLOCK) {
        return Err(txid);
    }
    match (txid, rd_u16(b, 6)) {
        (Some(t), Some(i)) => Ok((t, i)),
        _ => Err(None),
    }
}

pub fn encode_reply(txid: u32, status: Status) -> Vec<u8> {
    let mut b = Vec::with_capacity(REPLY_LEN);
    b.extend_from_slice(&txid.to_le_bytes());
    b.extend_from_slice(&(status as i32).to_le_bytes());
    b
}

pub fn decode_reply(b: &[u8]) -> Option<(u32, Status)> {
    if b.len() != REPLY_LEN {
        return None;
    }
    Some((rd_u32(b, 0)?, Status::from_i32(rd_u32(b, 4)? as i32)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golden_and_round_trip() {
        assert_eq!(encode_open_block(7), [7, 0, 0, 0, 1, 0, 0, 0]);
        assert_eq!(decode_request(&encode_open_block(7)), Ok((7, 0)));
        assert_eq!(encode_open_block_at(7, 3), [7, 0, 0, 0, 1, 0, 3, 0]);
        assert_eq!(decode_request(&encode_open_block_at(7, 3)), Ok((7, 3)));
        assert_eq!(encode_reply(7, Status::Busy), [7, 0, 0, 0, 2, 0, 0, 0]);
        assert_eq!(
            decode_reply(&encode_reply(9, Status::NoDevice)),
            Some((9, Status::NoDevice))
        );
    }

    #[test]
    fn malformed_input_is_an_error_never_a_panic() {
        assert_eq!(decode_request(&[]), Err(None));
        assert_eq!(decode_request(&[1, 2, 3, 4]), Err(Some(0x0403_0201)));
        assert_eq!(decode_request(&[5, 0, 0, 0, 9, 0, 0, 0]), Err(Some(5)));
        assert_eq!(decode_request(&[5, 0, 0, 0, 1, 0, 0, 0, 0]), Err(Some(5)));
        assert_eq!(decode_reply(&[0; 7]), None);
        assert_eq!(decode_reply(&[0, 0, 0, 0, 99, 0, 0, 0]), None);
    }
}
