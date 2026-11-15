//! The native network protocol (S8, NET-SPEC 7): what a program says to the network stack `netd`. All integers little
//! endian; every decoder is total (no panic on any input, the tests include a mutation fuzz).
//!
//! A **service connection** (from the registry: `/svc/net`, `/svc/net-priv`, `/svc/net-config`) carries request/reply
//! messages with transaction ids; an **endpoint channel** carries the datagram messages. Authority is which name a program's
//! view contains, not an identity: the class of a connection decides which operations and flags it may use.

use crate::{rd_u16, rd_u32};
use alloc::vec::Vec;

/// The three classes of authority (NET-SPEC 4.4), one registry name each.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Class {
    /// `/svc/net`: info; UDP and echo endpoints on ports 1024 and up; no broadcast, no source 0.0.0.0, no configuration.
    Normal,
    /// `/svc/net-priv`: normal plus any port, limited broadcast and source 0.0.0.0 (while unconfigured).
    Privileged,
    /// `/svc/net-config`: info, the ARP table, the counters, and setting or clearing the address. No endpoints.
    Config,
}

impl Class {
    pub const NAMES: [(&'static [u8], Class); 3] = [
        (b"net", Class::Normal),
        (b"net-priv", Class::Privileged),
        (b"net-config", Class::Config),
    ];
}

pub mod op {
    pub const INFO: u16 = 1;
    pub const OPEN: u16 = 2;
    pub const SET_CONFIG: u16 = 16;
    pub const CLEAR_CONFIG: u16 = 17;
    pub const GET_ARP: u16 = 18;
    pub const GET_STATS: u16 = 19;
}

/// Reply statuses (`i32`). `UNAVAILABLE` is the registry's, not ours.
pub mod status {
    pub const OK: i32 = 0;
    pub const BAD_REQUEST: i32 = 1;
    pub const REFUSED: i32 = 2;
    pub const ADDR_IN_USE: i32 = 3;
    pub const TOO_BIG: i32 = 4;
    pub const NO_ROUTE: i32 = 5;
    pub const QUOTA: i32 = 6;
    pub const NOT_CONFIGURED: i32 = 7;
    pub const NO_BUFFERS: i32 = 8;
    pub const NOT_SUPPORTED: i32 = 9;
}

pub mod kind {
    pub const UDP: u16 = 1;
    pub const ICMP_ECHO: u16 = 2;
}

/// `oflags` of `Open`.
pub mod oflags {
    /// May send the limited broadcast (privileged only).
    pub const BROADCAST: u32 = 1;
    /// May send from 0.0.0.0 while unconfigured (privileged only).
    pub const SRC_ANY: u32 = 2;
    pub const ALL: u32 = BROADCAST | SRC_ANY;
}

/// Limits in one place (NET-SPEC 7.4, 10).
pub mod limits {
    /// Connections per client (account): the service connection and its endpoints.
    pub const CONNS_PER_ACCOUNT: u32 = 32;
    /// Connections in the whole pool.
    pub const POOL_CONNS: u32 = 256;
    pub const UDP_MAX: usize = 1472;
    /// The lowest port a normal connection may bind.
    pub const FIRST_UNPRIVILEGED_PORT: u16 = 1024;
    /// The most ARP entries `GetArp` returns.
    pub const ARP_MAX: usize = 64;
}

pub const REQUEST_HEADER: usize = 8;
pub const REPLY_HEADER: usize = 8;

/// A decoded service request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Info,
    Open {
        kind: u16,
        local_port: u16,
        local_addr: [u8; 4],
        oflags: u32,
    },
    SetConfig {
        addr: [u8; 4],
        mask: [u8; 4],
        gateway: [u8; 4],
        dns: [u8; 4],
    },
    ClearConfig,
    GetArp,
    GetStats,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Malformed {
    /// The transaction id, if the message had one (so the refusal can name it).
    pub txid: Option<u32>,
}

pub fn encode_request(txid: u32, flags: u16, r: &Request) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&txid.to_le_bytes());
    let (op, body): (u16, Vec<u8>) = match r {
        Request::Info => (op::INFO, Vec::new()),
        Request::Open {
            kind,
            local_port,
            local_addr,
            oflags,
        } => {
            let mut v = Vec::new();
            v.extend_from_slice(&kind.to_le_bytes());
            v.extend_from_slice(&local_port.to_le_bytes());
            v.extend_from_slice(local_addr);
            v.extend_from_slice(&oflags.to_le_bytes());
            (op::OPEN, v)
        }
        Request::SetConfig {
            addr,
            mask,
            gateway,
            dns,
        } => {
            let mut v = Vec::new();
            for a in [addr, mask, gateway, dns] {
                v.extend_from_slice(a);
            }
            (op::SET_CONFIG, v)
        }
        Request::ClearConfig => (op::CLEAR_CONFIG, Vec::new()),
        Request::GetArp => (op::GET_ARP, Vec::new()),
        Request::GetStats => (op::GET_STATS, Vec::new()),
    };
    b.extend_from_slice(&op.to_le_bytes());
    b.extend_from_slice(&flags.to_le_bytes());
    b.extend_from_slice(&body);
    b
}

/// Decode a service request: `(txid, flags, request)`. Total: every other length or op is `Malformed`.
pub fn decode_request(b: &[u8]) -> Result<(u32, u16, Request), Malformed> {
    let txid = rd_u32(b, 0);
    let bad = Malformed { txid };
    let (Some(t), Some(op), Some(flags)) = (txid, rd_u16(b, 4), rd_u16(b, 6)) else {
        return Err(bad);
    };
    let body = &b[REQUEST_HEADER..];
    let ip = |at: usize| -> Option<[u8; 4]> { body.get(at..at + 4)?.try_into().ok() };
    let req = match (op, body.len()) {
        (op::INFO, 0) => Request::Info,
        (op::OPEN, 12) => Request::Open {
            kind: rd_u16(body, 0).ok_or(bad)?,
            local_port: rd_u16(body, 2).ok_or(bad)?,
            local_addr: ip(4).ok_or(bad)?,
            oflags: rd_u32(body, 8).ok_or(bad)?,
        },
        (op::SET_CONFIG, 16) => Request::SetConfig {
            addr: ip(0).ok_or(bad)?,
            mask: ip(4).ok_or(bad)?,
            gateway: ip(8).ok_or(bad)?,
            dns: ip(12).ok_or(bad)?,
        },
        (op::CLEAR_CONFIG, 0) => Request::ClearConfig,
        (op::GET_ARP, 0) => Request::GetArp,
        (op::GET_STATS, 0) => Request::GetStats,
        _ => return Err(bad),
    };
    Ok((t, flags, req))
}

pub fn encode_reply(txid: u32, status: i32, body: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(REPLY_HEADER + body.len());
    b.extend_from_slice(&txid.to_le_bytes());
    b.extend_from_slice(&status.to_le_bytes());
    b.extend_from_slice(body);
    b
}

/// `(txid, status, body)`; `None` for anything shorter than the header.
pub fn decode_reply(b: &[u8]) -> Option<(u32, i32, &[u8])> {
    Some((rd_u32(b, 0)?, rd_u32(b, 4)? as i32, b.get(REPLY_HEADER..)?))
}

/// What `Info` returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info {
    pub addr: [u8; 4],
    pub mask: [u8; 4],
    pub gateway: [u8; 4],
    pub dns: [u8; 4],
    pub mac: [u8; 6],
    pub mtu: u16,
    pub link: bool,
    pub configured: bool,
}

pub const INFO_LEN: usize = 28;

pub fn encode_info(i: &Info) -> Vec<u8> {
    let mut b = Vec::with_capacity(INFO_LEN);
    for a in [&i.addr, &i.mask, &i.gateway, &i.dns] {
        b.extend_from_slice(a);
    }
    b.extend_from_slice(&i.mac);
    b.extend_from_slice(&i.mtu.to_le_bytes());
    b.push(u8::from(i.link));
    b.push(u8::from(i.configured));
    b.extend_from_slice(&[0, 0]);
    b
}

pub fn decode_info(b: &[u8]) -> Option<Info> {
    if b.len() != INFO_LEN || b[26..28] != [0, 0] {
        return None;
    }
    let ip = |at: usize| -> Option<[u8; 4]> { b.get(at..at + 4)?.try_into().ok() };
    Some(Info {
        addr: ip(0)?,
        mask: ip(4)?,
        gateway: ip(8)?,
        dns: ip(12)?,
        mac: b.get(16..22)?.try_into().ok()?,
        mtu: rd_u16(b, 22)?,
        link: *b.get(24)? != 0,
        configured: *b.get(25)? != 0,
    })
}

/// The reply to `Open`: the binding made.
pub fn encode_open_reply(port: u16, addr: [u8; 4]) -> Vec<u8> {
    let mut b = Vec::with_capacity(8);
    b.extend_from_slice(&port.to_le_bytes());
    b.extend_from_slice(&[0, 0]);
    b.extend_from_slice(&addr);
    b
}

pub fn decode_open_reply(b: &[u8]) -> Option<(u16, [u8; 4])> {
    if b.len() != 8 {
        return None;
    }
    Some((rd_u16(b, 0)?, b.get(4..8)?.try_into().ok()?))
}

// ---- endpoint channel messages ----

pub mod ep {
    /// Client to stack.
    pub const SEND_UDP: u8 = 1;
    pub const SEND_ECHO: u8 = 2;
    /// Stack to client.
    pub const RECV: u8 = 0x81;
    pub const ECHO_REPLY: u8 = 0x82;
    pub const NOTICE: u8 = 0x83;
}

/// `Notice` codes (a `netcore::notice` subset).
pub mod notice {
    pub const SEND_FAILED: u16 = 1;
    pub const LINK_DOWN: u16 = 2;
    pub const LINK_UP: u16 = 3;
}

/// Messages a client sends on an endpoint channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EpRequest<'a> {
    SendUdp {
        cookie: u32,
        dst: [u8; 4],
        port: u16,
        payload: &'a [u8],
    },
    SendEcho {
        cookie: u32,
        dst: [u8; 4],
        seq: u16,
        payload: &'a [u8],
    },
}

/// `kind u8 | pad[3] | cookie u32 | dst_addr u32 | dst_port_or_seq u16 | pad u16 | payload`.
pub fn encode_send_udp(cookie: u32, dst: [u8; 4], port: u16, payload: &[u8]) -> Vec<u8> {
    let mut b = alloc::vec![ep::SEND_UDP, 0, 0, 0];
    b.extend_from_slice(&cookie.to_le_bytes());
    b.extend_from_slice(&dst);
    b.extend_from_slice(&port.to_le_bytes());
    b.extend_from_slice(&[0, 0]);
    b.extend_from_slice(payload);
    b
}

pub fn encode_send_echo(cookie: u32, dst: [u8; 4], seq: u16, payload: &[u8]) -> Vec<u8> {
    let mut b = encode_send_udp(cookie, dst, seq, payload);
    b[0] = ep::SEND_ECHO;
    b
}

pub const EP_SEND_HEADER: usize = 16;

pub fn decode_ep_request(b: &[u8]) -> Option<EpRequest<'_>> {
    if b.len() < EP_SEND_HEADER || b[1..4] != [0, 0, 0] || b[14..16] != [0, 0] {
        return None;
    }
    let cookie = rd_u32(b, 4)?;
    let dst: [u8; 4] = b.get(8..12)?.try_into().ok()?;
    let n = rd_u16(b, 12)?;
    let payload = b.get(EP_SEND_HEADER..)?;
    match b[0] {
        ep::SEND_UDP if payload.len() <= limits::UDP_MAX => Some(EpRequest::SendUdp {
            cookie,
            dst,
            port: n,
            payload,
        }),
        ep::SEND_ECHO if payload.len() <= limits::UDP_MAX - 8 => Some(EpRequest::SendEcho {
            cookie,
            dst,
            seq: n,
            payload,
        }),
        _ => None,
    }
}

/// Messages the stack sends on an endpoint channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EpEvent<'a> {
    Recv {
        dropped: u32,
        src: [u8; 4],
        port: u16,
        payload: &'a [u8],
    },
    EchoReply {
        dropped: u32,
        src: [u8; 4],
        seq: u16,
        ttl: u8,
        payload: &'a [u8],
    },
    Notice {
        code: u16,
        cookie: u32,
        detail: u32,
    },
}

/// `kind u8 | pad[3] | dropped u32 | src u32 | port_or_seq u16 | ttl u8 | pad u8 | payload`.
pub fn encode_recv(dropped: u32, src: [u8; 4], port: u16, payload: &[u8]) -> Vec<u8> {
    let mut b = alloc::vec![ep::RECV, 0, 0, 0];
    b.extend_from_slice(&dropped.to_le_bytes());
    b.extend_from_slice(&src);
    b.extend_from_slice(&port.to_le_bytes());
    b.extend_from_slice(&[0, 0]);
    b.extend_from_slice(payload);
    b
}

pub fn encode_echo_reply(dropped: u32, src: [u8; 4], seq: u16, ttl: u8, payload: &[u8]) -> Vec<u8> {
    let mut b = alloc::vec![ep::ECHO_REPLY, 0, 0, 0];
    b.extend_from_slice(&dropped.to_le_bytes());
    b.extend_from_slice(&src);
    b.extend_from_slice(&seq.to_le_bytes());
    b.push(ttl);
    b.push(0);
    b.extend_from_slice(payload);
    b
}

pub fn encode_notice(code: u16, cookie: u32, detail: u32) -> Vec<u8> {
    let mut b = alloc::vec![ep::NOTICE, 0, 0, 0];
    b.extend_from_slice(&code.to_le_bytes());
    b.extend_from_slice(&[0, 0]);
    b.extend_from_slice(&cookie.to_le_bytes());
    b.extend_from_slice(&detail.to_le_bytes());
    b
}

pub const EP_EVENT_HEADER: usize = 16;

pub fn decode_ep_event(b: &[u8]) -> Option<EpEvent<'_>> {
    match *b.first()? {
        ep::RECV | ep::ECHO_REPLY => {
            if b.len() < EP_EVENT_HEADER || b[1..4] != [0, 0, 0] {
                return None;
            }
            let dropped = rd_u32(b, 4)?;
            let src: [u8; 4] = b.get(8..12)?.try_into().ok()?;
            let n = rd_u16(b, 12)?;
            let payload = b.get(EP_EVENT_HEADER..)?;
            if b[0] == ep::RECV {
                (b[15] == 0).then_some(EpEvent::Recv {
                    dropped,
                    src,
                    port: n,
                    payload,
                })
            } else {
                Some(EpEvent::EchoReply {
                    dropped,
                    src,
                    seq: n,
                    ttl: b[14],
                    payload,
                })
            }
        }
        ep::NOTICE => {
            if b.len() != 16 || b[1..4] != [0, 0, 0] {
                return None;
            }
            Some(EpEvent::Notice {
                code: rd_u16(b, 4)?,
                cookie: rd_u32(b, 8)?,
                detail: rd_u32(b, 12)?,
            })
        }
        _ => None,
    }
}

/// `GetArp`: a count then `ip u32, mac[6], pad u16, age_ms u32` entries (16 bytes each).
pub fn encode_arp(entries: &[([u8; 4], [u8; 6], u32)]) -> Vec<u8> {
    let n = entries.len().min(limits::ARP_MAX);
    let mut b = Vec::with_capacity(4 + 16 * n);
    b.extend_from_slice(&(n as u32).to_le_bytes());
    for (ip, mac, age) in &entries[..n] {
        b.extend_from_slice(ip);
        b.extend_from_slice(mac);
        b.extend_from_slice(&[0, 0]);
        b.extend_from_slice(&age.to_le_bytes());
    }
    b
}

pub fn decode_arp(b: &[u8]) -> Option<Vec<([u8; 4], [u8; 6], u32)>> {
    let n = rd_u32(b, 0)? as usize;
    if n > limits::ARP_MAX || b.len() != 4 + 16 * n {
        return None;
    }
    (0..n)
        .map(|i| {
            let e = 4 + 16 * i;
            Some((
                b.get(e..e + 4)?.try_into().ok()?,
                b.get(e + 4..e + 10)?.try_into().ok()?,
                rd_u32(b, e + 12)?,
            ))
        })
        .collect()
}

/// `GetStats`: a count, then `name_id u16, pad u16, value u64` entries (12 bytes each).
pub fn encode_stats(pairs: &[(u16, u64)]) -> Vec<u8> {
    let mut b = Vec::with_capacity(4 + 12 * pairs.len());
    b.extend_from_slice(&(pairs.len() as u32).to_le_bytes());
    for (id, v) in pairs {
        b.extend_from_slice(&id.to_le_bytes());
        b.extend_from_slice(&[0, 0]);
        b.extend_from_slice(&v.to_le_bytes());
    }
    b
}

pub fn decode_stats(b: &[u8]) -> Option<Vec<(u16, u64)>> {
    let n = rd_u32(b, 0)? as usize;
    if n > 1024 || b.len() != 4 + 12 * n {
        return None;
    }
    (0..n)
        .map(|i| {
            let e = 4 + 12 * i;
            Some((
                rd_u16(b, e)?,
                u64::from_le_bytes(b.get(e + 4..e + 12)?.try_into().ok()?),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn requests_round_trip_and_have_goldens() {
        for r in [
            Request::Info,
            Request::Open {
                kind: kind::UDP,
                local_port: 7777,
                local_addr: [0; 4],
                oflags: oflags::BROADCAST,
            },
            Request::SetConfig {
                addr: [10, 0, 2, 15],
                mask: [255, 255, 255, 0],
                gateway: [10, 0, 2, 2],
                dns: [10, 0, 2, 3],
            },
            Request::ClearConfig,
            Request::GetArp,
            Request::GetStats,
        ] {
            let b = encode_request(9, 0, &r);
            assert_eq!(decode_request(&b), Ok((9, 0, r)));
        }
        assert_eq!(
            encode_request(7, 0, &Request::Info),
            [7, 0, 0, 0, 1, 0, 0, 0]
        );
        assert_eq!(
            encode_request(
                1,
                0,
                &Request::Open {
                    kind: 1,
                    local_port: 0x1e61,
                    local_addr: [0; 4],
                    oflags: 2
                }
            ),
            [1, 0, 0, 0, 2, 0, 0, 0, 1, 0, 0x61, 0x1e, 0, 0, 0, 0, 2, 0, 0, 0]
        );
    }

    #[test]
    fn malformed_requests_are_errors_that_keep_the_txid() {
        assert_eq!(decode_request(&[]), Err(Malformed { txid: None }));
        assert_eq!(
            decode_request(&[5, 0, 0, 0, 1, 0]),
            Err(Malformed { txid: Some(5) })
        );
        assert_eq!(
            decode_request(&[5, 0, 0, 0, 99, 0, 0, 0]),
            Err(Malformed { txid: Some(5) }),
            "an unknown op"
        );
        assert_eq!(
            decode_request(&[5, 0, 0, 0, 1, 0, 0, 0, 0]),
            Err(Malformed { txid: Some(5) }),
            "Info has no body"
        );
        let mut open = encode_request(
            5,
            0,
            &Request::Open {
                kind: 1,
                local_port: 1,
                local_addr: [0; 4],
                oflags: 0,
            },
        );
        open.pop();
        assert_eq!(decode_request(&open), Err(Malformed { txid: Some(5) }));
    }

    #[test]
    fn replies_and_info_round_trip() {
        let i = Info {
            addr: [10, 0, 2, 15],
            mask: [255, 255, 255, 0],
            gateway: [10, 0, 2, 2],
            dns: [10, 0, 2, 3],
            mac: [1, 2, 3, 4, 5, 6],
            mtu: 1500,
            link: true,
            configured: true,
        };
        let b = encode_info(&i);
        assert_eq!(b.len(), INFO_LEN);
        assert_eq!(decode_info(&b), Some(i));
        assert_eq!(decode_info(&b[..27]), None);
        let r = encode_reply(3, status::QUOTA, &b);
        assert_eq!(decode_reply(&r), Some((3, status::QUOTA, &b[..])));
        assert_eq!(decode_reply(&[1, 2, 3]), None);
        assert_eq!(
            decode_open_reply(&encode_open_reply(49152, [0; 4])),
            Some((49152, [0; 4]))
        );
    }

    #[test]
    fn endpoint_messages_round_trip() {
        let s = encode_send_udp(5, [10, 0, 2, 2], 7, b"hello");
        assert_eq!(
            decode_ep_request(&s),
            Some(EpRequest::SendUdp {
                cookie: 5,
                dst: [10, 0, 2, 2],
                port: 7,
                payload: b"hello"
            })
        );
        let e = encode_send_echo(6, [10, 0, 2, 2], 3, b"x");
        assert_eq!(
            decode_ep_request(&e),
            Some(EpRequest::SendEcho {
                cookie: 6,
                dst: [10, 0, 2, 2],
                seq: 3,
                payload: b"x"
            })
        );
        assert_eq!(
            decode_ep_request(&encode_send_udp(1, [0; 4], 1, &vec![0; 1473])),
            None,
            "an oversize payload"
        );
        assert!(decode_ep_request(&encode_send_udp(1, [0; 4], 1, &vec![0; 1472])).is_some());
        let r = encode_recv(2, [10, 0, 2, 2], 4000, b"abc");
        assert_eq!(
            decode_ep_event(&r),
            Some(EpEvent::Recv {
                dropped: 2,
                src: [10, 0, 2, 2],
                port: 4000,
                payload: b"abc"
            })
        );
        let r = encode_echo_reply(0, [10, 0, 2, 2], 9, 64, b"pp");
        assert_eq!(
            decode_ep_event(&r),
            Some(EpEvent::EchoReply {
                dropped: 0,
                src: [10, 0, 2, 2],
                seq: 9,
                ttl: 64,
                payload: b"pp"
            })
        );
        let n = encode_notice(notice::SEND_FAILED, 7, 5);
        assert_eq!(
            decode_ep_event(&n),
            Some(EpEvent::Notice {
                code: 1,
                cookie: 7,
                detail: 5
            })
        );
        assert_eq!(decode_ep_event(&n[..15]), None);
        assert_eq!(decode_ep_event(&[0x77]), None);
        assert_eq!(decode_ep_event(&[]), None);
    }

    #[test]
    fn tables_round_trip() {
        let arp = vec![
            ([10, 0, 2, 2], [1, 2, 3, 4, 5, 6], 1234u32),
            ([10, 0, 2, 3], [6, 5, 4, 3, 2, 1], 0),
        ];
        assert_eq!(decode_arp(&encode_arp(&arp)), Some(arp));
        assert_eq!(decode_arp(&[1, 0, 0, 0]), None, "a count with no entries");
        assert_eq!(decode_arp(&[65, 0, 0, 0]), None, "more than the bound");
        let st = vec![(0u16, 5u64), (7, u64::MAX)];
        assert_eq!(decode_stats(&encode_stats(&st)), Some(st));
        assert_eq!(decode_stats(&[2, 0, 0, 0, 0]), None);
    }

    #[test]
    fn no_decoder_panics_on_random_or_mutated_input() {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let valid: Vec<Vec<u8>> = vec![
            encode_request(
                1,
                0,
                &Request::Open {
                    kind: 1,
                    local_port: 7,
                    local_addr: [1; 4],
                    oflags: 3,
                },
            ),
            encode_request(
                2,
                0,
                &Request::SetConfig {
                    addr: [1; 4],
                    mask: [2; 4],
                    gateway: [3; 4],
                    dns: [4; 4],
                },
            ),
            encode_send_udp(1, [1; 4], 5, b"payload"),
            encode_recv(1, [1; 4], 5, b"payload"),
            encode_notice(1, 2, 3),
            encode_arp(&[([1; 4], [2; 6], 3)]),
            encode_stats(&[(1, 2)]),
            encode_info(&Info {
                addr: [1; 4],
                mask: [2; 4],
                gateway: [3; 4],
                dns: [4; 4],
                mac: [5; 6],
                mtu: 1500,
                link: true,
                configured: false,
            }),
        ];
        for _ in 0..60_000 {
            let n = (next() % 100) as usize;
            let b: Vec<u8> = (0..n).map(|_| next() as u8).collect();
            let _ = decode_request(&b);
            let _ = decode_ep_request(&b);
            let _ = decode_ep_event(&b);
            let _ = decode_reply(&b);
            let _ = decode_info(&b);
            let _ = decode_arp(&b);
            let _ = decode_stats(&b);
            let _ = decode_open_reply(&b);
        }
        for v in &valid {
            for cut in 0..v.len() {
                let b = &v[..cut];
                let _ = (
                    decode_request(b),
                    decode_ep_request(b),
                    decode_ep_event(b),
                    decode_info(b),
                    decode_arp(b),
                    decode_stats(b),
                );
            }
            for _ in 0..5_000 {
                let mut m = v.clone();
                let i = (next() as usize) % m.len();
                m[i] = next() as u8;
                let _ = (
                    decode_request(&m),
                    decode_ep_request(&m),
                    decode_ep_event(&m),
                    decode_info(&m),
                    decode_arp(&m),
                    decode_stats(&m),
                );
            }
        }
    }
}
