//! S9 step 4a: TCP inside the core, two cores over a netsim link: connect, accept, data, close, refusals, orphans,
//! loopback, restarts and a hostile link with many connections at once.
use netcore::*;
use netsim::{Faults, Link};
use std::collections::BTreeMap;

const MS: Instant = 1_000_000;
const MAC_A: [u8; 6] = [0x52, 0x54, 0, 0, 0, 0x0a];
const MAC_B: [u8; 6] = [0x52, 0x54, 0, 0, 0, 0x0b];
const IP_A: [u8; 4] = [10, 0, 2, 15];
const IP_B: [u8; 4] = [10, 0, 2, 20];

#[derive(Default)]
struct Dev {
    out: Vec<Vec<u8>>,
}
impl Device for Dev {
    fn tx(&mut self, f: &[u8]) -> bool {
        self.out.push(f.to_vec());
        true
    }
}

struct Host {
    core: Core,
    dev: Dev,
    events: Vec<TcpEvent>,
}

impl Host {
    fn new(mac: [u8; 6], ip: [u8; 4]) -> Host {
        let mut core = Core::new(mac);
        core.link(true);
        core.configure(
            Config {
                addr: ip,
                mask: [255, 255, 255, 0],
                gateway: [10, 0, 2, 2],
                dns: [0; 4],
            },
            0,
        );
        core.set_tcp_secret([7; 16]);
        Host {
            core,
            dev: Dev::default(),
            events: vec![],
        }
    }
}

struct Net {
    a: Host,
    b: Host,
    link: Link,
    now: Instant,
}

impl Net {
    fn new(faults: Faults, seed: u64) -> Net {
        Net {
            a: Host::new(MAC_A, IP_A),
            b: Host::new(MAC_B, IP_B),
            link: Link::new(seed, faults),
            now: 0,
        }
    }
    fn clean() -> Net {
        Net::new(
            Faults {
                min_delay: MS / 2,
                max_delay: MS / 2,
                ..Faults::NONE
            },
            1,
        )
    }
    /// Run both cores' egress, deliver what the link has due, collect events. One round at the current time.
    fn step(&mut self) {
        // egress and delivery repeat at one instant until nothing more happens (an adapter polls after every ingress)
        for _ in 0..50 {
            let before = (
                self.a.core.counters().tx_frames + self.b.core.counters().tx_frames,
                self.link.in_flight(),
            );
            self.step_once();
            let after = (
                self.a.core.counters().tx_frames + self.b.core.counters().tx_frames,
                self.link.in_flight(),
            );
            if before == after && self.a.dev.out.is_empty() && self.b.dev.out.is_empty() {
                break;
            }
        }
    }
    fn step_once(&mut self) {
        for (i, h) in [&mut self.a, &mut self.b].into_iter().enumerate() {
            h.core.egress(&mut h.dev, self.now, 64);
            for f in h.dev.out.drain(..) {
                self.link.send(i, self.now, &f);
            }
            h.events.extend(h.core.take_tcp_events());
        }
        for (to, f) in self.link.due(self.now) {
            let h = if to == 0 { &mut self.a } else { &mut self.b };
            h.core.ingress(&f, self.now);
            h.events.extend(h.core.take_tcp_events());
        }
    }
    /// Advance time to the next thing that can happen and run it; `false` when nothing is scheduled.
    fn advance(&mut self) -> bool {
        self.step();
        let next = [
            self.link.next_arrival(),
            self.a.core.next_deadline(),
            self.b.core.next_deadline(),
        ]
        .into_iter()
        .flatten()
        .filter(|&t| t > self.now)
        .min();
        match next {
            Some(t) => {
                self.now = t;
                true
            }
            None => false,
        }
    }
    /// Run until `until` ns of virtual time have passed.
    fn run_for(&mut self, dur: Instant) {
        let end = self.now + dur;
        while self.now < end {
            if !self.advance() {
                self.now = end;
            }
            self.now = self.now.min(end);
            if self.now == end {
                self.step();
                break;
            }
        }
    }
    fn run_until(&mut self, what: &str, mut cond: impl FnMut(&mut Net) -> bool) {
        for _ in 0..200_000 {
            if cond(self) {
                return;
            }
            if !self.advance() {
                self.now += MS;
            }
        }
        panic!("never: {what} (now {} ms)", self.now / MS);
    }
}

fn cfg() -> TcpConfig {
    TcpConfig::default()
}

fn has_event(h: &Host, f: impl Fn(&TcpEvent) -> bool) -> bool {
    h.events.iter().any(f)
}

/// Make a connection a -> b:80 and return (a's id, b's id).
fn connected(n: &mut Net) -> (ConnId, ConnId) {
    let l = n.b.core.tcp_listen(None, 80).unwrap();
    n.b.core.tcp_post_accept(l, 1, cfg()).unwrap();
    let a = n.a.core.tcp_connect(IP_B, 80, None, cfg(), n.now).unwrap();
    n.run_until("established on both sides", |n| {
        has_event(&n.a, |e| matches!(e, TcpEvent::Connected { .. }))
            && has_event(&n.b, |e| matches!(e, TcpEvent::Accepted { .. }))
    });
    let b =
        n.b.events
            .iter()
            .find_map(|e| {
                if let TcpEvent::Accepted { conn, .. } = e {
                    Some(*conn)
                } else {
                    None
                }
            })
            .unwrap();
    (a, b)
}

#[test]
fn a_connection_carries_data_both_ways_and_closes_cleanly_leaving_time_wait_records() {
    let mut n = Net::clean();
    let (a, b) = connected(&mut n);
    assert_eq!(n.a.core.tcp_write(a, b"hello server"), 12);
    n.run_until("server read", |n| n.b.core.tcp_readable(b) == 12);
    let mut buf = [0u8; 64];
    let k = n.b.core.tcp_read(b, &mut buf);
    assert_eq!(&buf[..k], b"hello server");
    n.b.core.tcp_write(b, b"hello client");
    n.run_until("client read", |n| n.a.core.tcp_readable(a) == 12);
    let k = n.a.core.tcp_read(a, &mut buf);
    assert_eq!(&buf[..k], b"hello client");
    // the client shuts down its write side; the server sees the FIN, then closes too
    n.a.core.tcp_shutdown_write(a);
    n.run_until("server sees the FIN", |n| n.b.core.tcp_eof(b));
    assert!(has_event(&n.b, |e| matches!(e, TcpEvent::PeerFin { .. })));
    n.b.core.tcp_shutdown_write(b);
    n.run_until("both closed", |n| {
        has_event(&n.a, |e| matches!(e, TcpEvent::Closed { .. }))
            && has_event(&n.b, |e| matches!(e, TcpEvent::Closed { .. }))
    });
    assert_eq!(
        (n.a.core.tcp_conn_count(), n.b.core.tcp_conn_count()),
        (0, 0),
        "the connections are gone from the tables"
    );
    assert_eq!(
        n.a.core.tcp_time_wait_count(),
        1,
        "the side that closed first keeps a small TIME-WAIT record"
    );
    // the record ends after 10 s
    n.run_for(11_000 * MS);
    assert_eq!(n.a.core.tcp_time_wait_count(), 0);
}

#[test]
fn a_syn_to_a_closed_port_is_reset_and_connect_fails_with_refused() {
    let mut n = Net::clean();
    let a = n.a.core.tcp_connect(IP_B, 81, None, cfg(), 0).unwrap();
    n.run_until("refused", |n| {
        has_event(&n.a, |e| {
            matches!(
                e,
                TcpEvent::ConnectFailed {
                    reason: FailReason::Refused,
                    ..
                }
            )
        })
    });
    assert_eq!(n.b.core.counters().tcp_rst_sent, 1);
    assert!(
        n.a.core.tcp_info(a).is_none(),
        "the failed connection left the table"
    );
    assert_eq!(n.a.core.tcp_conn_count(), 0);
}

#[test]
fn resets_are_limited_to_a_hundred_a_second_and_never_answer_a_reset() {
    let mut n = Net::clean();
    // let the ARP exchange finish first (its pending queue holds only four packets)
    n.a.core
        .tcp_connect(IP_B, 9, Some(29999), cfg(), 0)
        .unwrap();
    n.run_for(5 * MS);
    let base = n.b.core.counters().tcp_rst_sent;
    assert_eq!(base, 1);
    for i in 0..150u16 {
        n.a.core
            .tcp_connect(IP_B, 9, Some(30000 + i), cfg(), n.now)
            .unwrap();
    }
    n.run_for(5 * MS);
    let k = n.b.core.counters();
    assert_eq!(
        k.tcp_rst_sent, 100,
        "at most a hundred a second in all (the first one counted)"
    );
    assert_eq!(k.tcp_rst_rate_limited, 51);
    // a's own resets never draw resets back
    assert_eq!(n.a.core.counters().tcp_rst_sent, 0);
}

#[test]
fn without_a_posted_accept_a_syn_is_dropped_silently_and_the_listener_is_told_once_a_second() {
    let mut n = Net::clean();
    let l = n.b.core.tcp_listen(None, 80).unwrap();
    let a = n.a.core.tcp_connect(IP_B, 80, None, cfg(), 0).unwrap();
    n.run_for(3500 * MS);
    assert_eq!(n.b.core.tcp_conn_count(), 0);
    assert!(
        n.b.core.counters().tcp_listen_overflow >= 2,
        "SYNs retried at 1 and 3 s were dropped too"
    );
    assert_eq!(
        n.b.events
            .iter()
            .filter(|e| matches!(e, TcpEvent::AcceptStarved { .. }))
            .count(),
        2,
        "told at 0 s and 1 s, not for every SYN"
    );
    assert_eq!(
        n.b.core.counters().tcp_rst_sent,
        0,
        "no reset: the peer keeps trying"
    );
    // post an accept: the next SYN retransmission gets through
    n.b.core.tcp_post_accept(l, 5, cfg()).unwrap();
    n.run_until("established after the post", |n| {
        has_event(&n.a, |e| matches!(e, TcpEvent::Connected { .. }))
    });
    n.run_for(5 * MS);
    let _ = a;
    assert!(has_event(&n.b, |e| matches!(
        e,
        TcpEvent::Accepted { cookie: 5, .. }
    )));
}

#[test]
fn a_listener_holds_at_most_sixteen_posted_accepts_and_closing_it_drops_the_half_open() {
    let mut n = Net::clean();
    let l = n.b.core.tcp_listen(Some(IP_B), 80).unwrap();
    assert_eq!(n.b.core.tcp_listen(None, 80), Err(ListenError::AddrInUse));
    for i in 0..16 {
        n.b.core.tcp_post_accept(l, i, cfg()).unwrap();
    }
    assert_eq!(
        n.b.core.tcp_post_accept(l, 99, cfg()),
        Err(AcceptError::TooMany)
    );
    assert_eq!(n.b.core.tcp_posted_accepts(l), 16);
    // a half-open connection (the final ACK never arrives): drop the SYN-ACK's ACK by closing the listener first
    n.a.core.tcp_connect(IP_B, 80, None, cfg(), 0).unwrap();
    n.run_until("the SYN reached b", |n| n.b.core.tcp_conn_count() == 1); // its SYN-ACK is in flight
    assert!(!has_event(&n.b, |e| matches!(e, TcpEvent::Accepted { .. })));
    n.b.core.tcp_close_listener(l);
    assert_eq!(
        n.b.core.tcp_conn_count(),
        0,
        "the half-open connection went with the listener"
    );
    assert_eq!(n.b.core.tcp_listener_count(), 0);
}

#[test]
fn loopback_connects_to_our_own_address_and_to_127_0_0_1_without_touching_the_device() {
    let mut n = Net::clean();
    for target in [IP_A, [127, 0, 0, 1]] {
        let l = n.a.core.tcp_listen(None, 8080).unwrap();
        n.a.core.tcp_post_accept(l, 1, cfg()).unwrap();
        let c =
            n.a.core
                .tcp_connect(target, 8080, None, cfg(), n.now)
                .unwrap();
        n.run_for(5 * MS); // (the gratuitous ARP and anything else the interface says first)
        let before = n.a.core.counters().tx_frames;
        n.run_until("loopback connected", |n| {
            has_event(&n.a, |e| matches!(e, TcpEvent::Connected { .. }))
                && has_event(&n.a, |e| matches!(e, TcpEvent::Accepted { .. }))
        });
        n.a.core.tcp_write(c, b"to myself");
        let srv =
            n.a.events
                .iter()
                .find_map(|e| {
                    if let TcpEvent::Accepted { conn, .. } = e {
                        Some(*conn)
                    } else {
                        None
                    }
                })
                .unwrap();
        n.run_until("loopback data", |n| n.a.core.tcp_readable(srv) == 9);
        let mut b = [0u8; 16];
        let k = n.a.core.tcp_read(srv, &mut b);
        assert_eq!(&b[..k], b"to myself");
        assert_eq!(
            n.a.core.counters().tx_frames,
            before,
            "loopback never reaches the device (N23)"
        );
        n.a.core.tcp_abort(c);
        n.a.core.tcp_abort(srv);
        n.run_for(5 * MS);
        n.a.core.tcp_close_listener(l);
        n.a.events.clear();
    }
}

#[test]
fn a_restarted_stack_resets_the_peer_and_isns_never_go_backwards() {
    let mut n = Net::clean();
    let (a, _b) = connected(&mut n);
    n.run_for(5 * MS);
    // b "restarts": a fresh core with no connections, same secret and clock
    n.b = Host::new(MAC_B, IP_B);
    n.a.core.tcp_write(a, b"are you there?");
    n.run_until("the peer learned of the restart", |n| {
        has_event(&n.a, |e| {
            matches!(
                e,
                TcpEvent::Reset {
                    reason: ResetReason::PeerReset,
                    ..
                }
            )
        })
    });
    assert!(n.b.core.counters().tcp_no_connection >= 1 && n.b.core.counters().tcp_rst_sent >= 1);
    // and the ISN for a reconnect is larger than the first one (same secret, later clock)
    let t = nettcp::siphash::isn(&[7; 16], IP_A, 50000, IP_B, 80, n.now);
    let later = nettcp::siphash::isn(&[7; 16], IP_A, 50000, IP_B, 80, n.now + 1_000_000_000);
    assert!((later.wrapping_sub(t) as i32) > 0);
}

#[test]
fn an_orphan_finishes_its_data_and_one_with_nobody_to_read_is_reset_after_twenty_seconds() {
    let mut n = Net::clean();
    let (a, b) = connected(&mut n);
    // a writes and its owner goes away: the data still arrives, then FIN
    assert_eq!(n.a.core.tcp_write(a, &vec![9u8; 5000]), 5000);
    n.a.core.tcp_release(a, n.now);
    assert_eq!(n.a.core.tcp_orphan_count(), 1);
    n.run_until("all of it arrived", |n| n.b.core.tcp_readable(b) == 5000);
    n.run_until("and the FIN", |n| {
        n.b.core.tcp_eof(b)
            || n.b.core.tcp_readable(b) == 5000
                && has_event(&n.b, |e| matches!(e, TcpEvent::PeerFin { .. }))
    });
    // b never closes: the orphan side waits in FIN-WAIT-2 and is reset after 20 s
    n.run_for(21_000 * MS);
    assert_eq!(n.a.core.tcp_orphan_count(), 0);
    assert!(n.a.core.counters().tcp_orphan_reset >= 1);
    assert!(
        !has_event(&n.a, |e| matches!(e, TcpEvent::Reset { .. })),
        "nobody is told about an orphan"
    );
}

#[test]
fn at_most_thirty_two_orphans_the_oldest_is_reset() {
    let mut n = Net::clean();
    let l = n.b.core.tcp_listen(None, 80).unwrap();
    let mut ids = vec![];
    for i in 0..33u32 {
        n.b.core.tcp_post_accept(l, i, cfg()).ok();
        let c = n.a.core.tcp_connect(IP_B, 80, None, cfg(), n.now).unwrap();
        ids.push(c);
        n.run_until("up", |n| {
            n.a.core
                .tcp_info(c)
                .is_some_and(|i| i.state == nettcp::conn::State::Established)
        });
        n.a.core.tcp_write(c, b"x");
        n.run_for(MS);
    }
    for (k, &c) in ids.iter().enumerate() {
        n.a.core.tcp_release(c, n.now + k as u64);
    }
    assert_eq!(
        n.a.core.counters().tcp_orphan_reset,
        1,
        "the 33rd orphan reset the oldest"
    );
}

#[test]
fn an_unreachable_for_a_syn_in_flight_turns_a_later_timeout_into_unreachable() {
    let mut n = Net::clean();
    // nobody at 10.0.2.99: ARP fails, so use an address that routes via the gateway but never answers
    let c =
        n.a.core
            .tcp_connect([93, 184, 216, 34], 80, None, cfg(), 0)
            .unwrap();
    // pretend the gateway told us it is unreachable: build the ICMP the core would see
    let info = n.a.core.tcp_info(c).unwrap();
    let t = info.tuple;
    // (the quoted header must be one of ours: IPv4, our source, the ports)
    let mut quoted = vec![0u8; 28];
    quoted[0] = 0x45;
    quoted[9] = 6;
    quoted[12..16].copy_from_slice(&IP_A);
    quoted[16..20].copy_from_slice(&t.remote);
    quoted[20..22].copy_from_slice(&t.lport.to_be_bytes());
    quoted[22..24].copy_from_slice(&t.rport.to_be_bytes());
    let mut m = vec![0u8; 36];
    let k = netwire::icmp::write_unreachable(&mut m, 1, &quoted).unwrap();
    m.truncate(k);
    let mut f = vec![0u8; 14 + 20 + m.len()];
    netwire::eth::write_header(
        &mut f,
        MAC_A,
        [0x52, 0x55, 10, 0, 2, 2],
        netwire::eth::ETHERTYPE_IPV4,
    )
    .unwrap();
    let h = netwire::ipv4::Header {
        tos: 0,
        id: 1,
        flags_frag: 0,
        ttl: 64,
        proto: 1,
        src: [10, 0, 2, 2],
        dst: IP_A,
    };
    netwire::ipv4::write_header(&mut f[14..], &h, m.len()).unwrap();
    f[34..].copy_from_slice(&m);
    n.a.core.ingress(&f, 10 * MS);
    assert_eq!(n.a.core.counters().icmp_error_tcp, 1);
    // nothing answers the SYN: it times out at 15 s, reported as unreachable
    n.run_for(16_000 * MS);
    let evs = n.a.core.take_tcp_events();
    n.a.events.extend(evs);
    assert!(
        has_event(&n.a, |e| matches!(
            e,
            TcpEvent::ConnectFailed {
                reason: FailReason::Unreachable,
                ..
            }
        )),
        "{:?}",
        n.a.events
    );
}

#[test]
fn a_syn_with_a_higher_sequence_number_reuses_a_port_in_time_wait() {
    let mut n = Net::clean();
    let l = n.b.core.tcp_listen(None, 80).unwrap();
    n.b.core.tcp_post_accept(l, 1, cfg()).unwrap();
    let a =
        n.a.core
            .tcp_connect(IP_B, 80, Some(41000), cfg(), n.now)
            .unwrap();
    n.run_until("up", |n| {
        has_event(&n.b, |e| matches!(e, TcpEvent::Accepted { .. }))
    });
    n.a.core.tcp_shutdown_write(a);
    let b =
        n.b.events
            .iter()
            .find_map(|e| {
                if let TcpEvent::Accepted { conn, .. } = e {
                    Some(*conn)
                } else {
                    None
                }
            })
            .unwrap();
    n.run_until("b sees FIN", |n| n.b.core.tcp_eof(b));
    n.b.core.tcp_shutdown_write(b);
    n.run_for(20 * MS);
    // b closed second: a holds the TIME-WAIT record. Reconnect from the same local port: refused while the old number
    // is not smaller... the new SYN's ISN is larger (later clock), so b-side (also in nobody's TIME-WAIT) accepts.
    n.b.core.tcp_post_accept(l, 2, cfg()).unwrap();
    assert_eq!(
        n.a.core.tcp_connect(IP_B, 80, Some(41000), cfg(), n.now),
        Err(ConnectError::AddrInUse),
        "the four-tuple is reserved by TIME-WAIT"
    );
    n.run_for(11_000 * MS);
    assert!(
        n.a.core
            .tcp_connect(IP_B, 80, Some(41000), cfg(), n.now)
            .is_ok(),
        "free again after 10 s"
    );
}

// ---------------------------------------------------------------- many connections over a hostile link

#[test]
fn many_connections_over_a_lossy_reordering_link_all_deliver_their_bytes() {
    for seed in 1..=15u64 {
        let faults = Faults {
            loss_pct: seed % 6,
            dup_pct: seed % 3,
            reorder_pct: seed % 5,
            corrupt_pct: seed % 3,
            min_delay: MS / 4,
            max_delay: 3 * MS,
            ..Faults::NONE
        };
        let mut n = Net::new(faults, seed);
        let l = n.b.core.tcp_listen(None, 80).unwrap();
        let conns = 6usize;
        for i in 0..conns {
            n.b.core.tcp_post_accept(l, i as u32, cfg()).unwrap();
        }
        let ids: Vec<ConnId> = (0..conns)
            .map(|_| n.a.core.tcp_connect(IP_B, 80, None, cfg(), n.now).unwrap())
            .collect();
        let want: Vec<Vec<u8>> = (0..conns)
            .map(|i| {
                (0..40_000u32)
                    .map(|x| (x * (i as u32 + 3) + seed as u32) as u8)
                    .collect()
            })
            .collect();
        let mut sent = vec![0usize; conns];
        let mut got: BTreeMap<ConnId, Vec<u8>> = BTreeMap::new();
        let mut finished = 0;
        for _ in 0..400_000 {
            // the clients write what fits; the server reads everything and, at the end of each stream, shuts down
            for (i, &c) in ids.iter().enumerate() {
                if n.a
                    .core
                    .tcp_info(c)
                    .is_some_and(|x| x.state == nettcp::conn::State::Established)
                {
                    sent[i] += n.a.core.tcp_write(c, &want[i][sent[i]..]);
                    if sent[i] == want[i].len() {
                        n.a.core.tcp_shutdown_write(c);
                    }
                }
            }
            let accepted: Vec<ConnId> =
                n.b.events
                    .iter()
                    .filter_map(|e| {
                        if let TcpEvent::Accepted { conn, .. } = e {
                            Some(*conn)
                        } else {
                            None
                        }
                    })
                    .collect();
            for s in accepted {
                let mut buf = [0u8; 4096];
                loop {
                    let k = n.b.core.tcp_read(s, &mut buf);
                    if k == 0 {
                        break;
                    }
                    got.entry(s).or_default().extend_from_slice(&buf[..k]);
                }
                if n.b.core.tcp_eof(s)
                    && n.b
                        .core
                        .tcp_info(s)
                        .is_some_and(|i| i.state != nettcp::conn::State::LastAck)
                {
                    n.b.core.tcp_shutdown_write(s);
                }
            }
            finished = got.values().filter(|v| v.len() == 40_000).count();
            if finished == conns {
                break;
            }
            if !n.advance() {
                n.now += MS;
            }
        }
        assert_eq!(
            finished,
            conns,
            "seed {seed}: every connection delivered (now {} s)",
            n.now / (1000 * MS)
        );
        // each server connection's bytes are exactly one of the clients' streams
        let mut streams: Vec<&Vec<u8>> = got.values().collect();
        streams.sort();
        let mut wants: Vec<&Vec<u8>> = want.iter().collect();
        wants.sort();
        assert_eq!(streams, wants, "seed {seed}");
    }
}

#[test]
fn the_side_in_time_wait_accepts_a_new_syn_with_a_higher_sequence_number() {
    let mut n = Net::clean();
    let l = n.b.core.tcp_listen(None, 80).unwrap();
    n.b.core.tcp_post_accept(l, 1, cfg()).unwrap();
    let a =
        n.a.core
            .tcp_connect(IP_B, 80, Some(42000), cfg(), n.now)
            .unwrap();
    n.run_until("up", |n| {
        has_event(&n.b, |e| matches!(e, TcpEvent::Accepted { .. }))
    });
    let b =
        n.b.events
            .iter()
            .find_map(|e| {
                if let TcpEvent::Accepted { conn, .. } = e {
                    Some(*conn)
                } else {
                    None
                }
            })
            .unwrap();
    // b closes first, so b is the one that ends in TIME-WAIT
    n.b.core.tcp_shutdown_write(b);
    n.run_until("a sees the FIN", |n| n.a.core.tcp_eof(a));
    n.a.core.tcp_shutdown_write(a);
    n.run_for(20 * MS);
    assert_eq!(n.b.core.tcp_time_wait_count(), 1);
    assert_eq!(n.a.core.tcp_time_wait_count(), 0);
    // the same four-tuple again, long before 10 s are up: the new ISN is higher, so b takes it as a new connection
    n.b.events.clear();
    n.b.core.tcp_post_accept(l, 2, cfg()).unwrap();
    n.a.core
        .tcp_connect(IP_B, 80, Some(42000), cfg(), n.now)
        .unwrap();
    n.run_until("accepted again", |n| {
        has_event(&n.b, |e| matches!(e, TcpEvent::Accepted { cookie: 2, .. }))
    });
    assert_eq!(
        n.b.core.tcp_time_wait_count(),
        0,
        "the record made way for the new connection"
    );
}

#[test]
fn a_connection_whose_owner_has_unread_data_stays_in_the_table_until_it_has_read_it() {
    let mut n = Net::clean();
    let (a, b) = connected(&mut n);
    n.b.core.tcp_write(b, b"left unread");
    n.run_until("it arrived", |n| n.a.core.tcp_readable(a) == 11);
    // both sides close; a still holds 11 unread bytes
    n.b.core.tcp_shutdown_write(b);
    n.run_until("a sees the FIN", |n| {
        has_event(&n.a, |e| matches!(e, TcpEvent::PeerFin { .. }))
    });
    n.a.core.tcp_shutdown_write(a);
    n.run_for(50 * MS);
    assert!(
        n.a.core.tcp_info(a).is_some(),
        "the entry waits for its owner to read"
    );
    assert!(!has_event(&n.a, |e| matches!(e, TcpEvent::Closed { .. })));
    let mut buf = [0u8; 32];
    let k = n.a.core.tcp_read(a, &mut buf);
    assert_eq!(&buf[..k], b"left unread");
    n.run_for(50 * MS);
    assert!(
        has_event(&n.a, |e| matches!(e, TcpEvent::Closed { .. })),
        "closed once everything was read"
    );
    assert!(n.a.core.tcp_info(a).is_none());
    assert!(
        has_event(&n.a, |e| matches!(e, TcpEvent::Released { .. })),
        "and the release came last"
    );
}

#[test]
fn a_half_open_connection_that_dies_gives_its_posted_accept_back() {
    let mut n = Net::clean();
    let l = n.b.core.tcp_listen(None, 80).unwrap();
    n.b.core.tcp_post_accept(l, 1, cfg()).unwrap();
    n.a.core.tcp_connect(IP_B, 80, None, cfg(), 0).unwrap();
    n.run_until("the SYN took the accept", |n| {
        n.b.core.tcp_conn_count() == 1
    });
    assert_eq!(n.b.core.tcp_posted_accepts(l), 0);
    // the initiator vanishes (a restart): its fresh stack resets the SYN-ACK
    n.a = Host::new(MAC_A, IP_A);
    n.run_until("the accept is back", |n| {
        n.b.core.tcp_posted_accepts(l) == 1
    });
    assert_eq!(n.b.core.tcp_conn_count(), 0);
    // and it still works for a real connection
    n.a.core.tcp_connect(IP_B, 80, None, cfg(), n.now).unwrap();
    n.run_until("accepted", |n| {
        has_event(&n.b, |e| matches!(e, TcpEvent::Accepted { .. }))
    });
}

#[test]
fn a_syn_ack_that_times_out_gives_its_posted_accept_back() {
    let mut n = Net::clean();
    let l = n.b.core.tcp_listen(None, 80).unwrap();
    n.b.core.tcp_post_accept(l, 1, cfg()).unwrap();
    n.a.core.tcp_connect(IP_B, 80, None, cfg(), 0).unwrap();
    n.run_until("took", |n| n.b.core.tcp_conn_count() == 1);
    // a is silenced: it never answers the SYN-ACK
    n.a = Host::new(MAC_A, IP_A);
    n.a.core.link(false); // (its link is down: the SYN-ACK is never answered, not even with a reset)
    n.run_for(8_000 * MS);
    assert_eq!(
        n.b.core.tcp_posted_accepts(l),
        1,
        "the accept came back after the SYN-ACK gave up"
    );
    assert_eq!(n.b.core.tcp_conn_count(), 0);
}

#[test]
fn cancelling_an_accept_a_syn_already_took_drops_the_half_open_connection() {
    let mut n = Net::clean();
    let l = n.b.core.tcp_listen(None, 80).unwrap();
    n.b.core.tcp_post_accept(l, 7, cfg()).unwrap();
    n.a.core.tcp_connect(IP_B, 80, None, cfg(), 0).unwrap();
    n.run_until("took", |n| n.b.core.tcp_conn_count() == 1);
    assert!(n.b.core.tcp_cancel_accept(l, 7));
    assert_eq!(n.b.core.tcp_conn_count(), 0);
    assert_eq!(n.b.core.tcp_posted_accepts(l), 0, "and it was not put back");
}
