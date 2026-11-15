//! A deterministic link for tests (host only, `std`). Two ends; frames sent on one end arrive at the other after a
//! seeded random delay, and may be lost, duplicated or reordered, all from one seed so a failing run reproduces exactly.
//! Time is a number the test advances; nothing here reads a clock.

/// xorshift64*: deterministic and good enough for choosing faults.
#[derive(Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }
    /// True with probability `pct` percent.
    pub fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
}

/// How hostile the link is. All percentages; delays in the caller's time unit.
#[derive(Clone, Copy, Debug)]
pub struct Faults {
    pub loss_pct: u64,
    pub dup_pct: u64,
    /// A frame held back by an extra delay, so a later one overtakes it.
    pub reorder_pct: u64,
    pub min_delay: u64,
    pub max_delay: u64,
}

impl Faults {
    pub const NONE: Faults = Faults {
        loss_pct: 0,
        dup_pct: 0,
        reorder_pct: 0,
        min_delay: 0,
        max_delay: 0,
    };
}

struct InFlight {
    at: u64,
    seq: u64,
    to: usize,
    frame: Vec<u8>,
}

pub struct Link {
    rng: Rng,
    pub faults: Faults,
    flight: Vec<InFlight>,
    seq: u64,
    pub sent: u64,
    pub lost: u64,
    pub duplicated: u64,
}

impl Link {
    pub fn new(seed: u64, faults: Faults) -> Link {
        Link {
            rng: Rng::new(seed),
            faults,
            flight: Vec::new(),
            seq: 0,
            sent: 0,
            lost: 0,
            duplicated: 0,
        }
    }

    /// A frame leaves `from` (0 or 1) at time `now`.
    pub fn send(&mut self, from: usize, now: u64, frame: &[u8]) {
        self.sent += 1;
        let f = self.faults;
        if self.rng.chance(f.loss_pct) {
            self.lost += 1;
            return;
        }
        let copies = if self.rng.chance(f.dup_pct) {
            self.duplicated += 1;
            2
        } else {
            1
        };
        for _ in 0..copies {
            let mut delay =
                f.min_delay + self.rng.below(f.max_delay.saturating_sub(f.min_delay) + 1);
            if self.rng.chance(f.reorder_pct) {
                delay += f.max_delay + 1 + self.rng.below(f.max_delay + 1);
            }
            self.seq += 1;
            self.flight.push(InFlight {
                at: now + delay,
                seq: self.seq,
                to: 1 - from,
                frame: frame.to_vec(),
            });
        }
    }

    /// The frames that arrive at or before `now`, in arrival order, as `(to, frame)`.
    pub fn due(&mut self, now: u64) -> Vec<(usize, Vec<u8>)> {
        self.flight.sort_by_key(|f| (f.at, f.seq));
        let n = self.flight.iter().take_while(|f| f.at <= now).count();
        self.flight.drain(..n).map(|f| (f.to, f.frame)).collect()
    }

    pub fn in_flight(&self) -> usize {
        self.flight.len()
    }

    /// The time of the next arrival, if any.
    pub fn next_arrival(&self) -> Option<u64> {
        self.flight.iter().map(|f| f.at).min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_seed_gives_the_same_run() {
        let run = |seed| {
            let mut l = Link::new(
                seed,
                Faults {
                    loss_pct: 20,
                    dup_pct: 10,
                    reorder_pct: 20,
                    min_delay: 1,
                    max_delay: 5,
                },
            );
            let mut out = vec![];
            for i in 0..200u64 {
                l.send((i % 2) as usize, i, &[i as u8]);
                out.extend(l.due(i));
            }
            out.extend(l.due(10_000));
            out
        };
        assert_eq!(run(7), run(7));
        assert_ne!(run(7), run(8));
    }

    #[test]
    fn faults_do_what_they_say() {
        let mut l = Link::new(
            1,
            Faults {
                loss_pct: 50,
                dup_pct: 0,
                reorder_pct: 0,
                min_delay: 2,
                max_delay: 2,
            },
        );
        for i in 0..1000u64 {
            l.send(0, 0, &[i as u8]);
        }
        let got = l.due(1);
        assert!(got.is_empty(), "nothing arrives before its delay");
        let got = l.due(2);
        assert!(
            got.len() > 350 && got.len() < 650,
            "about half survive: {}",
            got.len()
        );
        assert_eq!(l.lost as usize + got.len(), 1000);
        assert!(got.iter().all(|(to, _)| *to == 1));
    }
}
