//! T188: the claim protocol of the FP and vector units (`kcore::fp::claim`, `drop_claim`, `light_target`, `quiet_return`) against a reference: the lock-free
//! paths of the kernel (a light save, a quiet return) only ever do what the locked path would have done as a no-op or into the same buffer, a thread always
//! sees its own registers, a kept token is never stale, and a claim never outlives the state it names.
//!
//! Mutations checked by hand (TESTING.md rule 9), reverted after each; all but the second make this test fail with the named invariant.
//! 1. `claim` without `f.forget()` on the old hart: I2 (a kept token on a hart whose claim is gone) and I3.
//! 2. `light_target` without the `claim == tid` compare: **survives**. A thread's claim on its own hart cannot change between its return and its trap (only a
//!    load on that hart replaces it, and the hart runs this thread), so the compare is defence in depth for the lock-free reader, not something a walk reaches.
//! 3. `quiet_return` accepting `Initial`: I3 (the zero state is never loaded).
//! 4. `drop_claim` doing nothing (a thread that ended or was written keeps its cells): I1.
//! 5. `claim` without the `cells.claim(old) == tid` guard: I3 (it clears a claim that now belongs to another thread, which then reloads needlessly, and the
//!    count of zero-load scenarios fails).

use kcore::fp::{
    claim, drop_claim, light_target, quiet_return, ClaimCells, FpHart, FpRegs, FpThread, Fs, Lazy,
    StateUnit, Tokens,
};
use std::cell::RefCell;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

struct Cells(RefCell<Vec<(u64, usize)>>);
impl ClaimCells for Cells {
    fn claim(&self, h: usize) -> u64 {
        self.0.borrow()[h].0
    }
    fn buf(&self, h: usize) -> usize {
        self.0.borrow()[h].1
    }
    fn set(&self, h: usize, tid: u64, buf: usize) {
        self.0.borrow_mut()[h] = (tid, buf);
    }
    fn clear(&self, h: usize) {
        self.0.borrow_mut()[h] = (0, 0);
    }
}

struct Unit {
    regs: FpRegs,
    loads: u32,
}
impl StateUnit for Unit {
    type Regs = FpRegs;
    fn save(&mut self) -> FpRegs {
        self.regs.clone()
    }
    fn load(&mut self, r: &FpRegs) {
        self.loads += 1;
        self.regs = r.clone();
    }
}

fn garbage(rng: &mut Rng) -> FpRegs {
    let mut f = [0u64; 32];
    for x in f.iter_mut() {
        *x = rng.next();
    }
    FpRegs { f, fcsr: 0 }
}

fn run(seed: u64, nh: usize, nt: usize, steps: usize) {
    let mut rng = Rng(seed);
    let cells = Cells(RefCell::new(vec![(0, 0); nh]));
    let mut tokens = Tokens::new();
    let mut units: Vec<Unit> = (0..nh)
        .map(|_| Unit {
            regs: garbage(&mut rng),
            loads: 0,
        })
        .collect();
    let mut harts: Vec<FpHart> = (0..nh).map(|_| FpHart::new()).collect();
    let mut ts: Vec<FpThread> = (0..nt).map(|_| FpThread::new()).collect();
    let mut alive = vec![true; nt];
    let mut want: Vec<Option<FpRegs>> = vec![None; nt];
    let mut frame = vec![Lazy::Off; nt];
    // the thread running on each hart (none: the hart runs something that has no unit state)
    let mut cur: Vec<Option<usize>> = vec![None; nh];
    let tid = |t: usize| t as u64 + 1;
    let on_some_hart = |cur: &Vec<Option<usize>>, t: usize| cur.contains(&Some(t));

    for step in 0..steps {
        let h = rng.below(nh as u64) as usize;
        match rng.below(10) {
            // schedule a thread onto this hart (a migration when it last ran elsewhere): the old one has trapped before, so its frame is not Dirty
            0 | 1 => {
                let t = rng.below(nt as u64) as usize;
                if alive[t] && !on_some_hart(&cur, t) {
                    if let Some(old) = cur[h] {
                        // (it leaves through a trap, which saved what it dirtied)
                        trap(
                            &cells,
                            &mut harts,
                            &mut ts,
                            &mut units,
                            &mut frame,
                            &mut tokens,
                            h,
                            old,
                            tid(old),
                            rng.below(2) == 0,
                        );
                        assert_ne!(frame[old], Lazy::Dirty);
                    }
                    cur[h] = Some(t);
                    // the return to user mode
                    ret(
                        &cells,
                        &mut harts,
                        &mut ts,
                        &mut units,
                        &want,
                        &mut frame,
                        &mut tokens,
                        h,
                        t,
                        tid(t),
                    );
                }
            }
            // a trap and the return: a light save when the claim allows it, the locked save otherwise
            2..=5 => {
                if let Some(t) = cur[h] {
                    trap(
                        &cells,
                        &mut harts,
                        &mut ts,
                        &mut units,
                        &mut frame,
                        &mut tokens,
                        h,
                        t,
                        tid(t),
                        rng.below(2) == 0,
                    );
                    ret(
                        &cells,
                        &mut harts,
                        &mut ts,
                        &mut units,
                        &want,
                        &mut frame,
                        &mut tokens,
                        h,
                        t,
                        tid(t),
                    );
                }
            }
            // user code: first use enables the unit; or it writes registers
            6 | 7 => {
                if let Some(t) = cur[h] {
                    if want[t].is_none() {
                        assert!(ts[t].enable(&mut tokens));
                        want[t] = Some(FpRegs::ZERO);
                        frame[t] = Lazy::Initial;
                        ret(
                            &cells,
                            &mut harts,
                            &mut ts,
                            &mut units,
                            &want,
                            &mut frame,
                            &mut tokens,
                            h,
                            t,
                            tid(t),
                        );
                    } else {
                        let i = rng.below(32) as usize;
                        let v = rng.next();
                        units[h].regs.f[i] = v;
                        want[t].as_mut().unwrap().f[i] = v;
                        frame[t] = Lazy::Dirty;
                    }
                }
            }
            // a debugger writes a parked thread's registers
            8 => {
                let t = rng.below(nt as u64) as usize;
                if alive[t] && want[t].is_some() && !on_some_hart(&cur, t) {
                    drop_claim(&cells, &ts[t], &mut harts, tid(t), true);
                    let r = garbage(&mut rng);
                    ts[t].replace_regs(&mut tokens, r.clone());
                    want[t] = Some(r);
                    frame[t] = Lazy::Initial;
                }
            }
            // a thread ends
            _ => {
                let t = rng.below(nt as u64) as usize;
                if alive[t] && !on_some_hart(&cur, t) && rng.below(4) == 0 {
                    drop_claim(&cells, &ts[t], &mut harts, tid(t), false);
                    ts[t] = FpThread::new();
                    alive[t] = false;
                    want[t] = None;
                    frame[t] = Lazy::Off;
                }
            }
        }
        // the invariants, for every hart and thread
        #[allow(clippy::needless_range_loop)] // (the hart index is what the cells take)
        for hh in 0..nh {
            let c = cells.claim(hh);
            if c != 0 {
                let t = (c - 1) as usize;
                // I1: one claim per thread, on its holder, naming its box
                assert!(
                    alive[t] && ts[t].enabled(),
                    "seed {seed} step {step}: I1 claim {c} names no state"
                );
                assert_eq!(
                    ts[t].holder(),
                    Some(hh),
                    "seed {seed} step {step}: I1 holder"
                );
                assert_eq!(
                    cells.buf(hh),
                    ts[t].regs_ptr().unwrap() as usize,
                    "seed {seed} step {step}: I1 buffer is not the thread's box"
                );
                for h2 in 0..nh {
                    assert!(
                        h2 == hh || cells.claim(h2) != c,
                        "seed {seed} step {step}: I1 two claims"
                    );
                }
            } else {
                assert_eq!(cells.buf(hh), 0);
            }
            // I2: a kept token never goes stale
            for t in 0..nt {
                if alive[t] && harts[hh].holds(&ts[t]) {
                    assert_eq!(
                        cells.claim(hh),
                        tid(t),
                        "seed {seed} step {step}: I2 hart {hh} holds thread {t}'s token without its claim"
                    );
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn ret(
    cells: &Cells,
    harts: &mut [FpHart],
    ts: &mut [FpThread],
    units: &mut [Unit],
    want: &[Option<FpRegs>],
    frame: &mut [Lazy],
    _tokens: &mut Tokens,
    h: usize,
    t: usize,
    tid: u64,
) {
    if want[t].is_none() {
        assert!(quiet_return(cells, h, tid, Lazy::Off));
        return;
    }
    if !quiet_return(cells, h, tid, frame[t]) {
        let fs = harts[h].return_to_user(&ts[t], &mut units[h]);
        assert_eq!(fs, Fs::Clean);
        claim(cells, &mut ts[t], harts, tid, h);
        if frame[t] == Lazy::Initial {
            frame[t] = Lazy::Clean;
        }
    }
    // I3: whichever path, the unit holds the thread's registers when it returns
    assert_eq!(
        &units[h].regs,
        want[t].as_ref().unwrap(),
        "I3: thread {t} on hart {h} does not see its own registers (frame {:?})",
        frame[t]
    );
}

#[allow(clippy::too_many_arguments)]
fn trap(
    cells: &Cells,
    harts: &mut [FpHart],
    ts: &mut [FpThread],
    units: &mut [Unit],
    frame: &mut [Lazy],
    tokens: &mut Tokens,
    h: usize,
    t: usize,
    tid: u64,
    light: bool,
) {
    if frame[t] != Lazy::Dirty {
        return;
    }
    if light {
        if let Some(buf) = light_target(cells, h, tid) {
            // I4: the address is this thread's box
            assert_eq!(
                buf,
                ts[t].regs_ptr().unwrap() as usize,
                "I4: a light save into another thread's box"
            );
            let regs = units[h].regs.clone();
            // SAFETY: the address is `ts[t]`'s box, alive and not aliased in this single-threaded model.
            unsafe { *(buf as *mut FpRegs) = regs };
            frame[t] = Lazy::Clean;
            return;
        }
    }
    assert!(harts[h].trap_entry(&mut ts[t], true, tokens, &mut units[h]));
    claim(cells, &mut ts[t], harts, tid, h);
    frame[t] = Lazy::Clean;
}

#[test]
fn claims_keep_every_thread_on_its_own_registers_and_no_token_stale() {
    for seed in 1..=300u64 {
        run(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1, 1, 3, 400);
        run(seed.wrapping_mul(0xC2B2_AE3D_27D4_EB4F) | 1, 2, 4, 500);
        run(seed.wrapping_mul(0x1656_67B1_9E37_79F9) | 1, 4, 6, 600);
    }
}

#[test]
fn a_thread_alone_on_its_hart_loads_once_and_returns_without_the_lock() {
    let cells = Cells(RefCell::new(vec![(0, 0)]));
    let mut tokens = Tokens::new();
    let mut unit = Unit {
        regs: FpRegs::ZERO,
        loads: 0,
    };
    let mut harts = vec![FpHart::new()];
    let mut t = FpThread::new();
    t.enable(&mut tokens);
    // first return: Initial, the lock is owed, one load
    assert!(!quiet_return(&cells, 0, 1, Lazy::Initial));
    assert_eq!(harts[0].return_to_user(&t, &mut unit), Fs::Clean);
    claim(&cells, &mut t, &mut harts, 1, 0);
    // later returns: quiet, no load
    for frame in [Lazy::Clean, Lazy::Dirty] {
        assert!(quiet_return(&cells, 0, 1, frame));
    }
    assert_eq!(unit.loads, 1);
}

#[test]
fn another_threads_claim_is_not_cleared_by_a_stale_holder() {
    let cells = Cells(RefCell::new(vec![(0, 0); 2]));
    let mut tokens = Tokens::new();
    let mut u = [
        Unit {
            regs: FpRegs::ZERO,
            loads: 0,
        },
        Unit {
            regs: FpRegs::ZERO,
            loads: 0,
        },
    ];
    let mut harts = vec![FpHart::new(), FpHart::new()];
    let (mut a, mut b) = (FpThread::new(), FpThread::new());
    a.enable(&mut tokens);
    b.enable(&mut tokens);
    // A claims hart 0, then B takes hart 0 over (A's holder is stale), then A runs on hart 1
    harts[0].return_to_user(&a, &mut u[0]);
    claim(&cells, &mut a, &mut harts, 1, 0);
    harts[0].return_to_user(&b, &mut u[0]);
    claim(&cells, &mut b, &mut harts, 2, 0);
    harts[1].return_to_user(&a, &mut u[1]);
    claim(&cells, &mut a, &mut harts, 1, 1);
    assert_eq!(cells.claim(0), 2, "B keeps hart 0");
    assert_eq!(cells.claim(1), 1);
    drop_claim(&cells, &a, &mut harts, 1, false);
    assert_eq!(cells.claim(1), 0);
    assert_eq!(cells.claim(0), 2);
}
