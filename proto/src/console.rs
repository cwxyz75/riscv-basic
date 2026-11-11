//! The console control protocol (S4-SPEC section 8): the shell registers the foreground job
//! (`KILL|WAIT|TRANSFER` process handles) so Ctrl-C can end it.

use crate::{rd_u16, rd_u32};
use alloc::vec::Vec;

pub const OP_SET_FOREGROUND: u16 = 1;
pub const OP_CLEAR_FOREGROUND: u16 = 2;
/// At most this many job handles in one `SetForeground`.
pub const MAX_JOBS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// The handles of the message (1..=MAX_JOBS) are the foreground set.
    SetForeground,
    ClearForeground,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlError {
    TooShort,
    BadOp,
    /// Handle count does not match the operation.
    BadHandles,
}

pub fn encode(txid: u32, c: Control) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&txid.to_le_bytes());
    let op = match c {
        Control::SetForeground => OP_SET_FOREGROUND,
        Control::ClearForeground => OP_CLEAR_FOREGROUND,
    };
    out.extend_from_slice(&op.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// Decode a control message that arrived with `nhandles` handles.
pub fn decode(b: &[u8], nhandles: usize) -> Result<(u32, Control), ControlError> {
    if b.len() != 8 {
        return Err(ControlError::TooShort);
    }
    let txid = rd_u32(b, 0).ok_or(ControlError::TooShort)?;
    if rd_u16(b, 6) != Some(0) {
        return Err(ControlError::BadOp);
    }
    match rd_u16(b, 4) {
        Some(OP_SET_FOREGROUND) if (1..=MAX_JOBS).contains(&nhandles) => {
            Ok((txid, Control::SetForeground))
        }
        Some(OP_CLEAR_FOREGROUND) if nhandles == 0 => Ok((txid, Control::ClearForeground)),
        Some(OP_SET_FOREGROUND) | Some(OP_CLEAR_FOREGROUND) => Err(ControlError::BadHandles),
        _ => Err(ControlError::BadOp),
    }
}
