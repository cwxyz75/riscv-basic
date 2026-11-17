//! The spawn protocol (UV8, OD118): how a program asks the shell that started it to start another program.
//!
//! One request, one reply, on the channel a program holds as `Role::ServiceSpawn` (it exists only if the starter granted it).
//! The request carries the argument vector, the extra environment and exactly three handles: the new program's standard input,
//! output and error (the requester passes duplicates; what it does not hold it cannot give). The reply carries the status and,
//! on success, a process handle (WAIT, INSPECT, KILL, TRANSFER) the requester waits on and reads the exit code from.
//!
//! Request: `txid:u32, op:u16 (1), reserved:u16 (0), nargs:u16, nenv:u16`, then the program's name, `nargs` argument strings
//! (`argv`, whose first is what the program sees as its own name: an applet name for `busybox`, say) and `nenv` environment
//! strings, each `len:u16` and that many bytes. Reply: `txid:u32, status:i32` (the launcher's reply shape).

use crate::{rd_u16, rd_u32};
use alloc::vec::Vec;

pub const OP_SPAWN: u16 = 1;
/// Send a signal to a program in the asker's chain: `txid:u32, op:u16 (2), reserved:u16 (0), pid:u32, sig:u32`.
pub const OP_SIGNAL: u16 = 2;
pub const SIGNAL_BYTES: usize = 16;
pub const HEADER: usize = 12;
/// The handles a request carries: stdin, stdout, stderr.
pub const HANDLES: usize = 3;
pub const MAX_ARGS: usize = 256;
pub const MAX_ENV: usize = 256;
pub const MAX_STRING: usize = 4096;
/// The whole request, so a hostile one cannot make the shell allocate without bound.
pub const MAX_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request<'a> {
    pub txid: u32,
    /// The program, by the name the shell knows commands by.
    pub prog: &'a [u8],
    pub args: Vec<&'a [u8]>,
    pub env: Vec<&'a [u8]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnError {
    TooShort,
    BadOp,
    TooMany,
    TooBig,
    /// Trailing bytes, or no program name.
    BadLength,
}

pub fn encode(
    txid: u32,
    prog: &[u8],
    args: &[&[u8]],
    env: &[&[u8]],
) -> Result<Vec<u8>, SpawnError> {
    if prog.is_empty() {
        return Err(SpawnError::BadLength);
    }
    if args.len() > MAX_ARGS || env.len() > MAX_ENV {
        return Err(SpawnError::TooMany);
    }
    let mut out = Vec::new();
    out.extend_from_slice(&txid.to_le_bytes());
    out.extend_from_slice(&OP_SPAWN.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(args.len() as u16).to_le_bytes());
    out.extend_from_slice(&(env.len() as u16).to_le_bytes());
    for s in core::iter::once(&prog).chain(args).chain(env) {
        if s.len() > MAX_STRING {
            return Err(SpawnError::TooBig);
        }
        out.extend_from_slice(&(s.len() as u16).to_le_bytes());
        out.extend_from_slice(s);
    }
    if out.len() > MAX_BYTES {
        return Err(SpawnError::TooBig);
    }
    Ok(out)
}

pub fn decode(b: &[u8]) -> Result<Request<'_>, SpawnError> {
    if b.len() < HEADER {
        return Err(SpawnError::TooShort);
    }
    if b.len() > MAX_BYTES {
        return Err(SpawnError::TooBig);
    }
    if rd_u16(b, 4) != Some(OP_SPAWN) || rd_u16(b, 6) != Some(0) {
        return Err(SpawnError::BadOp);
    }
    let nargs = rd_u16(b, 8).ok_or(SpawnError::TooShort)? as usize;
    let nenv = rd_u16(b, 10).ok_or(SpawnError::TooShort)? as usize;
    if nargs > MAX_ARGS || nenv > MAX_ENV {
        return Err(SpawnError::TooMany);
    }
    let mut at = HEADER;
    let take = |at: &mut usize| -> Result<&[u8], SpawnError> {
        let n = rd_u16(b, *at).ok_or(SpawnError::TooShort)? as usize;
        if n > MAX_STRING {
            return Err(SpawnError::TooBig);
        }
        let s = b.get(*at + 2..*at + 2 + n).ok_or(SpawnError::TooShort)?;
        *at += 2 + n;
        Ok(s)
    };
    let prog = take(&mut at)?;
    if prog.is_empty() {
        return Err(SpawnError::BadLength);
    }
    let mut args = Vec::with_capacity(nargs);
    for _ in 0..nargs {
        args.push(take(&mut at)?);
    }
    let mut env = Vec::with_capacity(nenv);
    for _ in 0..nenv {
        env.push(take(&mut at)?);
    }
    if at != b.len() {
        return Err(SpawnError::BadLength);
    }
    Ok(Request {
        txid: rd_u32(b, 0).ok_or(SpawnError::TooShort)?,
        prog,
        args,
        env,
    })
}

/// The reply to a spawn: `txid:u32, status:i32, pid:u32` (the new program's pid, 0 on failure), and on success its process
/// handle. The reply to a signal request is the same shape with pid 0.
pub fn encode_reply(txid: u32, status: i32, pid: u32) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&txid.to_le_bytes());
    out.extend_from_slice(&status.to_le_bytes());
    out.extend_from_slice(&pid.to_le_bytes());
    out
}

pub fn decode_reply(b: &[u8]) -> Option<(u32, i32, u32)> {
    if b.len() != 12 {
        return None;
    }
    Some((rd_u32(b, 0)?, rd_u32(b, 4)? as i32, rd_u32(b, 8)?))
}

pub fn encode_signal(txid: u32, pid: u32, sig: u32) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&txid.to_le_bytes());
    out.extend_from_slice(&OP_SIGNAL.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&pid.to_le_bytes());
    out.extend_from_slice(&sig.to_le_bytes());
    out
}

/// A request a program sends the shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message<'a> {
    Spawn(Request<'a>),
    Signal { txid: u32, pid: u32, sig: u32 },
}

pub fn decode_message(b: &[u8]) -> Result<Message<'_>, SpawnError> {
    if rd_u16(b, 4) == Some(OP_SIGNAL) {
        if b.len() != SIGNAL_BYTES || rd_u16(b, 6) != Some(0) {
            return Err(SpawnError::BadLength);
        }
        return Ok(Message::Signal {
            txid: rd_u32(b, 0).ok_or(SpawnError::TooShort)?,
            pid: rd_u32(b, 8).ok_or(SpawnError::TooShort)?,
            sig: rd_u32(b, 12).ok_or(SpawnError::TooShort)?,
        });
    }
    decode(b).map(Message::Spawn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let b = encode(7, b"busybox", &[b"echo", b"hi"], &[b"A=1"]).unwrap();
        let r = decode(&b).unwrap();
        assert_eq!(r.txid, 7);
        assert_eq!(r.prog, b"busybox");
        assert_eq!(r.args, [&b"echo"[..], b"hi"]);
        assert_eq!(r.env, [&b"A=1"[..]]);
    }

    #[test]
    fn refuses_what_is_malformed() {
        assert_eq!(encode(1, b"", &[], &[]), Err(SpawnError::BadLength));
        let good = encode(1, b"x", &[b"x"], &[]).unwrap();
        for cut in 0..good.len() {
            assert!(decode(&good[..cut]).is_err(), "cut {cut}");
        }
        let mut longer = good.clone();
        longer.push(0);
        assert_eq!(decode(&longer), Err(SpawnError::BadLength));
        let mut op = good.clone();
        op[4] = 2;
        assert_eq!(decode(&op), Err(SpawnError::BadOp));
        // a request with no argv is fine (the program's name is its own); one with an empty program name is not
        assert_eq!(
            decode(&encode(1, b"x", &[], &[]).unwrap())
                .unwrap()
                .args
                .len(),
            0
        );
        let mut noname = good.clone();
        noname[12] = 0;
        noname[13] = 0;
        assert!(decode(&noname).is_err());
        let big = alloc::vec![0u8; MAX_STRING + 1];
        assert_eq!(encode(1, b"x", &[&big], &[]), Err(SpawnError::TooBig));
    }

    #[test]
    fn never_panics_on_mutations() {
        let good = encode(9, b"find", &[b"find", b"-name", b"a"], &[b"K=V", b"X="]).unwrap();
        for i in 0..good.len() {
            for v in [0u8, 1, 0x7f, 0xff] {
                let mut m = good.clone();
                m[i] = v;
                let _ = decode(&m);
            }
        }
    }

    #[test]
    fn replies_and_signals_round_trip() {
        assert_eq!(decode_reply(&encode_reply(5, -3, 9)), Some((5, -3, 9)));
        assert_eq!(decode_reply(&[0; 8]), None);
        let m = encode_signal(4, 77, 15);
        assert_eq!(
            decode_message(&m),
            Ok(Message::Signal {
                txid: 4,
                pid: 77,
                sig: 15
            })
        );
        for cut in 0..m.len() {
            assert!(decode_message(&m[..cut]).is_err(), "cut {cut}");
        }
        let mut longer = m.clone();
        longer.push(0);
        assert!(decode_message(&longer).is_err());
        let sp = encode(1, b"x", &[b"x"], &[]).unwrap();
        assert!(matches!(decode_message(&sp), Ok(Message::Spawn(_))));
    }
}
