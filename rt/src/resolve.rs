//! The stub resolver in the client (S9-SPEC 11): `dns`'s state machine driven over a UDP endpoint of `/svc/net`. No process, no
//! registry name, no extra authority: a program that may use the network may resolve. Answers count only from the server's
//! address and port (5-tuple check), with the identifier and the question matching (`dns::parse_response`).
extern crate alloc;

use crate::net::{Net, NetError};
use crate::sync::Mutex;
use crate::sys;
use dns::{Cache, Failure, Query, Step};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveError {
    NoSuchName,
    NoData,
    Timeout,
    Truncated,
    BadName,
    /// The stack has no DNS server configured.
    NoServer,
    Net(NetError),
}

static CACHE: Mutex<Option<Cache>> = Mutex::new(None);
static COUNTER: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(1);

fn parse_quad(s: &[u8]) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut n = 0;
    for part in s.split(|&c| c == b'.') {
        if n == 4 || part.is_empty() || part.len() > 3 || !part.iter().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let v: u32 = part.iter().fold(0, |a, &d| a * 10 + u32::from(d - b'0'));
        if v > 255 {
            return None;
        }
        out[n] = v as u8;
        n += 1;
    }
    (n == 4).then_some(out)
}

/// Resolve `name` to an IPv4 address by `give_up` (an absolute tick). A dotted quad is parsed locally and `localhost` is
/// 127.0.0.1. `server` replaces the configured DNS server (the test hook). Positive answers are cached for the process.
pub fn resolve(
    net: &mut Net,
    name: &[u8],
    server: Option<([u8; 4], u16)>,
    give_up: u64,
) -> Result<[u8; 4], ResolveError> {
    if let Some(a) = parse_quad(name) {
        return Ok(a);
    }
    if name.eq_ignore_ascii_case(b"localhost") {
        return Ok([127, 0, 0, 1]);
    }
    if !dns::valid_name(name) {
        return Err(ResolveError::BadName);
    }
    let now = sys::clock_get();
    if let Some(a) = CACHE.lock().get_or_insert_with(Cache::new).get(name, now) {
        if let Some(first) = a.first() {
            return Ok(*first);
        }
    }
    let (addr, port) = match server {
        Some(s) => s,
        None => {
            let info = net.info(give_up).map_err(ResolveError::Net)?;
            if info.dns == [0; 4] {
                return Err(ResolveError::NoServer);
            }
            (info.dns, 53)
        }
    };
    let mut ep = net.open_udp(0, 0, give_up).map_err(ResolveError::Net)?;
    let seed = (sys::clock_get() as u32)
        .wrapping_mul(2_654_435_761)
        .wrapping_add(COUNTER.fetch_add(0x9e37, core::sync::atomic::Ordering::Relaxed))
        as u16;
    let mut q = Query::new(name, seed);
    loop {
        let now = sys::clock_get();
        if now >= give_up {
            return Err(ResolveError::Timeout);
        }
        match q.poll(now) {
            Step::Send(m) => {
                ep.send_to(addr, port, &m).map_err(ResolveError::Net)?;
                continue;
            }
            Step::Done(r) => return finish(name, r),
            Step::Wait => {}
        }
        let deadline = q.next_deadline().min(give_up);
        match ep.recv(deadline) {
            Ok(d) => {
                if d.src != addr || d.port != port {
                    continue; // not from the server we asked
                }
                if let Some(Step::Done(r)) = q.on_response(&d.payload) {
                    return finish(name, r);
                }
            }
            Err(NetError::TimedOut) => {}
            Err(e) => return Err(ResolveError::Net(e)),
        }
    }
}

fn finish(
    name: &[u8],
    r: Result<alloc::vec::Vec<[u8; 4]>, Failure>,
) -> Result<[u8; 4], ResolveError> {
    match r {
        Ok(addrs) => {
            // (the TTL is not carried by `Step`: cache for a minute, within the 300 s cap)
            CACHE
                .lock()
                .get_or_insert_with(Cache::new)
                .put(name, &addrs, 60, sys::clock_get());
            addrs.first().copied().ok_or(ResolveError::NoData)
        }
        Err(Failure::NoSuchName) => Err(ResolveError::NoSuchName),
        Err(Failure::NoData) => Err(ResolveError::NoData),
        Err(Failure::Truncated) => Err(ResolveError::Truncated),
        Err(Failure::Timeout) => Err(ResolveError::Timeout),
        Err(Failure::BadName) => Err(ResolveError::BadName),
    }
}
