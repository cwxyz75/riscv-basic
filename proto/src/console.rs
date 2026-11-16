//! The console control protocol (S4-SPEC section 8): the shell registers the foreground job
//! (`KILL|WAIT|TRANSFER` process handles) so Ctrl-C can end it.

use crate::{rd_u16, rd_u32};
use alloc::vec::Vec;

pub const OP_SET_FOREGROUND: u16 = 1;
pub const OP_CLEAR_FOREGROUND: u16 = 2;
/// Terminal modes, the window size and events (USER-VIEW-SPEC section 9, step UV4).
pub const OP_SET_MODE: u16 = 3;
pub const OP_GET_MODE: u16 = 4;
pub const OP_SET_SIZE: u16 = 5;
pub const OP_GET_SIZE: u16 = 6;
pub const OP_QUERY_SIZE: u16 = 7;
/// At most this many job handles in one `SetForeground`.
pub const MAX_JOBS: usize = 16;

/// `Mode` flags (the termios essentials): `ICANON` edits and delivers whole lines in the terminal server; `ECHO` echoes
/// input; `ISIG` turns Ctrl-C into an interrupt event and a kill of the foreground job instead of a byte (in canonical mode
/// Ctrl-C always interrupts). Cooked is all three; raw is none (bytes are delivered as they arrive, escape sequences
/// included); cbreak is `ECHO | ISIG`.
pub mod mode {
    pub const ICANON: u8 = 1;
    pub const ECHO: u8 = 2;
    pub const ISIG: u8 = 4;
    pub const ALL: u8 = 7;
    pub const COOKED: u8 = ALL;
    pub const RAW: u8 = 0;
    pub const CBREAK: u8 = ECHO | ISIG;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// The handles of the message (1..=MAX_JOBS) are the foreground set.
    SetForeground,
    ClearForeground,
    /// Set the terminal mode (`mode` flags).
    SetMode(u8),
    /// Reply value: the mode flags.
    GetMode,
    /// Set the window size (the owner knows it: a backend that reports it, or a user who said so).
    SetSize {
        cols: u16,
        rows: u16,
    },
    /// Reply value: `cols | rows << 16`.
    GetSize,
    /// Ask the host terminal how big it is (a cursor-position query on the serial line); the answer arrives later as
    /// [`Event::Resize`].
    QuerySize,
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
    let (op, arg) = match c {
        Control::SetForeground => (OP_SET_FOREGROUND, 0),
        Control::ClearForeground => (OP_CLEAR_FOREGROUND, 0),
        Control::SetMode(m) => (OP_SET_MODE, u16::from(m)),
        Control::GetMode => (OP_GET_MODE, 0),
        Control::SetSize { .. } => (OP_SET_SIZE, 0),
        Control::GetSize => (OP_GET_SIZE, 0),
        Control::QuerySize => (OP_QUERY_SIZE, 0),
    };
    out.extend_from_slice(&op.to_le_bytes());
    out.extend_from_slice(&arg.to_le_bytes());
    if let Control::SetSize { cols, rows } = c {
        out.extend_from_slice(&cols.to_le_bytes());
        out.extend_from_slice(&rows.to_le_bytes());
    }
    out
}

/// Decode a control message that arrived with `nhandles` handles.
pub fn decode(b: &[u8], nhandles: usize) -> Result<(u32, Control), ControlError> {
    if b.len() < 8 {
        return Err(ControlError::TooShort);
    }
    let txid = rd_u32(b, 0).ok_or(ControlError::TooShort)?;
    let op = rd_u16(b, 4).ok_or(ControlError::TooShort)?;
    let arg = rd_u16(b, 6).ok_or(ControlError::TooShort)?;
    let want_len = if op == OP_SET_SIZE { 12 } else { 8 };
    if b.len() != want_len {
        return Err(ControlError::TooShort);
    }
    // only `SetMode` uses the argument; everywhere else it is reserved and zero
    if arg != 0 && op != OP_SET_MODE {
        return Err(ControlError::BadOp);
    }
    let c = match op {
        OP_SET_FOREGROUND | OP_CLEAR_FOREGROUND => {
            let ok = if op == OP_SET_FOREGROUND {
                (1..=MAX_JOBS).contains(&nhandles)
            } else {
                nhandles == 0
            };
            if !ok {
                return Err(ControlError::BadHandles);
            }
            if op == OP_SET_FOREGROUND {
                Control::SetForeground
            } else {
                Control::ClearForeground
            }
        }
        OP_SET_MODE | OP_GET_MODE | OP_SET_SIZE | OP_GET_SIZE | OP_QUERY_SIZE if nhandles != 0 => {
            return Err(ControlError::BadHandles)
        }
        OP_SET_MODE => {
            if arg > u16::from(mode::ALL) {
                return Err(ControlError::BadOp);
            }
            Control::SetMode(arg as u8)
        }
        OP_GET_MODE => Control::GetMode,
        OP_SET_SIZE => {
            let cols = rd_u16(b, 8).ok_or(ControlError::TooShort)?;
            let rows = rd_u16(b, 10).ok_or(ControlError::TooShort)?;
            if cols == 0 || rows == 0 {
                return Err(ControlError::BadOp);
            }
            Control::SetSize { cols, rows }
        }
        OP_GET_SIZE => Control::GetSize,
        OP_QUERY_SIZE => Control::QuerySize,
        _ => return Err(ControlError::BadOp),
    };
    Ok((txid, c))
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

/// The reply to `GetMode` and `GetSize`: `txid:u32, status:i32, value:u32`.
pub fn encode_reply_value(txid: u32, status: i32, value: u32) -> Vec<u8> {
    let mut out = encode_reply(txid, status);
    out.extend_from_slice(&value.to_le_bytes());
    out
}

pub fn decode_reply_value(b: &[u8]) -> Option<(u32, i32, u32)> {
    if b.len() != 12 {
        return None;
    }
    Some((rd_u32(b, 0)?, rd_u32(b, 4)? as i32, rd_u32(b, 8)?))
}

/// The txid of an event: never a request's (the owner's requests count up from 1).
pub const EVENT_TXID: u32 = 0xFFFF_FFFF;

/// What the terminal tells its owner on the control channel, unasked. Control events are protocol messages, not signals
/// (USER-VIEW-SPEC section 9, item 3): **interrupt** (Ctrl-C with signals on and the terminal not in canonical mode: the
/// foreground job has been killed and no byte is delivered), **hangup** (the backend is gone: the serial line closed) and
/// **resize** (the window size changed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Resize { cols: u16, rows: u16 },
    Interrupt,
    Hangup,
}

pub fn encode_event(e: Event) -> Vec<u8> {
    let (kind, a, b) = match e {
        Event::Resize { cols, rows } => (1u16, cols, rows),
        Event::Interrupt => (2, 0, 0),
        Event::Hangup => (3, 0, 0),
    };
    let mut out = Vec::new();
    out.extend_from_slice(&EVENT_TXID.to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&a.to_le_bytes());
    out.extend_from_slice(&b.to_le_bytes());
    out
}

/// `None` for anything that is not an event (a reply, a malformed message).
pub fn decode_event(b: &[u8]) -> Option<Event> {
    if b.len() != 10 || rd_u32(b, 0)? != EVENT_TXID {
        return None;
    }
    let (kind, a, c) = (rd_u16(b, 4)?, rd_u16(b, 6)?, rd_u16(b, 8)?);
    match (kind, a, c) {
        (1, cols, rows) if cols != 0 && rows != 0 => Some(Event::Resize { cols, rows }),
        (2, 0, 0) => Some(Event::Interrupt),
        (3, 0, 0) => Some(Event::Hangup),
        _ => None,
    }
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

    #[test]
    fn the_terminal_controls_round_trip_with_goldens() {
        assert_eq!(
            encode(3, Control::SetMode(mode::RAW)),
            [3, 0, 0, 0, 3, 0, 0, 0]
        );
        assert_eq!(
            encode(3, Control::SetMode(mode::COOKED)),
            [3, 0, 0, 0, 3, 0, 7, 0]
        );
        assert_eq!(
            decode(&encode(3, Control::SetMode(mode::CBREAK)), 0),
            Ok((3, Control::SetMode(6)))
        );
        assert_eq!(encode(4, Control::GetMode), [4, 0, 0, 0, 4, 0, 0, 0]);
        assert_eq!(encode(5, Control::QuerySize), [5, 0, 0, 0, 7, 0, 0, 0]);
        let m = encode(
            6,
            Control::SetSize {
                cols: 132,
                rows: 43,
            },
        );
        assert_eq!(m, [6, 0, 0, 0, 5, 0, 0, 0, 132, 0, 43, 0]);
        assert_eq!(
            decode(&m, 0),
            Ok((
                6,
                Control::SetSize {
                    cols: 132,
                    rows: 43
                }
            ))
        );
        assert_eq!(
            decode(&encode(7, Control::GetSize), 0),
            Ok((7, Control::GetSize))
        );
        assert_eq!(
            decode(&encode(8, Control::GetMode), 0),
            Ok((8, Control::GetMode))
        );
        assert_eq!(
            decode(&encode(9, Control::QuerySize), 0),
            Ok((9, Control::QuerySize))
        );
    }

    #[test]
    fn the_terminal_controls_refuse_what_makes_no_sense() {
        // a mode with an undefined bit, a zero size, a handle where none belongs, a short or long SetSize
        assert_eq!(
            decode(&[1, 0, 0, 0, 3, 0, 8, 0], 0),
            Err(ControlError::BadOp)
        );
        assert_eq!(
            decode(&encode(1, Control::SetSize { cols: 0, rows: 5 }), 0),
            Err(ControlError::BadOp)
        );
        assert_eq!(
            decode(&encode(1, Control::SetSize { cols: 5, rows: 0 }), 0),
            Err(ControlError::BadOp)
        );
        assert_eq!(
            decode(&encode(1, Control::GetSize), 1),
            Err(ControlError::BadHandles)
        );
        let full = encode(1, Control::SetSize { cols: 5, rows: 5 });
        for n in 0..full.len() {
            assert!(decode(&full[..n], 0).is_err(), "{n}");
        }
        // the reserved argument is zero except in SetMode
        assert_eq!(
            decode(&[1, 0, 0, 0, 6, 0, 1, 0], 0),
            Err(ControlError::BadOp)
        );
        assert_eq!(
            decode(&[1, 0, 0, 0, 2, 0, 1, 0], 0),
            Err(ControlError::BadOp)
        );
    }

    #[test]
    fn value_replies_and_events() {
        let r = encode_reply_value(5, 0, 80 | 24 << 16);
        assert_eq!(r, [5, 0, 0, 0, 0, 0, 0, 0, 80, 0, 24, 0]);
        assert_eq!(decode_reply_value(&r), Some((5, 0, 80 | 24 << 16)));
        assert_eq!(decode_reply_value(&r[..8]), None);
        assert_eq!(
            decode_reply(&r),
            None,
            "a plain reply is exactly eight bytes"
        );
        // events carry a txid no request has, and the three kinds are distinguishable
        let rs = encode_event(Event::Resize {
            cols: 100,
            rows: 30,
        });
        assert_eq!(rs, [0xff, 0xff, 0xff, 0xff, 1, 0, 100, 0, 30, 0]);
        let (i, h) = (encode_event(Event::Interrupt), encode_event(Event::Hangup));
        assert_eq!(i, [0xff, 0xff, 0xff, 0xff, 2, 0, 0, 0, 0, 0]);
        assert_eq!(h, [0xff, 0xff, 0xff, 0xff, 3, 0, 0, 0, 0, 0]);
        assert_ne!(i, h);
        for (b, e) in [
            (
                &rs,
                Event::Resize {
                    cols: 100,
                    rows: 30,
                },
            ),
            (&i, Event::Interrupt),
            (&h, Event::Hangup),
        ] {
            assert_eq!(decode_event(b), Some(e));
            for n in 0..b.len() {
                assert_eq!(decode_event(&b[..n]), None);
            }
        }
        assert_eq!(
            decode_event(&encode_reply(5, 0)),
            None,
            "a reply is not an event"
        );
        assert_eq!(
            decode_event(&[0xff, 0xff, 0xff, 0xff, 1, 0, 0, 0, 5, 0]),
            None,
            "a zero-width resize"
        );
        assert_eq!(
            decode_event(&[0xff, 0xff, 0xff, 0xff, 9, 0, 0, 0, 0, 0]),
            None
        );
    }
}
