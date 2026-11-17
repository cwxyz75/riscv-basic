//! The startup message ("process args", S4-SPEC section 5): exactly one message on the
//! bootstrap channel. The handles travel in the message's handle array; this module only
//! encodes and decodes the bytes and checks the indices against the handle count.

use crate::{rd_u16, rd_u32};
use alloc::vec::Vec;

pub const MAGIC: u32 = 0x5354_4152;
pub const VERSION: u32 = 1;
pub const MAX_BYTES: usize = 16 * 1024;
pub const MAX_HANDLES: usize = 32;
pub const MAX_STRING: usize = 1024;
pub const MAX_ARGS: usize = 64;
pub const MAX_ENV: usize = 16;
pub const MAX_NAMESPACE: usize = 8;
pub const MAX_ROLES: usize = 16;

/// What a handle is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Role {
    Stdin = 1,
    Stdout = 2,
    ServiceFs = 3,
    ServiceLaunch = 4,
    ServiceConsole = 5,
    BootImage = 6,
    Serial = 7,
    ServiceInit = 8,
    /// The console server's end of the shell's terminal socket.
    Tty = 9,
    /// A job handle: for the launcher, the job it makes each program's job under.
    Job = 10,
    /// A block driver's device memory: the virtio-mmio slot (S6b).
    BlkMmio = 11,
    /// A block driver's DMA buffer (a DMA VMO with the DMA right).
    BlkDma = 12,
    /// A block driver's interrupt object (absent where the machine has no PLIC: poll).
    BlkIrq = 13,
    /// The channel a block driver serves the block protocol on.
    ServiceBlk = 14,
    /// The root resource (the device manager's authority).
    Resource = 15,
    /// The channel the device manager serves (`proto::devmgr`).
    ServiceDevmgr = 16,
    /// Standard error (S7a): a duplicate of the same stream as `Stdout` when nothing separate was
    /// asked for. A program with no `Stderr` writes errors to `Stdout`.
    Stderr = 17,
    /// The channel the service registry serves (`proto::file` as a directory, slice SD).
    ServiceRegistry = 18,
    /// The network driver's device memory: the virtio-mmio slot (S8).
    NetMmio = 19,
    /// The network driver's DMA memory, each with the DMA right: the receive pool, the transmit pool and the virtqueue page(s).
    NetRxDma = 20,
    NetTxDma = 21,
    NetQueueDma = 22,
    /// The network device's interrupt (absent where the machine has no controller to drive: the driver polls).
    NetIrq = 23,
    /// The shared ring VMO (control header and the four rings) of the driver/stack data path.
    NetRing = 24,
    /// The two doorbell events: the driver signals `NetEventToStack` (the stack waits on it) and the stack signals
    /// `NetEventToDriver`.
    NetEventToStack = 25,
    NetEventToDriver = 26,
    /// The control channel between the driver and the stack (READY, ATTACH, DETACH).
    NetCtl = 27,
    /// The stack's views of the pools: no DMA right (the stack never learns a physical address); the receive pool is read-only.
    NetRxPool = 28,
    NetTxPool = 29,
    /// The one-page configuration blob `netsup` holds across restarts (NET-SPEC 8.4).
    NetConfigBlob = 30,
    /// The block channel a filesystem server stores its volume on (FS-SPEC section 5): the protocol of `proto::blk`.
    BlockDevice = 31,
    /// A listener channel a volume server (`fsd`) serves: the registry forwards each open of a published volume name to it,
    /// and each forwarded endpoint becomes a connection to the volume's root.
    ServiceListener = 32,
    /// A second listener (the same volume under another name).
    ServiceListenerAlt = 33,
    /// A writable connection to the directory the shell makes each program's private `/tmp` in (UV2).
    TmpDir = 34,
    /// A connection to the `/dev` server's root (UV2).
    DevDir = 35,
    /// A channel a volume server writes one byte to once it has mounted its volume and is ready to serve (`volmgr` publishes
    /// the volume's names only then; a server that exits first closes it).
    Ready = 36,
    /// The channel `authd` serves (UV5a): one request, one reply, in text.
    ServiceAuth = 37,
    /// The channel the shell tells `procd` about its programs on (UV6): both ends hold this role.
    ServiceProc = 38,
    /// A connection to `procd`'s root, the shell's `/proc` (UV6).
    ProcDir = 39,
    /// The channel a terminal backend provider uses to attach, detach and hang up (UV7): `proto::console::term`.
    ServiceTerm = 40,
    /// The channel a program asks the shell on to start another program (UV8, OD118): `proto::spawn`. Present only when the
    /// starter granted it; a child's own grant is never wider than its parent's.
    ServiceSpawn = 41,
}

impl Role {
    pub fn from_u32(v: u32) -> Option<Role> {
        Some(match v {
            1 => Role::Stdin,
            2 => Role::Stdout,
            3 => Role::ServiceFs,
            4 => Role::ServiceLaunch,
            5 => Role::ServiceConsole,
            6 => Role::BootImage,
            7 => Role::Serial,
            8 => Role::ServiceInit,
            9 => Role::Tty,
            10 => Role::Job,
            11 => Role::BlkMmio,
            12 => Role::BlkDma,
            13 => Role::BlkIrq,
            14 => Role::ServiceBlk,
            15 => Role::Resource,
            16 => Role::ServiceDevmgr,
            17 => Role::Stderr,
            18 => Role::ServiceRegistry,
            19 => Role::NetMmio,
            20 => Role::NetRxDma,
            21 => Role::NetTxDma,
            22 => Role::NetQueueDma,
            23 => Role::NetIrq,
            24 => Role::NetRing,
            25 => Role::NetEventToStack,
            26 => Role::NetEventToDriver,
            27 => Role::NetCtl,
            28 => Role::NetRxPool,
            29 => Role::NetTxPool,
            30 => Role::NetConfigBlob,
            31 => Role::BlockDevice,
            32 => Role::ServiceListener,
            33 => Role::ServiceListenerAlt,
            34 => Role::TmpDir,
            35 => Role::DevDir,
            36 => Role::Ready,
            37 => Role::ServiceAuth,
            38 => Role::ServiceProc,
            39 => Role::ProcDir,
            40 => Role::ServiceTerm,
            41 => Role::ServiceSpawn,
            _ => return None,
        })
    }
}

/// Is `prefix` a namespace prefix? `"."` (the working directory), `"/"` (the root), or an absolute path of
/// components of printable ASCII (no `/` inside, no `..` or `.`), such as `"/svc"` (slice SD: service
/// connections are named entries, not roles). No trailing slash, no empty component.
pub fn valid_prefix(prefix: &[u8]) -> bool {
    if prefix == b"." || prefix == b"/" {
        return true;
    }
    if prefix.len() > MAX_STRING || prefix.first() != Some(&b'/') {
        return false;
    }
    prefix[1..].split(|&b| b == b'/').all(|c| {
        !c.is_empty() && c != b"." && c != b".." && c.iter().all(|&b| (0x21..0x7f).contains(&b))
    })
}

/// A decoded startup message. Handle indices index the message's handle array.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Startup {
    pub args: Vec<Vec<u8>>,
    pub env: Vec<Vec<u8>>,
    /// (prefix, handle index): `"."` the working directory, `"/"` the root.
    pub namespace: Vec<(Vec<u8>, u32)>,
    pub roles: Vec<(Role, u32)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupError {
    TooBig,
    BadMagic,
    BadVersion,
    Truncated,
    TooMany,
    StringTooLong,
    TrailingBytes,
    /// A handle index at or beyond the message's handle count.
    BadIndex,
    /// A handle index used twice.
    DuplicateHandle,
    UnknownRole,
    DuplicateRole,
    /// Encoding: a field exceeds its limit.
    Limit,
    /// A handle that arrived with the message but nothing refers to: unreferenced authority.
    UnreferencedHandle,
    /// A namespace prefix other than `"."` or `"/"`, or one used twice.
    BadNamespace,
    /// A NUL byte in an argument, environment string or prefix.
    NulInString,
}

impl Startup {
    /// Encode; fails if any limit is exceeded (so an encoder cannot produce what the decoder
    /// would refuse).
    /// Everything the decoder checks that does not need the handle count, so that the encoder
    /// produces nothing the decoder would refuse.
    fn validate(&self) -> Result<(), StartupError> {
        for s in self.args.iter().chain(self.env.iter()) {
            if s.contains(&0) {
                return Err(StartupError::NulInString);
            }
        }
        for (i, (prefix, idx)) in self.namespace.iter().enumerate() {
            if !valid_prefix(prefix) {
                return Err(StartupError::BadNamespace);
            }
            if self.namespace[..i].iter().any(|(p, _)| p == prefix) {
                return Err(StartupError::BadNamespace);
            }
            if self.namespace[..i].iter().any(|(_, j)| j == idx) {
                return Err(StartupError::DuplicateHandle);
            }
        }
        for (i, (role, idx)) in self.roles.iter().enumerate() {
            if self.roles[..i].iter().any(|(r, _)| r == role) {
                return Err(StartupError::DuplicateRole);
            }
            if self.roles[..i].iter().any(|(_, j)| j == idx)
                || self.namespace.iter().any(|(_, j)| j == idx)
            {
                return Err(StartupError::DuplicateHandle);
            }
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, StartupError> {
        self.validate()?;
        if self.args.len() > MAX_ARGS
            || self.env.len() > MAX_ENV
            || self.namespace.len() > MAX_NAMESPACE
            || self.roles.len() > MAX_ROLES
        {
            return Err(StartupError::Limit);
        }
        let mut out = Vec::new();
        for v in [
            MAGIC,
            VERSION,
            self.args.len() as u32,
            self.env.len() as u32,
            self.namespace.len() as u32,
            self.roles.len() as u32,
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        for s in self.args.iter().chain(self.env.iter()) {
            put_str(&mut out, s)?;
        }
        for (prefix, idx) in &self.namespace {
            put_str(&mut out, prefix)?;
            out.extend_from_slice(&idx.to_le_bytes());
        }
        for (role, idx) in &self.roles {
            out.extend_from_slice(&(*role as u32).to_le_bytes());
            out.extend_from_slice(&idx.to_le_bytes());
        }
        if out.len() > MAX_BYTES {
            return Err(StartupError::TooBig);
        }
        Ok(out)
    }

    /// Decode and validate against the number of handles that arrived with the message.
    pub fn decode(b: &[u8], nhandles: usize) -> Result<Startup, StartupError> {
        if b.len() > MAX_BYTES || nhandles > MAX_HANDLES {
            return Err(StartupError::TooBig);
        }
        if rd_u32(b, 0).ok_or(StartupError::Truncated)? != MAGIC {
            return Err(StartupError::BadMagic);
        }
        if rd_u32(b, 4).ok_or(StartupError::Truncated)? != VERSION {
            return Err(StartupError::BadVersion);
        }
        let argc = rd_u32(b, 8).ok_or(StartupError::Truncated)? as usize;
        let envc = rd_u32(b, 12).ok_or(StartupError::Truncated)? as usize;
        let nsc = rd_u32(b, 16).ok_or(StartupError::Truncated)? as usize;
        let nroles = rd_u32(b, 20).ok_or(StartupError::Truncated)? as usize;
        if argc > MAX_ARGS || envc > MAX_ENV || nsc > MAX_NAMESPACE || nroles > MAX_ROLES {
            return Err(StartupError::TooMany);
        }
        let mut at = 24;
        let mut s = Startup::default();
        for i in 0..argc + envc {
            let v = get_str(b, &mut at)?;
            if i < argc {
                s.args.push(v);
            } else {
                s.env.push(v);
            }
        }
        let mut used = [false; MAX_HANDLES];
        let mut claim = |idx: u32| -> Result<u32, StartupError> {
            let i = idx as usize;
            if i >= nhandles {
                return Err(StartupError::BadIndex);
            }
            if core::mem::replace(&mut used[i], true) {
                return Err(StartupError::DuplicateHandle);
            }
            Ok(idx)
        };
        for _ in 0..nsc {
            let prefix = get_str(b, &mut at)?;
            if !valid_prefix(&prefix) || s.namespace.iter().any(|(p, _)| *p == prefix) {
                return Err(StartupError::BadNamespace);
            }
            let idx = rd_u32(b, at).ok_or(StartupError::Truncated)?;
            at += 4;
            s.namespace.push((prefix, claim(idx)?));
        }
        for _ in 0..nroles {
            let role = rd_u32(b, at).ok_or(StartupError::Truncated)?;
            let idx = rd_u32(b, at + 4).ok_or(StartupError::Truncated)?;
            at += 8;
            let role = Role::from_u32(role).ok_or(StartupError::UnknownRole)?;
            if s.roles.iter().any(|(r, _)| *r == role) {
                return Err(StartupError::DuplicateRole);
            }
            s.roles.push((role, claim(idx)?));
        }
        if at != b.len() {
            return Err(StartupError::TrailingBytes);
        }
        // Every handle that arrived must be named: a handle nobody refers to is authority the
        // parent handed over by mistake.
        if used[..nhandles].iter().any(|u| !u) {
            return Err(StartupError::UnreferencedHandle);
        }
        if s.args.iter().chain(s.env.iter()).any(|x| x.contains(&0)) {
            return Err(StartupError::NulInString);
        }
        Ok(s)
    }

    /// The handle index of a role, if present.
    pub fn handle_of(&self, role: Role) -> Option<u32> {
        self.roles.iter().find(|(r, _)| *r == role).map(|(_, i)| *i)
    }
}

fn put_str(out: &mut Vec<u8>, s: &[u8]) -> Result<(), StartupError> {
    if s.len() > MAX_STRING {
        return Err(StartupError::StringTooLong);
    }
    out.extend_from_slice(&(s.len() as u16).to_le_bytes());
    out.extend_from_slice(s);
    Ok(())
}

fn get_str(b: &[u8], at: &mut usize) -> Result<Vec<u8>, StartupError> {
    let len = rd_u16(b, *at).ok_or(StartupError::Truncated)? as usize;
    if len > MAX_STRING {
        return Err(StartupError::StringTooLong);
    }
    let start = *at + 2;
    let end = start.checked_add(len).ok_or(StartupError::Truncated)?;
    let bytes = b.get(start..end).ok_or(StartupError::Truncated)?;
    *at = end;
    Ok(bytes.to_vec())
}
