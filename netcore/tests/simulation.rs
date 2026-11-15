//! H6 of NET-SPEC 12.1: deterministic simulation. Two cores over a link with seeded loss, duplication, reordering
//! and delay, a random mix of sends, receives and endpoint opens and closes. Properties: every delivered datagram
//! equals one that was sent, none reaches the wrong endpoint, ARP converges once frames get through, and no table or
//! queue ever exceeds its bound. A failing seed is printed and reproduces exactly (`SEED=n cargo test`).
use netcore::*;
use netsim::{Faults, Link, Rng};
use std::collections::{BTreeMap, BTreeSet};

const MAC: [[u8; 6]; 2] = [[0x52, 0x54, 0, 0, 0, 1], [0x52, 0x54, 0, 0, 0, 2]];
const IP: [[u8; 4]; 2] = [[10, 0, 2, 15], [10, 0, 2, 20]];

struct Node {
    core: Core,
    /// endpoint id -> its UDP port
    ports: BTreeMap<EpId, u16>,
    echoes: BTreeSet<EpId>,
}

struct World {
    nodes: [Node; 2],
    link: Link,
    now: Instant,
    /// every datagram sent: (from node, to port, payload)
    sent: BTreeSet<(usize, u16, Vec<u8>)>,
    delivered: u64,
}

struct Wire<'a> {
    link: &'a mut Link,
    from: usize,
    now: Instant,
}
impl Device for Wire<'_> {
    fn tx(&mut self, frame: &[u8]) -> bool {
        self.link.send(self.from, self.now, frame);
        true
    }
}

impl World {
    fn new(seed: u64, faults: Faults) -> World {
        let mk = |i: usize| {
            let mut core = Core::new(MAC[i]);
            core.link(true);
            core.configure(
                Config {
                    addr: IP[i],
                    mask: [255, 255, 255, 0],
                    gateway: [0; 4],
                    dns: [0; 4],
                },
                0,
            );
            Node {
                core,
                ports: BTreeMap::new(),
                echoes: BTreeSet::new(),
            }
        };
        World {
            nodes: [mk(0), mk(1)],
            link: Link::new(seed, faults),
            now: 0,
            sent: BTreeSet::new(),
            delivered: 0,
        }
    }

    fn apply(&mut self, i: usize, events: Vec<Event>, ctx: &str) {
        for ev in events {
            match ev {
                Event::Deliver {
                    ep, payload, src, ..
                } => {
                    let port = *self.nodes[i]
                        .ports
                        .get(&ep)
                        .unwrap_or_else(|| panic!("{ctx}: delivery to an unknown endpoint {ep}"));
                    assert_eq!(src, IP[1 - i], "{ctx}: wrong source address");
                    assert!(
                        self.sent.contains(&(1 - i, port, payload.clone())),
                        "{ctx}: delivered a datagram that was never sent to port {port}: {payload:?}"
                    );
                    self.delivered += 1;
                }
                Event::EchoReply { ep, .. } => {
                    assert!(
                        self.nodes[i].echoes.contains(&ep),
                        "{ctx}: an echo reply for an endpoint that is not an echo endpoint"
                    );
                }
                Event::Notice { .. } => {}
            }
        }
    }

    /// Move time forward to `t`, delivering what arrives and running the timers and egress of both nodes.
    fn run_to(&mut self, t: Instant, ctx: &str) {
        loop {
            let next = [
                self.link.next_arrival(),
                self.nodes[0].core.next_deadline(),
                self.nodes[1].core.next_deadline(),
            ]
            .into_iter()
            .flatten()
            .filter(|&x| x > self.now && x <= t)
            .min();
            match next {
                Some(n) => self.now = n,
                None => {
                    self.now = t;
                }
            }
            self.pump(ctx);
            if self.now >= t {
                break;
            }
        }
    }

    fn pump(&mut self, ctx: &str) {
        for _ in 0..4 {
            for (to, frame) in self.link.due(self.now) {
                let ev = self.nodes[to].core.ingress(&frame, self.now);
                self.apply(to, ev, ctx);
            }
            for i in 0..2 {
                let mut w = Wire {
                    link: &mut self.link,
                    from: i,
                    now: self.now,
                };
                let (_, ev) = self.nodes[i].core.egress(&mut w, self.now, 1000);
                self.apply(i, ev, ctx);
            }
            self.bounds(ctx);
        }
    }

    fn bounds(&self, ctx: &str) {
        for n in &self.nodes {
            assert!(
                n.core.arp_len() <= ARP_CACHE_MAX,
                "{ctx}: ARP cache over its bound"
            );
            assert!(
                n.core.pending_len() <= ARP_PENDING_TOTAL,
                "{ctx}: pending queues over their bound"
            );
            assert!(
                n.core.tx_queue_len() <= TX_QUEUE_MAX,
                "{ctx}: transmit queue over its bound"
            );
            assert_eq!(
                n.core.endpoint_count(),
                n.ports.len() + n.echoes.len(),
                "{ctx}: endpoint bookkeeping"
            );
        }
    }
}

fn run_seed(seed: u64, steps: usize) -> u64 {
    let mut rng = Rng::new(seed);
    let faults = Faults {
        loss_pct: rng.below(30),
        dup_pct: rng.below(15),
        reorder_pct: rng.below(30),
        min_delay: 0,
        max_delay: 3 * MS,
        ..Faults::NONE
    };
    let mut w = World::new(seed, faults);
    let mut counter = 0u32;
    for step in 0..steps {
        let ctx = format!("seed {seed} step {step} faults {faults:?}");
        let i = rng.below(2) as usize;
        match rng.below(10) {
            0 | 1 => {
                // open a UDP endpoint on a small set of ports (so they collide and are reused)
                let port = if rng.below(4) == 0 {
                    0
                } else {
                    7000 + rng.below(6) as u16
                };
                if let Ok((ep, p)) = w.nodes[i].core.open_udp(port, None, false) {
                    assert!(
                        w.nodes[i].ports.values().all(|&q| q != p),
                        "{ctx}: port {p} bound twice"
                    );
                    w.nodes[i].ports.insert(ep, p);
                } else {
                    assert!(
                        w.nodes[i].ports.values().any(|&q| q == port),
                        "{ctx}: a bind failed for a free port"
                    );
                }
            }
            2 => {
                let eps: Vec<EpId> = w.nodes[i].ports.keys().copied().collect();
                if !eps.is_empty() {
                    let ep = eps[rng.below(eps.len() as u64) as usize];
                    w.nodes[i].core.close(ep);
                    w.nodes[i].ports.remove(&ep);
                }
            }
            3 => {
                if let Ok((ep, _)) = w.nodes[i].core.open_echo(false) {
                    w.nodes[i].echoes.insert(ep);
                }
            }
            4..=7 => {
                let eps: Vec<EpId> = w.nodes[i].ports.keys().copied().collect();
                if let Some(&ep) = eps.get(rng.below(eps.len().max(1) as u64) as usize) {
                    let dport = 7000 + rng.below(6) as u16;
                    counter += 1;
                    let len = rng.below(300) as usize;
                    let mut payload = counter.to_be_bytes().to_vec();
                    payload.extend((0..len).map(|k| (k as u8) ^ (counter as u8)));
                    w.sent.insert((i, dport, payload.clone()));
                    let now = w.now;
                    let _ = w.nodes[i]
                        .core
                        .send_udp(ep, IP[1 - i], dport, &payload, counter, now);
                }
            }
            8 => {
                let eps: Vec<EpId> = w.nodes[i].echoes.iter().copied().collect();
                if let Some(&ep) = eps.get(rng.below(eps.len().max(1) as u64) as usize) {
                    let now = w.now;
                    let _ = w.nodes[i].core.send_echo(ep, IP[1 - i], 1, b"ping", 0, now);
                }
            }
            _ => {}
        }
        let to = w.now + rng.below(2 * MS);
        w.run_to(to, &ctx);
    }
    // convergence: with the faults off and time to settle, a fresh datagram each way is delivered (ARP converged)
    let ctx = format!("seed {seed} settle");
    w.link.faults = Faults::NONE;
    let t = w.now + 10 * SEC;
    w.run_to(t, &ctx);
    for i in 0..2 {
        let (ep, port) = w.nodes[1 - i]
            .core
            .open_udp(9000 + i as u16, None, false)
            .unwrap();
        w.nodes[1 - i].ports.insert(ep, port);
        let (sep, _) = w.nodes[i].core.open_udp(0, None, false).unwrap();
        w.nodes[i].ports.insert(sep, 0);
        let payload = vec![0xee, i as u8];
        w.sent.insert((i, 9000 + i as u16, payload.clone()));
        let before = w.delivered;
        let now = w.now;
        w.nodes[i]
            .core
            .send_udp(sep, IP[1 - i], 9000 + i as u16, &payload, 0, now)
            .unwrap();
        let t = w.now + 5 * SEC;
        w.run_to(t, &ctx);
        assert!(
            w.delivered > before,
            "{ctx}: a datagram from node {i} was not delivered after the link was clean"
        );
    }
    w.delivered
}

#[test]
fn two_cores_over_a_hostile_link_keep_every_property() {
    let (from, to) = match std::env::var("SEED") {
        Ok(s) => {
            let n: u64 = s.parse().unwrap();
            (n, n + 1)
        }
        Err(_) => (1, 501),
    };
    let mut total = 0;
    for seed in from..to {
        total += run_seed(seed, 150);
    }
    if to - from > 100 {
        assert!(
            total > 5_000,
            "the simulation barely delivered anything ({total}): it proves nothing"
        );
    }
}

#[test]
#[ignore = "tier 2: 20,000 seeds (cargo test --release -- --ignored)"]
fn the_long_simulation() {
    for seed in 1000..21_000 {
        let _ = run_seed(seed, 150);
    }
}
