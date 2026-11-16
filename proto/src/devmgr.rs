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
/// Connect another client: the request carries one handle, the new client's end of a channel, and the manager
/// serves it as one more connection (slice DM). Same layout as OpenBlock; `index` must be 0.
pub const OP_CONNECT: u16 = 2;
/// Claim virtio network device number `index` (S8, NET-SPEC 4.3): same request layout as `OpenBlock`. The reply is a
/// [`NetClaim`] and carries the handles in [`NET_CLAIM_HANDLES`] order. Released when the claimant's connection closes.
pub const OP_CLAIM_NET: u16 = 3;
/// A `ClaimNet` reply: `txid u32 | status i32 | flags u32 | rx_pages u32 | tx_pages u32 | queue_pages u32`.
pub const NET_CLAIM_REPLY_LEN: usize = 24;
/// Flag: the last handle is the device's interrupt.
pub const NET_FLAG_IRQ: u32 = 1;
/// The handles of a successful claim, in order: the MMIO VMO (read-write), the receive-pool DMA VMO, the
/// transmit-pool DMA VMO, the virtqueue DMA VMO (each with the DMA right) and, if `NET_FLAG_IRQ`, the interrupt.
pub const NET_CLAIM_HANDLES: usize = 4;
/// Claim virtio entropy device number `index` (S9 step 5): same request layout. The reply is a [`RngClaim`] and carries the
/// handles in [`RNG_CLAIM_HANDLES`] order (the MMIO VMO, then the DMA VMO). Released when the claimant's connection closes.
pub const OP_CLAIM_RNG: u16 = 4;
/// A `ClaimRng` reply: `txid u32 | status i32 | flags u32 | dma_pages u32`.
pub const RNG_CLAIM_REPLY_LEN: usize = 16;
pub const RNG_CLAIM_HANDLES: usize = 2;
/// Claim the real-time clock device number `index` (the wall clock, FS-SPEC section 7; a `google,goldfish-rtc` on QEMU `virt`):
/// same request layout. The reply is a plain [`Status`] reply and, for `Ok`, carries one handle: a **read-only** VMO of the
/// device's register page. Any number of clients may hold one (reading the time changes nothing).
pub const OP_CLAIM_RTC: u16 = 5;
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
    /// The manager's connection limit is reached (`Connect`).
    Quota = 5,
}

impl Status {
    pub fn from_i32(v: i32) -> Option<Status> {
        Some(match v {
            0 => Status::Ok,
            1 => Status::NoDevice,
            2 => Status::Busy,
            3 => Status::Failed,
            4 => Status::BadRequest,
            5 => Status::Quota,
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

/// A `Connect` request (the client's endpoint travels with it).
pub fn encode_connect(txid: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(REQUEST_LEN);
    b.extend_from_slice(&txid.to_le_bytes());
    b.extend_from_slice(&OP_CONNECT.to_le_bytes());
    b.extend_from_slice(&0u16.to_le_bytes());
    b
}

/// The operation of a request, if the message is long enough to have one.
pub fn op_of(b: &[u8]) -> Option<u16> {
    rd_u16(b, 4)
}

/// `Ok(txid)` for a well-formed `Connect` request.
pub fn decode_connect(b: &[u8]) -> Result<u32, Option<u32>> {
    let txid = rd_u32(b, 0);
    if b.len() != REQUEST_LEN || rd_u16(b, 4) != Some(OP_CONNECT) || rd_u16(b, 6) != Some(0) {
        return Err(txid);
    }
    txid.ok_or(None)
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

/// Ask for network device number `index` (lowest slot address first).
pub fn encode_claim_rng(txid: u32, index: u16) -> Vec<u8> {
    let mut b = Vec::with_capacity(REQUEST_LEN);
    b.extend_from_slice(&txid.to_le_bytes());
    b.extend_from_slice(&OP_CLAIM_RNG.to_le_bytes());
    b.extend_from_slice(&index.to_le_bytes());
    b
}

pub fn decode_claim_rng(b: &[u8]) -> Result<(u32, u16), Option<u32>> {
    let txid = rd_u32(b, 0);
    if b.len() != REQUEST_LEN || rd_u16(b, 4) != Some(OP_CLAIM_RNG) {
        return Err(txid);
    }
    match (txid, rd_u16(b, 6)) {
        (Some(t), Some(i)) => Ok((t, i)),
        _ => Err(None),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RngClaim {
    pub flags: u32,
    pub dma_pages: u32,
}

pub fn encode_claim_rng_reply(txid: u32, status: Status, c: &RngClaim) -> Vec<u8> {
    let mut b = Vec::with_capacity(RNG_CLAIM_REPLY_LEN);
    b.extend_from_slice(&txid.to_le_bytes());
    b.extend_from_slice(&(status as i32).to_le_bytes());
    b.extend_from_slice(&c.flags.to_le_bytes());
    b.extend_from_slice(&c.dma_pages.to_le_bytes());
    b
}

pub fn decode_claim_rng_reply(b: &[u8]) -> Option<(u32, Status, Option<RngClaim>)> {
    if b.len() == REPLY_LEN {
        let (t, s) = decode_reply(b)?;
        return Some((t, s, None));
    }
    if b.len() != RNG_CLAIM_REPLY_LEN {
        return None;
    }
    Some((
        rd_u32(b, 0)?,
        Status::from_i32(rd_u32(b, 4)? as i32)?,
        Some(RngClaim {
            flags: rd_u32(b, 8)?,
            dma_pages: rd_u32(b, 12)?,
        }),
    ))
}

pub fn encode_claim_rtc(txid: u32, index: u16) -> Vec<u8> {
    let mut b = Vec::with_capacity(REQUEST_LEN);
    b.extend_from_slice(&txid.to_le_bytes());
    b.extend_from_slice(&OP_CLAIM_RTC.to_le_bytes());
    b.extend_from_slice(&index.to_le_bytes());
    b
}

pub fn decode_claim_rtc(b: &[u8]) -> Result<(u32, u16), Option<u32>> {
    let txid = rd_u32(b, 0);
    if b.len() != REQUEST_LEN || rd_u16(b, 4) != Some(OP_CLAIM_RTC) {
        return Err(txid);
    }
    match (txid, rd_u16(b, 6)) {
        (Some(t), Some(i)) => Ok((t, i)),
        _ => Err(None),
    }
}

pub fn encode_claim_net(txid: u32, index: u16) -> Vec<u8> {
    let mut b = Vec::with_capacity(REQUEST_LEN);
    b.extend_from_slice(&txid.to_le_bytes());
    b.extend_from_slice(&OP_CLAIM_NET.to_le_bytes());
    b.extend_from_slice(&index.to_le_bytes());
    b
}

/// `Ok((txid, index))` for a well-formed `ClaimNet` request; `Err(txid if there was one)` otherwise.
pub fn decode_claim_net(b: &[u8]) -> Result<(u32, u16), Option<u32>> {
    let txid = rd_u32(b, 0);
    if b.len() != REQUEST_LEN || rd_u16(b, 4) != Some(OP_CLAIM_NET) {
        return Err(txid);
    }
    match (txid, rd_u16(b, 6)) {
        (Some(t), Some(i)) => Ok((t, i)),
        _ => Err(None),
    }
}

/// The sizes a claim reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetClaim {
    pub flags: u32,
    pub rx_pages: u32,
    pub tx_pages: u32,
    pub queue_pages: u32,
}

pub fn encode_claim_net_reply(txid: u32, status: Status, c: &NetClaim) -> Vec<u8> {
    let mut b = Vec::with_capacity(NET_CLAIM_REPLY_LEN);
    b.extend_from_slice(&txid.to_le_bytes());
    b.extend_from_slice(&(status as i32).to_le_bytes());
    for v in [c.flags, c.rx_pages, c.tx_pages, c.queue_pages] {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b
}

/// A `ClaimNet` reply, or the plain 8-byte error reply (`Err` carries it).
pub fn decode_claim_net_reply(b: &[u8]) -> Option<(u32, Status, Option<NetClaim>)> {
    if b.len() == REPLY_LEN {
        let (t, s) = decode_reply(b)?;
        return Some((t, s, None));
    }
    if b.len() != NET_CLAIM_REPLY_LEN {
        return None;
    }
    Some((
        rd_u32(b, 0)?,
        Status::from_i32(rd_u32(b, 4)? as i32)?,
        Some(NetClaim {
            flags: rd_u32(b, 8)?,
            rx_pages: rd_u32(b, 12)?,
            tx_pages: rd_u32(b, 16)?,
            queue_pages: rd_u32(b, 20)?,
        }),
    ))
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
    fn claim_rtc_golden_and_round_trip() {
        assert_eq!(encode_claim_rtc(7, 1), [7, 0, 0, 0, 5, 0, 1, 0]);
        assert_eq!(decode_claim_rtc(&encode_claim_rtc(7, 1)), Ok((7, 1)));
        assert_eq!(decode_claim_rtc(&encode_claim_rng(7, 1)), Err(Some(7)));
        assert_eq!(decode_claim_rng(&encode_claim_rtc(7, 1)), Err(Some(7)));
        assert_eq!(decode_claim_rtc(&[1, 2]), Err(None));
        assert_eq!(decode_claim_rtc(&[1, 0, 0, 0, 5, 0]), Err(Some(1)));
    }

    #[test]
    fn claim_rng_golden_and_round_trip() {
        assert_eq!(encode_claim_rng(7, 1), [7, 0, 0, 0, 4, 0, 1, 0]);
        assert_eq!(decode_claim_rng(&encode_claim_rng(7, 1)), Ok((7, 1)));
        assert_eq!(decode_claim_rng(&encode_claim_net(7, 1)), Err(Some(7)));
        assert_eq!(decode_claim_net(&encode_claim_rng(7, 1)), Err(Some(7)));
        assert_eq!(decode_claim_rng(&[1, 2]), Err(None));
        let c = RngClaim {
            flags: 0,
            dma_pages: 2,
        };
        let r = encode_claim_rng_reply(9, Status::Ok, &c);
        assert_eq!(r.len(), RNG_CLAIM_REPLY_LEN);
        assert_eq!(decode_claim_rng_reply(&r), Some((9, Status::Ok, Some(c))));
        assert_eq!(
            decode_claim_rng_reply(&encode_reply(9, Status::NoDevice)),
            Some((9, Status::NoDevice, None))
        );
        assert_eq!(decode_claim_rng_reply(&r[..15]), None);
    }

    #[test]
    fn claim_net_golden_and_round_trip() {
        assert_eq!(encode_claim_net(7, 1), [7, 0, 0, 0, 3, 0, 1, 0]);
        assert_eq!(decode_claim_net(&encode_claim_net(7, 1)), Ok((7, 1)));
        assert_eq!(
            decode_claim_net(&encode_open_block(7)),
            Err(Some(7)),
            "another op"
        );
        assert_eq!(
            decode_request(&encode_claim_net(7, 0)),
            Err(Some(7)),
            "OpenBlock refuses it too"
        );
        assert_eq!(decode_claim_net(&[1, 2]), Err(None));
        assert_eq!(decode_claim_net(&[5, 0, 0, 0, 3, 0, 0, 0, 0]), Err(Some(5)));
        let c = NetClaim {
            flags: NET_FLAG_IRQ,
            rx_pages: 64,
            tx_pages: 64,
            queue_pages: 2,
        };
        let r = encode_claim_net_reply(9, Status::Ok, &c);
        assert_eq!(r.len(), NET_CLAIM_REPLY_LEN);
        assert_eq!(decode_claim_net_reply(&r), Some((9, Status::Ok, Some(c))));
        assert_eq!(
            decode_claim_net_reply(&encode_reply(9, Status::NoDevice)),
            Some((9, Status::NoDevice, None)),
            "an error is the short reply"
        );
        assert_eq!(decode_claim_net_reply(&[0; 12]), None);
        assert_eq!(decode_claim_net_reply(&r[..23]), None);
    }

    #[test]
    fn malformed_input_is_an_error_never_a_panic() {
        // Connect: same layout, its own op, index 0
        assert_eq!(decode_connect(&encode_connect(9)), Ok(9));
        assert_eq!(op_of(&encode_connect(9)), Some(OP_CONNECT));
        assert_eq!(decode_connect(&encode_open_block_at(9, 0)), Err(Some(9)));
        assert_eq!(decode_request(&encode_connect(9)), Err(Some(9)));
        let mut bad = encode_connect(9);
        bad[6] = 1; // a nonzero index
        assert_eq!(decode_connect(&bad), Err(Some(9)));
        assert_eq!(decode_connect(&[]), Err(None));
        assert_eq!(Status::from_i32(5), Some(Status::Quota));
        assert_eq!(decode_request(&[]), Err(None));
        assert_eq!(decode_request(&[1, 2, 3, 4]), Err(Some(0x0403_0201)));
        assert_eq!(decode_request(&[5, 0, 0, 0, 9, 0, 0, 0]), Err(Some(5)));
        assert_eq!(decode_request(&[5, 0, 0, 0, 1, 0, 0, 0, 0]), Err(Some(5)));
        assert_eq!(decode_reply(&[0; 7]), None);
        assert_eq!(decode_reply(&[0, 0, 0, 0, 99, 0, 0, 0]), None);
    }
}
