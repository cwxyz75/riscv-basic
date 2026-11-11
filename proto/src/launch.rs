//! The launcher protocol (S4-SPEC section 6): `Spawn` carries the image's extent in a VMO
//! (element 0 of the message's handles) and the startup message to deliver to the child.

use crate::{rd_u16, rd_u32, rd_u64, startup};
use alloc::vec::Vec;

pub const OP_SPAWN: u16 = 1;
pub const MAX_IMAGE: u64 = 4 * 1024 * 1024;
/// Handles a `Spawn` may carry: the image VMO plus at most the startup message's.
pub const MAX_HANDLES: usize = 1 + startup::MAX_HANDLES;
pub const HEADER: usize = 28;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spawn<'a> {
    pub txid: u32,
    /// Where the ELF file starts in the image VMO, and its length.
    pub image_offset: u64,
    pub image_len: u64,
    /// The encoded startup message for the child (`startup::Startup::encode`).
    pub startup: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchError {
    TooShort,
    BadOp,
    BadImage,
    BadStartup,
    BadLength,
}

impl Spawn<'_> {
    pub fn encode(&self) -> Result<Vec<u8>, LaunchError> {
        if self.image_len == 0 || self.image_len > MAX_IMAGE {
            return Err(LaunchError::BadImage);
        }
        if self.startup.len() > startup::MAX_BYTES {
            return Err(LaunchError::BadStartup);
        }
        let mut out = Vec::new();
        out.extend_from_slice(&self.txid.to_le_bytes());
        out.extend_from_slice(&OP_SPAWN.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&self.image_offset.to_le_bytes());
        out.extend_from_slice(&self.image_len.to_le_bytes());
        out.extend_from_slice(&(self.startup.len() as u32).to_le_bytes());
        out.extend_from_slice(self.startup);
        Ok(out)
    }
}

pub fn decode_spawn(b: &[u8]) -> Result<Spawn<'_>, LaunchError> {
    if b.len() < HEADER {
        return Err(LaunchError::TooShort);
    }
    if rd_u16(b, 4) != Some(OP_SPAWN) || rd_u16(b, 6) != Some(0) {
        return Err(LaunchError::BadOp);
    }
    let image_offset = rd_u64(b, 8).ok_or(LaunchError::TooShort)?;
    let image_len = rd_u64(b, 16).ok_or(LaunchError::TooShort)?;
    if image_len == 0 || image_len > MAX_IMAGE || image_offset.checked_add(image_len).is_none() {
        return Err(LaunchError::BadImage);
    }
    let slen = rd_u32(b, 24).ok_or(LaunchError::TooShort)? as usize;
    if slen > startup::MAX_BYTES {
        return Err(LaunchError::BadStartup);
    }
    if b.len() - HEADER != slen {
        return Err(LaunchError::BadLength);
    }
    Ok(Spawn {
        txid: rd_u32(b, 0).ok_or(LaunchError::TooShort)?,
        image_offset,
        image_len,
        startup: &b[HEADER..],
    })
}

/// The launcher's reply: `txid:u32, status:i32` (and the process handle in the handle array
/// on success).
pub fn encode_reply(txid: u32, status: i32) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&txid.to_le_bytes());
    out.extend_from_slice(&status.to_le_bytes());
    out
}

pub fn decode_reply(b: &[u8]) -> Option<(u32, i32)> {
    if b.len() != 8 {
        return None;
    }
    Some((rd_u32(b, 0)?, rd_u32(b, 4)? as i32))
}
