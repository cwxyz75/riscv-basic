//! The FP ownership protocol (slice A) against a reference model: every thread keeps exactly its own
//! FP values across traps, preemption, other threads using the unit, and migration between harts; a
//! thread that never enables FP never sees anything; a freshly enabled thread starts from zeros.
//! Written before the kernel code (acceptance first).

use kcore::fp::{FpHart, FpRegs, FpThread, FpUnit, Fs, Tokens};

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

/// A hart's register file, with a counter of how many loads and saves it did.
struct Unit {
    regs: FpRegs,
    loads: u32,
    saves: u32,
}
impl Unit {
    fn new(rng: &mut Rng) -> Unit {
        // start with garbage, as a real unit does: another thread's leftovers
        let mut f = [0u64; 32];
        for x in f.iter_mut() {
            *x = rng.next();
        }
        Unit {
            regs: FpRegs {
                f,
                fcsr: rng.next() as u32 & 0xff,
            },
            loads: 0,
            saves: 0,
        }
    }
}
impl FpUnit for Unit {
    fn save(&mut self) -> FpRegs {
        self.saves += 1;
        self.regs.clone()
    }
    fn load(&mut self, r: &FpRegs) {
        self.loads += 1;
        self.regs = r.clone();
    }
}

fn run_model(seed: u64, harts: usize, threads: usize, steps: usize) -> (u32, u32) {
    let mut rng = Rng(seed);
    let mut tokens = Tokens::new();
    let mut units: Vec<Unit> = (0..harts).map(|_| Unit::new(&mut rng)).collect();
    let mut hs: Vec<FpHart> = (0..harts).map(|_| FpHart::new()).collect();
    let mut ts: Vec<FpThread> = (0..threads).map(|_| FpThread::new()).collect();
    // the reference: what each enabled thread's registers must be
    let mut want: Vec<Option<FpRegs>> = vec![None; threads];
    for _ in 0..steps {
        let t = rng.below(threads as u64) as usize;
        let h = rng.below(harts as u64) as usize;
        // a thread runs on a hart: return to user mode ...
        let fs = hs[h].return_to_user(&ts[t], &mut units[h]);
        match &want[t] {
            None => assert_eq!(
                fs,
                Fs::Off,
                "a thread with no FP state returns with the unit off"
            ),
            Some(w) => {
                assert_eq!(fs, Fs::Clean);
                assert_eq!(
                    &units[h].regs, w,
                    "thread {t} on hart {h} sees its own registers"
                );
            }
        }
        let mut dirty = false;
        // ... first use: an illegal-instruction trap enables it (from zeros), then it retries
        if want[t].is_none() && rng.below(3) == 0 {
            assert!(ts[t].enable(&mut tokens));
            assert!(!ts[t].enable(&mut tokens), "a second enable is refused");
            want[t] = Some(FpRegs::ZERO);
            let fs = hs[h].return_to_user(&ts[t], &mut units[h]);
            assert_eq!(fs, Fs::Clean);
            assert_eq!(
                units[h].regs,
                FpRegs::ZERO,
                "a fresh FP thread starts from zeros"
            );
        }
        // user code: write some registers (marks the unit dirty)
        if let Some(w) = want[t].as_mut() {
            if rng.below(2) == 0 {
                for _ in 0..1 + rng.below(4) {
                    let i = rng.below(32) as usize;
                    let v = rng.next();
                    units[h].regs.f[i] = v;
                    w.f[i] = v;
                }
                if rng.below(2) == 0 {
                    let c = rng.next() as u32 & 0xff;
                    units[h].regs.fcsr = c;
                    w.fcsr = c;
                }
                dirty = true;
            }
        }
        // the trap back into the kernel
        assert!(hs[h].trap_entry(&mut ts[t], dirty, &mut tokens, &mut units[h]));
        if let Some(w) = &want[t] {
            assert_eq!(
                ts[t].regs().unwrap(),
                w,
                "the saved state is the thread's state"
            );
        }
    }
    (
        units.iter().map(|u| u.loads).sum(),
        units.iter().map(|u| u.saves).sum(),
    )
}

#[test]
fn threads_keep_their_own_fp_state_across_traps_migration_and_sharing() {
    for seed in 1..=60 {
        run_model(seed * 7919, 1, 3, 400);
        run_model(seed * 104729, 4, 6, 600);
    }
}

#[test]
fn a_hart_that_already_holds_the_state_does_not_reload_it() {
    let mut rng = Rng(5);
    let mut tokens = Tokens::new();
    let mut unit = Unit::new(&mut rng);
    let mut hart = FpHart::new();
    let mut t = FpThread::new();
    t.enable(&mut tokens);
    assert_eq!(hart.return_to_user(&t, &mut unit), Fs::Clean);
    assert_eq!(unit.loads, 1);
    // a syscall and back with no FP use: nothing to save, nothing to load
    assert!(hart.trap_entry(&mut t, false, &mut tokens, &mut unit));
    assert_eq!(hart.return_to_user(&t, &mut unit), Fs::Clean);
    assert_eq!((unit.loads, unit.saves), (1, 0));
    // after it dirtied the registers: one save, and still no reload
    unit.regs.f[3] = 42;
    assert!(hart.trap_entry(&mut t, true, &mut tokens, &mut unit));
    assert_eq!(hart.return_to_user(&t, &mut unit), Fs::Clean);
    assert_eq!((unit.loads, unit.saves), (1, 1));
    assert_eq!(t.regs().unwrap().f[3], 42);
}

#[test]
fn dirty_state_for_a_thread_that_never_enabled_fp_is_a_bug_and_is_reported() {
    let mut rng = Rng(9);
    let mut tokens = Tokens::new();
    let mut unit = Unit::new(&mut rng);
    let mut hart = FpHart::new();
    let mut t = FpThread::new();
    assert!(!hart.trap_entry(&mut t, true, &mut tokens, &mut unit));
}

#[test]
fn a_stale_hart_reloads_after_the_thread_ran_elsewhere() {
    let mut rng = Rng(11);
    let mut tokens = Tokens::new();
    let mut a = Unit::new(&mut rng);
    let mut b = Unit::new(&mut rng);
    let (mut ha, mut hb) = (FpHart::new(), FpHart::new());
    let mut t = FpThread::new();
    t.enable(&mut tokens);
    ha.return_to_user(&t, &mut a);
    a.regs.f[0] = 1;
    ha.trap_entry(&mut t, true, &mut tokens, &mut a);
    // the thread moves to hart B and changes its state there
    hb.return_to_user(&t, &mut b);
    b.regs.f[0] = 2;
    hb.trap_entry(&mut t, true, &mut tokens, &mut b);
    // back on hart A, whose registers still hold the old value: it must reload
    ha.return_to_user(&t, &mut a);
    assert_eq!(a.regs.f[0], 2);
}
