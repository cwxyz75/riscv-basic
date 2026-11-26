//! IOMMU4: the per-hart interrupt-identity allocator (`kcore::msiid`, IOMMU-SPEC 8.1, IO12) against a model of the machine around it.
//!
//! The model is what the kernel drives: each hart's interrupt file (its enable bits and pending bits, with the source each pending
//! message came from), the APLIC's target per source, messages **in flight** (sent to a target and not landed yet: a real interconnect
//! does not deliver at once), interrupt objects that fire, get acknowledged on any hart (a move) or are destroyed, harts that service
//! their own file only when told (opening identities, retiring them), and harts taken offline (`evacuate`). A reference map of which
//! identities are taken is kept apart from the allocator's own state. What must hold, after every step:
//!
//! * a message is taken only on an identity routed to its own source (Live, or Retiring after a move), or discarded on an identity
//!   whose route ended: **never on a Free identity or another route's** (no lost or misdelivered message: IO10, IO12);
//! * every identity open in a hart's file has a route (IO12), and identity 0 is never open or given; identity 1 is the inter-hart one;
//! * a source has at most one Live identity, and the APLIC targets it;
//! * `alloc` never returns an identity that is taken (a Retiring one included), and only a retirement frees one;
//! * at the end, with everything destroyed and retired, every identity is Free again and every message sent was taken.
//!
//! * a message is delivered only to the object it was sent for, never to the next object of the same source;
//! * a hart that is offline holds no route: nothing Opening, and every Live identity on it is moving away (IOMMU-SPEC 8.5).
//!
//! The mutations it was seen to catch: `fault-msiid-free-at-move` (the old identity freed at the move instead of at its retirement),
//! `alloc` handing out identity 1, the `num_ids` bound off by one (identity `num_ids + 1` handed out, or `num_ids` never), `evacuate`
//! dropping a new route that its hart had not opened yet, `end` keeping the moved-from identities of an ended route for the source's
//! next object, and an acknowledgement on a departing hart cancelling the move its evacuation started.
use kcore::msiid::{Evacuated, IdSet, MsiIds, Opened, Place, Route, Slot, IPI};
use std::collections::{BTreeMap, BTreeSet};

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

#[derive(Clone, Copy, Debug)]
struct Obj {
    at: Place,
    armed: bool,
    /// which object of its source this is (a source's objects are destroyed and created again)
    generation: u64,
}

/// A message: its source in the low 32 bits, the generation of the object it was sent for above.
fn src(m: u64) -> u32 {
    m as u32
}

struct Sim {
    ids: MsiIds,
    harts: u16,
    n: u16,
    eie: Vec<BTreeSet<u16>>,
    /// pending bits: the sources whose messages set it (several messages of one source merge into one bit)
    eip: Vec<BTreeMap<u16, Vec<u64>>>,
    generations: u64,
    objs: BTreeMap<u32, Obj>,
    target: BTreeMap<u32, Place>,
    inflight: Vec<(u64, Place)>,
    work: BTreeSet<u16>,
    online: Vec<bool>,
    /// the reference: which identities are taken (independent of the allocator's own state)
    taken: BTreeSet<Place>,
    sent: u64,
    took: u64,
    delivered: BTreeMap<u32, u64>,
}

impl Sim {
    fn new(harts: u16, n: u16) -> Sim {
        Sim {
            ids: MsiIds::new(harts as usize, u32::from(n)).unwrap(),
            harts,
            n,
            eie: (0..harts).map(|_| [IPI].into_iter().collect()).collect(),
            eip: (0..harts).map(|_| BTreeMap::new()).collect(),
            objs: BTreeMap::new(),
            target: BTreeMap::new(),
            inflight: Vec::new(),
            generations: 0,
            work: BTreeSet::new(),
            online: vec![true; harts as usize],
            taken: BTreeSet::new(),
            sent: 0,
            took: 0,
            delivered: BTreeMap::new(),
        }
    }

    fn note_taken(&mut self, p: Place) {
        assert!(p.id >= 2, "identity {} given out at {p:?}", p.id);
        assert!(
            p.id <= self.n,
            "identity {} is past num-ids {}",
            p.id,
            self.n
        );
        assert!(
            self.taken.insert(p),
            "{p:?} given out while it is still taken (Retiring, Opening or Live)"
        );
    }

    /// The hart opens identity `p` in its own file (the identity was just allocated there and nothing targets it yet).
    fn open(&mut self, p: Place) -> Opened {
        assert!(
            !self.eip[p.hart as usize].contains_key(&p.id),
            "a stale pending bit on {p:?} when it is opened: a message reached it while it had no route"
        );
        self.eie[p.hart as usize].insert(p.id);
        let o = self.ids.opened(p).expect("an Opening identity");
        if let Some(r) = o.retiring {
            self.work.insert(r.hart);
        }
        o
    }

    fn aim(&mut self, s: u32, p: Place) {
        self.target.insert(s, p);
        if let Some(o) = self.objs.get_mut(&s) {
            o.at = p;
        }
    }

    fn create(&mut self, s: u32, hart: u16) {
        if self.objs.contains_key(&s) {
            return;
        }
        if !self.online[hart as usize] {
            // (a thread still on a hart that is leaving: the identity goes to an online hart, which opens it and aims the source later)
            let online = self.online.clone();
            let Some(p) = self.ids.alloc_any(Route::Wired(s), |h| online[h as usize]) else {
                return;
            };
            assert!(
                online[p.hart as usize],
                "a new route placed on an offline hart"
            );
            self.note_taken(p);
            self.generations += 1;
            self.objs.insert(
                s,
                Obj {
                    at: p,
                    armed: true,
                    generation: self.generations,
                },
            );
            self.work.insert(p.hart);
            return;
        }
        let Some(id) = self.ids.alloc(hart, Route::Wired(s)) else {
            assert_eq!(
                self.ids.free_on(hart),
                0,
                "alloc failed with free identities"
            );
            return;
        };
        let p = Place { hart, id };
        self.note_taken(p);
        self.generations += 1;
        self.objs.insert(
            s,
            Obj {
                at: p,
                armed: true,
                generation: self.generations,
            },
        );
        self.open(p);
        self.aim(s, p);
    }

    fn destroy(&mut self, s: u32) {
        let Some(o) = self.objs.remove(&s) else {
            return;
        };
        let cancelled = self.ids.end(o.at).expect("a live object's identity ends");
        self.target.remove(&s);
        self.work.insert(o.at.hart);
        if let Some(c) = cancelled {
            self.work.insert(c.hart);
        }
    }

    /// The object was fired; it is acknowledged on `hart`: the route moves there (opened at once: the hart is the caller's own) and
    /// the source is unmasked.
    fn ack(&mut self, s: u32, hart: u16) {
        let Some(o) = self.objs.get(&s).copied() else {
            return;
        };
        if o.armed {
            return;
        }
        // (as the kernel's `reenable`: only an acknowledgement that will move the route cancels a move under way; one on a hart that is
        // leaving, or with no free identity, leaves the route and its move as they are)
        if o.at.hart != hart && self.online[hart as usize] && self.ids.free_on(hart) > 0 {
            if let Some(Slot::Live { to: Some(t), .. }) = self.ids.slot(o.at) {
                self.ids.end(t).unwrap();
                self.work.insert(t.hart);
            }
            if let Some(id) = self.ids.move_to(o.at, hart) {
                let p = Place { hart, id };
                self.note_taken(p);
                self.open(p);
                self.aim(s, p);
            }
        }
        self.objs.get_mut(&s).unwrap().armed = true;
    }

    /// A move the target hart opens later (as a hart going offline asks of the others).
    fn remote_move(&mut self, s: u32, hart: u16) {
        let Some(o) = self.objs.get(&s).copied() else {
            return;
        };
        if !self.online[hart as usize] {
            return;
        }
        if let Some(id) = self.ids.move_to(o.at, hart) {
            self.note_taken(Place { hart, id });
            self.work.insert(hart);
        }
    }

    fn fire(&mut self, s: u32) {
        if let (Some(o), Some(&t)) = (self.objs.get(&s), self.target.get(&s)) {
            if o.armed {
                self.inflight.push((o.generation << 32 | u64::from(s), t));
                self.sent += 1;
            }
        }
    }

    fn land(&mut self, k: usize) {
        let (m, p) = self.inflight.swap_remove(k);
        let s = src(m);
        let pend = self.eip[p.hart as usize].entry(p.id).or_default();
        if let Some(&other) = pend.first() {
            let other = src(other);
            assert_eq!(
                other, s,
                "{p:?} has messages of sources {other} and {s} pending: one identity, two routes"
            );
        }
        pend.push(m);
    }

    fn land_all_to(&mut self, p: Place) {
        while let Some(k) = self.inflight.iter().position(|&(_, q)| q == p) {
            self.land(k);
        }
    }

    /// A message (or several merged) of source `s` taken on `p`: to its route, or discarded where the route ended.
    fn take(&mut self, p: Place, srcs: Vec<u64>) {
        let s = src(srcs[0]);
        self.took += srcs.len() as u64;
        match self.ids.slot(p) {
            Some(Slot::Live {
                route: Route::Wired(r),
                ..
            })
            | Some(Slot::Retiring {
                route: Route::Wired(r),
                moved: true,
            }) => {
                assert_eq!(
                    r, s,
                    "a message of source {s} delivered to source {r} on {p:?}"
                );
                *self.delivered.entry(s).or_default() += 1;
                if let Some(o) = self.objs.get(&s) {
                    for &m in &srcs {
                        assert_eq!(
                            m >> 32,
                            o.generation,
                            "a message sent for an earlier object of source {s} delivered to the object that replaced it, on {p:?}"
                        );
                    }
                }
                if let Some(o) = self.objs.get_mut(&s) {
                    o.armed = false; // masked: the object fired
                }
            }
            Some(Slot::Retiring {
                route: Route::Wired(r),
                moved: false,
            }) => assert_eq!(
                r, s,
                "a message of source {s} discarded on source {r}'s ended identity {p:?}"
            ),
            other => panic!("a message of source {s} taken on {p:?} with no route: {other:?}"),
        }
    }

    fn claim(&mut self, hart: u16) {
        let h = hart as usize;
        let ready: Vec<u16> = self.eip[h]
            .keys()
            .copied()
            .filter(|id| self.eie[h].contains(id))
            .collect();
        for id in ready {
            let srcs = self.eip[h].remove(&id).unwrap();
            self.take(Place { hart, id }, srcs);
        }
    }

    fn service(&mut self, hart: u16) {
        if !self.work.remove(&hart) {
            return;
        }
        // (what the kernel does on the message that told it: first the claims, then the identities with work)
        self.claim(hart);
        let work: Vec<(u16, Slot)> = self.ids.work_on(hart).collect();
        for (id, slot) in work {
            let p = Place { hart, id };
            match slot {
                Slot::Opening { route, .. } => {
                    let o = self.open(p);
                    assert_eq!(o.route, route);
                    if let Route::Wired(s) = route {
                        self.aim(s, p);
                    }
                }
                Slot::Retiring { .. } => {
                    // the hart closes the identity; whatever was sent to it before the APLIC was aimed elsewhere has landed (A12)
                    self.eie[hart as usize].remove(&id);
                    self.land_all_to(p);
                    if let Some(srcs) = self.eip[hart as usize].remove(&id) {
                        self.take(p, srcs);
                    }
                    self.ids.retired(p).expect("a Retiring identity retires");
                    assert!(self.taken.remove(&p), "{p:?} retired but it was not taken");
                }
                _ => unreachable!(),
            }
        }
    }

    fn offline(&mut self, hart: u16) {
        if !self.online[hart as usize] || self.online.iter().filter(|&&o| o).count() < 2 {
            return;
        }
        let before = self.snapshot();
        let online = self.online.clone();
        let mut changes = Vec::new();
        match self
            .ids
            .evacuate(hart, |h| online[h as usize], |c| changes.push(c))
        {
            Ok(()) => {
                self.online[hart as usize] = false;
                for c in changes {
                    match c {
                        Evacuated::Cancelled { at, .. } => {
                            self.work.insert(at.hart);
                        }
                        Evacuated::Replaced { from, to, route } => {
                            assert!(online[to.hart as usize], "placed again on an offline hart");
                            assert_eq!(from.hart, hart);
                            let Route::Wired(s) = route else {
                                panic!("{route:?} replaced")
                            };
                            self.note_taken(to);
                            self.objs.get_mut(&s).expect("an object's new route").at = to;
                            self.work.insert(to.hart);
                            self.work.insert(from.hart);
                        }
                        Evacuated::Moved { to, from, .. } => {
                            assert_ne!(to.hart, hart, "evacuated onto the departing hart");
                            assert!(online[to.hart as usize], "evacuated onto an offline hart");
                            assert_ne!(from.hart, to.hart);
                            self.note_taken(to);
                            self.work.insert(to.hart);
                        }
                    }
                }
            }
            Err(()) => {
                assert!(changes.is_empty(), "a refused evacuation reported changes");
                assert_eq!(
                    before,
                    self.snapshot(),
                    "a refused evacuation changed something"
                );
            }
        }
    }

    fn snapshot(&self) -> Vec<Option<Slot>> {
        (0..self.harts)
            .flat_map(|hart| (0..=self.n).map(move |id| Place { hart, id }))
            .map(|p| self.ids.slot(p))
            .collect()
    }

    fn check(&self) {
        let mut live: BTreeMap<u32, u32> = BTreeMap::new();
        for hart in 0..self.harts {
            let mut free = 0;
            assert_eq!(self.ids.slot(Place { hart, id: 0 }), Some(Slot::Free));
            assert_eq!(
                self.ids.slot(Place { hart, id: IPI }),
                Some(Slot::Live {
                    route: Route::Ipi,
                    to: None
                })
            );
            assert_eq!(
                self.ids.slot(Place {
                    hart,
                    id: self.n + 1
                }),
                None
            );
            for id in 2..=self.n {
                let p = Place { hart, id };
                let s = self.ids.slot(p).unwrap();
                if s == Slot::Free {
                    free += 1;
                    assert!(
                        !self.taken.contains(&p),
                        "{p:?} is Free but was never retired"
                    );
                } else {
                    assert!(
                        self.taken.contains(&p),
                        "{p:?} is {s:?} but was never given out"
                    );
                }
                if let Slot::Live {
                    route: Route::Wired(src),
                    ..
                } = s
                {
                    *live.entry(src).or_default() += 1;
                }
            }
            assert_eq!(
                self.ids.free_on(hart),
                free,
                "the free count of hart {hart}"
            );
            for &id in &self.eie[hart as usize] {
                assert!(id != 0, "identity 0 is open");
                let s = self.ids.slot(Place { hart, id }).unwrap();
                assert_ne!(
                    s,
                    Slot::Free,
                    "identity {id} is open on hart {hart} with no route (IO12)"
                );
                assert!(!matches!(s, Slot::Opening { .. }));
            }
        }
        // a hart that is offline holds no route: nothing Opening, and every Live identity is moving away (IOMMU-SPEC 8.5)
        for hart in 0..self.harts {
            if self.online[hart as usize] {
                continue;
            }
            for id in 2..=self.n {
                let p = Place { hart, id };
                match self.ids.slot(p).unwrap() {
                    Slot::Opening { .. } | Slot::Live { to: None, .. } => {
                        panic!("{p:?} is {:?} on an offline hart", self.ids.slot(p))
                    }
                    _ => {}
                }
            }
        }
        for (&src, &k) in &live {
            assert!(k <= 1, "source {src} has {k} Live identities");
        }
        for (&src, &p) in &self.target {
            assert!(
                matches!(self.ids.slot(p), Some(Slot::Live { route: Route::Wired(x), .. }) if x == src),
                "the APLIC aims source {src} at {p:?}, which is not its Live identity: {:?}",
                self.ids.slot(p)
            );
            assert_eq!(self.objs[&src].at, p);
        }
    }

    fn drain(&mut self) {
        for s in self.objs.keys().copied().collect::<Vec<_>>() {
            self.destroy(s);
        }
        for _ in 0..4 {
            while let Some(&(_, p)) = self.inflight.first() {
                let _ = p;
                self.land(0);
            }
            for h in 0..self.harts {
                self.work.insert(h);
                self.service(h);
                self.claim(h);
            }
        }
        self.check();
        assert!(self.inflight.is_empty());
        assert_eq!(
            self.ids.free_total(),
            u64::from(self.harts) * u64::from(self.n - 1),
            "every identity comes back"
        );
        assert!(self.taken.is_empty(), "taken at the end: {:?}", self.taken);
        for h in 0..self.harts as usize {
            assert_eq!(
                self.eie[h],
                [IPI].into_iter().collect(),
                "only the inter-hart identity is open"
            );
            assert!(self.eip[h].is_empty());
        }
        assert_eq!(self.sent, self.took, "every message sent was taken");
    }
}

fn run(seed: u64, harts: u16, n: u16, sources: u32, steps: usize) {
    let mut r = Lcg(seed);
    let mut sim = Sim::new(harts, n);
    for _ in 0..steps {
        let s = 1 + r.below(u64::from(sources)) as u32;
        let h = r.below(u64::from(harts)) as u16;
        match r.below(100) {
            0..=14 => sim.create(s, h),
            15..=22 => sim.destroy(s),
            23..=42 => sim.ack(s, h),
            43..=47 => sim.remote_move(s, h),
            48..=67 => sim.fire(s),
            68..=79 => {
                if !sim.inflight.is_empty() {
                    let k = r.below(sim.inflight.len() as u64) as usize;
                    sim.land(k);
                }
            }
            80..=86 => sim.claim(h),
            87..=96 => sim.service(h),
            97 => sim.offline(h),
            _ => sim.online[h as usize] = true,
        }
        sim.check();
    }
    sim.drain();
}

#[test]
fn identities_route_every_message_to_its_own_source_and_all_come_back() {
    for seed in 1..=150u64 {
        // few identities, so exhaustion, reuse and wrap-round are common; and the reference machine's 255
        run(seed, 1 + (seed % 4) as u16, 7, 9, 1500);
        run(seed ^ 0x5555, 1 + (seed % 3) as u16 + 1, 15, 30, 1500);
    }
    for seed in 1..=10u64 {
        run(seed, 4, 255, 96, 6000);
    }
}

#[test]
fn identities_zero_and_one_are_never_given_and_the_bound_is_num_ids() {
    for n in [2u16, 7, 63, 64, 255, 2047] {
        let mut ids = MsiIds::new(2, u32::from(n)).unwrap();
        let mut got = BTreeSet::new();
        while let Some(id) = ids.alloc(0, Route::Wired(5)) {
            assert!((2..=n).contains(&id), "identity {id} with num-ids {n}");
            assert!(got.insert(id));
        }
        assert_eq!(
            got.len(),
            usize::from(n) - 1,
            "every identity 2..={n} was given"
        );
        assert_eq!(ids.free_on(0), 0);
        assert_eq!(ids.free_on(1), n - 1, "the other hart's space is its own");
        assert_eq!(ids.alloc(0, Route::Ipi), None);
        assert_eq!(
            ids.alloc(1, Route::Ipi),
            None,
            "the inter-hart interrupt is never allocated"
        );
    }
    assert!(MsiIds::new(1, 1).is_none());
    assert!(MsiIds::new(1, 2048).is_none());
    assert!(MsiIds::new(0, 255).is_none());
    let mut ids = MsiIds::new(1, 255).unwrap();
    assert_eq!(
        ids.end(Place { hart: 0, id: IPI }),
        Err(()),
        "the inter-hart route never ends"
    );
    assert_eq!(ids.move_to(Place { hart: 0, id: IPI }, 0), None);
    assert_eq!(ids.slot(Place { hart: 1, id: 2 }), None);
    assert_eq!(ids.slot(Place { hart: 0, id: 256 }), None);
}

#[test]
fn allocation_is_next_fit_so_reuse_is_delayed_and_every_band_is_used() {
    let mut ids = MsiIds::new(1, 255).unwrap();
    let mut seen = Vec::new();
    for _ in 0..600 {
        let id = ids.alloc(0, Route::Wired(3)).unwrap();
        let p = Place { hart: 0, id };
        ids.opened(p).unwrap();
        ids.end(p).unwrap();
        ids.retired(p).unwrap();
        seen.push(id);
    }
    // 2, 3, ..., 255, 2, 3, ...: one route re-created again and again walks the whole space
    for (k, id) in seen.iter().enumerate() {
        assert_eq!(u32::from(*id), 2 + (k as u32 % 254));
    }
    // a Retiring identity is skipped, and comes back only once retired
    let a = ids.alloc(0, Route::Wired(1)).unwrap();
    ids.opened(Place { hart: 0, id: a }).unwrap();
    ids.end(Place { hart: 0, id: a }).unwrap();
    for _ in 0..300 {
        let b = ids.alloc(0, Route::Wired(2)).unwrap();
        assert_ne!(a, b, "a Retiring identity was handed out");
        let p = Place { hart: 0, id: b };
        ids.opened(p).unwrap();
        ids.end(p).unwrap();
        ids.retired(p).unwrap();
    }
    assert_eq!(
        ids.retired(Place { hart: 0, id: a }),
        Some((Route::Wired(1), false))
    );
}

#[test]
fn a_move_keeps_the_old_identity_until_the_new_one_is_opened_and_retired_after() {
    let mut ids = MsiIds::new(2, 63).unwrap();
    let a = Place {
        hart: 0,
        id: ids.alloc(0, Route::Wired(7)).unwrap(),
    };
    ids.opened(a).unwrap();
    let b = Place {
        hart: 1,
        id: ids.move_to(a, 1).unwrap(),
    };
    assert_eq!(
        ids.slot(a),
        Some(Slot::Live {
            route: Route::Wired(7),
            to: Some(b)
        })
    );
    assert_eq!(ids.move_to(a, 1), None, "one move at a time");
    assert_eq!(ids.routes_on(0), 1);
    assert_eq!(ids.routes_on(1), 1);
    let o = ids.opened(b).unwrap();
    assert_eq!(
        o,
        Opened {
            route: Route::Wired(7),
            retiring: Some(a),
            held: None
        }
    );
    assert_eq!(
        ids.slot(a),
        Some(Slot::Retiring {
            route: Route::Wired(7),
            moved: true
        })
    );
    assert_eq!(ids.free_on(0), 61, "not free until retired");
    assert_eq!(ids.routes_on(0), 0);
    assert_eq!(ids.retired(a), Some((Route::Wired(7), true)));
    assert_eq!(ids.free_on(0), 62);
    // ending a route that is moving cancels the move; ending the move only leaves the route where it was
    let c = Place {
        hart: 0,
        id: ids.move_to(b, 0).unwrap(),
    };
    assert_eq!(ids.end(c), Ok(None));
    assert_eq!(
        ids.slot(b),
        Some(Slot::Live {
            route: Route::Wired(7),
            to: None
        })
    );
    let d = Place {
        hart: 0,
        id: ids.move_to(b, 0).unwrap(),
    };
    assert_eq!(ids.end(b), Ok(Some(d)));
    assert_eq!(
        ids.slot(d),
        Some(Slot::Retiring {
            route: Route::Wired(7),
            moved: false
        })
    );
    assert_eq!(ids.opened(d), None, "a cancelled identity is never opened");
}

#[test]
fn exhaustion_spills_to_another_hart_or_fails_cleanly() {
    let mut ids = MsiIds::new(3, 7).unwrap();
    let all = |_| true;
    let mut placed = Vec::new();
    // the fewest routes first: round the harts
    for k in 0..18u32 {
        let p = ids.alloc_any(Route::Vector(k), all).unwrap();
        placed.push(p);
    }
    assert_eq!(placed.iter().filter(|p| p.hart == 0).count(), 6);
    assert_eq!(
        ids.alloc_any(Route::Vector(99), all),
        None,
        "every hart is full"
    );
    assert_eq!(ids.alloc(1, Route::Wired(1)), None);
    // only hart 2 usable, and it is full: none; free one there and it is used
    assert_eq!(ids.alloc_any(Route::Vector(99), |h| h == 2), None);
    let on2 = *placed.iter().find(|p| p.hart == 2).unwrap();
    ids.end(on2).unwrap();
    assert_eq!(
        ids.alloc_any(Route::Vector(99), |h| h == 2),
        None,
        "Retiring is not free"
    );
    ids.retired(on2).unwrap();
    assert_eq!(
        ids.alloc_any(Route::Vector(99), |h| h == 2),
        Some(Place {
            hart: 2,
            id: on2.id
        })
    );
}

#[test]
fn an_evacuation_is_all_or_nothing() {
    let mut ids = MsiIds::new(3, 7).unwrap();
    // hart 0 holds 4 routes; harts 1 and 2 have 2 + 1 free: too few
    for k in 0..4 {
        let id = ids.alloc(0, Route::Wired(k)).unwrap();
        ids.opened(Place { hart: 0, id }).unwrap();
    }
    for k in 0..4 {
        let id = ids.alloc(1, Route::Kernel(k)).unwrap();
        ids.opened(Place { hart: 1, id }).unwrap();
    }
    for k in 0..5 {
        let id = ids.alloc(2, Route::Vector(k)).unwrap();
        ids.opened(Place { hart: 2, id }).unwrap();
    }
    let before = ids.clone();
    let mut changes = Vec::new();
    assert_eq!(ids.evacuate(0, |_| true, |c| changes.push(c)), Err(()));
    assert!(changes.is_empty());
    for hart in 0..3 {
        for id in 0..=7 {
            let p = Place { hart, id };
            assert_eq!(ids.slot(p), before.slot(p));
        }
    }
    // free one more: now it fits; every route of hart 0 is moving, none onto hart 0
    let p = Place { hart: 2, id: 2 };
    ids.end(p).unwrap();
    ids.retired(p).unwrap();
    assert_eq!(ids.evacuate(0, |_| true, |c| changes.push(c)), Ok(()));
    assert_eq!(changes.len(), 4);
    for c in &changes {
        let Evacuated::Moved { from, to, .. } = *c else {
            panic!()
        };
        assert_eq!(from.hart, 0);
        assert_ne!(to.hart, 0);
        assert!(matches!(ids.slot(to), Some(Slot::Opening { from: Some(f), .. }) if f == from));
    }
    // an evacuation with nothing to move succeeds; a second one finds every route already leaving
    changes.clear();
    assert_eq!(ids.evacuate(0, |_| true, |c| changes.push(c)), Ok(()));
    assert!(changes.is_empty());
}

#[test]
fn the_id_set_holds_every_identity_up_to_the_maximum() {
    let mut s = IdSet::new();
    assert!(s.is_empty());
    for id in [1u32, 2, 63, 64, 127, 128, 255, 256, 2047] {
        s.insert(id);
    }
    s.insert(2048); // not an identity
    assert!(s.contains(64) && s.contains(2047) && !s.contains(65) && !s.contains(2048));
    assert_eq!(
        s.iter().collect::<Vec<_>>(),
        vec![1, 2, 63, 64, 127, 128, 255, 256, 2047]
    );
}
