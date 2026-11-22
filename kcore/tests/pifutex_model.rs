//! The PI futex against an independent reference (L23, PF1, PF2): random lock, unlock, give-up and owner-death steps over a few threads and
//! futexes; the reference keeps the owner and the queue of each futex as plain lists and **recomputes every effective priority from the
//! blocked-on edges** after each step, and the real structure (`PiFutexes` over `Donations`) must agree.
mod common;
use common::{Rng, SEEDS, STEPS};
use kcore::donate::Donations;
use kcore::pifutex::{
    fits, Lock, Outcome, PiError, PiFutexes, Unlock, Word, OWNER_DIED, OWNER_MASK,
};
use std::cell::Cell;
use std::collections::BTreeMap;

struct W(Cell<u32>);
impl Word for W {
    fn load(&self) -> u32 {
        self.0.get()
    }
    fn cas(&self, old: u32, new: u32) -> Result<u32, u32> {
        if self.0.get() == old {
            self.0.set(new);
            Ok(old)
        } else {
            Err(self.0.get())
        }
    }
}

const THREADS: u64 = 6;
const KEYS: u64 = 3;

#[derive(Default)]
struct Reference {
    base: BTreeMap<u64, u8>,
    /// per key: the owner and the queue, best first
    owner: BTreeMap<u64, u64>,
    queue: BTreeMap<u64, Vec<u64>>,
    dead: BTreeMap<u64, bool>,
}

impl Reference {
    /// The thread `t` is blocked on, if it is queued somewhere.
    fn blocked_on(&self, t: u64) -> Option<u64> {
        self.queue
            .iter()
            .find(|(_, q)| q.contains(&t))
            .map(|(k, _)| *k)
    }
    /// The effective priority of every thread, from the edges alone: the larger of the base and the effective priorities of those blocked on
    /// a futex it owns (the fixpoint of the definition).
    fn effs(&self) -> BTreeMap<u64, u8> {
        let mut eff = self.base.clone();
        loop {
            let mut changed = false;
            for (k, q) in &self.queue {
                let Some(o) = self.owner.get(k) else { continue };
                for t in q {
                    let e = eff[t];
                    if e > eff[o] {
                        eff.insert(*o, e);
                        changed = true;
                    }
                }
            }
            if !changed {
                return eff;
            }
        }
    }
    /// Would `t` waiting on `k` close a cycle (the owner is, through its own waits, waiting for `t`)?
    fn cycle(&self, t: u64, k: u64) -> bool {
        let mut at = self.owner[&k];
        for _ in 0..=THREADS {
            if at == t {
                return true;
            }
            match self.blocked_on(at) {
                Some(k2) => at = self.owner[&k2],
                None => return false,
            }
        }
        true
    }
}

fn apply(d: &Donations, r: &Reference, ctx: &str) {
    d.audit()
        .unwrap_or_else(|e| panic!("{ctx}: donations: {e}"));
    let want = r.effs();
    for t in 1..=THREADS {
        assert_eq!(d.eff(t), Some(want[&t]), "{ctx}: thread {t}");
    }
}

#[test]
fn random_pi_futex_steps_agree_with_the_recompute_everything_reference() {
    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed);
        let mut pi = PiFutexes::new();
        let mut d = Donations::new();
        let mut r = Reference::default();
        let words: Vec<W> = (0..KEYS).map(|_| W(Cell::new(0))).collect();
        for t in 1..=THREADS {
            let b = rng.below(40) as u8;
            d.add_thread(t, b);
            r.base.insert(t, b);
        }
        for step in 0..STEPS {
            let ctx = format!("seed {seed} step {step}");
            let t = 1 + rng.below(THREADS);
            let k = rng.below(KEYS);
            let key = (1, 0x1000 + 4 * k);
            let blocked = r.blocked_on(t).is_some();
            match rng.below(6) {
                // a runnable thread locks
                0 | 1 if !blocked => {
                    let res = pi.lock(&mut d, &words[k as usize], key, t, |_| true);
                    let owner = r.owner.get(&k).copied();
                    if r.dead.get(&k).copied().unwrap_or(false) {
                        assert_eq!(res, Err(PiError::BadState), "{ctx}: a poisoned lock");
                    } else {
                        match owner {
                            None => {
                                assert_eq!(res, Ok(Lock::Acquired), "{ctx}");
                                r.owner.insert(k, t);
                            }
                            Some(o) if o == t => assert_eq!(res, Err(PiError::BadState), "{ctx}"),
                            Some(_) if r.cycle(t, k) => {
                                assert_eq!(
                                    res,
                                    Err(PiError::BadState),
                                    "{ctx}: a cycle is refused"
                                );
                            }
                            Some(_) => {
                                assert!(matches!(res, Ok(Lock::Blocked { .. })), "{ctx}: {res:?}");
                                // queue position: after every waiter of at least its effective priority (FIFO among equals)
                                let eff = r.effs();
                                let q = r.queue.entry(k).or_default();
                                let at = q.iter().position(|w| eff[w] < eff[&t]).unwrap_or(q.len());
                                q.insert(at, t);
                            }
                        }
                    }
                }
                // the owner unlocks
                2 | 3 if r.owner.get(&k) == Some(&t) && !blocked => {
                    let res = pi.unlock(&mut d, &words[k as usize], key, t);
                    let q = r.queue.entry(k).or_default();
                    if q.is_empty() {
                        assert_eq!(res, Ok(Unlock::Free), "{ctx}");
                        r.owner.remove(&k);
                        assert_eq!(words[k as usize].0.get() & OWNER_MASK, 0, "{ctx}");
                    } else {
                        let first = q.remove(0);
                        match res {
                            Ok(Unlock::Handed { to, .. }) => {
                                assert_eq!(to, first, "{ctx}: PF2, the head gets it");
                                assert_eq!(pi.take_outcome(first), Some(Outcome::Granted), "{ctx}");
                            }
                            other => panic!("{ctx}: {other:?}"),
                        }
                        r.owner.insert(k, first);
                        assert_eq!(
                            words[k as usize].0.get() & OWNER_MASK,
                            first as u32,
                            "{ctx}: the word names the new owner"
                        );
                    }
                    if r.queue.get(&k).is_some_and(|q| q.is_empty()) {
                        r.queue.remove(&k);
                    }
                }
                // a non-owner unlocks: refused
                2 if r.owner.get(&k) != Some(&t) && !blocked => {
                    let res = pi.unlock(&mut d, &words[k as usize], key, t);
                    assert_eq!(res, Err(PiError::NotOwner), "{ctx}");
                }
                // a waiter gives up
                4 if blocked => {
                    let kk = r.blocked_on(t).unwrap();
                    let key = (1, 0x1000 + 4 * kk);
                    let _ = pi.cancel(&mut d, t);
                    let q = r.queue.get_mut(&kk).unwrap();
                    q.retain(|&w| w != t);
                    if q.is_empty() {
                        r.queue.remove(&kk);
                    }
                    assert!(!pi.still_waiting(t), "{ctx} {key:?}");
                }
                // an owner dies (it is not blocked: dead threads do not wait)
                5 if !blocked && r.owner.values().any(|&o| o == t) => {
                    let (woken, _) = pi.owner_died(&mut d, t);
                    let mut want: Vec<u64> = Vec::new();
                    for (kk, o) in r.owner.clone() {
                        if o != t {
                            continue;
                        }
                        if let Some(q) = r.queue.remove(&kk) {
                            want.extend(q);
                            // the word is poisoned: that futex is dead until a test resets it
                            PiFutexes::poison(&words[kk as usize]);
                            assert!(words[kk as usize].0.get() & OWNER_DIED != 0, "{ctx}");
                            r.dead.insert(kk, true);
                            r.owner.remove(&kk);
                        } else {
                            // no waiters: no record, the dead owner's word stays (the next locker finds the owner dead): model it as free
                            words[kk as usize].0.set(0);
                            r.owner.remove(&kk);
                        }
                    }
                    let mut got: Vec<u64> = woken.into_iter().flat_map(|(_, w)| w).collect();
                    got.sort();
                    want.sort();
                    assert_eq!(
                        got, want,
                        "{ctx}: exactly the waiters of its futexes are woken"
                    );
                    for t in &got {
                        assert_eq!(pi.take_outcome(*t), Some(Outcome::Died), "{ctx}");
                    }
                    // the dead thread leaves the graph and comes back as a new, idle one (ids are not reused in the kernel; here the model does)
                    let _ = d.remove_thread(t);
                    d.add_thread(t, r.base[&t]);
                    // a poisoned futex becomes usable again when the test says so
                    for kk in 0..KEYS {
                        if r.dead.get(&kk).copied().unwrap_or(false) && rng.below(2) == 0 {
                            words[kk as usize].0.set(0);
                            r.dead.remove(&kk);
                        }
                    }
                }
                _ => {}
            }
            apply(&d, &r, &ctx);
            pi.audit(&d).unwrap_or_else(|e| panic!("{ctx}: {e}"));
            let queued: usize = r.queue.values().map(|q| q.len()).sum();
            assert_eq!(pi.len(), queued, "{ctx}");
        }
    }
}

#[test]
fn a_thread_id_that_does_not_fit_cannot_use_a_pi_futex() {
    let mut d = Donations::new();
    let mut pi = PiFutexes::new();
    let w = W(Cell::new(0));
    let big = u64::from(OWNER_MASK) + 1;
    d.add_thread(big, 5);
    assert!(!fits(big) && !fits(0) && fits(1));
    assert_eq!(
        pi.lock(&mut d, &w, (1, 0), big, |_| true),
        Err(PiError::NoId)
    );
    assert_eq!(pi.unlock(&mut d, &w, (1, 0), big), Err(PiError::NoId));
}

#[test]
fn the_classic_inversion_is_bounded() {
    // low owns it, high waits: low runs at high's priority until it unlocks; a mid-priority thread never outranks it meanwhile
    let mut d = Donations::new();
    let mut pi = PiFutexes::new();
    let w = W(Cell::new(0));
    d.add_thread(1, 5); // low
    d.add_thread(2, 20); // mid
    d.add_thread(3, 40); // high
    let key = (1, 0x1000);
    assert_eq!(pi.lock(&mut d, &w, key, 1, |_| true), Ok(Lock::Acquired));
    assert!(matches!(
        pi.lock(&mut d, &w, key, 3, |_| true),
        Ok(Lock::Blocked { .. })
    ));
    assert_eq!(
        d.eff(1),
        Some(40),
        "the owner runs at the waiter's priority"
    );
    assert!(
        d.eff(1) > d.eff(2),
        "so the mid-priority thread cannot hold it off"
    );
    assert!(matches!(
        pi.unlock(&mut d, &w, key, 1),
        Ok(Unlock::Handed { to: 3, .. })
    ));
    assert_eq!(d.eff(1), Some(5), "the boost is gone with the hand-over");
}

#[test]
fn a_deadlock_is_refused_and_leaves_nothing_queued() {
    let mut d = Donations::new();
    let mut pi = PiFutexes::new();
    let (wa, wb) = (W(Cell::new(0)), W(Cell::new(0)));
    d.add_thread(1, 5);
    d.add_thread(2, 6);
    let (ka, kb) = ((1, 0x1000), (1, 0x2000));
    assert_eq!(pi.lock(&mut d, &wa, ka, 1, |_| true), Ok(Lock::Acquired));
    assert_eq!(pi.lock(&mut d, &wb, kb, 2, |_| true), Ok(Lock::Acquired));
    assert!(matches!(
        pi.lock(&mut d, &wb, kb, 1, |_| true),
        Ok(Lock::Blocked { .. })
    ));
    // 2 asking for 1's lock would close the cycle: BAD_STATE, no sleep
    assert_eq!(
        pi.lock(&mut d, &wa, ka, 2, |_| true),
        Err(PiError::BadState)
    );
    assert!(!pi.still_waiting(2));
    d.audit().unwrap();
    pi.audit(&d).unwrap();
}

#[test]
fn a_dead_owner_that_no_one_handed_the_lock_on_poisons_it() {
    let mut d = Donations::new();
    let mut pi = PiFutexes::new();
    let w = W(Cell::new(0));
    d.add_thread(1, 5);
    d.add_thread(2, 6);
    w.0.set(1); // thread 1 locked it in user mode and exited without a kernel record
    assert_eq!(
        pi.lock(&mut d, &w, (1, 0), 2, |t| t != 1),
        Err(PiError::BadState)
    );
    assert!(w.0.get() & OWNER_DIED != 0, "the word says the owner died");
    assert_eq!(
        pi.lock(&mut d, &w, (1, 0), 2, |_| true),
        Err(PiError::BadState),
        "and stays poisoned"
    );
}
