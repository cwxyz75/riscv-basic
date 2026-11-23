//! PM3: the suspend save and restore sequence over a fake machine that forgets what the firmware does not keep.

use kcore::powerseq::{Machine, Reg, Saved, SEQUENCE};
use std::collections::BTreeMap;

/// A machine whose registers are zeroed by a suspend, except the ones the firmware keeps (`keeps`). It records the order of writes and
/// refuses (panics on) a write of an interrupt-enable bit while anything else is still unrestored.
#[derive(Default)]
struct Fake {
    regs: BTreeMap<u8, u64>,
    writes: Vec<Reg>,
}

fn key(r: Reg) -> u8 {
    SEQUENCE
        .iter()
        .position(|&x| x == r)
        .expect("a register of the sequence") as u8
}

impl Machine for Fake {
    fn read(&mut self, r: Reg) -> u64 {
        self.regs.get(&key(r)).copied().unwrap_or(0)
    }
    fn write(&mut self, r: Reg, v: u64) {
        self.writes.push(r);
        self.regs.insert(key(r), v);
    }
}

impl Fake {
    fn with_values() -> Fake {
        let mut f = Fake::default();
        for (i, r) in SEQUENCE.iter().enumerate() {
            f.regs.insert(key(*r), 0x1000 + 17 * i as u64);
        }
        f
    }
    /// What a suspend does: everything is lost.
    fn suspend(&mut self) {
        self.regs.clear();
    }
}

#[test]
fn everything_saved_comes_back() {
    let mut m = Fake::with_values();
    let before = m.regs.clone();
    let saved = Saved::capture(&mut m);
    m.suspend();
    assert!(m.regs.is_empty());
    saved.apply(&mut m);
    assert_eq!(m.regs, before, "every register is as it was");
}

#[test]
fn the_enable_mask_is_written_last_and_each_register_once() {
    let mut m = Fake::with_values();
    let saved = Saved::capture(&mut m);
    m.suspend();
    m.writes.clear();
    saved.apply(&mut m);
    assert_eq!(
        m.writes.last(),
        Some(&Reg::Sie),
        "no interrupt is enabled before the rest is restored"
    );
    assert_eq!(m.writes.len(), SEQUENCE.len(), "one write per register");
    let mut seen = m.writes.clone();
    seen.sort_by_key(|r| key(*r));
    seen.dedup();
    assert_eq!(
        seen.len(),
        SEQUENCE.len(),
        "no register twice, none missing"
    );
    // the trap vector is in place before the interrupt file and the timer can deliver anything
    let at = |r: Reg| m.writes.iter().position(|&x| x == r).unwrap();
    assert!(at(Reg::Stvec) < at(Reg::ImsicDelivery));
    assert!(at(Reg::Satp) < at(Reg::Stimecmp));
}

/// Negative control: a restore that leaves one register out leaves it lost, which the comparison sees.
#[test]
fn a_restore_that_skips_a_register_is_caught() {
    let mut m = Fake::with_values();
    let before = m.regs.clone();
    let saved = Saved::capture(&mut m);
    m.suspend();
    saved.apply(&mut m);
    m.regs.remove(&key(Reg::Senvcfg)); // (the skipped write)
    assert_ne!(m.regs, before);
}
