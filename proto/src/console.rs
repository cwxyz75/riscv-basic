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

/// The server's reply to a control message: `txid:u32, status:i32` (0 = done).
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_round_trip_and_handle_counts() {
        let m = encode(7, Control::SetForeground);
        assert_eq!(decode(&m, 1), Ok((7, Control::SetForeground)));
        assert_eq!(decode(&m, MAX_JOBS), Ok((7, Control::SetForeground)));
        assert_eq!(decode(&m, 0), Err(ControlError::BadHandles));
        assert_eq!(decode(&m, MAX_JOBS + 1), Err(ControlError::BadHandles));
        let c = encode(9, Control::ClearForeground);
        assert_eq!(decode(&c, 0), Ok((9, Control::ClearForeground)));
        assert_eq!(decode(&c, 1), Err(ControlError::BadHandles));
    }

    #[test]
    fn malformed_control_messages_are_errors() {
        let m = encode(1, Control::SetForeground);
        for n in 0..m.len() {
            assert_eq!(decode(&m[..n], 1), Err(ControlError::TooShort));
        }
        let mut long = m.clone();
        long.push(0);
        assert_eq!(decode(&long, 1), Err(ControlError::TooShort));
        let mut bad_op = m.clone();
        bad_op[4] = 99;
        assert_eq!(decode(&bad_op, 1), Err(ControlError::BadOp));
        let mut reserved = m;
        reserved[6] = 1;
        assert_eq!(decode(&reserved, 1), Err(ControlError::BadOp));
    }

    #[test]
    fn decode_never_panics_on_any_mutation() {
        let base = encode(0x0102_0304, Control::SetForeground);
        for i in 0..base.len() {
            for v in 0..=255u8 {
                let mut m = base.clone();
                m[i] = v;
                for nh in [0usize, 1, 16, 17] {
                    let _ = decode(&m, nh);
                }
            }
        }
        for n in 0..32 {
            let _ = decode(&alloc::vec![0xa5u8; n], 1);
        }
    }

    #[test]
    fn reply_round_trip() {
        assert_eq!(decode_reply(&encode_reply(5, -3)), Some((5, -3)));
        assert_eq!(decode_reply(&[0; 7]), None);
        assert_eq!(decode_reply(&[0; 9]), None);
    }
}
