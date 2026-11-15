//! H4 of NET-SPEC 12.1: scripted packet tests. Exact frames in, exact frames and events out, with a controlled clock.
use netcore::*;
use netwire::{arp, eth, icmp, ipv4, udp, BROADCAST_IP, BROADCAST_MAC};

const ME: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
const GW_MAC: [u8; 6] = [0x52, 0x55, 10, 0, 2, 2];
const PEER_MAC: [u8; 6] = [0x52, 0x55, 10, 0, 2, 20];
const MY_IP: [u8; 4] = [10, 0, 2, 15];
const GW: [u8; 4] = [10, 0, 2, 2];
const PEER: [u8; 4] = [10, 0, 2, 20];
const FAR: [u8; 4] = [93, 184, 216, 34];

fn cfg() -> Config {
    Config {
        addr: MY_IP,
        mask: [255, 255, 255, 0],
        gateway: GW,
        dns: [10, 0, 2, 3],
    }
}

#[derive(Default)]
struct Dev {
    sent: Vec<Vec<u8>>,
    full: bool,
}
impl Device for Dev {
    fn tx(&mut self, f: &[u8]) -> bool {
        if self.full {
            return false;
        }
        self.sent.push(f.to_vec());
        true
    }
}

fn up() -> Core {
    let mut c = Core::new(ME);
    c.link(true);
    c.configure(cfg(), 0);
    let mut d = Dev::default();
    c.egress(&mut d, 0, 100); // the gratuitous ARP
    c
}

fn drain(c: &mut Core, now: Instant) -> Vec<Vec<u8>> {
    let mut d = Dev::default();
    let (_, ev) = c.egress(&mut d, now, 1000);
    assert!(ev.iter().all(|e| matches!(e, Event::Notice { .. })));
    d.sent
}

fn ip_frame(dst_mac: [u8; 6], src_mac: [u8; 6], h: ipv4::Header, payload: &[u8]) -> Vec<u8> {
    let mut f = vec![0u8; 14 + 20 + payload.len()];
    eth::write_header(&mut f, dst_mac, src_mac, eth::ETHERTYPE_IPV4).unwrap();
    ipv4::write_header(&mut f[14..], &h, payload.len()).unwrap();
    f[34..].copy_from_slice(payload);
    f
}

fn hdr(src: [u8; 4], dst: [u8; 4], proto: u8) -> ipv4::Header {
    ipv4::Header {
        tos: 0,
        id: 9,
        flags_frag: 0,
        ttl: 64,
        proto,
        src,
        dst,
    }
}

fn udp_frame(src: [u8; 4], dst: [u8; 4], sport: u16, dport: u16, payload: &[u8]) -> Vec<u8> {
    let mut u = vec![0u8; 8 + payload.len()];
    let n = udp::write(&mut u, src, dst, sport, dport, payload).unwrap();
    u.truncate(n);
    ip_frame(ME, PEER_MAC, hdr(src, dst, 17), &u)
}

fn echo_frame(src: [u8; 4], dst: [u8; 4], reply: bool, id: u16, seq: u16, data: &[u8]) -> Vec<u8> {
    let mut m = vec![0u8; 8 + data.len()];
    let n = icmp::write_echo(&mut m, reply, id, seq, data).unwrap();
    m.truncate(n);
    ip_frame(ME, PEER_MAC, hdr(src, dst, 1), &m)
}

fn arp_frame(dst: [u8; 6], a: arp::Arp) -> Vec<u8> {
    let mut f = vec![0u8; 60];
    eth::write_header(&mut f, dst, a.sha, eth::ETHERTYPE_ARP).unwrap();
    arp::write(&mut f[14..], &a).unwrap();
    f
}

fn parse_ip(f: &[u8]) -> (eth::Eth<'_>, ipv4::Ipv4<'_>) {
    let e = eth::parse(f).unwrap();
    assert_eq!(e.ethertype, eth::ETHERTYPE_IPV4);
    (e, ipv4::parse(e.payload).unwrap())
}

// ---- ARP ----

#[test]
fn an_arp_request_for_us_is_answered_and_the_asker_learned() {
    let mut c = up();
    let req = arp::Arp {
        op: 1,
        sha: PEER_MAC,
        spa: PEER,
        tha: [0; 6],
        tpa: MY_IP,
    };
    assert!(c.ingress(&arp_frame(BROADCAST_MAC, req), 0).is_empty());
    let out = drain(&mut c, 0);
    assert_eq!(out.len(), 1);
    let e = eth::parse(&out[0]).unwrap();
    assert_eq!(
        (e.dst, e.src, e.ethertype),
        (PEER_MAC, ME, eth::ETHERTYPE_ARP)
    );
    let r = arp::parse(e.payload).unwrap();
    assert_eq!(
        r,
        arp::Arp {
            op: 2,
            sha: ME,
            spa: MY_IP,
            tha: PEER_MAC,
            tpa: PEER
        }
    );
    assert_eq!(out[0].len(), 60, "padded to the Ethernet minimum");
    assert_eq!(c.arp_len(), 1, "the requester was learned");
    // a request for someone else is ignored (but nothing counts it as an error)
    let other = arp::Arp {
        op: 1,
        sha: PEER_MAC,
        spa: PEER,
        tha: [0; 6],
        tpa: [10, 0, 2, 77],
    };
    c.ingress(&arp_frame(BROADCAST_MAC, other), 0);
    assert!(drain(&mut c, 0).is_empty());
}

#[test]
fn a_gratuitous_arp_announces_a_new_address_once() {
    let mut c = Core::new(ME);
    c.link(true);
    c.configure(cfg(), 0);
    let out = drain(&mut c, 0);
    assert_eq!(out.len(), 1);
    let a = arp::parse(eth::parse(&out[0]).unwrap().payload).unwrap();
    assert_eq!((a.op, a.spa, a.tpa, a.sha), (1, MY_IP, MY_IP, ME));
    c.configure(cfg(), 1); // the same configuration again: nothing new to announce
    assert!(drain(&mut c, 1).is_empty());
}

#[test]
fn an_arp_reply_is_learned_and_a_waiting_send_goes_out() {
    let mut c = up();
    let (ep, port) = c.open_udp(0, None, false).unwrap();
    assert!((49152..=65535).contains(&port));
    c.send_udp(ep, PEER, 7777, b"hello", 5, 0).unwrap();
    let out = drain(&mut c, 0);
    assert_eq!(out.len(), 1, "only the ARP request: the datagram waits");
    let req = arp::parse(eth::parse(&out[0]).unwrap().payload).unwrap();
    assert_eq!((req.op, req.tpa, req.spa), (1, PEER, MY_IP));
    assert_eq!(c.pending_len(), 1);
    let reply = arp::Arp {
        op: 2,
        sha: PEER_MAC,
        spa: PEER,
        tha: ME,
        tpa: MY_IP,
    };
    c.ingress(&arp_frame(ME, reply), 10);
    let out = drain(&mut c, 10);
    assert_eq!(out.len(), 1);
    let (e, ip) = parse_ip(&out[0]);
    assert_eq!(
        (e.dst, ip.header.dst, ip.header.src, ip.header.ttl),
        (PEER_MAC, PEER, MY_IP, 64)
    );
    let u = udp::parse(ip.payload, MY_IP, PEER).unwrap();
    assert_eq!(
        (u.src_port, u.dst_port, u.payload),
        (port, 7777, &b"hello"[..])
    );
    assert_eq!(c.pending_len(), 0);
    // the cache is used from now on: no new ARP request
    c.send_udp(ep, PEER, 7777, b"again", 6, 20).unwrap();
    assert_eq!(drain(&mut c, 20).len(), 1);
    assert_eq!(c.counters().arp_requests_sent, 1);
}

#[test]
fn three_unanswered_arp_requests_end_in_noroute() {
    let mut c = up();
    let (ep, _) = c.open_udp(0, None, false).unwrap();
    c.send_udp(ep, PEER, 9, b"x", 77, 0).unwrap();
    assert_eq!(drain(&mut c, 0).len(), 1, "request 1 at once");
    assert_eq!(c.next_deadline(), Some(SEC));
    assert_eq!(
        drain(&mut c, SEC / 2).len(),
        0,
        "nothing before the retry time"
    );
    assert_eq!(drain(&mut c, SEC).len(), 1, "request 2 at 1 s");
    assert_eq!(drain(&mut c, 2 * SEC).len(), 1, "request 3 at 2 s");
    let mut d = Dev::default();
    let (_, ev) = c.egress(&mut d, 3 * SEC, 10);
    assert!(d.sent.is_empty(), "no fourth request");
    assert_eq!(
        ev,
        vec![Event::Notice {
            ep,
            code: notice::SEND_FAILED,
            cookie: 77,
            detail: status::NO_ROUTE as u32
        }],
        "the cookie-tagged send reports NoRoute"
    );
    assert_eq!(c.counters().arp_requests_sent, 3, "exactly three requests");
    assert_eq!(c.pending_len(), 0);
    assert_eq!(c.next_deadline(), None);
}

#[test]
fn the_pending_queues_are_bounded() {
    let mut c = up();
    let (ep, _) = c.open_udp(0, None, false).unwrap();
    for i in 0..6 {
        c.send_udp(ep, PEER, 9, &[i], i as u32, 0).unwrap();
    }
    assert_eq!(c.pending_len(), 4, "four per entry");
    assert_eq!(
        c.counters().arp_queue_dropped,
        2,
        "the oldest were dropped and counted"
    );
    // sixteen in all across entries
    for h in 30..60u8 {
        c.send_udp(ep, [10, 0, 2, h], 9, b"y", 0, 0).unwrap();
    }
    assert!(c.pending_len() <= 16, "{}", c.pending_len());
    // the oldest dropped is gone: when the first entry is answered only the newest four of its queue appear
    let reply = arp::Arp {
        op: 2,
        sha: PEER_MAC,
        spa: PEER,
        tha: ME,
        tpa: MY_IP,
    };
    c.ingress(&arp_frame(ME, reply), 1);
    let _ = drain(&mut c, 1);
    assert!(c.pending_len() <= 16);
}

#[test]
fn a_full_cache_evicts_the_entry_closest_to_expiry() {
    let mut c = up();
    for i in 0..ARP_CACHE_MAX as u8 {
        let a = arp::Arp {
            op: 2,
            sha: [0x52, 0x55, 10, 0, 2, 100 + i],
            spa: [10, 0, 2, 100 + i],
            tha: ME,
            tpa: MY_IP,
        };
        c.ingress(&arp_frame(ME, a), u64::from(i) * MS); // learned a millisecond apart: .100 expires first
    }
    assert_eq!(c.arp_len(), ARP_CACHE_MAX);
    let a = arp::Arp {
        op: 2,
        sha: [0x52, 0x55, 10, 0, 2, 99],
        spa: [10, 0, 2, 99],
        tha: ME,
        tpa: MY_IP,
    };
    c.ingress(&arp_frame(ME, a), 100 * MS);
    assert_eq!(c.arp_len(), ARP_CACHE_MAX, "never above the bound");
    assert_eq!(c.counters().arp_evictions, 1);
    let table = c.arp_table(100 * MS);
    assert!(
        !table.iter().any(|e| e.0 == [10, 0, 2, 100]),
        "the entry closest to expiry was the one evicted"
    );
    assert!(
        table.iter().any(|e| e.0 == [10, 0, 2, 101]) && table.iter().any(|e| e.0 == [10, 0, 2, 99])
    );
}

#[test]
fn an_entry_expires_after_sixty_seconds_without_confirmation() {
    let mut c = up();
    let a = arp::Arp {
        op: 2,
        sha: PEER_MAC,
        spa: PEER,
        tha: ME,
        tpa: MY_IP,
    };
    c.ingress(&arp_frame(ME, a), 0);
    let (ep, _) = c.open_udp(0, None, false).unwrap();
    c.send_udp(ep, PEER, 9, b"x", 0, 59 * SEC).unwrap();
    assert_eq!(c.counters().arp_requests_sent, 0, "still cached at 59 s");
    c.send_udp(ep, PEER, 9, b"x", 0, 61 * SEC).unwrap();
    assert_eq!(
        c.counters().arp_requests_sent,
        1,
        "gone at 61 s: resolved again"
    );
}

#[test]
fn only_our_subnet_and_unicast_senders_are_learned() {
    let mut c = up();
    let far = arp::Arp {
        op: 2,
        sha: PEER_MAC,
        spa: FAR,
        tha: ME,
        tpa: MY_IP,
    };
    c.ingress(&arp_frame(ME, far), 0);
    let multi = arp::Arp {
        op: 2,
        sha: [0x01, 0, 0x5e, 0, 0, 1],
        spa: PEER,
        tha: ME,
        tpa: MY_IP,
    };
    c.ingress(&arp_frame(ME, multi), 0);
    let bcast = arp::Arp {
        op: 2,
        sha: PEER_MAC,
        spa: [10, 0, 2, 255],
        tha: ME,
        tpa: MY_IP,
    };
    c.ingress(&arp_frame(ME, bcast), 0);
    assert_eq!(c.arp_len(), 0);
}

// ---- ICMP echo ----

#[test]
fn an_echo_request_is_answered_with_the_same_data() {
    let mut c = up();
    let data: Vec<u8> = (0..100u8).collect();
    c.ingress(&echo_frame(PEER, MY_IP, false, 0x1234, 7, &data), 0);
    let out = drain(&mut c, 0);
    assert_eq!(out.len(), 1);
    let (e, ip) = parse_ip(&out[0]);
    assert_eq!(
        (
            e.dst,
            ip.header.src,
            ip.header.dst,
            ip.header.ttl,
            ip.header.proto
        ),
        (PEER_MAC, MY_IP, PEER, 64, 1)
    );
    let m = icmp::parse(ip.payload).unwrap();
    assert_eq!((m.kind, m.id, m.seq, m.body), (0, 0x1234, 7, &data[..]));
}

#[test]
fn echo_replies_are_rate_limited_to_one_hundred_a_second() {
    let mut c = up();
    let mut sent = 0;
    for i in 0..150u16 {
        c.ingress(&echo_frame(PEER, MY_IP, false, 1, i, b"x"), 5);
        sent += drain(&mut c, 5).len();
    }
    assert_eq!(sent, 100);
    assert_eq!(c.counters().icmp_echo_rate_limited, 50);
    c.ingress(&echo_frame(PEER, MY_IP, false, 1, 999, b"x"), 5 + SEC);
    assert_eq!(drain(&mut c, 5 + SEC).len(), 1, "the bucket refilled");
}

#[test]
fn an_echo_to_a_broadcast_address_is_not_answered() {
    let mut c = up();
    c.ingress(&echo_frame(PEER, BROADCAST_IP, false, 1, 1, b"x"), 0);
    c.ingress(&echo_frame(PEER, [10, 0, 2, 255], false, 1, 2, b"x"), 0);
    assert!(drain(&mut c, 0).is_empty());
}

#[test]
fn an_echo_reply_goes_to_the_endpoint_with_that_identifier() {
    let mut c = up();
    let (a, ida) = c.open_echo(false).unwrap();
    let (b, idb) = c.open_echo(false).unwrap();
    assert_ne!(ida, idb, "identifiers are unique among live endpoints");
    let ev = c.ingress(&echo_frame(GW, MY_IP, true, idb, 3, b"pong"), 0);
    assert_eq!(
        ev,
        vec![Event::EchoReply {
            ep: b,
            src: GW,
            seq: 3,
            ttl: 64,
            payload: b"pong".to_vec()
        }]
    );
    assert!(c
        .ingress(&echo_frame(GW, MY_IP, true, 0xbeef, 1, b""), 0)
        .is_empty());
    assert_eq!(c.counters().icmp_unmatched, 1);
    c.close(a);
    c.close(b);
    let (_, id3) = c.open_echo(false).unwrap();
    assert!(
        id3 != ida && id3 != idb,
        "a freed identifier is not reused at once"
    );
}

#[test]
fn a_ping_goes_to_the_gateway_through_arp() {
    let mut c = up();
    let (ep, id) = c.open_echo(false).unwrap();
    c.send_echo(ep, GW, 1, b"hi", 0, 0).unwrap();
    let out = drain(&mut c, 0);
    let a = arp::parse(eth::parse(&out[0]).unwrap().payload).unwrap();
    assert_eq!(a.tpa, GW);
    let reply = arp::Arp {
        op: 2,
        sha: GW_MAC,
        spa: GW,
        tha: ME,
        tpa: MY_IP,
    };
    c.ingress(&arp_frame(ME, reply), 1);
    let out = drain(&mut c, 1);
    let (e, ip) = parse_ip(&out[0]);
    assert_eq!((e.dst, ip.header.dst), (GW_MAC, GW));
    let m = icmp::parse(ip.payload).unwrap();
    assert_eq!((m.kind, m.id, m.seq, m.body), (8, id, 1, &b"hi"[..]));
}

// ---- UDP ----

#[test]
fn udp_is_delivered_to_the_bound_endpoint() {
    let mut c = up();
    let (ep, port) = c.open_udp(7777, None, false).unwrap();
    assert_eq!(port, 7777);
    let ev = c.ingress(&udp_frame(PEER, MY_IP, 4000, 7777, b"abc"), 0);
    assert_eq!(
        ev,
        vec![Event::Deliver {
            ep,
            src: PEER,
            src_port: 4000,
            payload: b"abc".to_vec()
        }]
    );
    assert_eq!(c.open_udp(7777, None, false), Err(BindError::AddrInUse));
    c.close(ep);
    assert!(
        c.open_udp(7777, None, false).is_ok(),
        "the port is free at once"
    );
}

#[test]
fn a_datagram_to_a_closed_port_gets_a_rate_limited_port_unreachable() {
    let mut c = up();
    for i in 0..15u16 {
        c.ingress(&udp_frame(PEER, MY_IP, 4000 + i, 9, b"x"), 3);
    }
    let out = drain(&mut c, 3);
    assert_eq!(out.len(), 10, "at most ten a second");
    let (e, ip) = parse_ip(&out[0]);
    assert_eq!((e.dst, ip.header.proto), (PEER_MAC, 1));
    let m = icmp::parse(ip.payload).unwrap();
    assert_eq!((m.kind, m.code), (3, 3));
    assert_eq!(
        (m.body[0], m.body.len()),
        (0x45, 28),
        "the offending IP header and 8 bytes are quoted"
    );
    assert_eq!(c.counters().icmp_unreachable_rate_limited, 5);
    // never for a broadcast destination
    let mut c = up();
    c.ingress(&udp_frame(PEER, BROADCAST_IP, 4000, 9, b"x"), 0);
    c.ingress(&udp_frame(PEER, [10, 0, 2, 255], 4000, 9, b"x"), 0);
    assert!(drain(&mut c, 0).is_empty());
}

#[test]
fn a_padded_frame_delivers_only_what_the_ip_length_says() {
    let mut c = up();
    let (ep, _) = c.open_udp(7777, None, false).unwrap();
    let mut f = udp_frame(PEER, MY_IP, 4000, 7777, b"Z");
    assert_eq!(f.len(), 14 + 20 + 8 + 1);
    f.resize(60, 0); // Ethernet padding
    let ev = c.ingress(&f, 0);
    assert_eq!(
        ev,
        vec![Event::Deliver {
            ep,
            src: PEER,
            src_port: 4000,
            payload: b"Z".to_vec()
        }]
    );
}

#[test]
fn udp_sends_are_checked() {
    let mut c = up();
    let (ep, _) = c.open_udp(0, None, false).unwrap();
    assert_eq!(
        c.send_udp(ep, PEER, 9, &vec![0; UDP_MAX + 1], 0, 0),
        Err(SendError::TooBig),
        "1473 bytes"
    );
    assert!(
        c.send_udp(ep, PEER, 9, &vec![0; UDP_MAX], 0, 0).is_ok(),
        "1472 bytes"
    );
    assert_eq!(
        c.send_udp(ep, BROADCAST_IP, 9, b"x", 0, 0),
        Err(SendError::Refused),
        "broadcast needs privilege"
    );
    assert_eq!(
        c.send_udp(ep, [10, 0, 2, 255], 9, b"x", 0, 0),
        Err(SendError::Refused),
        "and so does the subnet broadcast"
    );
    assert_eq!(
        c.send_udp(999, PEER, 9, b"x", 0, 0),
        Err(SendError::NoSuchEndpoint)
    );
    let (pep, _) = c.open_udp(68, None, true).unwrap();
    c.send_udp(pep, BROADCAST_IP, 67, b"x", 0, 0).unwrap();
    let out = drain(&mut c, 0);
    let last = out.last().unwrap();
    assert_eq!(
        eth::parse(last).unwrap().dst,
        BROADCAST_MAC,
        "a privileged broadcast goes to the broadcast MAC"
    );
}

#[test]
fn routing_uses_the_gateway_off_link_and_fails_without_one() {
    let mut c = up();
    let (ep, _) = c.open_udp(0, None, false).unwrap();
    c.send_udp(ep, FAR, 53, b"q", 0, 0).unwrap();
    let a = arp::parse(eth::parse(&drain(&mut c, 0)[0]).unwrap().payload).unwrap();
    assert_eq!(a.tpa, GW, "an off-link destination resolves the gateway");
    let mut c = Core::new(ME);
    c.link(true);
    c.configure(
        Config {
            gateway: [0; 4],
            ..cfg()
        },
        0,
    );
    let (ep, _) = c.open_udp(0, None, false).unwrap();
    assert_eq!(
        c.send_udp(ep, FAR, 53, b"q", 0, 0),
        Err(SendError::NoRoute),
        "no gateway and off-link"
    );
}

#[test]
fn with_no_address_only_dhcp_is_accepted_and_sends_need_one() {
    let mut c = Core::new(ME);
    c.link(true);
    let (ep, _) = c.open_udp(0, None, false).unwrap();
    assert_eq!(
        c.send_udp(ep, PEER, 9, b"x", 0, 0),
        Err(SendError::NotConfigured)
    );
    let (dhcp_ep, _) = c.open_udp(68, None, true).unwrap();
    // a DHCP reply to the client port, broadcast: delivered
    let mut u = vec![0u8; 8 + 4];
    let n = udp::write(&mut u, GW, BROADCAST_IP, 67, 68, b"offr").unwrap();
    u.truncate(n);
    let f = ip_frame(BROADCAST_MAC, GW_MAC, hdr(GW, BROADCAST_IP, 17), &u);
    let ev = c.ingress(&f, 0);
    assert_eq!(ev.len(), 1);
    assert!(matches!(&ev[0], Event::Deliver { ep, .. } if *ep == dhcp_ep));
    // anything else is not for us
    c.ingress(&udp_frame(PEER, MY_IP, 1, 7777, b"x"), 0);
    assert_eq!(c.counters().rx_ip_not_for_us, 1);
    // a privileged endpoint may broadcast from 0.0.0.0
    c.send_udp(dhcp_ep, BROADCAST_IP, 67, b"disc", 0, 0)
        .unwrap();
    let out = drain(&mut c, 0);
    let (_, ip) = parse_ip(out.last().unwrap());
    assert_eq!(ip.header.src, [0; 4]);
}

// ---- S8 follow-up item 4: transports are dispatched, and an ICMP error reaches the one it quotes ----

/// A destination-unreachable from `from` to us, quoting the first 28 bytes of a packet `quoted_src` -> `quoted_dst`.
fn unreachable_frame(from: [u8; 4], code: u8, quoted: &[u8]) -> Vec<u8> {
    let mut m = vec![0u8; 8 + 28];
    let n = icmp::write_unreachable(&mut m, code, quoted).unwrap();
    m.truncate(n);
    ip_frame(ME, GW_MAC, hdr(from, MY_IP, 1), &m)
}

/// The IP packet (header and 8 bytes of payload) that we sent: protocol `proto`, ports `sport` and `dport`.
fn our_packet(proto: u8, dst: [u8; 4], sport: u16, dport: u16) -> Vec<u8> {
    let mut p = vec![0u8; 20 + 8];
    ipv4::write_header(&mut p, &hdr(MY_IP, dst, proto), 8).unwrap();
    p[20..22].copy_from_slice(&sport.to_be_bytes());
    p[22..24].copy_from_slice(&dport.to_be_bytes());
    p
}

#[test]
fn a_tcp_segment_reaches_the_tcp_dispatch_which_in_s8_counts_it() {
    let mut c = up();
    c.ingress(&ip_frame(ME, PEER_MAC, hdr(PEER, MY_IP, 6), &[0; 20]), 0);
    assert_eq!(c.counters().rx_unknown_protocol, 1);
    assert!(
        drain(&mut c, 0).is_empty(),
        "and nothing is sent back: no RST in S8"
    );
}

#[test]
fn an_icmp_error_goes_to_the_transport_it_quotes() {
    let mut c = up();
    c.ingress(&unreachable_frame(GW, 3, &our_packet(17, FAR, 5000, 53)), 0);
    c.ingress(&unreachable_frame(GW, 1, &our_packet(17, FAR, 5001, 53)), 0);
    c.ingress(&unreachable_frame(GW, 3, &our_packet(6, FAR, 40000, 80)), 0);
    c.ingress(&unreachable_frame(GW, 3, &our_packet(1, FAR, 0, 0)), 0);
    let k = c.counters();
    assert_eq!(k.icmp_unreachable_received, 4);
    assert_eq!(
        (
            k.icmp_error_udp,
            k.icmp_error_tcp,
            k.icmp_error_other_protocol
        ),
        (2, 1, 1)
    );
    assert_eq!(k.icmp_error_malformed, 0);
}

#[test]
fn an_icmp_error_quoting_something_we_did_not_send_is_not_passed_on() {
    let mut c = up();
    // quoting a packet from somebody else's address
    let mut theirs = our_packet(17, FAR, 5000, 53);
    theirs[12..16].copy_from_slice(&PEER);
    c.ingress(&unreachable_frame(GW, 3, &theirs), 0);
    // too short to hold the ports, and not IPv4 at all
    c.ingress(
        &unreachable_frame(GW, 3, &our_packet(17, FAR, 5000, 53)[..22]),
        0,
    );
    let mut v6 = our_packet(17, FAR, 5000, 53);
    v6[0] = 0x65;
    c.ingress(&unreachable_frame(GW, 3, &v6), 0);
    let k = c.counters();
    assert_eq!(k.icmp_error_malformed, 3);
    assert_eq!((k.icmp_error_udp, k.icmp_error_tcp), (0, 0));
}

// ---- drops, each with exactly its counter ----

fn only(c: &Core, f: impl Fn(&Counters) -> u64, what: &str) {
    let k = c.counters();
    assert_eq!(f(k), 1, "{what}");
    let total = k.rx_runt
        + k.rx_not_for_us
        + k.rx_unknown_ethertype
        + k.rx_ip_malformed
        + k.rx_ip_checksum
        + k.rx_fragment
        + k.rx_ttl
        + k.rx_ip_not_for_us
        + k.rx_unknown_protocol
        + k.rx_icmp_malformed
        + k.rx_udp_malformed;
    assert_eq!(total, 1, "{what}: exactly one drop counter moved");
}

#[test]
fn every_kind_of_bad_frame_is_dropped_with_exactly_its_counter() {
    let good = udp_frame(PEER, MY_IP, 4000, 9, b"x");
    let mk = || {
        let mut c = up();
        c.open_udp(9, None, false).unwrap();
        c
    };
    let mut c = mk();
    c.ingress(&good[..10], 0);
    only(&c, |k| k.rx_runt, "a runt");
    let mut c = mk();
    let mut wrong_mac = good.clone();
    wrong_mac[0] ^= 0x10;
    c.ingress(&wrong_mac, 0);
    only(&c, |k| k.rx_not_for_us, "another station's frame");
    let mut c = mk();
    let mut ty = good.clone();
    ty[12] = 0x86;
    ty[13] = 0xdd;
    c.ingress(&ty, 0);
    only(&c, |k| k.rx_unknown_ethertype, "an IPv6 frame");
    let mut c = mk();
    let mut bad = good.clone();
    bad[24] ^= 1;
    c.ingress(&bad, 0);
    only(&c, |k| k.rx_ip_checksum, "a bad header checksum");
    let mut c = mk();
    let frag = ip_frame(
        ME,
        PEER_MAC,
        ipv4::Header {
            flags_frag: 0x2000,
            ..hdr(PEER, MY_IP, 17)
        },
        &[0; 16],
    );
    c.ingress(&frag, 0);
    only(&c, |k| k.rx_fragment, "a first fragment");
    let mut c = mk();
    let frag2 = ip_frame(
        ME,
        PEER_MAC,
        ipv4::Header {
            flags_frag: 0x00b9,
            ..hdr(PEER, MY_IP, 17)
        },
        &[0; 16],
    );
    c.ingress(&frag2, 0);
    only(&c, |k| k.rx_fragment, "a later fragment");
    let mut c = mk();
    let ttl0 = ip_frame(
        ME,
        PEER_MAC,
        ipv4::Header {
            ttl: 0,
            ..hdr(PEER, MY_IP, 17)
        },
        &[0; 16],
    );
    c.ingress(&ttl0, 0);
    only(&c, |k| k.rx_ttl, "ttl 0");
    let mut c = mk();
    c.ingress(&udp_frame(PEER, [10, 0, 2, 99], 1, 9, b"x"), 0);
    only(&c, |k| k.rx_ip_not_for_us, "another host's address");
    let mut c = mk();
    c.ingress(&ip_frame(ME, PEER_MAC, hdr(PEER, MY_IP, 99), &[0; 8]), 0);
    only(&c, |k| k.rx_unknown_protocol, "an unknown protocol");
    let mut c = mk();
    let mut mal = good.clone();
    mal[14] = 0x65;
    c.ingress(&mal, 0);
    only(&c, |k| k.rx_ip_malformed, "IP version 6 in an IPv4 frame");
}

#[test]
fn the_fragments_of_a_big_datagram_are_dropped_and_counted() {
    // slirp fragments a 3000-byte datagram: a first fragment of 1514 bytes (more-fragments set) and a later one
    let mut c = up();
    let first = ip_frame(
        ME,
        PEER_MAC,
        ipv4::Header {
            flags_frag: 0x2000,
            ..hdr(PEER, MY_IP, 17)
        },
        &vec![0u8; 1480],
    );
    let rest = ip_frame(
        ME,
        PEER_MAC,
        ipv4::Header {
            flags_frag: 0x00b9,
            ..hdr(PEER, MY_IP, 17)
        },
        &vec![0u8; 1480],
    );
    assert_eq!(first.len(), 1514);
    c.ingress(&first, 0);
    c.ingress(&rest, 0);
    assert_eq!(c.counters().rx_fragment, 2);
    assert!(drain(&mut c, 0).is_empty(), "nothing is answered");
}

#[test]
fn link_down_drops_everything_and_tells_the_endpoints() {
    let mut c = up();
    let (ep, _) = c.open_udp(7777, None, false).unwrap();
    c.send_udp(ep, PEER, 9, b"x", 0, 0).unwrap();
    let ev = c.link(false);
    assert_eq!(
        ev,
        vec![Event::Notice {
            ep,
            code: notice::LINK_DOWN,
            cookie: 0,
            detail: 0
        }]
    );
    assert!(drain(&mut c, 0).is_empty(), "queued frames are gone");
    assert!(c
        .ingress(&udp_frame(PEER, MY_IP, 1, 7777, b"x"), 0)
        .is_empty());
    assert_eq!(
        c.counters().rx_runt,
        1,
        "counted as received while the link is down"
    );
    let ev = c.link(true);
    assert_eq!(
        ev,
        vec![Event::Notice {
            ep,
            code: notice::LINK_UP,
            cookie: 0,
            detail: 0
        }]
    );
    assert!(c.link(true).is_empty(), "no change, no notice");
}

#[test]
fn a_full_device_keeps_the_frames_for_later() {
    let mut c = up();
    c.ingress(&echo_frame(PEER, MY_IP, false, 1, 1, b"x"), 0);
    let mut d = Dev {
        full: true,
        ..Dev::default()
    };
    let (n, _) = c.egress(&mut d, 0, 10);
    assert_eq!(n, 0);
    assert_eq!(c.tx_queue_len(), 1);
    d.full = false;
    assert_eq!(c.egress(&mut d, 0, 10).0, 1);
    assert_eq!(c.egress(&mut d, 0, 10).0, 0);
}

#[test]
fn ephemeral_ports_rotate_and_do_not_reuse_a_freed_port_at_once() {
    let mut c = up();
    let (a, pa) = c.open_udp(0, None, false).unwrap();
    c.close(a);
    let (_, pb) = c.open_udp(0, None, false).unwrap();
    assert_ne!(pa, pb);
}

#[test]
fn the_counters_have_stable_ids_and_names() {
    let c = up();
    let pairs = c.counters().pairs();
    assert!(pairs.iter().any(|p| p.1 == "rx_fragment"));
    for (i, p) in pairs.iter().enumerate() {
        assert_eq!(usize::from(p.0), i);
    }
}

#[test]
fn echo_identifiers_are_never_shared_by_live_endpoints() {
    let mut c = up();
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..65_535 {
        let (_, id) = c.open_echo(false).unwrap();
        assert!(seen.insert(id), "identifier {id} handed out twice");
    }
    assert_eq!(
        c.open_echo(false),
        Err(BindError::NoPorts),
        "all 65535 are taken"
    );
}
