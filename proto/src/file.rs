//! The file protocol (S4-SPEC section 7): framing of requests and replies, operation and
//! status numbers, rights, and path validation. The server logic lives in `memfs`.

use crate::{rd_u16, rd_u32, rd_u64};
use alloc::vec::Vec;

pub const REQUEST_HEADER: usize = 32;
pub const REPLY_HEADER: usize = 20;
/// Largest `Read`/`Write` data.
pub const MAX_IO: usize = 32 * 1024;
/// Largest whole message: 33 KiB (S4-SPEC section 7), header plus payload of at most 32 KiB.
pub const MAX_MESSAGE: usize = 33 * 1024;
pub const MAX_COMPONENT: usize = 255;
pub const MAX_COMPONENTS: usize = 32;
pub const MAX_PATH: usize = 1024;

/// Protocol rights (not kernel rights).
pub mod rights {
    pub const READ: u32 = 1;
    pub const WRITE: u32 = 2;
    pub const ENUMERATE: u32 = 4;
    pub const CREATE: u32 = 8;
    pub const ALL: u32 = 15;
}

/// `Open` flags.
pub mod flags {
    pub const CREATE: u16 = 1;
    pub const EXCLUSIVE: u16 = 2;
    pub const TRUNCATE: u16 = 4;
    pub const DIRECTORY: u16 = 8;
    pub const ALL: u16 = 15;
}

/// Operations.
pub mod op {
    pub const OPEN: u16 = 1;
    pub const CLONE: u16 = 2;
    pub const READDIR: u16 = 3;
    pub const MKDIR: u16 = 4;
    pub const UNLINK: u16 = 5;
    pub const STAT: u16 = 6;
    pub const READ: u16 = 10;
    pub const WRITE: u16 = 11;
    pub const GETSIZE: u16 = 12;
    pub const SETSIZE: u16 = 13;
    pub const FSTAT: u16 = 14;
    pub const GETVMO: u16 = 15;

    /// How many handles a request with this op carries (`None`: unknown op).
    pub fn handles(op: u16) -> Option<usize> {
        match op {
            OPEN | CLONE => Some(1),
            READDIR | MKDIR | UNLINK | STAT | READ | WRITE | GETSIZE | SETSIZE | FSTAT | GETVMO => {
                Some(0)
            }
            _ => None,
        }
    }

    /// Does this op belong to a directory connection (else a file connection)?
    pub fn is_directory_op(op: u16) -> bool {
        matches!(op, OPEN | CLONE | READDIR | MKDIR | UNLINK | STAT)
    }
}

/// Protocol statuses beyond the kernel's (SYSCALLS.md): negative `i32`.
pub mod status {
    pub const NOT_FOUND: i32 = -100;
    pub const EXISTS: i32 = -101;
    pub const NOT_EMPTY: i32 = -102;
    pub const IS_DIRECTORY: i32 = -103;
    pub const NOT_DIRECTORY: i32 = -104;
    pub const NAME_TOO_LONG: i32 = -105;
    pub const QUOTA: i32 = -106;
    pub const BAD_ELF: i32 = -107;
    pub const BAD_REQUEST: i32 = -108;
}

/// A request, borrowing its payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request<'a> {
    pub txid: u32,
    pub op: u16,
    pub flags: u16,
    pub rights: u32,
    pub a: u64,
    pub b: u64,
    pub payload: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    TooShort,
    TooBig,
    /// A flag or right bit that no operation defines.
    BadBits,
    /// The length field disagrees with the bytes that follow.
    BadLength,
}

impl Request<'_> {
    pub fn encode(&self) -> Result<Vec<u8>, FrameError> {
        if self.payload.len() > MAX_IO {
            return Err(FrameError::TooBig);
        }
        let mut out = Vec::with_capacity(REQUEST_HEADER + self.payload.len());
        out.extend_from_slice(&self.txid.to_le_bytes());
        out.extend_from_slice(&self.op.to_le_bytes());
        out.extend_from_slice(&self.flags.to_le_bytes());
        out.extend_from_slice(&self.rights.to_le_bytes());
        out.extend_from_slice(&self.a.to_le_bytes());
        out.extend_from_slice(&self.b.to_le_bytes());
        out.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());
        out.extend_from_slice(self.payload);
        Ok(out)
    }
}

/// Decode a request. The `txid` is returned separately by [`peek_txid`] when this fails, so
/// a server can still send `BAD_REQUEST`.
pub fn decode_request(b: &[u8]) -> Result<Request<'_>, FrameError> {
    if b.len() > MAX_MESSAGE {
        return Err(FrameError::TooBig);
    }
    if b.len() < REQUEST_HEADER {
        return Err(FrameError::TooShort);
    }
    let len = rd_u32(b, 28).ok_or(FrameError::TooShort)? as usize;
    if b.len() - REQUEST_HEADER != len {
        return Err(FrameError::BadLength);
    }
    if len > MAX_IO {
        return Err(FrameError::TooBig);
    }
    let flags = rd_u16(b, 6).ok_or(FrameError::TooShort)?;
    let rights = rd_u32(b, 8).ok_or(FrameError::TooShort)?;
    if flags & !flags::ALL != 0 || rights & !rights::ALL != 0 {
        return Err(FrameError::BadBits);
    }
    Ok(Request {
        txid: rd_u32(b, 0).ok_or(FrameError::TooShort)?,
        op: rd_u16(b, 4).ok_or(FrameError::TooShort)?,
        flags,
        rights,
        a: rd_u64(b, 12).ok_or(FrameError::TooShort)?,
        b: rd_u64(b, 20).ok_or(FrameError::TooShort)?,
        payload: &b[REQUEST_HEADER..],
    })
}

/// The transaction id of a request, if the message is long enough to have one.
pub fn peek_txid(b: &[u8]) -> Option<u32> {
    rd_u32(b, 0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply<'a> {
    pub txid: u32,
    pub status: i32,
    pub a: u64,
    pub payload: &'a [u8],
}

impl Reply<'_> {
    pub fn encode(&self) -> Result<Vec<u8>, FrameError> {
        if self.payload.len() > MAX_IO {
            return Err(FrameError::TooBig);
        }
        let mut out = Vec::with_capacity(REPLY_HEADER + self.payload.len());
        out.extend_from_slice(&self.txid.to_le_bytes());
        out.extend_from_slice(&self.status.to_le_bytes());
        out.extend_from_slice(&self.a.to_le_bytes());
        out.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());
        out.extend_from_slice(self.payload);
        Ok(out)
    }
}

pub fn decode_reply(b: &[u8]) -> Result<Reply<'_>, FrameError> {
    if b.len() > MAX_MESSAGE {
        return Err(FrameError::TooBig);
    }
    if b.len() < REPLY_HEADER {
        return Err(FrameError::TooShort);
    }
    let len = rd_u32(b, 16).ok_or(FrameError::TooShort)? as usize;
    if b.len() - REPLY_HEADER != len {
        return Err(FrameError::BadLength);
    }
    if len > MAX_IO {
        return Err(FrameError::TooBig);
    }
    Ok(Reply {
        txid: rd_u32(b, 0).ok_or(FrameError::TooShort)?,
        status: rd_u32(b, 4).ok_or(FrameError::TooShort)? as i32,
        a: rd_u64(b, 8).ok_or(FrameError::TooShort)?,
        payload: &b[REPLY_HEADER..],
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathError {
    /// Empty path, a leading `/`, an empty component (`//`, trailing `/`), NUL.
    Malformed,
    /// `.` or `..`.
    Dots,
    TooLong,
    TooDeep,
}

/// Validate one name (a directory entry, or a path component).
pub fn valid_name(n: &[u8]) -> Result<(), PathError> {
    if n.is_empty() || n.contains(&0) || n.contains(&b'/') {
        return Err(PathError::Malformed);
    }
    if n == b"." || n == b".." {
        return Err(PathError::Dots);
    }
    if n.len() > MAX_COMPONENT {
        return Err(PathError::TooLong);
    }
    Ok(())
}

/// Split a relative path into validated components. `..` can never appear, so a directory
/// connection is a sandbox boundary. An *empty* path is `Malformed` here: the server treats an
/// empty `Open` path as `Clone` (S4-SPEC section 7) before calling this.
pub fn split_path(p: &[u8]) -> Result<Vec<&[u8]>, PathError> {
    if p.is_empty() || p.len() > MAX_PATH {
        return Err(if p.is_empty() {
            PathError::Malformed
        } else {
            PathError::TooLong
        });
    }
    let mut out = Vec::new();
    for c in p.split(|&b| b == b'/') {
        valid_name(c)?;
        out.push(c);
        if out.len() > MAX_COMPONENTS {
            return Err(PathError::TooDeep);
        }
    }
    Ok(out)
}

/// One directory entry as `ReadDir` returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub is_dir: bool,
    pub name: Vec<u8>,
}

/// Pack entries as `kind:u8 (1 file, 2 directory), name_len:u16, name`, stopping before `max`
/// bytes; returns the bytes and how many entries fit.
pub fn encode_entries(entries: &[DirEntry], max: usize) -> (Vec<u8>, usize) {
    let mut out = Vec::new();
    let mut n = 0;
    for e in entries {
        if out.len() + 3 + e.name.len() > max {
            break;
        }
        out.push(if e.is_dir { 2 } else { 1 });
        out.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
        out.extend_from_slice(&e.name);
        n += 1;
    }
    (out, n)
}

pub fn decode_entries(b: &[u8]) -> Result<Vec<DirEntry>, FrameError> {
    let mut at = 0;
    let mut out = Vec::new();
    while at < b.len() {
        let kind = *b.get(at).ok_or(FrameError::TooShort)?;
        let n = rd_u16(b, at + 1).ok_or(FrameError::TooShort)? as usize;
        let name = b
            .get(at + 3..at + 3 + n)
            .ok_or(FrameError::BadLength)?
            .to_vec();
        if !(kind == 1 || kind == 2) || valid_name(&name).is_err() {
            return Err(FrameError::BadLength);
        }
        out.push(DirEntry {
            is_dir: kind == 2,
            name,
        });
        at += 3 + n;
    }
    Ok(out)
}
