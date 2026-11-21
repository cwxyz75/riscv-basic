//! The vector twin of `fp_model.rs` (PL2): the same ownership protocol (`kcore::fp`, one model with two instances) against a reference
//! model, for a unit whose registers are `vlenb` bytes long: every thread keeps exactly its own vector registers, `vstart`, `vcsr`, `vl`
//! and `vtype` across traps, preemption, other threads using the unit and migration between harts; a thread that never enables the unit
//! never sees anything; a freshly enabled thread starts from zeros. Also the decoder that tells a vector instruction from an FP one.
//! Written before the kernel code (acceptance first); mutation-checked by skipping the save.

use kcore::fp::{is_vector_insn, FpHart, Fs, StateUnit, Tokens, VecRegs, VecThread};

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

/// A hart's vector register file with counters.
struct Unit {
    regs: VecRegs,
    loads: u32,
    saves: u32,
}
impl Unit {
    fn new(rng: &mut Rng, vlenb: usize) -> Unit {
        // start with another thread's leftovers
        let mut regs = VecRegs::zero(vlenb);
        for b in regs.v.iter_mut() {
            *b = rng.next() as u8;
        }
        regs.vstart = rng.below(4);
        regs.vcsr = rng.below(8);
        regs.vl = rng.below(100);
        regs.vtype = rng.below(64);
        Unit {
            regs,
            loads: 0,
            saves: 0,
        }
    }
}
impl StateUnit for Unit {
    type Regs = VecRegs;
    fn save(&mut self) -> VecRegs {
        self.saves += 1;
        self.regs.clone()
    }
    fn load(&mut self, r: &VecRegs) {
        self.loads += 1;
        self.regs = r.clone();
    }
}

fn run_model(seed: u64, harts: usize, threads: usize, steps: usize, vlenb: usize) -> (u32, u32) {
    let mut rng = Rng(seed);
    let mut tokens = Tokens::new();
    let mut units: Vec<Unit> = (0..harts).map(|_| Unit::new(&mut rng, vlenb)).collect();
    let mut hs: Vec<FpHart> = (0..harts).map(|_| FpHart::new()).collect();
    let mut ts: Vec<VecThread> = (0..threads).map(|_| VecThread::new()).collect();
    let mut want: Vec<Option<VecRegs>> = vec![None; threads];
    for _ in 0..steps {
        let t = rng.below(threads as u64) as usize;
        let h = rng.below(harts as u64) as usize;
        let vs = hs[h].return_to_user(&ts[t], &mut units[h]);
        match &want[t] {
            None => assert_eq!(
                vs,
                Fs::Off,
                "a thread with no vector state returns with the unit off"
            ),
            Some(w) => {
                assert_eq!(vs, Fs::Clean);
                assert_eq!(
                    &units[h].regs, w,
                    "thread {t} on hart {h} sees its own registers"
                );
            }
        }
        let mut dirty = false;
        if want[t].is_none() && rng.below(3) == 0 {
            assert!(ts[t].enable_with(&mut tokens, VecRegs::zero(vlenb)));
            assert!(
                !ts[t].enable_with(&mut tokens, VecRegs::zero(vlenb)),
                "a second enable is refused"
            );
            want[t] = Some(VecRegs::zero(vlenb));
            let vs = hs[h].return_to_user(&ts[t], &mut units[h]);
            assert_eq!(vs, Fs::Clean);
            assert_eq!(
                units[h].regs,
                VecRegs::zero(vlenb),
                "a fresh vector thread starts from zeros"
            );
        }
        if let Some(w) = want[t].as_mut() {
            if rng.below(2) == 0 {
                for _ in 0..1 + rng.below(6) {
                    let i = rng.below(32 * vlenb as u64) as usize;
                    let v = rng.next() as u8;
                    units[h].regs.v[i] = v;
                    w.v[i] = v;
                }
                if rng.below(2) == 0 {
                    // a vsetvl and a vector CSR write
                    let (vl, vtype, vcsr, vstart) =
                        (rng.below(64), rng.below(256), rng.below(8), rng.below(4));
                    units[h].regs.vl = vl;
                    units[h].regs.vtype = vtype;
                    units[h].regs.vcsr = vcsr;
                    units[h].regs.vstart = vstart;
                    w.vl = vl;
                    w.vtype = vtype;
                    w.vcsr = vcsr;
                    w.vstart = vstart;
                }
                dirty = true;
            }
        }
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
fn threads_keep_their_own_vector_state_across_traps_migration_and_sharing() {
    for vlenb in [16usize, 32] {
        for seed in 1..=60 {
            run_model(seed * 7919, 1, 3, 400, vlenb);
            run_model(seed * 104729, 4, 6, 600, vlenb);
        }
    }
}

#[test]
fn a_hart_that_already_holds_the_vector_state_does_not_reload_it() {
    let mut rng = Rng(5);
    let mut tokens = Tokens::new();
    let mut unit = Unit::new(&mut rng, 16);
    let mut hart = FpHart::new();
    let mut t = VecThread::new();
    t.enable_with(&mut tokens, VecRegs::zero(16));
    assert_eq!(hart.return_to_user(&t, &mut unit), Fs::Clean);
    assert_eq!(unit.loads, 1);
    assert!(hart.trap_entry(&mut t, false, &mut tokens, &mut unit));
    assert_eq!(hart.return_to_user(&t, &mut unit), Fs::Clean);
    assert_eq!((unit.loads, unit.saves), (1, 0));
    unit.regs.v[17] = 42;
    assert!(hart.trap_entry(&mut t, true, &mut tokens, &mut unit));
    assert_eq!(hart.return_to_user(&t, &mut unit), Fs::Clean);
    assert_eq!((unit.loads, unit.saves), (1, 1));
    assert_eq!(t.regs().unwrap().v[17], 42);
}

#[test]
fn dirty_vector_state_for_a_thread_that_never_enabled_it_is_a_bug_and_is_reported() {
    let mut rng = Rng(9);
    let mut tokens = Tokens::new();
    let mut unit = Unit::new(&mut rng, 16);
    let mut t = VecThread::new();
    assert!(!FpHart::new().trap_entry(&mut t, true, &mut tokens, &mut unit));
}

#[test]
fn vector_instructions_are_told_from_fp_ones_by_their_encoding() {
    // OP-V: vsetvli t0, zero, e8, m1, ta, ma (the suite's probe), vadd.vv v1, v2, v3, vsetvl
    for insn in [0x0c00_72d7u64, 0x0221_80d7, 0x80c5_72d7] {
        assert!(is_vector_insn(insn), "{insn:#x}");
    }
    // vle8.v v1, (a0): LOAD-FP with width 0; vle64.v: width 7; vse32.v: STORE-FP with width 6
    for insn in [0x0205_0087u64, 0x0205_7087, 0x0205_6027] {
        assert!(is_vector_insn(insn), "{insn:#x}");
    }
    // csrr t0, vlenb (the first thing a program may do); csrr t0, vl; csrw vcsr, a0
    for insn in [0xc220_22f3u64, 0xc200_22f3, 0x00f5_1073] {
        assert!(is_vector_insn(insn), "{insn:#x}");
    }
    // csrr t0, sstatus is not (it is the genuinely illegal one), nor is ecall
    for insn in [0x1000_22f3u64, 0x73] {
        assert!(!is_vector_insn(insn), "{insn:#x}");
    }
    // flw, fld, fsw, fsd (widths 2 and 3), fadd.d (OP-FP, 0x53), a compressed c.fld, no instruction at all, a plain addi
    for insn in [
        0x0005_2087u64,
        0x0005_3087,
        0x0015_2027,
        0x0015_3027,
        0x0221_f0d3,
        0x2000,
        0,
        0x0015_0513,
    ] {
        assert!(!is_vector_insn(insn), "{insn:#x}");
    }
}
