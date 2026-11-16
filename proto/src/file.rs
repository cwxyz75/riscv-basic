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
    /// May publish and unpublish service names (the registry, slice SD). Meaningless on a filesystem.
    pub const PUBLISH: u32 = 16;
    pub const ALL: u32 = 31;
}

/// `Open` flags.
pub mod flags {
    pub const CREATE: u16 = 1;
    pub const EXCLUSIVE: u16 = 2;
    pub const TRUNCATE: u16 = 4;
    pub const DIRECTORY: u16 = 8;
    /// Do not follow a symlink that is the last component of the path (`OPEN`, `STAT`): act on the link itself (`lstat`,
    /// `O_NOFOLLOW`). Symlinks in the middle of a path are never followed by a server (see `status::SYMLINK`).
    pub const NOFOLLOW: u16 = 16;
    pub const ALL: u16 = 31;
}

/// Operations.
pub mod op {
    pub const OPEN: u16 = 1;
    pub const CLONE: u16 = 2;
    /// Like `CLONE`, but the new connection is the root of a **new account** with the limits in the request
    /// (`a`: connections in the low 32 bits and requests in flight in the high 32; `b`: bytes held for slow
    /// readers). Valid on a connection of either kind. The new account's limits may not exceed the
    /// connection's own account's (BAD_REQUEST), and everything it uses is also charged to that account.
    /// A server that does not support accounts answers NOT_SUPPORTED. (Slice R, OD84, R-SPEC.md section 3.)
    pub const ACCOUNT: u16 = 8;
    /// Registry (slice SD, SD-SPEC.md): publish a service name; `payload` is the name, the endpoint is the
    /// listener channel the registry will forward connections to. Needs the `PUBLISH` right.
    pub const PUBLISH: u16 = 16;
    /// Registry: remove a name (`payload`). Needs `PUBLISH`.
    pub const UNPUBLISH: u16 = 17;
    /// Registry: a clone whose view allows only the names in `payload` (separated by NUL bytes), each of which
    /// must already be allowed. The endpoint is the new connection.
    pub const VIEW: u16 = 18;
    pub const READDIR: u16 = 3;
    pub const MKDIR: u16 = 4;
    pub const UNLINK: u16 = 5;
    /// Move or replace an entry. `a` is the length of the old path; the payload is the old path
    /// followed by the new one (both relative to the directory connection).
    pub const RENAME: u16 = 7;
    pub const STAT: u16 = 6;
    pub const READ: u16 = 10;
    pub const WRITE: u16 = 11;
    pub const GETSIZE: u16 = 12;
    pub const SETSIZE: u16 = 13;
    pub const FSTAT: u16 = 14;
    pub const GETVMO: u16 = 15;
    // Attributes, indexes, queries and symlinks (FS-SPEC 9.1). A server that does not support them answers NOT_SUPPORTED.
    /// Set (replace) an attribute of the connection's node: `payload` is `ty:u8, name_len:u16, name, value`.
    pub const ATTR_SET: u16 = 20;
    /// `payload` is the name; the reply's `a` is the type and its payload the value.
    pub const ATTR_GET: u16 = 21;
    /// `payload` is the name.
    pub const ATTR_REMOVE: u16 = 22;
    /// `payload` is the name to continue after (empty: from the start); the reply is names as `len:u16, name`, and `a` is 1
    /// if there are more.
    pub const ATTR_LIST: u16 = 23;
    /// Create and back-fill an index (right `INDEX` is not yet defined; the server decides): `payload` is `ty:u8, name`.
    pub const MAKE_INDEX: u16 = 24;
    /// Query below the directory connection (needs `ENUMERATE`): see [`super::Query`].
    pub const QUERY: u16 = 25;
    /// Create a symlink (needs `CREATE`): `a` is the length of the path, the payload is the path then the target.
    pub const SYMLINK: u16 = 26;
    /// `payload` is a path; the reply payload is the link's target.
    pub const READLINK: u16 = 27;
    /// Make everything written so far durable (`fsync`/`sync`): the reply comes when the volume has committed. Valid on a
    /// connection of either kind; a server with nothing to flush answers Ok.
    pub const SYNC: u16 = 28;

    /// How many handles a request with this op carries (`None`: unknown op).
    pub fn handles(op: u16) -> Option<usize> {
        match op {
            OPEN | CLONE | ACCOUNT | PUBLISH | VIEW => Some(1),
            READDIR | MKDIR | UNLINK | RENAME | STAT | READ | WRITE | GETSIZE | SETSIZE | FSTAT
            | GETVMO | UNPUBLISH | ATTR_SET | ATTR_GET | ATTR_REMOVE | ATTR_LIST | MAKE_INDEX
            | QUERY | SYMLINK | READLINK | SYNC => Some(0),
            _ => None,
        }
    }

    /// Does this op belong to a directory connection only? (`CLONE`, `ACCOUNT` and `SYNC` are valid on both kinds; every
    /// other op that is not listed here belongs to file connections.)
    pub fn is_directory_op(op: u16) -> bool {
        matches!(
            op,
            OPEN | READDIR
                | MKDIR
                | UNLINK
                | RENAME
                | STAT
                | PUBLISH
                | UNPUBLISH
                | VIEW
                | MAKE_INDEX
                | QUERY
                | SYMLINK
                | READLINK
        )
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
    /// The service behind a published name is gone (its listener closed): try again later. Never says that a
    /// name exists to a connection whose view hides it.
    pub const UNAVAILABLE: i32 = -109;
    /// No attribute of that name (FS-SPEC 9.1).
    pub const NO_ATTR: i32 = -110;
    /// No index of that name.
    pub const NO_INDEX: i32 = -111;
    /// An attribute value or query term that does not fit its type, or is too long.
    pub const BAD_VALUE: i32 = -112;
    /// A symlink target that is malformed, absolute, or could climb above the connection root.
    pub const BAD_TARGET: i32 = -113;
    /// The storage under the server failed or a block did not verify (a damaged volume): never retried by the server.
    pub const IO_ERROR: i32 = -114;
    /// A walk met a symlink: the reply's `a` is how many components of the path were consumed (the link included) and its
    /// payload is the link's target text. The client resolves the target lexically from the link's own directory and tries
    /// again (`pathres`, USER-VIEW-SPEC section 5): a server never follows a link. Given for `OPEN`, `STAT`, `SYMLINK` and
    /// `READLINK` paths; the final component is reported only by `OPEN` and `STAT` without `NOFOLLOW`.
    pub const SYMLINK: i32 = -115;
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

/// Longest symlink target.
pub const MAX_LINK_TARGET: usize = 1024;

/// Check a symlink target (FS-SPEC 9.1): 1 to 1024 bytes, no NUL, not absolute, components separated by single `/` with none
/// empty, each a valid name, `.` or `..`; and, counting a name as one level down and `..` as one up from the link's directory
/// (`depth` levels below the connection root it is created through), never above the root. The same rule is the client's
/// guard when it resolves a link it finds: a target that fails it is not followed.
pub fn check_link_target(target: &[u8], depth: usize) -> Result<(), PathError> {
    if target.is_empty() || target.len() > MAX_LINK_TARGET || target.contains(&0) {
        return Err(PathError::Malformed);
    }
    if target[0] == b'/' {
        return Err(PathError::Malformed);
    }
    let mut level = depth as i64;
    for c in target.split(|&b| b == b'/') {
        match c {
            b"." => {}
            b".." => {
                level -= 1;
                if level < 0 {
                    return Err(PathError::Dots);
                }
            }
            _ => {
                valid_name(c)?;
                level += 1;
            }
        }
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
    /// A symbolic link (kind byte 3). Never also `is_dir`.
    pub is_link: bool,
    pub name: Vec<u8>,
}

/// Pack entries as `kind:u8 (1 file, 2 directory, 3 symlink), name_len:u16, name`, stopping before `max`
/// bytes; returns the bytes and how many entries fit.
pub fn encode_entries(entries: &[DirEntry], max: usize) -> (Vec<u8>, usize) {
    let mut out = Vec::new();
    let mut n = 0;
    for e in entries {
        if out.len() + 3 + e.name.len() > max {
            break;
        }
        out.push(if e.is_dir {
            2
        } else if e.is_link {
            3
        } else {
            1
        });
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
        if !(1..=3).contains(&kind) || valid_name(&name).is_err() {
            return Err(FrameError::BadLength);
        }
        out.push(DirEntry {
            is_dir: kind == 2,
            is_link: kind == 3,
            name,
        });
        at += 3 + n;
    }
    Ok(out)
}

// -------------------------------------------------------------------------------- stat (USER-VIEW-SPEC section 6)

/// The advisory POSIX attributes (USER-VIEW-SPEC section 6, Q8 resolved 2026-11-17): `U64` attributes holding the mode (the
/// permission and set-id bits, `0o7777`), the owner and the group. A server reports them in `STAT` when present; the libc
/// supplies defaults when absent. Nothing enforces them: rights on handles do.
pub const ATTR_POSIX_MODE: &[u8] = b"posix.mode";
pub const ATTR_POSIX_UID: &[u8] = b"posix.uid";
pub const ATTR_POSIX_GID: &[u8] = b"posix.gid";

/// What `STAT` and `FSTAT` return: the reply's `a` is `size` and the payload is
/// `kind:u8 (1 file, 2 directory, 3 symlink), mtime:u64 (ns), mode:u32, uid:u32, gid:u32`; a mode, uid or gid the node has no
/// attribute for is [`ABSENT`]. A directory's size is its entry count, a symlink's its target's length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatInfo {
    pub kind: u8,
    pub size: u64,
    pub mtime: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

/// The value of a `StatInfo` field the node has no attribute for.
pub const ABSENT: u32 = u32::MAX;
pub const STAT_PAYLOAD: usize = 21;

impl StatInfo {
    pub fn encode_payload(&self) -> Vec<u8> {
        let mut p = Vec::with_capacity(STAT_PAYLOAD);
        p.push(self.kind);
        p.extend_from_slice(&self.mtime.to_le_bytes());
        p.extend_from_slice(&self.mode.to_le_bytes());
        p.extend_from_slice(&self.uid.to_le_bytes());
        p.extend_from_slice(&self.gid.to_le_bytes());
        p
    }

    /// `a` is the reply's `a`.
    pub fn decode(a: u64, payload: &[u8]) -> Result<StatInfo, FrameError> {
        if payload.len() != STAT_PAYLOAD || !(1..=3).contains(&payload[0]) {
            return Err(FrameError::BadLength);
        }
        Ok(StatInfo {
            kind: payload[0],
            size: a,
            mtime: rd_u64(payload, 1).ok_or(FrameError::TooShort)?,
            mode: rd_u32(payload, 9).ok_or(FrameError::TooShort)?,
            uid: rd_u32(payload, 13).ok_or(FrameError::TooShort)?,
            gid: rd_u32(payload, 17).ok_or(FrameError::TooShort)?,
        })
    }
}

/// The value of an advisory attribute (8 bytes, little endian) as a `StatInfo` field: [`ABSENT`] if it is missing, malformed or
/// does not fit 32 bits.
pub fn attr_u32(value: Option<&[u8]>) -> u32 {
    match value.and_then(|v| <[u8; 8]>::try_from(v).ok()) {
        // (a value too big for 32 bits, or equal to the sentinel itself, reads as absent)
        Some(b) => match u32::try_from(u64::from_le_bytes(b)) {
            Ok(v) if v != ABSENT => v,
            _ => ABSENT,
        },
        None => ABSENT,
    }
}

// -------------------------------------------------------------------------------- attributes, queries (FS-SPEC 9.1)

fn put16(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u16).to_le_bytes());
    out.extend_from_slice(b);
}

fn get16<'a>(b: &'a [u8], at: &mut usize) -> Result<&'a [u8], FrameError> {
    let n = rd_u16(b, *at).ok_or(FrameError::TooShort)? as usize;
    let v = b.get(*at + 2..*at + 2 + n).ok_or(FrameError::BadLength)?;
    *at += 2 + n;
    Ok(v)
}

/// `ATTR_SET`'s payload: `ty:u8, name_len:u16, name, value`.
pub fn encode_attr_set(ty: u8, name: &[u8], value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(3 + name.len() + value.len());
    out.push(ty);
    put16(&mut out, name);
    out.extend_from_slice(value);
    out
}

pub fn decode_attr_set(b: &[u8]) -> Result<(u8, &[u8], &[u8]), FrameError> {
    let ty = *b.first().ok_or(FrameError::TooShort)?;
    let mut at = 1;
    let name = get16(b, &mut at)?;
    Ok((ty, name, &b[at..]))
}

/// `MAKE_INDEX`'s payload: `ty:u8, name`.
pub fn encode_make_index(ty: u8, name: &[u8]) -> Vec<u8> {
    let mut out = alloc::vec![ty];
    out.extend_from_slice(name);
    out
}

pub fn decode_make_index(b: &[u8]) -> Result<(u8, &[u8]), FrameError> {
    let ty = *b.first().ok_or(FrameError::TooShort)?;
    Ok((ty, &b[1..]))
}

/// Pack names as `len:u16, name`, stopping before `max` bytes; returns the bytes and how many fit (`ATTR_LIST`'s reply).
pub fn encode_names(names: &[Vec<u8>], max: usize) -> (Vec<u8>, usize) {
    let mut out = Vec::new();
    let mut n = 0;
    for name in names {
        if out.len() + 2 + name.len() > max {
            break;
        }
        put16(&mut out, name);
        n += 1;
    }
    (out, n)
}

pub fn decode_names(b: &[u8]) -> Result<Vec<Vec<u8>>, FrameError> {
    let mut at = 0;
    let mut out = Vec::new();
    while at < b.len() {
        out.push(get16(b, &mut at)?.to_vec());
    }
    Ok(out)
}

/// A query's term on an index key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryTerm<'a> {
    Eq(&'a [u8]),
    /// Lower bound inclusive, upper bound exclusive, either optional.
    Range {
        lo: Option<&'a [u8]>,
        hi: Option<&'a [u8]>,
    },
}

/// `QUERY`: the request's `a` is `max`, its `b` the flags below, its payload the fields in order. Flags: bit 0 `Eq` (else a
/// range), bit 1 has `lo` (or the `Eq` value is always present), bit 2 has `hi`, bit 3 has `after`. Fields: `index` (`len:u16,
/// bytes`), then the `Eq` value, or `lo` if present and `hi` if present (each `len:u16, bytes`), then `after` if present (`key`
/// as `len:u16, bytes`, then the node id as `u64`). The reply's `a` is 1 if there were more matches than were returned (the
/// `max` was reached or the hits did not fit the message), and its payload is the hits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query<'a> {
    pub index: &'a [u8],
    pub term: QueryTerm<'a>,
    pub after: Option<(&'a [u8], u64)>,
    pub max: u64,
}

pub mod query_flags {
    pub const EQ: u64 = 1;
    pub const LO: u64 = 2;
    pub const HI: u64 = 4;
    pub const AFTER: u64 = 8;
    pub const ALL: u64 = 15;
}

impl Query<'_> {
    /// `(b, payload)` of the request (`a` is `max`).
    pub fn encode(&self) -> (u64, Vec<u8>) {
        let mut flags = 0;
        let mut out = Vec::new();
        put16(&mut out, self.index);
        match &self.term {
            QueryTerm::Eq(v) => {
                flags |= query_flags::EQ;
                put16(&mut out, v);
            }
            QueryTerm::Range { lo, hi } => {
                if let Some(l) = lo {
                    flags |= query_flags::LO;
                    put16(&mut out, l);
                }
                if let Some(h) = hi {
                    flags |= query_flags::HI;
                    put16(&mut out, h);
                }
            }
        }
        if let Some((k, id)) = self.after {
            flags |= query_flags::AFTER;
            put16(&mut out, k);
            out.extend_from_slice(&id.to_le_bytes());
        }
        (flags, out)
    }
}

pub fn decode_query(max: u64, flags: u64, b: &[u8]) -> Result<Query<'_>, FrameError> {
    if flags & !query_flags::ALL != 0
        || (flags & query_flags::EQ != 0 && flags & (query_flags::LO | query_flags::HI) != 0)
    {
        return Err(FrameError::BadBits);
    }
    let mut at = 0;
    let index = get16(b, &mut at)?;
    let term = if flags & query_flags::EQ != 0 {
        QueryTerm::Eq(get16(b, &mut at)?)
    } else {
        let lo = if flags & query_flags::LO != 0 {
            Some(get16(b, &mut at)?)
        } else {
            None
        };
        let hi = if flags & query_flags::HI != 0 {
            Some(get16(b, &mut at)?)
        } else {
            None
        };
        QueryTerm::Range { lo, hi }
    };
    let after = if flags & query_flags::AFTER != 0 {
        let k = get16(b, &mut at)?;
        let id = rd_u64(b, at).ok_or(FrameError::TooShort)?;
        at += 8;
        Some((k, id))
    } else {
        None
    };
    if at != b.len() {
        return Err(FrameError::BadLength);
    }
    Ok(Query {
        index,
        term,
        after,
        max,
    })
}

/// One `QUERY` result: `key_len:u16, key, id:u64, path_len:u16, path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryHit {
    pub key: Vec<u8>,
    pub id: u64,
    pub path: Vec<u8>,
}

/// Pack hits, stopping before `max` bytes; returns the bytes and how many fit.
pub fn encode_hits(hits: &[QueryHit], max: usize) -> (Vec<u8>, usize) {
    let mut out = Vec::new();
    let mut n = 0;
    for h in hits {
        if out.len() + 12 + h.key.len() + h.path.len() > max {
            break;
        }
        put16(&mut out, &h.key);
        out.extend_from_slice(&h.id.to_le_bytes());
        put16(&mut out, &h.path);
        n += 1;
    }
    (out, n)
}

pub fn decode_hits(b: &[u8]) -> Result<Vec<QueryHit>, FrameError> {
    let mut at = 0;
    let mut out = Vec::new();
    while at < b.len() {
        let key = get16(b, &mut at)?.to_vec();
        let id = rd_u64(b, at).ok_or(FrameError::TooShort)?;
        at += 8;
        let path = get16(b, &mut at)?.to_vec();
        out.push(QueryHit { key, id, path });
    }
    Ok(out)
}

/// `SYMLINK`'s payload: the path then the target (the request's `a` is the path's length).
pub fn encode_symlink(path: &[u8], target: &[u8]) -> (u64, Vec<u8>) {
    let mut out = path.to_vec();
    out.extend_from_slice(target);
    (path.len() as u64, out)
}

pub fn decode_symlink(a: u64, b: &[u8]) -> Result<(&[u8], &[u8]), FrameError> {
    let n = usize::try_from(a).map_err(|_| FrameError::BadLength)?;
    if n > b.len() {
        return Err(FrameError::BadLength);
    }
    Ok((&b[..n], &b[n..]))
}
