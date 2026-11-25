//! Every decoder and accessor, bit by bit, against the field positions of QEMU v11.1.3 `hw/riscv/riscv-iommu-bits.h` (IOMMU2;
//! the full mutants run found accessors no other test read). Each single-bit test sets one bit at a time over all 64 and checks
//! that exactly the named bit answers.

mod common;

use common::Rng;
use iommucore::cmdq::CmdQueue;
use iommucore::ddt::{Ddt, Format, Levels};
use iommucore::faultq::{FaultQueue, FaultRecord, TransactionType};
use iommucore::iopt::{self, Iopt, PtMode};
use iommucore::iova::Iova;
use iommucore::msi::{Mrif, MsiPte};
use iommucore::regs::{self, Caps, Cqcsr, Fqcsr, Igs};
use iommucore::sim::FakeMem;
use iommucore::PhysMem64;

/// (name, bit, accessor)
type Field<T> = (&'static str, u32, fn(T) -> bool);

fn only_bit(name: &str, bit: u32, f: impl Fn(u64) -> bool) {
    for n in 0..64 {
        assert_eq!(f(1 << n), n == bit, "{name}: bit {n}");
    }
}

#[test]
fn capability_bits() {
    // bits.h:77-98.
    let single: [Field<Caps>; 19] = [
        ("sv32", 8, Caps::sv32),
        ("sv39", 9, Caps::sv39),
        ("sv48", 10, Caps::sv48),
        ("sv57", 11, Caps::sv57),
        ("svrsw60t59b", 14, Caps::svrsw60t59b),
        ("sv32x4", 16, Caps::sv32x4),
        ("sv39x4", 17, Caps::sv39x4),
        ("sv48x4", 18, Caps::sv48x4),
        ("sv57x4", 19, Caps::sv57x4),
        ("msi_flat", 22, Caps::msi_flat),
        ("msi_mrif", 23, Caps::msi_mrif),
        ("amo_hwad", 24, Caps::amo_hwad),
        ("ats", 25, Caps::ats),
        ("t2gpa", 26, Caps::t2gpa),
        ("hpm", 30, Caps::hpm),
        ("dbg", 31, Caps::dbg),
        ("pd8", 38, Caps::pd8),
        ("pd17", 39, Caps::pd17),
        ("pd20", 40, Caps::pd20),
    ];
    for (name, bit, f) in single {
        only_bit(name, bit, |v| f(Caps(v)));
    }
    // IGS 29:28 (bits.h:92, 100-104), PAS 37:32, version 7:0.
    assert_eq!(Caps(0).igs(), Igs::Msi);
    assert_eq!(Caps(1 << 28).igs(), Igs::Wsi);
    assert_eq!(Caps(2 << 28).igs(), Igs::Both);
    assert_eq!(Caps(3 << 28).igs(), Igs::Reserved);
    assert_eq!(Caps(!(3 << 28)).igs(), Igs::Msi);
    assert_eq!(Caps(0x3f << 32).pas(), 63);
    assert_eq!(Caps(0x29 << 32 | 1 << 38 | 1 << 31).pas(), 41);
    assert_eq!(Caps(0x10).version(), (1, 0));
    assert_eq!(Caps(0xa7 | 1 << 8).version(), (10, 7));
}

#[test]
fn the_capability_check_takes_exactly_the_width_it_needs() {
    let caps = iommucore::sim::QEMU_CAPS; // PAS 56
    assert_eq!(Caps(caps).pas(), 56);
    assert!(Caps(caps).check(&regs::Required { pas_bits: 56 }).is_ok());
    assert!(Caps(caps).check(&regs::Required { pas_bits: 57 }).is_err());
    // WSI or both is required; MSI-only and reserved are refused.
    for (igs, ok) in [(0u64, false), (1, true), (2, true), (3, false)] {
        let c = (caps & !(3 << 28)) | (igs << 28);
        assert_eq!(
            Caps(c).check(&regs::Required { pas_bits: 40 }).is_ok(),
            ok,
            "igs {igs}"
        );
    }
}

#[test]
fn queue_csr_bits() {
    // bits.h:150-178: enable 0, interrupt enable 1, memory fault 8, cmd_to / fqof 9, cmd_ill 10, fence_w_ip 11, on 16, busy 17.
    let c: [Field<Cqcsr>; 6] = [
        ("cq enabled", 0, Cqcsr::enabled),
        ("cq on", 16, Cqcsr::on),
        ("cq busy", 17, Cqcsr::busy),
        ("cq mem_fault", 8, Cqcsr::mem_fault),
        ("cq timeout", 9, Cqcsr::timeout),
        ("cq illegal", 10, Cqcsr::illegal),
    ];
    for (name, bit, f) in c {
        only_bit(name, bit, |v| f(Cqcsr(v as u32)));
    }
    for bit in 0..32 {
        assert_eq!(
            Cqcsr(1 << bit).failed(),
            matches!(bit, 8..=10),
            "cq failed: bit {bit}"
        );
    }
    let f: [Field<Fqcsr>; 5] = [
        ("fq enabled", 0, Fqcsr::enabled),
        ("fq on", 16, Fqcsr::on),
        ("fq busy", 17, Fqcsr::busy),
        ("fq mem_fault", 8, Fqcsr::mem_fault),
        ("fq overflow", 9, Fqcsr::overflow),
    ];
    for (name, bit, f) in f {
        only_bit(name, bit, |v| f(Fqcsr(v as u32)));
    }
    // The acknowledgement keeps enable and interrupt enable and writes 1 to the two error bits that are set, nothing else.
    for bit in 0..32 {
        let want = if matches!(bit, 0 | 1 | 8 | 9) {
            1 << bit
        } else {
            0
        };
        assert_eq!(
            Fqcsr(1 << bit).acknowledge(),
            want,
            "acknowledge: bit {bit}"
        );
    }
    assert_eq!(Fqcsr(0x3_0303).acknowledge(), 0x303);
}

#[test]
fn interrupt_vector_and_debug_translation_fields() {
    // bits.h:267-271: four 4-bit fields.
    assert_eq!(regs::icvec(1, 2, 3, 4), 0x4321);
    assert_eq!(regs::icvec(0xf, 0, 0, 0), 0xf);
    assert_eq!(regs::icvec(0, 0xf, 0, 0), 0xf0);
    assert_eq!(regs::icvec(0, 0, 0xf, 0), 0xf00);
    assert_eq!(regs::icvec(0, 0, 0, 0xf), 0xf000);
    assert_eq!(regs::icvec(0x1f, 0x1f, 0x1f, 0x1f), 0xffff);
    // bits.h:254-264: GO 0, NW 3, DID 63:40; response FAULT 0, PPN 53:10.
    assert_eq!(regs::tr_req_ctl(0, true), 1);
    assert_eq!(regs::tr_req_ctl(0, false), 9);
    assert_eq!(regs::tr_req_ctl(0x12_3456, true), 0x1234_5600_0000_0001);
    assert_eq!(regs::tr_req_ctl(0xff_ffff, false), 0xffff_ff00_0000_0009);
    assert!(regs::tr_busy(1));
    assert!(!regs::tr_busy(!1));
    assert_eq!(regs::decode_tr_response(0x20c0_0000), Some(0x8300_0000));
    assert_eq!(regs::decode_tr_response(0x20c0_0001), None);
    assert_eq!(
        regs::decode_tr_response(0xffff_ffff_ffff_fffe),
        Some(0xff_ffff_ffff_f000) // PPN 53:10, 44 bits
    );
    // Queue base: LOG2SZ-1 in 4:0, PPN in 53:10 (bits.h:64, 129-130).
    assert_eq!(regs::decode_queue_base(0x2004_0007), (0x8010_0000, 256));
    assert_eq!(regs::decode_queue_base(1), (0, 4));
    assert_eq!(
        regs::queue_base(0xff_ffff_ffff_f000, 2),
        Some(0x3f_ffff_ffff_fc00)
    );
    assert_eq!(regs::queue_base(0x100_0000_0000_0000, 2), None); // past the 44-bit PPN
}

#[test]
fn pte_bits() {
    // The privileged architecture's PTE: V R W X U G A D in bits 0 to 7.
    let bits = [
        iopt::PTE_V,
        iopt::PTE_R,
        iopt::PTE_W,
        iopt::PTE_X,
        iopt::PTE_U,
        iopt::PTE_G,
        iopt::PTE_A,
        iopt::PTE_D,
    ];
    for (n, b) in bits.iter().enumerate() {
        assert_eq!(*b, 1 << n);
    }
    assert_eq!(iopt::PTE_HIGH, 0x3ff << 54);
}

#[test]
fn small_accessors() {
    let mem = FakeMem::new();
    assert_eq!(CmdQueue::new(0x8010_0000, 64).unwrap().entries(), 64);
    assert_eq!(FaultQueue::new(0x8010_1000, 32).unwrap().entries(), 32);
    let t = Iopt::new(&mem, 0x8000_0000, PtMode::Sv48x4).unwrap();
    let root = t.context_root(0x1234).unwrap();
    assert_eq!(
        (root.pa(), root.mode(), root.soft_id()),
        (0x8000_0000, PtMode::Sv48x4, 0x1234)
    );
    let a = Iova::new(0x1_2345, 77).unwrap();
    assert_eq!((a.first_page(), a.pages()), (0x1_2345, 77));
    // An MSI entry is valid by bit 0 only.
    let e = MsiPte::mrif(0x8300_0200, 0x2800_0000, 77).unwrap();
    assert!(e.is_valid());
    assert!(!MsiPte::invalid().is_valid());
    let f = Mrif::new(0x8300_0400).unwrap();
    f.reset(&mem);
    for id in [1u16, 63, 64, 2047] {
        assert!(!f.is_enabled(&mem, id).unwrap());
        f.enable(&mem, id).unwrap();
        assert!(f.is_enabled(&mem, id).unwrap());
        assert!(!f.is_enabled(&mem, id ^ 1).unwrap(), "{id}");
        f.disable(&mem, id).unwrap();
    }
}

#[test]
fn pci_addresses_and_fault_records_round_trip() {
    // bus 15:8, device 7:3, function 2:0.
    let rec = |device_id| FaultRecord {
        cause: 0,
        ttype: TransactionType::None,
        device_id,
        pv: false,
        pid: 0,
        privileged: false,
        iotval: 0,
        iotval2: 0,
    };
    assert_eq!(rec(0x08).pci_address(), (0, 1, 0));
    assert_eq!(rec(0xffff).pci_address(), (0xff, 0x1f, 7));
    assert_eq!(rec(0x1_a3c5).pci_address(), (0xa3, 0x18, 5));
    // Every field survives encode then decode, in random combinations (the decoder is checked against hex in `vectors`).
    let mut rng = Rng::new(7);
    for _ in 0..2000 {
        let r = FaultRecord {
            cause: rng.below(1 << 12) as u16,
            ttype: TransactionType::from_code(rng.below(10) as u8),
            device_id: rng.below(1 << 24) as u32,
            pv: rng.chance(50),
            pid: rng.below(1 << 20) as u32,
            privileged: rng.chance(50),
            iotval: rng.next(),
            iotval2: rng.next(),
        };
        assert_eq!(FaultRecord::decode(r.encode()), r);
    }
}

#[test]
fn a_new_directory_is_cleared_over_old_contents() {
    // `Ddt::new` zeroes its root page: a page left with valid-looking contexts must read back as empty.
    for format in [Format::Base, Format::Extended] {
        let mem = FakeMem::new();
        for i in 0..512 {
            mem.write(0x8400_0000 + i * 8, 0x11);
        }
        let d = Ddt::new(&mem, 0x8400_0000, Levels::One, format).unwrap();
        for i in 0..512 {
            assert_eq!(mem.read(0x8400_0000 + i * 8), 0, "{format:?} word {i}");
        }
        assert!(d.get(&mem, 3).is_none_or(|w| w == [0; 8]));
    }
}

#[test]
fn a_fault_queue_reads_every_word_of_a_record() {
    let mem = FakeMem::new();
    let mut q = FaultQueue::new(0x8010_1000, 4).unwrap();
    for slot in 0..4u64 {
        let base = 0x8010_1000 + slot * 32;
        mem.write(base, 0x17 | (slot << 40));
        mem.write(base + 8, 0xdead);
        mem.write(base + 16, 0x1000 + slot);
        mem.write(base + 24, 0x2000 + slot);
    }
    for slot in 0..3u64 {
        let r = q.pop(&mem, 3).unwrap();
        assert_eq!(
            (r.device_id, r.iotval, r.iotval2),
            (slot as u32, 0x1000 + slot, 0x2000 + slot)
        );
    }
    assert_eq!(q.pop(&mem, 3), None);
}
