//! A DNS stub resolver's pure core (S9-SPEC 11): build a query, parse and validate an answer, follow CNAMEs inside it, keep a
//! small cache, and drive the retry schedule. Sans-IO: time and bytes are passed in. Total: no input can make it panic,
//! loop, or allocate more than a fixed bound.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;

/// The longest name (RFC 1035: 255 bytes on the wire, 253 as text).
pub const MAX_NAME: usize = 253;
/// The most compression pointers followed in one name.
pub const MAX_JUMPS: usize = 16;
/// The most CNAME links followed.
pub const MAX_CNAMES: usize = 8;
pub const TYPE_A: u16 = 1;
pub const TYPE_CNAME: u16 = 5;
pub const CLASS_IN: u16 = 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// The name is empty, too long, has an empty or over-long label, or a character that does not belong.
    BadName,
    /// The message is shorter than it claims or malformed (a pointer loop, a label past the end, ...).
    Malformed,
}

/// Is `name` a plausible host name? Letters, digits, `-` (not first or last in a label), `_`, labels of 1 to 63 bytes, at most 253 in all, an optional
/// trailing dot.
pub fn valid_name(name: &[u8]) -> bool {
    let name = name.strip_suffix(b".").unwrap_or(name);
    if name.is_empty() || name.len() > MAX_NAME {
        return false;
    }
    name.split(|&c| c == b'.').all(|l| {
        !l.is_empty()
            && l.len() <= 63
            && l[0] != b'-'
            && l[l.len() - 1] != b'-'
            && l.iter()
                .all(|&c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    })
}

/// Build a query for `name` (type A, class IN, recursion desired, no EDNS) with identifier `id`.
pub fn build_query(id: u16, name: &[u8]) -> Result<Vec<u8>, Error> {
    if !valid_name(name) {
        return Err(Error::BadName);
    }
    let name = name.strip_suffix(b".").unwrap_or(name);
    let mut m = Vec::with_capacity(18 + name.len());
    m.extend_from_slice(&id.to_be_bytes());
    m.extend_from_slice(&[0x01, 0x00]); // RD
    m.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]); // one question
    for l in name.split(|&c| c == b'.') {
        m.push(l.len() as u8);
        m.extend_from_slice(l);
    }
    m.push(0);
    m.extend_from_slice(&TYPE_A.to_be_bytes());
    m.extend_from_slice(&CLASS_IN.to_be_bytes());
    Ok(m)
}

fn be16(b: &[u8], at: usize) -> Result<u16, Error> {
    b.get(at..at + 2)
        .map(|x| u16::from_be_bytes([x[0], x[1]]))
        .ok_or(Error::Malformed)
}

/// Decode the name at `at` (following at most `MAX_JUMPS` pointers) into lowercase text with dots; returns the name and the
/// offset just past it *in the original position* (after the first pointer or the terminating zero).
pub fn read_name(msg: &[u8], at: usize) -> Result<(Vec<u8>, usize), Error> {
    let mut out: Vec<u8> = Vec::new();
    let mut pos = at;
    let mut end: Option<usize> = None;
    let mut jumps = 0;
    loop {
        let len = *msg.get(pos).ok_or(Error::Malformed)?;
        match len & 0xc0 {
            0 => {
                pos += 1;
                if len == 0 {
                    return Ok((out, end.unwrap_or(pos)));
                }
                let l = usize::from(len);
                let label = msg.get(pos..pos + l).ok_or(Error::Malformed)?;
                if !out.is_empty() {
                    out.push(b'.');
                }
                if out.len() + l > MAX_NAME {
                    return Err(Error::Malformed);
                }
                out.extend(label.iter().map(|c| c.to_ascii_lowercase()));
                pos += l;
            }
            0xc0 => {
                let target = (usize::from(len & 0x3f) << 8)
                    | usize::from(*msg.get(pos + 1).ok_or(Error::Malformed)?);
                if end.is_none() {
                    end = Some(pos + 2);
                }
                jumps += 1;
                // a pointer must point backwards (strictly before itself) and only so many may be followed
                if jumps > MAX_JUMPS || target >= pos {
                    return Err(Error::Malformed);
                }
                pos = target;
            }
            _ => return Err(Error::Malformed), // the reserved label types
        }
    }
}

/// What a validated answer said.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Reply {
    /// One or more A records for the name asked (CNAME chain followed): the addresses and the smallest TTL.
    Addresses { addrs: Vec<[u8; 4]>, ttl: u32 },
    /// NXDOMAIN.
    NoSuchName,
    /// A good response with no A record for the name.
    NoData,
    /// SERVFAIL or REFUSED and the like: as good as no answer.
    ServerFailure,
    /// The truncated bit was set and no usable A record came with it.
    Truncated,
}

/// Parse `msg` as the response to the query `(id, name)`. `Ok(None)` for anything that is not our answer (wrong id, not a
/// response, a different question): the caller keeps waiting.
pub fn parse_response(msg: &[u8], id: u16, name: &[u8]) -> Result<Option<Reply>, Error> {
    if msg.len() < 12 {
        return Err(Error::Malformed);
    }
    if be16(msg, 0)? != id {
        return Ok(None);
    }
    let flags = be16(msg, 2)?;
    if flags & 0x8000 == 0 {
        return Ok(None); // a query, not a response
    }
    let (qd, an) = (usize::from(be16(msg, 4)?), usize::from(be16(msg, 6)?));
    if qd != 1 {
        return Ok(None);
    }
    let want = name.strip_suffix(b".").unwrap_or(name);
    let (qname, mut pos) = read_name(msg, 12)?;
    if be16(msg, pos)? != TYPE_A
        || be16(msg, pos + 2)? != CLASS_IN
        || !qname.eq_ignore_ascii_case(want)
    {
        return Ok(None);
    }
    pos += 4;
    let rcode = flags & 0xf;
    let truncated = flags & 0x0200 != 0;
    match rcode {
        0 => {}
        3 => return Ok(Some(Reply::NoSuchName)),
        _ => return Ok(Some(Reply::ServerFailure)),
    }
    // the answers: follow the CNAME chain from the name asked
    let mut current: Vec<u8> = qname;
    let mut cnames = 0;
    let mut addrs: Vec<[u8; 4]> = Vec::new();
    let mut ttl = u32::MAX;
    for _ in 0..an.min(64) {
        let (owner, p) = match read_name(msg, pos) {
            Ok(x) => x,
            Err(_) if truncated => break, // a cut-off record at the end of a truncated answer
            Err(e) => return Err(e),
        };
        let (ty, class, rttl, rdlen) = match (
            be16(msg, p),
            be16(msg, p + 2),
            msg.get(p + 4..p + 8),
            be16(msg, p + 8),
        ) {
            (Ok(t), Ok(c), Some(l), Ok(r)) => (
                t,
                c,
                u32::from_be_bytes([l[0], l[1], l[2], l[3]]),
                usize::from(r),
            ),
            _ if truncated => break,
            _ => return Err(Error::Malformed),
        };
        let rdata_at = p + 10;
        let Some(rdata) = msg.get(rdata_at..rdata_at + rdlen) else {
            if truncated {
                break;
            }
            return Err(Error::Malformed);
        };
        pos = rdata_at + rdlen;
        if class != CLASS_IN || owner != current {
            continue;
        }
        match ty {
            TYPE_A if rdlen == 4 => {
                addrs.push([rdata[0], rdata[1], rdata[2], rdata[3]]);
                ttl = ttl.min(rttl);
            }
            TYPE_CNAME => {
                cnames += 1;
                if cnames > MAX_CNAMES {
                    return Err(Error::Malformed);
                }
                let (target, _) = read_name(msg, rdata_at)?;
                current = target;
                ttl = ttl.min(rttl);
            }
            _ => {}
        }
    }
    if !addrs.is_empty() {
        return Ok(Some(Reply::Addresses { addrs, ttl }));
    }
    Ok(Some(if truncated {
        Reply::Truncated
    } else {
        Reply::NoData
    }))
}

// ---------------------------------------------------------------------------------------------------- cache

pub const CACHE_ENTRIES: usize = 16;
pub const MAX_TTL_SECS: u32 = 300;

#[derive(Default)]
pub struct Cache {
    entries: Vec<(Vec<u8>, Vec<[u8; 4]>, u64)>, // (name, addresses, expires at ms)
}

impl Cache {
    pub fn new() -> Cache {
        Cache::default()
    }

    pub fn get(&mut self, name: &[u8], now_ms: u64) -> Option<Vec<[u8; 4]>> {
        self.entries.retain(|e| e.2 > now_ms);
        let n = name.strip_suffix(b".").unwrap_or(name);
        self.entries
            .iter()
            .find(|e| e.0.eq_ignore_ascii_case(n))
            .map(|e| e.1.clone())
    }

    pub fn put(&mut self, name: &[u8], addrs: &[[u8; 4]], ttl_secs: u32, now_ms: u64) {
        if addrs.is_empty() || ttl_secs == 0 {
            return;
        }
        let n = name.strip_suffix(b".").unwrap_or(name).to_ascii_lowercase();
        self.entries.retain(|e| e.0 != n && e.2 > now_ms);
        if self.entries.len() >= CACHE_ENTRIES {
            self.entries.remove(0);
        }
        let life = u64::from(ttl_secs.min(MAX_TTL_SECS)) * 1000;
        self.entries.push((n, addrs.to_vec(), now_ms + life));
    }
}

// ---------------------------------------------------------------------------------------------------- the query

/// What the caller should do next.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Step {
    /// Send this query now (then call `next_deadline` for when to look again).
    Send(Vec<u8>),
    /// Nothing to do until the deadline.
    Wait,
    /// The lookup is over.
    Done(Result<Vec<[u8; 4]>, Failure>),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Failure {
    NoSuchName,
    NoData,
    Truncated,
    /// No answer in time (or only server failures).
    Timeout,
    BadName,
}

/// One lookup: sends at 0, 1 s and 3 s, fails 4 s after the last send (7 s in all).
pub struct Query {
    name: Vec<u8>,
    first_id: u16,
    sent: usize,
    start_ms: u64,
    started: bool,
    done: bool,
}

const SENDS: [u64; 3] = [0, 1000, 3000];
const FAIL_AT: u64 = 7000;

impl Query {
    /// `id_seed` makes the first identifier; each re-send uses the next one (so a late answer to an earlier send is still ours).
    pub fn new(name: &[u8], id_seed: u16) -> Query {
        Query {
            name: name.to_vec(),
            first_id: id_seed,
            sent: 0,
            start_ms: 0,
            started: false,
            done: false,
        }
    }

    fn id(&self, n: usize) -> u16 {
        self.first_id.wrapping_add(n as u16)
    }

    pub fn poll(&mut self, now_ms: u64) -> Step {
        if self.done {
            return Step::Wait;
        }
        if !self.started {
            self.started = true;
            self.start_ms = now_ms;
        }
        if self.sent < SENDS.len() && now_ms >= self.start_ms + SENDS[self.sent] {
            let id = self.id(self.sent);
            self.sent += 1;
            return match build_query(id, &self.name) {
                Ok(q) => Step::Send(q),
                Err(_) => {
                    self.done = true;
                    Step::Done(Err(Failure::BadName))
                }
            };
        }
        if self.sent == SENDS.len() && now_ms >= self.start_ms + FAIL_AT {
            self.done = true;
            return Step::Done(Err(Failure::Timeout));
        }
        Step::Wait
    }

    /// When `poll` next has something to do (milliseconds on the caller's clock).
    pub fn next_deadline(&self) -> u64 {
        if self.sent < SENDS.len() {
            self.start_ms + SENDS[self.sent]
        } else {
            self.start_ms + FAIL_AT
        }
    }

    /// Feed a received datagram. `Some(Done(..))` when it settles the lookup; ignored (None) otherwise.
    pub fn on_response(&mut self, msg: &[u8]) -> Option<Step> {
        if self.done {
            return None;
        }
        for n in 0..self.sent {
            match parse_response(msg, self.id(n), &self.name) {
                Ok(Some(r)) => {
                    let out = match r {
                        Reply::Addresses { addrs, .. } => Ok(addrs),
                        Reply::NoSuchName => Err(Failure::NoSuchName),
                        Reply::NoData => Err(Failure::NoData),
                        Reply::Truncated => Err(Failure::Truncated),
                        Reply::ServerFailure => return None, // as good as no answer: keep waiting and retrying
                    };
                    self.done = true;
                    return Some(Step::Done(out));
                }
                Ok(None) => continue,
                Err(_) => return None,
            }
        }
        None
    }
}
