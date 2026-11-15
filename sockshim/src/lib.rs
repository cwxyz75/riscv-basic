//! The decisions of the socket shim in the C library, kept pure so they are tested on the host (S9-SPEC 9): which `errno` a
//! native outcome becomes, how `sockaddr_in` and dotted quads are read and written, which socket options are accepted, and
//! which `poll` events a descriptor's state reports. The C library (`libc/src/socket.rs`) only applies them.
#![no_std]
#![forbid(unsafe_code)]

pub mod errno {
    pub const EINTR: i32 = 4;
    pub const EBADF: i32 = 9;
    pub const EAGAIN: i32 = 11;
    pub const ENOMEM: i32 = 12;
    pub const EACCES: i32 = 13;
    pub const EINVAL: i32 = 22;
    pub const ENFILE: i32 = 23;
    pub const EMFILE: i32 = 24;
    pub const ENOTTY: i32 = 25;
    pub const EPIPE: i32 = 32;
    pub const ENOTSOCK: i32 = 88;
    pub const EDESTADDRREQ: i32 = 89;
    pub const EMSGSIZE: i32 = 90;
    pub const ENOPROTOOPT: i32 = 92;
    pub const EPROTONOSUPPORT: i32 = 93;
    pub const EOPNOTSUPP: i32 = 95;
    pub const EAFNOSUPPORT: i32 = 97;
    pub const EADDRINUSE: i32 = 98;
    pub const EADDRNOTAVAIL: i32 = 99;
    pub const ENETDOWN: i32 = 100;
    pub const ENETUNREACH: i32 = 101;
    pub const ECONNRESET: i32 = 104;
    pub const ENOBUFS: i32 = 105;
    pub const EISCONN: i32 = 106;
    pub const ENOTCONN: i32 = 107;
    pub const ETIMEDOUT: i32 = 110;
    pub const ECONNREFUSED: i32 = 111;
    pub const EHOSTUNREACH: i32 = 113;
    pub const EALREADY: i32 = 114;
    pub const EINPROGRESS: i32 = 115;
}
use errno::*;

/// A native stack status (`proto::net::status`, as an `i32`) to the `errno` a C program sees (S9-SPEC 9.5).
pub fn errno_of_status(s: i32) -> i32 {
    match s {
        1 => EINVAL,        // BadRequest: a library bug
        2 => EACCES,        // Refused
        3 => EADDRINUSE,    // AddrInUse
        4 => EMSGSIZE,      // TooBig
        5 => ENETUNREACH,   // NoRoute
        6 => EMFILE,        // Quota: this process has too many
        7 => ENETDOWN,      // NotConfigured
        8 => ENOBUFS,       // NoBuffers
        9 => EOPNOTSUPP,    // NotSupported
        10 => ECONNREFUSED, // ConnRefused
        11 => ETIMEDOUT,    // TimedOut
        12 => ECONNRESET,   // ConnReset
        13 => ENOTCONN,     // NotConnected
        14 => EISCONN,      // AlreadyConnected
        15 => EHOSTUNREACH, // HostUnreachable
        16 => EINVAL,       // BadHandle: a library bug
        -109 => ENETDOWN,   // the registry's UNAVAILABLE: the stack is gone
        _ => EINVAL,
    }
}

/// A kernel status from a socket or channel call to an `errno` (SHOULD_WAIT is `EAGAIN`; NO_MEMORY is `ENOBUFS`).
pub fn errno_of_kernel(s: i32) -> i32 {
    match s {
        -1 | -2 => EBADF, // BAD_HANDLE, WRONG_TYPE
        -3 => EACCES,     // ACCESS_DENIED
        -4 => EINVAL,     // INVALID_ARGS
        -5 => ENOBUFS,    // NO_MEMORY
        -6 => EAGAIN,     // SHOULD_WAIT
        -7 => EPIPE,      // PEER_CLOSED
        -9 => EMSGSIZE,   // TOO_BIG
        -10 => EPIPE,     // BAD_STATE (a write after our own shutdown)
        -12 => ETIMEDOUT, // TIMED_OUT
        _ => EINVAL,
    }
}

pub const AF_INET: u16 = 2;
pub const SOCK_STREAM: i32 = 1;
pub const SOCK_DGRAM: i32 = 2;
pub const SOCK_NONBLOCK: i32 = 0o4000;
pub const SOCK_CLOEXEC: i32 = 0o2000000;
pub const SOCKADDR_IN_LEN: usize = 16;

/// `socket(domain, type, protocol)`: `Ok(kind)` with `kind` `SOCK_STREAM` or `SOCK_DGRAM`, or the `errno` (S9-SPEC 9.4).
pub fn check_socket_args(domain: i32, ty: i32, protocol: i32) -> Result<(i32, bool), i32> {
    if domain != i32::from(AF_INET) {
        return Err(EAFNOSUPPORT);
    }
    let nonblock = ty & SOCK_NONBLOCK != 0;
    let kind = ty & !(SOCK_NONBLOCK | SOCK_CLOEXEC);
    match (kind, protocol) {
        (SOCK_STREAM, 0 | 6) | (SOCK_DGRAM, 0 | 17) => Ok((kind, nonblock)),
        _ => Err(EPROTONOSUPPORT),
    }
}

/// Read a `struct sockaddr_in` (`family u16 | port be16 | addr[4] | zero[8]`): `(addr, port)`, or the `errno`.
pub fn parse_sockaddr_in(b: &[u8]) -> Result<([u8; 4], u16), i32> {
    if b.len() < SOCKADDR_IN_LEN {
        return Err(EINVAL);
    }
    if u16::from_ne_bytes([b[0], b[1]]) != AF_INET {
        return Err(EAFNOSUPPORT);
    }
    Ok(([b[4], b[5], b[6], b[7]], u16::from_be_bytes([b[2], b[3]])))
}

pub fn format_sockaddr_in(addr: [u8; 4], port: u16) -> [u8; SOCKADDR_IN_LEN] {
    let mut b = [0u8; SOCKADDR_IN_LEN];
    b[..2].copy_from_slice(&AF_INET.to_ne_bytes());
    b[2..4].copy_from_slice(&port.to_be_bytes());
    b[4..8].copy_from_slice(&addr);
    b
}

/// `inet_pton(AF_INET, ...)`: strictly four decimal fields 0 to 255, no leading zeros beyond a lone `0`, nothing else.
pub fn inet_pton4(s: &[u8]) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut n = 0;
    for part in s.split(|&c| c == b'.') {
        if n == 4 || part.is_empty() || part.len() > 3 || (part.len() > 1 && part[0] == b'0') {
            return None;
        }
        let mut v = 0u32;
        for &c in part {
            if !c.is_ascii_digit() {
                return None;
            }
            v = v * 10 + u32::from(c - b'0');
        }
        if v > 255 {
            return None;
        }
        out[n] = v as u8;
        n += 1;
    }
    (n == 4).then_some(out)
}

/// `inet_ntop(AF_INET, ...)` into `out`: the bytes written (no NUL), or `None` if `out` is too small.
pub fn inet_ntop4(a: [u8; 4], out: &mut [u8]) -> Option<usize> {
    let mut i = 0;
    for (k, v) in a.iter().enumerate() {
        if k > 0 {
            *out.get_mut(i)? = b'.';
            i += 1;
        }
        let digits = [*v / 100, *v / 10 % 10, *v % 10];
        let skip = if *v >= 100 {
            0
        } else if *v >= 10 {
            1
        } else {
            2
        };
        for d in &digits[skip..] {
            *out.get_mut(i)? = b'0' + d;
            i += 1;
        }
    }
    Some(i)
}

pub mod opt {
    pub const SOL_SOCKET: i32 = 1;
    pub const IPPROTO_TCP: i32 = 6;
    pub const SO_REUSEADDR: i32 = 2;
    pub const SO_TYPE: i32 = 3;
    pub const SO_ERROR: i32 = 4;
    pub const SO_BROADCAST: i32 = 6;
    pub const SO_SNDBUF: i32 = 7;
    pub const SO_RCVBUF: i32 = 8;
    pub const SO_KEEPALIVE: i32 = 9;
    pub const SO_OOBINLINE: i32 = 10;
    pub const SO_LINGER: i32 = 13;
    pub const SO_REUSEPORT: i32 = 15;
    pub const SO_RCVTIMEO: i32 = 20;
    pub const SO_SNDTIMEO: i32 = 21;
    pub const TCP_NODELAY: i32 = 1;
}

/// What `setsockopt` does with an option.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SetOpt {
    /// Accepted and meaningless (`SO_REUSEADDR`: the stack already lets a listener bind beside TIME-WAIT).
    Accept,
    /// Becomes the send or receive buffer size (clamped 4 to 64 KiB) at the next `connect` or `listen`.
    SndBuf(u32),
    RcvBuf(u32),
    /// `SO_RCVTIMEO` / `SO_SNDTIMEO` in milliseconds (0: none).
    RcvTimeoutMs(u64),
    SndTimeoutMs(u64),
    NoDelay(bool),
    /// `SO_LINGER {1, 0}`: close sends an abort. `false`: off.
    LingerAbort(bool),
}

/// The decision for `setsockopt(level, name, value)` where `value` is the option bytes as the C program passed them.
pub fn set_option(level: i32, name: i32, value: &[u8]) -> Result<SetOpt, i32> {
    use opt::*;
    let int = || -> Result<i32, i32> {
        value
            .get(..4)
            .map(|b| i32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
            .ok_or(EINVAL)
    };
    match (level, name) {
        (SOL_SOCKET, SO_REUSEADDR) => Ok(SetOpt::Accept),
        (SOL_SOCKET, SO_SNDBUF) => Ok(SetOpt::SndBuf(int()?.max(0) as u32)),
        (SOL_SOCKET, SO_RCVBUF) => Ok(SetOpt::RcvBuf(int()?.max(0) as u32)),
        (SOL_SOCKET, SO_RCVTIMEO | SO_SNDTIMEO) => {
            // struct timeval { long tv_sec; long tv_usec } on LP64: 16 bytes
            if value.len() < 16 {
                return Err(EINVAL);
            }
            let sec = i64::from_ne_bytes(value[..8].try_into().map_err(|_| EINVAL)?);
            let usec = i64::from_ne_bytes(value[8..16].try_into().map_err(|_| EINVAL)?);
            if sec < 0 || !(0..1_000_000).contains(&usec) {
                return Err(EINVAL);
            }
            let ms = (sec as u64)
                .saturating_mul(1000)
                .saturating_add(usec as u64 / 1000);
            Ok(if name == SO_RCVTIMEO {
                SetOpt::RcvTimeoutMs(ms)
            } else {
                SetOpt::SndTimeoutMs(ms)
            })
        }
        (SOL_SOCKET, SO_LINGER) => {
            if value.len() < 8 {
                return Err(EINVAL);
            }
            let on = i32::from_ne_bytes([value[0], value[1], value[2], value[3]]);
            let secs = i32::from_ne_bytes([value[4], value[5], value[6], value[7]]);
            match (on, secs) {
                (0, _) => Ok(SetOpt::LingerAbort(false)),
                (_, 0) => Ok(SetOpt::LingerAbort(true)),
                _ => Err(EINVAL),
            }
        }
        (SOL_SOCKET, SO_OOBINLINE) => Err(EOPNOTSUPP),
        (SOL_SOCKET, SO_BROADCAST | SO_KEEPALIVE | SO_REUSEPORT) => Err(ENOPROTOOPT),
        (IPPROTO_TCP, TCP_NODELAY) => Ok(SetOpt::NoDelay(int()? != 0)),
        _ => Err(ENOPROTOOPT),
    }
}

pub const POLLIN: i16 = 1;
pub const POLLOUT: i16 = 4;
pub const POLLERR: i16 = 8;
pub const POLLHUP: i16 = 16;
pub const POLLNVAL: i16 = 32;

/// What a stream descriptor reports (S9-SPEC 9.3). `readable`: the data socket has bytes, or has ended; `writable`: the data
/// socket has room; `failed`: a `Reset` arrived or the control channel closed without `PeerFin`.
pub fn poll_stream(want: i16, readable: bool, writable: bool, failed: bool) -> i16 {
    let mut r = 0;
    if readable {
        r |= POLLIN;
    }
    if writable {
        r |= POLLOUT;
    }
    if failed {
        r |= POLLERR | POLLHUP;
    }
    r & (want | POLLERR | POLLHUP)
}

/// A connecting stream: writable (and `POLLERR` if it failed) once the outcome is known; never readable before.
pub fn poll_connecting(want: i16, outcome_known: bool, failed: bool) -> i16 {
    let mut r = 0;
    if outcome_known {
        r |= POLLOUT;
    }
    if failed {
        r |= POLLERR;
    }
    r & (want | POLLERR | POLLHUP)
}
