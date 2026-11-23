//! The save and restore sequence of `system_suspend` (PM3, KERNEL-V1-SPEC 5.5), as a model over a machine with named registers.
//!
//! Before the firmware's suspend-to-RAM the kernel saves what the firmware does not keep (the supervisor CSRs and the interrupt controller's
//! per-hart enable state), and on resume writes it back in an order that never lets an interrupt in before the rest is in place: the trap
//! vector, scratch, the address-space and counter registers first, then the interrupt file, then the timer compare, and the enable mask last.
//! The kernel's CSR access is behind `Machine`; the host tests run the sequence over a fake that forgets everything it is not given back.

/// A register the sequence saves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reg {
    /// The thread pointer (the kernel keeps its per-hart record there).
    Tp,
    Sscratch,
    Stvec,
    Satp,
    Senvcfg,
    Scounteren,
    /// The IMSIC's `eidelivery`, `eithreshold` and the first `eie` word of this hart's supervisor file.
    ImsicDelivery,
    ImsicThreshold,
    ImsicEie0,
    /// The timer compare (Sstc).
    Stimecmp,
    /// The supervisor interrupt-enable mask: written last.
    Sie,
}

/// Every saved register, in the order of restoring.
pub const SEQUENCE: [Reg; 11] = [
    Reg::Tp,
    Reg::Sscratch,
    Reg::Stvec,
    Reg::Satp,
    Reg::Senvcfg,
    Reg::Scounteren,
    Reg::ImsicDelivery,
    Reg::ImsicThreshold,
    Reg::ImsicEie0,
    Reg::Stimecmp,
    Reg::Sie,
];

/// The machine's registers, as the sequence sees them.
pub trait Machine {
    fn read(&mut self, r: Reg) -> u64;
    fn write(&mut self, r: Reg, v: u64);
}

/// What was saved: one value per register of `SEQUENCE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    vals: [u64; SEQUENCE.len()],
}

impl Saved {
    /// Read every register of the sequence.
    pub fn capture(m: &mut impl Machine) -> Saved {
        let mut vals = [0; SEQUENCE.len()];
        for (slot, r) in vals.iter_mut().zip(SEQUENCE) {
            *slot = m.read(r);
        }
        Saved { vals }
    }

    /// Write them back, in the order of `SEQUENCE`: the enable mask last. Only registers of the sequence are written.
    pub fn apply(&self, m: &mut impl Machine) {
        for (v, r) in self.vals.iter().zip(SEQUENCE) {
            m.write(r, *v);
        }
    }
}
