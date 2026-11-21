//! BSD sockets over the native network protocol (S9 step 6, S9-SPEC 9): a shim, never the reverse. A socket is a descriptor of
//! the third kind (beside the standard streams and files). The decisions (errno table, sockaddr and inet formats, option
//! rules, poll masks) are in the host-tested `sockshim` crate; this file applies them to `rt::net`.
//!
//! Blocking is done here, by waiting on the objects' signals; the native objects are never blocking. A blocking call may wait
//! for ever on the program's own objects (as a pipe read does); every request to the stack carries a deadline.

use crate::errno;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ffi::{c_int, c_void};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use rt::abi::signals as sig;
use rt::fs::Fs;
use rt::net::{Listener, Net, NetError, Stream, Udp};
use rt::sync::Mutex;
use sockshim::errno::*;
use sockshim::{opt, SetOpt};

/// The `/svc` directory of the namespace (0 if the program has no network view).
pub static SVC: AtomicU64 = AtomicU64::new(0);

static NET: Mutex<Option<Net>> = Mutex::new(None);
static NET_TXIDS: AtomicU32 = AtomicU32::new(1 << 20);

const NEVER: u64 = u64::MAX;

fn now() -> u64 {
    rt::sys::clock_get()
}

fn fail(e: c_int) -> c_int {
    errno::set(e);
    -1
}

fn failn(e: c_int) -> isize {
    errno::set(e);
    -1
}

/// Run `f` with the process's connection to `/svc/net`, opening it on first use. `EACCES` if the program has no such name.
fn with_net<R>(f: impl FnOnce(&mut Net) -> Result<R, NetError>) -> Result<R, c_int> {
    let mut g = NET.lock();
    if g.is_none() {
        let h = SVC.load(Ordering::Relaxed);
        if h == 0 {
            return Err(EACCES);
        }
        let mut svc = Fs::new_at(h, NET_TXIDS.fetch_add(1 << 12, Ordering::Relaxed));
        match Net::connect(&mut svc, b"net", now() + 10_000) {
            Ok(n) => *g = Some(n),
            Err(NetError::Sys(_)) => return Err(ENETDOWN),
            Err(e) => return Err(errno_of_net(e)),
        }
    }
    let net = g.as_mut().unwrap();
    match f(net) {
        Ok(r) => Ok(r),
        Err(e) => {
            if e == NetError::Closed {
                *g = None; // the stack died: the next call opens a new connection
            }
            Err(errno_of_net(e))
        }
    }
}

/// `with_net` for the other modules of the library (`getaddrinfo`), with the stack's own errors as plain values.
pub(crate) fn with_net_pub<R>(f: impl FnOnce(&mut Net) -> Result<R, NetError>) -> Result<R, c_int> {
    with_net(f)
}

fn errno_of_net(e: NetError) -> c_int {
    match e {
        NetError::Stack(s) => sockshim::errno_of_status(s),
        NetError::Sys(s) => sockshim::errno_of_kernel(s),
        NetError::TimedOut => EAGAIN,
        NetError::Closed => ENETDOWN,
        NetError::Garbled => EINVAL,
    }
}

// ---------------------------------------------------------------------------------------------------- the descriptor

enum St {
    New,
    Udp {
        ep: Udp,
        peer: Option<([u8; 4], u16)>,
    },
    Connecting(Stream),
    Stream(Stream),
    Listener(Listener),
    Dead(c_int),
}

pub struct Sock {
    stream: bool,
    nonblock: bool,
    st: St,
    rcv_ms: u64,
    snd_ms: u64,
    snd_buf: u32,
    rcv_buf: u32,
    nodelay: bool,
    linger_abort: bool,
    bound: Option<([u8; 4], u16)>,
    shut_rd: bool,
    /// A pending asynchronous error for `SO_ERROR` (a failed nonblocking connect).
    err: c_int,
}

pub type SockRef = Arc<Mutex<Sock>>;

fn deadline_of(ms: u64) -> u64 {
    if ms == 0 {
        NEVER
    } else {
        now() + ms
    }
}

// ---------------------------------------------------------------------------------------------------- creation

#[no_mangle]
pub extern "C" fn socket(domain: c_int, ty: c_int, protocol: c_int) -> c_int {
    let (kind, nonblock) = match sockshim::check_socket_args(domain, ty, protocol) {
        Ok(x) => x,
        Err(e) => return fail(e),
    };
    let s = Sock {
        stream: kind == sockshim::SOCK_STREAM,
        nonblock,
        st: St::New,
        rcv_ms: 0,
        snd_ms: 0,
        snd_buf: 0,
        rcv_buf: 0,
        nodelay: false,
        linger_abort: false,
        bound: None,
        shut_rd: false,
        err: 0,
    };
    match crate::fd::install_socket(Arc::new(Mutex::new(s))) {
        Some(fd) => fd,
        None => fail(EMFILE),
    }
}

fn get(fd: c_int) -> Result<SockRef, c_int> {
    match crate::fd::socket_of(fd) {
        Ok(Some(s)) => Ok(s),
        Ok(None) => Err(ENOTSOCK),
        Err(()) => Err(EBADF),
    }
}

unsafe fn addr_in(addr: *const c_void, len: u32) -> Result<([u8; 4], u16), c_int> {
    if addr.is_null() {
        return Err(EINVAL);
    }
    let b = core::slice::from_raw_parts(addr as *const u8, (len as usize).min(128));
    sockshim::parse_sockaddr_in(b)
}

unsafe fn put_addr(addr: *mut c_void, len: *mut u32, a: ([u8; 4], u16)) {
    if addr.is_null() || len.is_null() {
        return;
    }
    let b = sockshim::format_sockaddr_in(a.0, a.1);
    let n = (*len as usize).min(b.len());
    core::ptr::copy_nonoverlapping(b.as_ptr(), addr as *mut u8, n);
    *len = b.len() as u32;
}

#[no_mangle]
pub unsafe extern "C" fn bind(fd: c_int, addr: *const c_void, len: u32) -> c_int {
    let sock = match get(fd) {
        Ok(s) => s,
        Err(e) => return fail(e),
    };
    let (a, port) = match addr_in(addr, len) {
        Ok(x) => x,
        Err(e) => return fail(e),
    };
    let mut s = sock.lock();
    if !matches!(s.st, St::New) || s.bound.is_some() {
        return fail(EINVAL);
    }
    if !s.stream {
        // a UDP socket opens its endpoint now
        match with_net(|n| n.open_udp(port, 0, now() + 5000)) {
            Ok(ep) => {
                s.bound = Some((a, ep.port));
                s.st = St::Udp { ep, peer: None };
                0
            }
            Err(e) => fail(e),
        }
    } else {
        s.bound = Some((a, port));
        0
    }
}

/// An unbound UDP socket gets its endpoint on first use (an ephemeral port).
fn ensure_udp(s: &mut Sock) -> Result<(), c_int> {
    if matches!(s.st, St::New) && !s.stream {
        let ep = with_net(|n| n.open_udp(0, 0, now() + 5000))?;
        s.bound = Some(([0; 4], ep.port));
        s.st = St::Udp { ep, peer: None };
    }
    Ok(())
}

#[no_mangle]
pub unsafe extern "C" fn connect(fd: c_int, addr: *const c_void, len: u32) -> c_int {
    let sock = match get(fd) {
        Ok(s) => s,
        Err(e) => return fail(e),
    };
    let (a, port) = match addr_in(addr, len) {
        Ok(x) => x,
        Err(e) => return fail(e),
    };
    let mut s = sock.lock();
    if !s.stream {
        if let Err(e) = ensure_udp(&mut s) {
            return fail(e);
        }
        if let St::Udp { peer, .. } = &mut s.st {
            *peer = Some((a, port));
            return 0;
        }
        return fail(EINVAL);
    }
    match &s.st {
        St::New => {}
        St::Connecting(_) => return fail(EALREADY),
        St::Stream(_) => return fail(EISCONN),
        _ => return fail(EINVAL),
    }
    let (snd, rcv, nodelay) = (s.snd_buf, s.rcv_buf, s.nodelay);
    let begun = with_net(|n| {
        n.begin_connect(
            a,
            port,
            if nodelay {
                proto::net::oflags::NODELAY
            } else {
                0
            },
            snd,
            rcv,
            now() + 5000,
        )
    });
    let mut st = match begun {
        Ok(st) => st,
        Err(e) => return fail(e),
    };
    if s.nonblock {
        s.st = St::Connecting(st);
        return fail(EINPROGRESS);
    }
    let deadline = deadline_of(s.snd_ms);
    match st.finish_connect(deadline) {
        Ok(()) => {
            s.st = St::Stream(st);
            0
        }
        Err(NetError::TimedOut) => {
            s.st = St::Connecting(st);
            fail(EAGAIN)
        }
        Err(e) => fail(errno_of_net(e)),
    }
}

#[no_mangle]
pub extern "C" fn listen(fd: c_int, backlog: c_int) -> c_int {
    let sock = match get(fd) {
        Ok(s) => s,
        Err(e) => return fail(e),
    };
    let mut s = sock.lock();
    if !s.stream || !matches!(s.st, St::New) {
        return fail(EOPNOTSUPP);
    }
    let Some((_, port)) = s.bound else {
        return fail(EDESTADDRREQ);
    };
    let (snd, rcv, nodelay) = (s.snd_buf, s.rcv_buf, s.nodelay);
    let n = (backlog.max(1) as usize).min(proto::net::limits::POSTED_ACCEPTS_MAX);
    let made = with_net(|net| {
        let mut l = net.listen_stream(
            port,
            0,
            if nodelay {
                proto::net::oflags::NODELAY
            } else {
                0
            },
            now() + 5000,
        )?;
        for _ in 0..n {
            l.post(snd, rcv)?;
        }
        Ok(l)
    });
    match made {
        Ok(l) => {
            s.bound = Some((s.bound.map_or([0; 4], |b| b.0), l.port));
            s.st = St::Listener(l);
            0
        }
        Err(e) => fail(e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn accept(fd: c_int, addr: *mut c_void, len: *mut u32) -> c_int {
    accept4(fd, addr, len, 0)
}

#[no_mangle]
pub unsafe extern "C" fn accept4(
    fd: c_int,
    addr: *mut c_void,
    len: *mut u32,
    flags: c_int,
) -> c_int {
    let sock = match get(fd) {
        Ok(s) => s,
        Err(e) => return fail(e),
    };
    let (stream, remote) = {
        let mut s = sock.lock();
        let (nonblock, rcv_ms) = (s.nonblock, s.rcv_ms);
        let St::Listener(l) = &mut s.st else {
            return fail(EINVAL);
        };
        let deadline = if nonblock { now() } else { deadline_of(rcv_ms) };
        match l.accept(deadline) {
            Ok(st) => {
                let r = st.remote();
                (st, r)
            }
            Err(e) => return fail(errno_of_net(e)),
        }
    };
    let new = Sock {
        stream: true,
        nonblock: flags & sockshim::SOCK_NONBLOCK != 0,
        st: St::Stream(stream),
        rcv_ms: 0,
        snd_ms: 0,
        snd_buf: 0,
        rcv_buf: 0,
        nodelay: false,
        linger_abort: false,
        bound: None,
        shut_rd: false,
        err: 0,
    };
    match crate::fd::install_socket(Arc::new(Mutex::new(new))) {
        Some(nfd) => {
            if let Some(r) = remote {
                put_addr(addr, len, r);
            }
            nfd
        }
        None => fail(EMFILE),
    }
}

// ---------------------------------------------------------------------------------------------------- data

/// Receive on a socket descriptor (`read`, `recv`, `recvfrom`). `from` receives the datagram's source.
pub fn sock_recv(
    sock: &SockRef,
    buf: &mut [u8],
    dontwait: bool,
    from: Option<&mut Option<([u8; 4], u16)>>,
) -> isize {
    let mut s = sock.lock();
    if s.shut_rd {
        return 0;
    }
    let nonblock = s.nonblock || dontwait;
    let deadline = if nonblock {
        now()
    } else {
        deadline_of(s.rcv_ms)
    };
    match &mut s.st {
        St::Stream(st) => match st.read(buf, deadline) {
            Ok(n) => n as isize,
            Err(e) => failn(errno_of_net(e)),
        },
        St::Udp { ep, peer } => loop {
            match ep.recv(deadline) {
                Ok(d) => {
                    if peer.is_some_and(|p| p != (d.src, d.port)) {
                        continue; // a connected UDP socket sees only its peer's datagrams
                    }
                    let n = d.payload.len().min(buf.len());
                    buf[..n].copy_from_slice(&d.payload[..n]);
                    if let Some(f) = from {
                        *f = Some((d.src, d.port));
                    }
                    return n as isize;
                }
                Err(e) => return failn(errno_of_net(e)),
            }
        },
        St::Connecting(_) | St::New | St::Listener(_) => failn(ENOTCONN),
        St::Dead(e) => failn(*e),
    }
}

/// Send on a socket descriptor (`write`, `send`, `sendto`). `to` is the destination of a datagram.
/// `sock_send` as `write` and `send` have it: a send on a closed stream raises SIGPIPE (the lock is not held then) unless
/// `nosignal` (`MSG_NOSIGNAL`), and fails with `EPIPE` either way.
pub fn send_signalling(
    sock: &SockRef,
    data: &[u8],
    dontwait: bool,
    to: Option<([u8; 4], u16)>,
    nosignal: bool,
) -> isize {
    let r = sock_send(sock, data, dontwait, to);
    if r < 0 && !nosignal && errno::get() == EPIPE {
        crate::sig::sigpipe();
        errno::set(EPIPE);
    }
    r
}

pub fn sock_send(sock: &SockRef, data: &[u8], dontwait: bool, to: Option<([u8; 4], u16)>) -> isize {
    let mut s = sock.lock();
    let nonblock = s.nonblock || dontwait;
    let deadline = if nonblock {
        now()
    } else {
        deadline_of(s.snd_ms)
    };
    if !s.stream {
        if let Err(e) = ensure_udp(&mut s) {
            return failn(e);
        }
    }
    match &mut s.st {
        St::Stream(st) => {
            if data.is_empty() {
                return 0;
            }
            // blocking: all of it (the C contract of a blocking send); nonblocking: what fits
            if nonblock {
                match st.write(data, deadline) {
                    Ok(n) => n as isize,
                    Err(e) => failn(match e {
                        NetError::Stack(12) => EPIPE,
                        e => errno_of_net(e),
                    }),
                }
            } else {
                let mut done = 0;
                while done < data.len() {
                    match st.write(&data[done..], deadline) {
                        Ok(n) => done += n,
                        Err(e) => {
                            if done > 0 {
                                break;
                            }
                            return failn(if st.was_reset() && e == NetError::Stack(12) {
                                ECONNRESET
                            } else {
                                EPIPE
                            });
                        }
                    }
                }
                done as isize
            }
        }
        St::Udp { ep, peer } => {
            let Some((a, p)) = to.or(*peer) else {
                return failn(EDESTADDRREQ);
            };
            if data.len() > proto::net::limits::UDP_MAX {
                return failn(EMSGSIZE);
            }
            match ep.send_to(a, p, data) {
                Ok(()) => data.len() as isize,
                Err(e) => failn(errno_of_net(e)),
            }
        }
        St::Connecting(_) | St::New | St::Listener(_) => {
            failn(if s.stream { ENOTCONN } else { EINVAL })
        }
        St::Dead(e) => failn(*e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn send(fd: c_int, buf: *const c_void, n: usize, flags: c_int) -> isize {
    sendto(fd, buf, n, flags, core::ptr::null(), 0)
}

const MSG_DONTWAIT: c_int = 0x40;
const MSG_NOSIGNAL: c_int = 0x4000;
const MSG_OOB: c_int = 1;
const MSG_PEEK: c_int = 2;

#[no_mangle]
pub unsafe extern "C" fn sendto(
    fd: c_int,
    buf: *const c_void,
    n: usize,
    flags: c_int,
    to: *const c_void,
    tolen: u32,
) -> isize {
    let sock = match get(fd) {
        Ok(s) => s,
        Err(e) => return failn(e),
    };
    if flags & MSG_OOB != 0 {
        return failn(EOPNOTSUPP);
    }
    let dest = if to.is_null() {
        None
    } else {
        match addr_in(to, tolen) {
            Ok(a) => Some(a),
            Err(e) => return failn(e),
        }
    };
    let data: &[u8] = if n == 0 {
        &[]
    } else {
        core::slice::from_raw_parts(buf as *const u8, n)
    };
    send_signalling(
        &sock,
        data,
        flags & MSG_DONTWAIT != 0,
        dest,
        flags & MSG_NOSIGNAL != 0,
    )
}

#[no_mangle]
pub unsafe extern "C" fn recv(fd: c_int, buf: *mut c_void, n: usize, flags: c_int) -> isize {
    recvfrom(
        fd,
        buf,
        n,
        flags,
        core::ptr::null_mut(),
        core::ptr::null_mut(),
    )
}

#[no_mangle]
pub unsafe extern "C" fn recvfrom(
    fd: c_int,
    buf: *mut c_void,
    n: usize,
    flags: c_int,
    from: *mut c_void,
    fromlen: *mut u32,
) -> isize {
    let sock = match get(fd) {
        Ok(s) => s,
        Err(e) => return failn(e),
    };
    if flags & (MSG_OOB | MSG_PEEK) != 0 {
        return failn(EOPNOTSUPP);
    }
    if n == 0 {
        return 0;
    }
    let out = core::slice::from_raw_parts_mut(buf as *mut u8, n);
    let mut src = None;
    let r = sock_recv(&sock, out, flags & MSG_DONTWAIT != 0, Some(&mut src));
    if r >= 0 {
        if let Some(a) = src {
            put_addr(from, fromlen, a);
        }
    }
    r
}

#[no_mangle]
pub extern "C" fn shutdown(fd: c_int, how: c_int) -> c_int {
    let sock = match get(fd) {
        Ok(s) => s,
        Err(e) => return fail(e),
    };
    let mut s = sock.lock();
    if !(0..=2).contains(&how) {
        return fail(EINVAL);
    }
    if how == 0 || how == 2 {
        s.shut_rd = true;
    }
    if how >= 1 {
        match &mut s.st {
            St::Stream(st) => st.shutdown_write(),
            St::New | St::Connecting(_) | St::Listener(_) => return fail(ENOTCONN),
            _ => {}
        }
    }
    0
}

pub fn sock_close(sock: SockRef) {
    let mut s = sock.lock();
    let abort = s.linger_abort;
    if let St::Stream(st) = &mut s.st {
        if abort {
            st.abort();
        }
    }
    s.st = St::Dead(EBADF); // dropping the endpoint closes its channel and socket
}

// ---------------------------------------------------------------------------------------------------- names and options

#[no_mangle]
pub unsafe extern "C" fn getsockname(fd: c_int, addr: *mut c_void, len: *mut u32) -> c_int {
    let sock = match get(fd) {
        Ok(s) => s,
        Err(e) => return fail(e),
    };
    let s = sock.lock();
    let a = match &s.st {
        St::Stream(st) => st.local(),
        St::Connecting(st) => st.local(),
        St::Udp { ep, .. } => Some((s.bound.map_or([0; 4], |b| b.0), ep.port)),
        _ => s.bound,
    };
    match a {
        Some(a) => {
            put_addr(addr, len, a);
            0
        }
        None => {
            put_addr(addr, len, ([0; 4], 0));
            0
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn getpeername(fd: c_int, addr: *mut c_void, len: *mut u32) -> c_int {
    let sock = match get(fd) {
        Ok(s) => s,
        Err(e) => return fail(e),
    };
    let s = sock.lock();
    let a = match &s.st {
        St::Stream(st) => st.remote(),
        St::Udp { peer, .. } => *peer,
        _ => None,
    };
    match a {
        Some(a) => {
            put_addr(addr, len, a);
            0
        }
        None => fail(ENOTCONN),
    }
}

#[no_mangle]
pub unsafe extern "C" fn setsockopt(
    fd: c_int,
    level: c_int,
    name: c_int,
    val: *const c_void,
    len: u32,
) -> c_int {
    let sock = match get(fd) {
        Ok(s) => s,
        Err(e) => return fail(e),
    };
    let bytes: &[u8] = if val.is_null() {
        &[]
    } else {
        core::slice::from_raw_parts(val as *const u8, (len as usize).min(64))
    };
    let mut s = sock.lock();
    match sockshim::set_option(level, name, bytes) {
        Ok(SetOpt::Accept) => 0,
        Ok(SetOpt::SndBuf(v)) => {
            s.snd_buf = v;
            0
        }
        Ok(SetOpt::RcvBuf(v)) => {
            s.rcv_buf = v;
            0
        }
        Ok(SetOpt::RcvTimeoutMs(ms)) => {
            s.rcv_ms = ms;
            0
        }
        Ok(SetOpt::SndTimeoutMs(ms)) => {
            s.snd_ms = ms;
            0
        }
        Ok(SetOpt::NoDelay(on)) => {
            s.nodelay = on;
            0
        }
        Ok(SetOpt::LingerAbort(on)) => {
            s.linger_abort = on;
            0
        }
        Err(e) => fail(e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn getsockopt(
    fd: c_int,
    level: c_int,
    name: c_int,
    val: *mut c_void,
    len: *mut u32,
) -> c_int {
    let sock = match get(fd) {
        Ok(s) => s,
        Err(e) => return fail(e),
    };
    if val.is_null() || len.is_null() || (*len as usize) < 4 {
        return fail(EINVAL);
    }
    let mut s = sock.lock();
    let v: i32 = match (level, name) {
        (opt::SOL_SOCKET, opt::SO_TYPE) => {
            if s.stream {
                sockshim::SOCK_STREAM
            } else {
                sockshim::SOCK_DGRAM
            }
        }
        (opt::SOL_SOCKET, opt::SO_ERROR) => {
            // the outcome of a nonblocking connect, once known; an error is reported once
            if let St::Connecting(st) = &mut s.st {
                match st.connect_outcome() {
                    Some(Ok(())) => {
                        let St::Connecting(st) = core::mem::replace(&mut s.st, St::New) else {
                            unreachable!()
                        };
                        s.st = St::Stream(st);
                    }
                    Some(Err(e)) => s.err = sockshim::errno_of_status(e),
                    None => {}
                }
            }
            core::mem::take(&mut s.err)
        }
        (opt::SOL_SOCKET, opt::SO_REUSEADDR) => 1,
        (opt::SOL_SOCKET, opt::SO_SNDBUF) => s.snd_buf as i32,
        (opt::SOL_SOCKET, opt::SO_RCVBUF) => s.rcv_buf as i32,
        (opt::IPPROTO_TCP, opt::TCP_NODELAY) => i32::from(s.nodelay),
        _ => return fail(ENOPROTOOPT),
    };
    core::ptr::copy_nonoverlapping(v.to_ne_bytes().as_ptr(), val as *mut u8, 4);
    *len = 4;
    0
}

const F_GETFL: c_int = 3;
const F_SETFL: c_int = 4;
const O_NONBLOCK: c_int = 0o4000;

/// `fcntl` for the flag a socket descriptor supports.
pub fn sock_fcntl(sock: &SockRef, cmd: c_int, arg: c_int) -> c_int {
    let mut s = sock.lock();
    match cmd {
        F_GETFL => 2 | if s.nonblock { O_NONBLOCK } else { 0 },
        F_SETFL => {
            s.nonblock = arg & O_NONBLOCK != 0;
            0
        }
        1 | 2 => 0, // F_GETFD / F_SETFD
        _ => fail(EINVAL),
    }
}

// ---------------------------------------------------------------------------------------------------- poll and select

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PollFd {
    pub fd: c_int,
    pub events: i16,
    pub revents: i16,
}

/// The current readiness of a socket and the objects whose signals can change it: `(revents, [(handle, mask)])`.
fn sock_ready(s: &mut Sock, events: i16) -> (i16, Vec<(u64, u32)>) {
    let mut watch = Vec::new();
    let ev = match &mut s.st {
        St::Stream(st) => {
            let sigs = rt::sys::object_signals(st.socket()).a1 as u32;
            let readable = sigs & (sig::READABLE | sig::PEER_CLOSED | sig::WRITE_CLOSED) != 0;
            let writable = sigs & sig::WRITABLE != 0;
            let failed = st.failed();
            watch.push((
                st.socket(),
                sig::READABLE | sig::PEER_CLOSED | sig::WRITE_CLOSED | sig::WRITABLE,
            ));
            watch.push((st.channel(), sig::READABLE | sig::PEER_CLOSED));
            sockshim::poll_stream(events, readable, writable, failed)
        }
        St::Connecting(st) => {
            let out = st.connect_outcome();
            watch.push((st.channel(), sig::READABLE | sig::PEER_CLOSED));
            let failed = matches!(out, Some(Err(_)));
            if let Some(Err(e)) = out {
                s.err = sockshim::errno_of_status(e);
            }
            sockshim::poll_connecting(events, out.is_some(), failed)
        }
        St::Listener(l) => {
            watch.push((l.channel(), sig::READABLE | sig::PEER_CLOSED));
            let sigs = rt::sys::object_signals(l.channel()).a1 as u32;
            let mut r = 0;
            if sigs & sig::READABLE != 0 {
                r |= sockshim::POLLIN;
            }
            if sigs & sig::PEER_CLOSED != 0 {
                r |= sockshim::POLLHUP;
            }
            r & (events | sockshim::POLLHUP)
        }
        St::Udp { ep, .. } => {
            watch.push((ep.channel(), sig::READABLE | sig::PEER_CLOSED));
            let sigs = rt::sys::object_signals(ep.channel()).a1 as u32;
            let mut r = sockshim::POLLOUT;
            if sigs & sig::READABLE != 0 {
                r |= sockshim::POLLIN;
            }
            if sigs & sig::PEER_CLOSED != 0 {
                r |= sockshim::POLLHUP;
            }
            r & (events | sockshim::POLLHUP)
        }
        St::New => 0,
        St::Dead(_) => sockshim::POLLHUP,
    };
    (ev, watch)
}

/// `poll`: sockets are checked now, and if none is ready the objects behind them are bound to a port and waited on.
/// Descriptors that are not sockets (files, standard streams) are always ready.
#[no_mangle]
pub unsafe extern "C" fn poll(fds: *mut PollFd, nfds: u64, timeout_ms: c_int) -> c_int {
    if fds.is_null() && nfds > 0 {
        return fail(EINVAL);
    }
    // (no descriptors is a plain timed wait, and a null pointer is not a slice)
    let list: &mut [PollFd] = if nfds == 0 {
        &mut []
    } else {
        core::slice::from_raw_parts_mut(fds, nfds as usize)
    };
    let deadline = if timeout_ms < 0 {
        NEVER
    } else {
        now() + timeout_ms as u64
    };
    let mut port = 0u64;
    loop {
        // a signal's handler that ran ends the call (`EINTR`), before and while it waits
        if crate::sig::deliver().ran {
            if port != 0 {
                rt::sys::handle_close(port);
            }
            return fail(4);
        }
        let mut ready = 0;
        let mut watch: Vec<(u64, u32)> = Vec::new();
        for p in list.iter_mut() {
            p.revents = 0;
            if p.fd < 0 {
                continue;
            }
            match crate::fd::socket_of(p.fd) {
                Ok(Some(sock)) => {
                    let mut s = sock.lock();
                    let (r, w) = sock_ready(&mut s, p.events);
                    p.revents = r;
                    watch.extend(w);
                }
                Ok(None) => p.revents = p.events & (sockshim::POLLIN | sockshim::POLLOUT),
                Err(()) => p.revents = sockshim::POLLNVAL,
            }
            ready += i32::from(p.revents != 0);
        }
        if ready > 0 || timeout_ms == 0 || now() >= deadline {
            if port != 0 {
                rt::sys::handle_close(port);
            }
            return ready;
        }
        // nothing ready: wait for any of the objects (one-shot bindings on a fresh port), then look again
        if port != 0 {
            rt::sys::handle_close(port);
        }
        let p = rt::sys::port_create();
        if p.status != 0 {
            return fail(ENOBUFS);
        }
        port = p.a1 as u64;
        for (i, (h, mask)) in watch.iter().enumerate() {
            let _ =
                rt::sys::port_bind_flags(port, *h, *mask, i as u64, rt::abi::port_flags::ONESHOT);
        }
        let sigch = crate::sig::channel();
        if sigch != 0 {
            let _ = rt::sys::port_bind_flags(
                port,
                sigch,
                rt::abi::signals::READABLE,
                u64::MAX,
                rt::abi::port_flags::ONESHOT,
            );
        }
        // (no bindable object, such as a descriptor in the New state only: just sleep out the time)
        let _ = rt::sys::port_wait(port, deadline.min(now() + 1000));
    }
}

#[repr(C)]
pub struct FdSet {
    bits: [u64; 4],
}

#[no_mangle]
pub unsafe extern "C" fn select(
    n: c_int,
    r: *mut FdSet,
    w: *mut FdSet,
    e: *mut FdSet,
    tv: *const [i64; 2],
) -> c_int {
    let n = (n.max(0) as usize).min(256);
    let test = |s: *mut FdSet, i: usize| !s.is_null() && (*s).bits[i / 64] >> (i % 64) & 1 != 0;
    let mut fds = Vec::new();
    for i in 0..n {
        let mut ev = 0;
        if test(r, i) {
            ev |= sockshim::POLLIN;
        }
        if test(w, i) {
            ev |= sockshim::POLLOUT;
        }
        if ev != 0 || test(e, i) {
            fds.push(PollFd {
                fd: i as c_int,
                events: ev,
                revents: 0,
            });
        }
    }
    let timeout = if tv.is_null() {
        -1
    } else {
        (((*tv)[0] * 1000 + (*tv)[1] / 1000).clamp(0, i32::MAX as i64)) as c_int
    };
    let res = poll(fds.as_mut_ptr(), fds.len() as u64, timeout);
    if res < 0 {
        return res;
    }
    for s in [r, w, e] {
        if !s.is_null() {
            (*s).bits = [0; 4];
        }
    }
    let mut count = 0;
    for p in &fds {
        let i = p.fd as usize;
        let mut hit = false;
        if p.revents & (sockshim::POLLIN | sockshim::POLLHUP | sockshim::POLLERR) != 0
            && p.events & sockshim::POLLIN != 0
            && !r.is_null()
        {
            (*r).bits[i / 64] |= 1 << (i % 64);
            hit = true;
        }
        if p.revents & (sockshim::POLLOUT | sockshim::POLLERR) != 0
            && p.events & sockshim::POLLOUT != 0
            && !w.is_null()
        {
            (*w).bits[i / 64] |= 1 << (i % 64);
            hit = true;
        }
        count += i32::from(hit);
    }
    count
}

// ---------------------------------------------------------------------------------------------------- inet

#[no_mangle]
pub unsafe extern "C" fn inet_pton(af: c_int, src: *const u8, dst: *mut c_void) -> c_int {
    if af != 2 {
        errno::set(EAFNOSUPPORT);
        return -1;
    }
    let s = core::ffi::CStr::from_ptr(src as *const core::ffi::c_char).to_bytes();
    match sockshim::inet_pton4(s) {
        Some(a) => {
            core::ptr::copy_nonoverlapping(a.as_ptr(), dst as *mut u8, 4);
            1
        }
        None => 0,
    }
}

#[no_mangle]
pub unsafe extern "C" fn inet_ntop(
    af: c_int,
    src: *const c_void,
    dst: *mut u8,
    size: u32,
) -> *const u8 {
    if af != 2 {
        errno::set(EAFNOSUPPORT);
        return core::ptr::null();
    }
    let mut a = [0u8; 4];
    core::ptr::copy_nonoverlapping(src as *const u8, a.as_mut_ptr(), 4);
    let out = core::slice::from_raw_parts_mut(dst, size as usize);
    match sockshim::inet_ntop4(a, out) {
        Some(n) if n < out.len() => {
            out[n] = 0;
            dst
        }
        _ => {
            errno::set(28); // ENOSPC
            core::ptr::null()
        }
    }
}
