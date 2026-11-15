//! A deterministic link for tests (host only, `std`). Two ends; frames sent on one end arrive at the other after a
//! seeded random delay, and may be lost, duplicated, reordered or corrupted (one bit flipped), all from one seed so a
//! failing run reproduces exactly. Each direction may also have a rate limit with a bounded queue that tail-drops, and one
//! end may be replaced (a restart: what was on its way to the old one is gone). Time is a number the test advances;
//! nothing here reads a clock.

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
    /// A frame arrives with exactly one bit flipped (the length is unchanged).
    pub corrupt_pct: u64,
    /// Bytes each direction can start transmitting per time unit; 0 is unlimited. A frame of `n` bytes occupies the
    /// transmitter for `ceil(n / rate)` units, and its delay is counted from when it finishes.
    pub rate: u64,
    /// With a rate: how many frames may wait behind the one being transmitted; one more is dropped at the tail.
    pub queue_limit: usize,
}

impl Faults {
    pub const NONE: Faults = Faults {
        loss_pct: 0,
        dup_pct: 0,
        reorder_pct: 0,
        min_delay: 0,
        max_delay: 0,
        corrupt_pct: 0,
        rate: 0,
        queue_limit: 0,
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
    /// Per direction (the sending end): when the transmitter is free, and when each waiting frame will start.
    free_at: [u64; 2],
    waiting: [Vec<u64>; 2],
    pub sent: u64,
    pub lost: u64,
    pub duplicated: u64,
    pub corrupted: u64,
    pub tail_dropped: u64,
    /// Frames discarded because their destination end was replaced.
    pub orphaned: u64,
}

impl Link {
    pub fn new(seed: u64, faults: Faults) -> Link {
        Link {
            rng: Rng::new(seed),
            faults,
            flight: Vec::new(),
            seq: 0,
            free_at: [0; 2],
            waiting: [Vec::new(), Vec::new()],
            sent: 0,
            lost: 0,
            duplicated: 0,
            corrupted: 0,
            tail_dropped: 0,
            orphaned: 0,
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
        // the rate limiter: one transmitter per direction, a bounded queue behind it
        let mut tx_done = now;
        if f.rate > 0 {
            self.waiting[from].retain(|&t| t > now);
            if self.waiting[from].len() >= f.queue_limit && self.free_at[from] > now {
                self.tail_dropped += 1;
                return;
            }
            let start = self.free_at[from].max(now);
            tx_done = start + (frame.len() as u64).div_ceil(f.rate).max(1);
            self.free_at[from] = tx_done;
            if start > now {
                self.waiting[from].push(start);
            }
        }
        for _ in 0..copies {
            let mut delay =
                f.min_delay + self.rng.below(f.max_delay.saturating_sub(f.min_delay) + 1);
            if self.rng.chance(f.reorder_pct) {
                delay += f.max_delay + 1 + self.rng.below(f.max_delay + 1);
            }
            self.seq += 1;
            let mut copy = frame.to_vec();
            if !copy.is_empty() && self.rng.chance(f.corrupt_pct) {
                let at = self.rng.below(copy.len() as u64) as usize;
                copy[at] ^= 1 << self.rng.below(8);
                self.corrupted += 1;
            }
            self.flight.push(InFlight {
                at: tx_done + delay,
                seq: self.seq,
                to: 1 - from,
                frame: copy,
            });
        }
    }

    /// The frames that arrive at or before `now`, in arrival order, as `(to, frame)`.
    pub fn due(&mut self, now: u64) -> Vec<(usize, Vec<u8>)> {
        self.flight.sort_by_key(|f| (f.at, f.seq));
        let n = self.flight.iter().take_while(|f| f.at <= now).count();
        self.flight.drain(..n).map(|f| (f.to, f.frame)).collect()
    }

    /// End `end` (0 or 1) is replaced by a fresh one, as when a process restarts: every frame still on its way to it is
    /// gone. Frames the old end already sent are still delivered (they were on the wire). Returns how many were discarded.
    pub fn replace_endpoint(&mut self, end: usize) -> usize {
        let before = self.flight.len();
        self.flight.retain(|f| f.to != end);
        let n = before - self.flight.len();
        self.orphaned += n as u64;
        n
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
                    corrupt_pct: 10,
                    ..Faults::NONE
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
                ..Faults::NONE
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

    #[test]
    fn corruption_flips_exactly_one_bit_and_only_when_asked() {
        let mut l = Link::new(
            3,
            Faults {
                corrupt_pct: 100,
                ..Faults::NONE
            },
        );
        let sent: Vec<u8> = (0..100u8).collect();
        for _ in 0..200 {
            l.send(0, 0, &sent);
        }
        let got = l.due(0);
        assert_eq!(got.len(), 200);
        assert_eq!(l.corrupted, 200);
        let mut places = std::collections::HashSet::new();
        for (_, f) in &got {
            assert_eq!(f.len(), sent.len());
            let diff: Vec<usize> = (0..f.len()).filter(|&i| f[i] != sent[i]).collect();
            assert_eq!(diff.len(), 1, "one byte differs");
            assert_eq!((f[diff[0]] ^ sent[diff[0]]).count_ones(), 1, "by one bit");
            places.insert(diff[0]);
        }
        assert!(
            places.len() > 40,
            "the flipped byte varies: {}",
            places.len()
        );
        // none asked for: none happens, and the frames are byte for byte
        let mut clean = Link::new(3, Faults::NONE);
        for _ in 0..200 {
            clean.send(0, 0, &sent);
        }
        assert!(clean.due(0).iter().all(|(_, f)| *f == sent));
        assert_eq!(clean.corrupted, 0);
    }

    #[test]
    fn a_rate_limit_spaces_frames_and_the_queue_tail_drops() {
        let mut l = Link::new(
            1,
            Faults {
                rate: 100,
                queue_limit: 3,
                min_delay: 5,
                max_delay: 5,
                ..Faults::NONE
            },
        );
        let frame = [0u8; 100]; // one time unit each
        for i in 0..10u8 {
            let mut f = frame;
            f[0] = i;
            l.send(0, 0, &f);
        }
        // one transmitting plus three waiting are kept; six are dropped at the tail
        assert_eq!(l.tail_dropped, 6);
        // the other direction has its own transmitter
        l.send(1, 0, &frame);
        assert_eq!(l.tail_dropped, 6);
        let mut arrivals = vec![];
        for t in 0..20 {
            for (to, f) in l.due(t) {
                arrivals.push((t, to, f[0]));
            }
        }
        // transmission ends at 1, 2, 3, 4, then 5 units of delay: the kept frames are the first four, in order
        let from0: Vec<_> = arrivals.iter().filter(|a| a.1 == 1).collect();
        assert_eq!(
            from0.iter().map(|a| (a.0, a.2)).collect::<Vec<_>>(),
            vec![(6, 0), (7, 1), (8, 2), (9, 3)]
        );
        assert_eq!(
            arrivals
                .iter()
                .filter(|a| a.1 == 0)
                .map(|a| a.0)
                .collect::<Vec<_>>(),
            vec![6]
        );
        // after the queue drained, the limiter accepts again, and a big frame takes proportionally longer
        l.send(0, 100, &[0u8; 350]);
        assert_eq!(l.tail_dropped, 6);
        assert_eq!(l.next_arrival(), Some(100 + 4 + 5));
    }

    #[test]
    fn a_replaced_end_loses_what_was_on_its_way_and_keeps_what_it_sent() {
        let mut l = Link::new(
            1,
            Faults {
                min_delay: 10,
                max_delay: 10,
                ..Faults::NONE
            },
        );
        for i in 0..5u8 {
            l.send(0, 0, &[i]); // on its way to end 1
        }
        l.send(1, 0, &[9]); // end 1's last words, on their way to end 0
        assert_eq!(l.replace_endpoint(1), 5);
        assert_eq!(l.orphaned, 5);
        assert_eq!(l.in_flight(), 1);
        let got = l.due(10);
        assert_eq!(
            got,
            vec![(0, vec![9])],
            "what the old end sent still arrives"
        );
        // the new end works
        l.send(0, 20, &[7]);
        assert_eq!(l.due(30), vec![(1, vec![7])]);
        // replacing an end with nothing on its way discards nothing
        assert_eq!(l.replace_endpoint(1), 0);
    }
}
