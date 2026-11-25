//! The two walkers (IOMMU2; IOMMU-SPEC 13): the simulated IOMMU's page walk (written from the specification's translation
//! process, top down, in `sim`) against `iopt::translate` (written with the mapper), over random tables in every format,
//! including deliberately corrupt ones: a non-leaf with R set (a superpage, aligned or not), a pointer at the leaf level, the
//! high bits, W without R, X only, U, A or D clear, garbage. For every sampled address and both directions the two must agree
//! on fault or address. The simulator is reached the way a device reaches the IOMMU: through `ddtp`, the directory and a device
//! context, by a device read and by the debug translation interface (which also covers writes without writing memory).

mod common;

use common::{Rng, SEEDS};
use iommucore::ddt::{Ddt, DeviceContext, Format, Levels};
use iommucore::iopt::{Access, Iopt, Op, PtMode};
use iommucore::regs;
use iommucore::sim::{FakeFrames, FakeMem, Sim, SimConfig};
use iommucore::{PhysMem64, Regs};

const MODES: [PtMode; 6] = [
    PtMode::Sv39x4,
    PtMode::Sv48x4,
    PtMode::Sv57x4,
    PtMode::Sv39,
    PtMode::Sv48,
    PtMode::Sv57,
];
const DID: u32 = 0x08;

/// Every valid entry reachable from the root: (address of the entry, its level).
fn entries(mem: &FakeMem, t: &Iopt) -> Vec<(u64, u32)> {
    let mut out = Vec::new();
    let mut stack = vec![(t.root(), t.mode().levels() - 1)];
    while let Some((table, level)) = stack.pop() {
        let n = if level + 1 == t.mode().levels() {
            t.mode().root_bytes() / 8
        } else {
            512
        };
        for i in 0..n {
            let at = table + i * 8;
            let pte = mem.read(at);
            if pte & 1 == 0 {
                continue;
            }
            out.push((at, level));
            if pte & 0xe == 0 && level > 0 && pte >> 54 == 0 {
                stack.push((((pte >> 10) & ((1 << 44) - 1)) << 12, level - 1));
            }
        }
    }
    out
}

/// (The bits are literals, not the crate's constants: a wrong constant must not move both walkers' inputs with it.)
fn corrupt(rng: &mut Rng, mem: &FakeMem, at: u64, level: u32) {
    let pte = mem.read(at);
    let leaf = pte & 0xe != 0;
    let new = match rng.below(12) {
        0 => pte | 0x02, // a non-leaf becomes a (probably misaligned) superpage
        1 if level > 0 => {
            // A well-formed superpage leaf, aligned to its level.
            let ppn = (rng.below(1 << 20) << (9 * level)) & ((1 << 44) - 1);
            (ppn << 10) | 0xd7
        }
        2 if leaf => (pte & !0x3ff) | 0x01, // a pointer where a leaf was (at level 0: a fault)
        3 => pte | (1 << (54 + rng.below(10))), // a high bit
        4 if leaf => pte & !0x02 | 0x04,    // W without R
        5 if leaf => (pte & !0xe) | 0x08,   // execute only
        6 if leaf => pte & !0x10,
        7 if leaf => pte & !0x40,
        8 if leaf => pte & !0x80,
        9 => pte & !0x01,
        10 => rng.next() & !(0x3ffu64 << 54) | 1, // garbage, valid, no high bit
        _ => rng.next(),
    };
    mem.write(at, new);
}

/// The simulated IOMMU's answer for `op` at `iova`: by a device read (exact address) or the debug interface (page).
fn sim_translate(sim: &mut Sim<FakeMem>, iova: u64, op: Op) -> Result<u64, ()> {
    match op {
        Op::Read => sim.read(DID, iova).map_err(|_| ()),
        Op::Write => {
            sim.write64(regs::TR_REQ_IOVA, iova & !0xfff);
            sim.write64(regs::TR_REQ_CTL, regs::tr_req_ctl(DID, true));
            assert!(!regs::tr_busy(sim.read64(regs::TR_REQ_CTL)));
            regs::decode_tr_response(sim.read64(regs::TR_RESPONSE))
                .map(|page| page | (iova & 0xfff))
                .ok_or(())
        }
    }
}

#[test]
fn the_two_walkers_agree_on_random_and_corrupt_tables() {
    let mut compared = [0u64; 2]; // (agreed on an address, agreed on a fault)
    for seed in 0..SEEDS {
        let mode = MODES[(seed % 6) as usize];
        let mut rng = Rng::new(seed);
        let mem = FakeMem::new();
        let mut frames = FakeFrames::new(&mem, 0x1_0000_0000, 1 << 12);
        let mut t = Iopt::new(&mem, 0x8000_0000, mode).unwrap();
        let mut mapped = Vec::new();
        for _ in 0..40 {
            let raw = match rng.below(3) {
                0 => rng.below(32) << 12,
                1 => (rng.below(4) << 30) | (rng.below(4) << 21) | (rng.below(8) << 12),
                _ => rng.next() & ((1 << mode.iova_bits()) - 1) & !0xfff,
            };
            let bits = mode.iova_bits();
            let iova = if mode.second_stage() {
                raw
            } else {
                (((raw << (64 - bits)) as i64) >> (64 - bits)) as u64
            };
            let pa = 0x2_0000_0000 + (rng.below(1 << 16) << 12);
            let access = if rng.chance(50) {
                Access::Read
            } else {
                Access::ReadWrite
            };
            if t.map(&mem, &mut frames, iova, pa, access).is_ok() {
                mapped.push(iova);
            }
        }
        let mut d = Ddt::new(&mem, 0x8400_0000, Levels::One, Format::Extended).unwrap();
        d.set(
            &mem,
            &mut frames,
            DID,
            &DeviceContext::translate(t.context_root(5).unwrap(), None),
        )
        .unwrap();

        for round in 0..6 {
            if round > 0 {
                let all = entries(&mem, &t);
                for _ in 0..1 + rng.below(3) {
                    let (at, level) = all[rng.below(all.len() as u64) as usize];
                    corrupt(&mut rng, &mem, at, level);
                }
            }
            // A fresh simulated IOMMU each round: its translation cache must not hide the corruption.
            let mut sim = Sim::new(&mem, SimConfig::qemu());
            sim.write64(regs::DDTP, d.ddtp().bits());
            for _ in 0..40 {
                let base = if rng.chance(80) {
                    mapped[rng.below(mapped.len() as u64) as usize]
                } else {
                    rng.next() & !0xfff
                };
                let iova = base + (rng.below(4096) & !7);
                for op in [Op::Read, Op::Write] {
                    let mine = t.translate(&mem, iova, op).map_err(|_| ());
                    let theirs = sim_translate(&mut sim, iova, op);
                    assert_eq!(
                        mine, theirs,
                        "seed {seed} {mode:?} round {round} {op:?} at {iova:#x}"
                    );
                    compared[usize::from(mine.is_err())] += 1;
                }
            }
        }
    }
    // Both outcomes were exercised in quantity.
    assert!(compared[0] > 1000 && compared[1] > 1000, "{compared:?}");
}
