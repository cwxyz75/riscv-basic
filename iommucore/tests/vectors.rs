//! Spec-derived vectors (IOMMU2; IOMMU-SPEC 13 "Independent oracles"; IOMMU0 item 9). Every expected value below was computed by
//! hand from the field positions of QEMU 11.1.3's `hw/riscv/riscv-iommu-bits.h` (cited as `bits.h:<line>`), QEMU's
//! `hw/riscv/riscv-iommu.c` (cited as `iommu.c:<line>`), the observed values of the IOMMU0 facts record
//! (`docs/records/notes/2026-11-25-iommu-qemu-facts.md`) and the AIA specification's MRIF table, never by calling the code under
//! test to produce them. QEMU's header is the independent implementation; the ratified specification text was not available
//! (IOMMU0 says so), and QEMU's qtest is not a source of vectors (GPL, and it has no walk vectors: IOMMU0 item 9).

use iommucore::cmdq::{CmdQueue, Command};
use iommucore::ddt::{self, Ddt, DeviceContext, Format, Levels, TableRoot};
use iommucore::faultq::{FaultQueue, FaultRecord, TransactionType};
use iommucore::iopt::{self, Access, PtMode};
use iommucore::msi::{self, Mrif, MsiPte, MsiTable, MsiWindow};
use iommucore::regs::{self, Caps, Ddtp, DdtpMode, Missing, Required};
use iommucore::sim::{FakeFrames, FakeMem, QEMU_CAPS};
use iommucore::{PhysMem64, TableFrames};

// ---- registers --------------------------------------------------------------------------------------------------------------

#[test]
fn register_offsets_are_the_headers() {
    // bits.h:76, 107, 113, 128, 133, 136, 139, 144, 147, 161, 172, 190, 251, 254, 261, 267, 274
    assert_eq!(regs::CAPABILITIES, 0x00);
    assert_eq!(regs::FCTL, 0x08);
    assert_eq!(regs::DDTP, 0x10);
    assert_eq!(regs::CQB, 0x18);
    assert_eq!(regs::CQH, 0x20);
    assert_eq!(regs::CQT, 0x24);
    assert_eq!(regs::FQB, 0x28);
    assert_eq!(regs::FQH, 0x30);
    assert_eq!(regs::FQT, 0x34);
    assert_eq!(regs::CQCSR, 0x48);
    assert_eq!(regs::FQCSR, 0x4c);
    assert_eq!(regs::IPSR, 0x54);
    assert_eq!(regs::TR_REQ_IOVA, 0x258);
    assert_eq!(regs::TR_REQ_CTL, 0x260);
    assert_eq!(regs::TR_RESPONSE, 0x268);
    assert_eq!(regs::ICVEC, 0x2f8);
    assert_eq!(regs::MSI_CFG_TBL, 0x300);
}

#[test]
fn the_observed_capabilities_decode() {
    // IOMMU0 "Corrections": capabilities = 0x0000_0078_e2cf_4f10 on QEMU 11.1.3; fields bits.h:77-98.
    let c = Caps(0x0000_0078_e2cf_4f10);
    assert_eq!(c.version(), (1, 0)); // bits 7:0 = 0x10
    assert!(c.sv32() && c.sv39() && c.sv48() && c.sv57()); // bits 8-11
    assert!(c.svrsw60t59b()); // bit 14
    assert!(c.sv32x4() && c.sv39x4() && c.sv48x4() && c.sv57x4()); // bits 16-19
    assert!(c.msi_flat() && c.msi_mrif()); // bits 22, 23 (IOMMU0 item 13)
    assert!(!c.amo_hwad()); // bit 24 clear: no hardware A/D updates
    assert!(c.ats() && !c.t2gpa()); // bits 25, 26
    assert_eq!(c.igs(), regs::Igs::Both); // bits 29:28 = 2 (IOMMU0 item 13: "IGS = both")
    assert!(c.hpm() && c.dbg()); // bits 30, 31
    assert_eq!(c.pas(), 56); // bits 37:32 = 0x38
    assert!(c.pd8() && !c.pd17() && !c.pd20()); // bits 38-40
    assert_eq!(c.0, QEMU_CAPS);
    assert_eq!(c.check(&Required { pas_bits: 40 }), Ok(()));
}

#[test]
fn the_capability_check_names_the_first_missing_row() {
    let ok = Caps(QEMU_CAPS);
    let req = Required { pas_bits: 40 };
    let without = |bit: u32| Caps(ok.0 & !(1 << bit));
    assert_eq!(
        Caps((ok.0 & !0xff) | 0x11).check(&req),
        Err(Missing::Version)
    );
    assert_eq!(without(17).check(&req), Err(Missing::Sv39x4));
    assert_eq!(without(22).check(&req), Err(Missing::MsiFlat));
    assert_eq!(without(23).check(&req), Err(Missing::MsiMrif));
    assert_eq!(without(29).check(&req), Err(Missing::WiredInterrupts)); // IGS = 0: MSI only
    assert_eq!(
        Caps(ok.0 | (1 << 28)).check(&req),
        Err(Missing::WiredInterrupts)
    ); // IGS = 3: reserved
    assert_eq!(Caps(ok.0 & !(1 << 29) | (1 << 28)).check(&req), Ok(())); // IGS = 1: wired only
    assert_eq!(
        ok.check(&Required { pas_bits: 57 }),
        Err(Missing::PhysAddrWidth)
    );
    // The order of IOMMU-SPEC 2.3: with Sv39x4 and MSI_MRIF both missing, Sv39x4 is named.
    assert_eq!(
        Caps(ok.0 & !(1 << 17) & !(1 << 23)).check(&req),
        Err(Missing::Sv39x4)
    );
    // The refusal lines of IOMMU-SPEC 2.5.
    assert_eq!(Missing::MsiMrif.to_string(), "IOMMU: no MRIF");
    assert_eq!(Missing::Sv39x4.to_string(), "IOMMU: missing Sv39x4");
    assert_eq!(Missing::PhysAddrWidth.to_string(), "IOMMU: missing PAS");
    assert_eq!(Required::covering(0x1_0000_0000), Required { pas_bits: 32 });
    assert_eq!(Required::covering(0x1_0000_0001), Required { pas_bits: 33 });
}

#[test]
fn ddtp_values() {
    // bits.h:113-125: mode in 3:0 (Off 0, Bare 1, 1LVL 2, 2LVL 3, 3LVL 4), busy bit 4, PPN in 53:10.
    assert_eq!(Ddtp::off().bits(), 0);
    assert_eq!(
        Ddtp::directory(Levels::One, 0x8300_0000).bits(),
        0x20c0_0002
    );
    assert_eq!(
        Ddtp::directory(Levels::Two, 0x8300_0000).bits(),
        0x20c0_0003
    );
    assert_eq!(
        Ddtp::directory(Levels::Three, 0x8000_0000).bits(),
        0x2000_0004
    );
    assert_eq!(Ddtp::bare().bits(), 1); // the negative control only
    let r = regs::decode_ddtp(0x20c0_0012);
    assert_eq!(r.mode, DdtpMode::Directory(Levels::One));
    assert!(r.busy);
    assert_eq!(r.root_pa, 0x8300_0000);
    assert_eq!(regs::decode_ddtp(1).mode, DdtpMode::Bare);
    assert_eq!(regs::decode_ddtp(7).mode, DdtpMode::Reserved(7));
}

#[test]
fn queue_registers() {
    // bits.h:64, 129-130: LOG2SZ-1 in 4:0 (QEMU iommu.c:2164 sizes the ring as 2 << field), PPN in 53:10.
    assert_eq!(regs::queue_base(0x8010_0000, 256), Some(0x2004_0007));
    assert_eq!(regs::queue_base(0x8010_1000, 128), Some(0x2004_0406));
    assert_eq!(regs::decode_queue_base(0x2004_0007), (0x8010_0000, 256));
    assert_eq!(regs::queue_base(0x8010_0800, 256), None);
    assert_eq!(regs::queue_base(0x8010_0000, 100), None);
    assert_eq!(CmdQueue::new(0x8010_0000, 256).unwrap().cqb(), 0x2004_0007);
    assert_eq!(
        FaultQueue::new(0x8010_1000, 128).unwrap().fqb(),
        0x2004_0406
    );
    // 256 commands of 16 bytes and 128 records of 32 bytes are one page each (IOMMU-SPEC 2.3); one more is refused.
    assert!(CmdQueue::new(0x8010_0000, 512).is_err());
    assert!(FaultQueue::new(0x8010_1000, 256).is_err());
    // A base past the 44-bit PPN field is refused rather than written as base 0.
    assert!(CmdQueue::new(1 << 56, 256).is_err());
    assert!(FaultQueue::new(1 << 56, 128).is_err());
    // bits.h:66-71, 162-178.
    assert_eq!(regs::QCSR_EN, 1);
    assert_eq!(regs::QCSR_IE, 2);
    assert_eq!(regs::QCSR_MF, 0x100);
    assert_eq!(regs::FQCSR_OF, 0x200);
    assert_eq!(regs::CQCSR_CMD_TO, 0x200);
    assert_eq!(regs::CQCSR_CMD_ILL, 0x400);
    assert_eq!(regs::QCSR_ON, 0x1_0000);
    assert_eq!(regs::QCSR_BUSY, 0x2_0000);
    // IOMMU0 item 4: cqcsr and fqcsr read 0x10001 after bring-up (enabled, active).
    let csr = regs::Fqcsr(0x10001);
    assert!(csr.enabled() && csr.on() && !csr.overflow() && !csr.mem_fault());
    let csr = regs::Fqcsr(0x10303);
    assert!(csr.overflow() && csr.mem_fault());
    assert_eq!(csr.acknowledge(), 0x303);
    assert!(regs::Cqcsr(0x10401).failed() && regs::Cqcsr(0x10401).illegal());
    assert!(!regs::Cqcsr(0x10001).failed());
}

#[test]
fn fctl_icvec_and_debug_registers() {
    // bits.h:108-110, 268-271, 255-264.
    assert_eq!(regs::fctl_wired(), 2);
    assert_eq!(regs::FCTL_BE, 1);
    assert_eq!(regs::icvec(1, 2, 3, 4), 0x4321);
    assert_eq!(regs::tr_req_ctl(0x08, false), 0x0000_0800_0000_0009);
    assert_eq!(regs::tr_req_ctl(0x08, true), 0x0000_0800_0000_0001);
    assert_eq!(regs::decode_tr_response(0x20c0_0000), Some(0x8300_0000));
    assert_eq!(regs::decode_tr_response(1), None);
}

// ---- commands and fault records ---------------------------------------------------------------------------------------------

#[test]
fn command_encodings() {
    // IOFENCE.C: opcode 2 (bits.h:348), func 0, AV bit 10 (bits.h:350), PR bit 12 and PW bit 13 (the specification's
    // IOFENCE.C layout; QEMU's reserved mask 31:14 at bits.h:351 starts just above them), DATA 63:32 (bits.h:352), and the
    // address >> 2 in the second doubleword (iommu.c:2008).
    assert_eq!(
        Command::iofence_c(Some((0x8020_0000, 0xdead_beef))).unwrap(),
        Command {
            dword0: 0xdead_beef_0000_3402,
            dword1: 0x2008_0000
        }
    );
    assert_eq!(
        Command::iofence_c(None).unwrap(),
        Command {
            dword0: 0x3002,
            dword1: 0
        }
    );
    // An address the command cannot name (not 4-byte aligned) is refused, not truncated.
    assert_eq!(Command::iofence_c(Some((0x8020_0002, 1))), None);
    // IOTINVAL.GVMA: opcode 1 (bits.h:339), func 1 (bits.h:341), AV bit 10, GV bit 33, GSCID 59:44 (bits.h:342-346); the
    // address's bits 63:12 in 61:10 of the second doubleword (iommu.c:2033: iova = (dword1 << 2) & page mask).
    assert_eq!(
        Command::iotinval_gvma(5, Some(0x100_0000_3000)),
        Command {
            dword0: 0x0000_5002_0000_0481,
            dword1: 0x40_0000_0c00
        }
    );
    assert_eq!(
        Command::iotinval_gvma(0xffff, None),
        Command {
            dword0: 0x0fff_f002_0000_0081,
            dword1: 0
        }
    );
    // IOTINVAL.VMA: func 0, PSCID 31:12, PSCV bit 32 (bits.h:340-344).
    assert_eq!(Command::iotinval_vma(Some(7), None).dword0, 0x1_0000_7001);
    assert_eq!(
        Command::iotinval_vma(Some(7), Some(0x100_0000_5000)),
        Command {
            dword0: 0x1_0000_7401,
            dword1: 0x40_0000_1400
        }
    );
    // IODIR.INVAL_DDT: opcode 3, func 0, DV bit 33, DID 63:40 (bits.h:354-359).
    assert_eq!(
        Command::iodir_inval_ddt(Some(0x08)).dword0,
        0x0000_0802_0000_0003
    );
    assert_eq!(Command::iodir_inval_ddt(None).dword0, 3);
}

#[test]
fn fault_record_decoding() {
    // bits.h:26-37: cause 11:0, PID 31:12, PV 32, TTYPE 39:34, DID 63:40; then reserved, iotval, iotval2. IOMMU0 item 3: a DMA
    // write through a leaf with U clear by device 0x08 faults with cause 23, ttype 3.
    let r = FaultRecord::decode([0x0000_080c_0000_0017, 0, 0x100_0000_1000, 0x100_0000_1000]);
    assert_eq!(r.cause, 23);
    assert_eq!(r.ttype, TransactionType::UntranslatedWrite);
    assert_eq!(r.ttype.direction(), "write");
    assert_eq!(r.device_id, 0x08);
    assert_eq!(r.pci_address(), (0, 1, 0)); // IOMMU0 item 1: the testdev is 00:01.0
    assert!(!r.pv);
    assert_eq!(r.iotval, 0x100_0000_1000);
    assert_eq!(r.iotval2, 0x100_0000_1000);
    // A read (ttype 2, bits.h:440) by device 0x18 with cause 256.
    let r = FaultRecord::decode([0x0000_1808_0000_0100, 0, 0x1000, 0]);
    assert_eq!(
        (r.cause, r.ttype, r.device_id),
        (256, TransactionType::UntranslatedRead, 0x18)
    );
    assert_eq!(r.ttype.direction(), "read");
    // PID and PV.
    let r = FaultRecord::decode([(0x1234 << 12) | (1 << 32) | 5, 0, 0, 0]);
    assert_eq!((r.cause, r.pid, r.pv), (5, 0x1234, true));
    // Every field round-trips through the simulator's encoder.
    let r = FaultRecord {
        cause: 262,
        ttype: TransactionType::UntranslatedWrite,
        device_id: 0xab_cdef,
        pv: false,
        pid: 0,
        privileged: false,
        iotval: 7,
        iotval2: 9,
    };
    assert_eq!(FaultRecord::decode(r.encode()), r);
    assert_eq!(r.encode()[0], 0xabcd_ef0c_0000_0106);
}

#[test]
fn fault_causes() {
    // bits.h:386-417, and IOMMU0 items 2, 8, 10.
    use iommucore::faultq::*;
    assert_eq!(CAUSE_READ_GUEST_PAGE_FAULT, 21);
    assert_eq!(CAUSE_WRITE_GUEST_PAGE_FAULT, 23);
    assert_eq!(CAUSE_DMA_DISABLED, 256);
    assert_eq!(CAUSE_DDT_INVALID, 258);
    assert_eq!(CAUSE_DDT_MISCONFIGURED, 259);
    assert_eq!(CAUSE_MSI_LOAD_FAULT, 261);
    assert_eq!(CAUSE_MSI_INVALID, 262);
    assert_eq!(CAUSE_MSI_MISCONFIGURED, 263);
    assert_eq!(CAUSE_MRIF_FAULT, 264);
    assert_eq!(CAUSE_MSI_WRITE_FAULT, 273);
}

// ---- device contexts and the directory --------------------------------------------------------------------------------------

#[test]
fn device_context_encodings() {
    // bits.h:282-323, 419-423: tc V bit 0, DTF bit 4; iohgatp PPN 43:0, GSCID 59:44, MODE 63:60 (Sv39x4 = 8); ta PSCID 31:12;
    // fsc MODE 63:60 (Sv39 = 8); msiptp PPN 43:0, MODE 63:60 (Flat = 1); mask and pattern in page units.
    let mem = FakeMem::new();
    let window = MsiWindow::new(0x100_0000_0000, 8).unwrap();
    let table = MsiTable::new(&mem, 0x8200_0000, window).unwrap();
    let root = TableRoot::new(0x8100_0000, PtMode::Sv39x4, 3).unwrap();
    assert_eq!(
        DeviceContext::translate(root, Some(&table)).words(),
        [
            0x1,
            0x8000_3000_0008_1000,
            0,
            0,
            0x1000_0000_0008_2000,
            0xff,
            0x1000_0000,
            0
        ]
    );
    let empty = TableRoot::new(0x8104_0000, PtMode::Sv39x4, 0).unwrap();
    assert_eq!(
        DeviceContext::deny(empty).words(),
        [0x11, 0x8000_0000_0008_1040, 0, 0, 0, 0, 0, 0]
    );
    assert_eq!(DeviceContext::invalid().words(), [0; 8]);
    let first = TableRoot::new(0x8100_0000, PtMode::Sv39, 0x12345).unwrap();
    assert_eq!(
        DeviceContext::translate(first, None).words(),
        [0x1, 0, 0x1234_5000, 0x8000_0000_0008_1000, 0, 0, 0, 0]
    );
    // Sv48x4 and Sv57x4 are modes 9 and 10 (bits.h:319-320).
    let r48 = TableRoot::new(0x8100_0000, PtMode::Sv48x4, 1).unwrap();
    assert_eq!(DeviceContext::translate(r48, None).words()[1] >> 60, 9);
    let r57 = TableRoot::new(0x8100_0000, PtMode::Sv57x4, 1).unwrap();
    assert_eq!(DeviceContext::translate(r57, None).words()[1] >> 60, 10);
    // A second-stage root must be 16 KiB aligned (iommu.c:888-899); GSCID is 16 bits and PSCID 20.
    assert!(TableRoot::new(0x8100_1000, PtMode::Sv39x4, 0).is_none());
    assert!(TableRoot::new(0x8100_1000, PtMode::Sv39, 0).is_some());
    assert!(TableRoot::new(0x8100_0000, PtMode::Sv39x4, 0x1_0000).is_none());
    assert!(TableRoot::new(0x8100_0000, PtMode::Sv39, 0xf_ffff).is_some());
    assert!(TableRoot::new(0x8100_0000, PtMode::Sv39, 0x10_0000).is_none());
}

#[test]
fn no_valid_context_has_both_stages_bare() {
    // IO2: every valid context any constructor makes names a page table in exactly one stage.
    let mem = FakeMem::new();
    let window = MsiWindow::new(0x100_0000_0000, 8).unwrap();
    let table = MsiTable::new(&mem, 0x8200_0000, window).unwrap();
    for mode in [
        PtMode::Sv39,
        PtMode::Sv48,
        PtMode::Sv57,
        PtMode::Sv39x4,
        PtMode::Sv48x4,
        PtMode::Sv57x4,
    ] {
        let root = TableRoot::new(0x8100_0000, mode, 1).unwrap();
        for ctx in [
            DeviceContext::deny(root),
            DeviceContext::translate(root, None),
            DeviceContext::translate(root, Some(&table)),
        ] {
            let w = ctx.words();
            assert!(ctx.is_valid());
            let g = w[1] >> 60 != 0;
            let s = w[3] >> 60 != 0;
            assert!(g ^ s, "{mode:?}: {w:x?}");
            // Hardware A/D updates are off in every context (bits.h:301-302: GADE 7, SADE 8).
            assert_eq!(w[0] & 0x180, 0);
        }
    }
}

#[test]
fn directory_split_for_each_format() {
    // iommu.c:1230-1250: an extended context's device id splits [23:15][14:6][5:0], a base context's [23:16][15:7][6:0];
    // leaf pages hold 64 extended (64-byte) or 128 base (32-byte) contexts; directory entries are V | PPN << 10 (bits.h:278).
    let id = 0xab_cdef;
    for (format, top, mid, leaf, size) in [
        (Format::Extended, 0x157u64, 0x137u64, 0x2fu64, 64u64),
        (Format::Base, 0xab, 0x19b, 0x6f, 32),
    ] {
        let mem = FakeMem::new();
        let mut frames = FakeFrames::new(&mem, 0x9000_0000, 16);
        let mut d = Ddt::new(&mem, 0x8000_0000, Levels::Three, format).unwrap();
        let root = TableRoot::new(0x8100_0000, PtMode::Sv39x4, 0).unwrap();
        d.set(&mem, &mut frames, id, &DeviceContext::deny(root))
            .unwrap();
        let de1 = mem.read(0x8000_0000 + top * 8);
        assert_eq!(de1 & 1, 1, "{format:?}");
        assert_eq!(de1 & !((1u64 << 54) - 1), 0);
        let mid_page = (de1 >> 10) << 12;
        let de2 = mem.read(mid_page + mid * 8);
        assert_eq!(de2 & 1, 1, "{format:?}");
        let leaf_page = (de2 >> 10) << 12;
        assert_eq!(mem.read(leaf_page + leaf * size), 0x11, "{format:?}");
        assert_eq!(mem.read(leaf_page + leaf * size + 8), 0x8000_0000_0008_1000);
        assert_eq!(frames.live(), 2);
    }
    // The widest device id each mode covers (iommu.c:1218-1227).
    assert_eq!(ddt::device_id_bits(Levels::One, Format::Extended), 6);
    assert_eq!(ddt::device_id_bits(Levels::Two, Format::Extended), 15);
    assert_eq!(ddt::device_id_bits(Levels::Three, Format::Extended), 24);
    assert_eq!(ddt::device_id_bits(Levels::One, Format::Base), 7);
    assert_eq!(ddt::device_id_bits(Levels::Two, Format::Base), 16);
    assert_eq!(ddt::device_id_bits(Levels::Three, Format::Base), 24);
}

#[test]
fn one_level_directory_holds_the_context_in_the_root() {
    // 1LVL, extended: device 0x08's context at root + 8 * 64 (IOMMU0 item 3 ran exactly this shape).
    let mem = FakeMem::new();
    let mut frames = FakeFrames::new(&mem, 0x9000_0000, 4);
    let mut d = Ddt::new(&mem, 0x8300_0000, Levels::One, Format::Extended).unwrap();
    let root = TableRoot::new(0x8100_0000, PtMode::Sv39x4, 2).unwrap();
    d.set(
        &mem,
        &mut frames,
        0x08,
        &DeviceContext::translate(root, None),
    )
    .unwrap();
    assert_eq!(mem.read(0x8300_0000 + 8 * 64), 1);
    assert_eq!(mem.read(0x8300_0000 + 8 * 64 + 8), 0x8000_2000_0008_1000);
    assert_eq!(frames.allocs, 0);
    assert_eq!(d.ddtp().bits(), 0x20c0_0002);
    assert!(d
        .set(&mem, &mut frames, 64, &DeviceContext::invalid())
        .is_err());
    // The base format cannot hold an MSI page table.
    let mut b = Ddt::new(&mem, 0x8400_0000, Levels::One, Format::Base).unwrap();
    let window = MsiWindow::new(0x100_0000_0000, 8).unwrap();
    let table = MsiTable::new(&mem, 0x8200_0000, window).unwrap();
    assert_eq!(
        b.set(
            &mem,
            &mut frames,
            1,
            &DeviceContext::translate(root, Some(&table))
        ),
        Err(ddt::DdtError::MsiNeedsExtended)
    );
    b.set(
        &mem,
        &mut frames,
        127,
        &DeviceContext::translate(root, None),
    )
    .unwrap();
    assert_eq!(mem.read(0x8400_0000 + 127 * 32), 1);
}

// ---- page tables ------------------------------------------------------------------------------------------------------------

#[test]
fn leaf_encodings() {
    // The privileged architecture's PTE (V 0, R 1, W 2, X 3, U 4, G 5, A 6, D 7, PPN 53:10). IOMMU0 item 3: a second-stage leaf
    // needs U = 1, and A and D set when hardware updates are off; the first stage without a process id is a user access too
    // (iommu.c:495-500, 510-519).
    assert_eq!(iopt::leaf(0x8765_4000, Access::ReadWrite), 0x21d9_50d7);
    assert_eq!(iopt::leaf(0x8765_4000, Access::Read), 0x21d9_50d3);
}

#[test]
fn sv39x4_walk_indices() {
    // A second-stage root is 2048 entries (the top index has 11 bits: iommu.c:419-423, "widened"); a 41-bit address
    // 0x1ab_cdef_f000 indexes 0x6af (bits 40:30), 0x06f (bits 29:21) and 0x0ff (bits 20:12).
    let mem = FakeMem::new();
    let mut frames = FakeFrames::new(&mem, 0x9000_0000, 8);
    let mut t = iopt::Iopt::new(&mem, 0x8100_0000, PtMode::Sv39x4).unwrap();
    let iova = 0x1ab_cdef_f000u64;
    t.map(&mem, &mut frames, iova, 0x8765_4000, Access::ReadWrite)
        .unwrap();
    let (i2, i1, i0) = (
        (iova >> 30) & 0x7ff,
        (iova >> 21) & 0x1ff,
        (iova >> 12) & 0x1ff,
    );
    assert_eq!((i2, i1, i0), (0x6af, 0x06f, 0x0ff));
    let l2 = mem.read(0x8100_0000 + i2 * 8);
    assert_eq!(l2 & 0x3ff, 1, "a non-leaf is V only");
    let l1 = mem.read(((l2 >> 10) << 12) + i1 * 8);
    assert_eq!(l1 & 0x3ff, 1);
    assert_eq!(mem.read(((l1 >> 10) << 12) + i0 * 8), 0x21d9_50d7);
    // 2^41 is out of range for Sv39x4 (41 bits); for Sv39 the address must be sign-extended from bit 38.
    assert_eq!(
        t.map(&mem, &mut frames, 1 << 41, 0x8765_4000, Access::Read),
        Err(iopt::MapError::OutOfRange)
    );
    let mut s = iopt::Iopt::new(&mem, 0x8200_0000, PtMode::Sv39).unwrap();
    assert_eq!(
        s.map(&mem, &mut frames, 1 << 39, 0x8765_4000, Access::Read),
        Err(iopt::MapError::OutOfRange)
    );
    assert!(s
        .map(
            &mem,
            &mut frames,
            0xffff_ffc0_0000_0000,
            0x8765_4000,
            Access::Read
        )
        .is_ok());
    assert!(iopt::Iopt::new(&mem, 0x8100_1000, PtMode::Sv39x4).is_err());
    assert_eq!(PtMode::Sv39x4.root_bytes(), 16384);
    assert_eq!(PtMode::Sv39x4.iova_bits(), 41);
    assert_eq!(PtMode::Sv57.iova_bits(), 57);
}

// ---- MSI --------------------------------------------------------------------------------------------------------------------

#[test]
fn pext_vectors() {
    // iommu.c:195-205: val = a b c d e f g h, ext = 1 0 1 0 0 1 1 0, ret = a c f g.
    assert_eq!(msi::pext(0b1011_0110, 0b1010_0110), 0b1111);
    assert_eq!(msi::pext(0b0100_1001, 0b1010_0110), 0b0000);
    assert_eq!(msi::pext(0b1000_0000, 0b1010_0110), 0b1000);
    assert_eq!(msi::pext(0b0000_0010, 0b1010_0110), 0b0001);
    assert_eq!(msi::pext(u64::MAX, 0xff), 0xff);
    assert_eq!(msi::pext(0x1234_5678, 0), 0);
}

#[test]
fn msi_window_and_file_numbers() {
    // IOMMU-SPEC 3.3 and 8.2: the window is 256 pages at IO_BASE = 2^40; page k selects entry k.
    let w = MsiWindow::new(1 << 40, 8).unwrap();
    assert_eq!((w.mask(), w.pattern()), (0xff, 1 << 28));
    assert_eq!(w.files(), 256);
    assert!(w.matches(1 << 40));
    assert!(w.matches((1 << 40) + 255 * 4096 + 0xffc));
    assert!(!w.matches((1 << 40) + 256 * 4096));
    assert!(!w.matches((1 << 40) - 4096));
    assert!(!w.matches(0));
    assert_eq!(w.file_number((1 << 40) + 7 * 4096), 7);
    assert_eq!(w.file_number((1 << 40) + 255 * 4096 + 4), 255);
    assert!(MsiWindow::new((1 << 40) + 4096, 8).is_err());
    assert!(MsiWindow::new(1 << 40, 9).is_err());
}

#[test]
fn msi_entry_encodings() {
    // bits.h:452-469: V bit 0, M bits 2:1 (MRIF 1, basic 3), MRIF address 55:9 in 53:7, C bit 63; NID 9:0, notice PPN 53:10,
    // NID MSB bit 60 (iommu.c:813-815).
    let e = MsiPte::mrif(0x8300_0200, 0x2800_0000, 1029).unwrap();
    assert_eq!(e.words(), [0x20c0_0083, 0x1000_0000_0a00_0005]);
    assert_eq!(e.notice(), (0x2800_0000, 1029));
    assert_eq!(
        MsiPte::mrif(0x8300_0200, 0x2800_0000, 1).unwrap().words()[1],
        0xa00_0001
    );
    assert_eq!(
        MsiPte::mrif(0x8300_0200, 0x2800_0000, 2047)
            .unwrap()
            .words()[1],
        0x1000_0000_0a00_03ff
    );
    assert!(MsiPte::mrif(0x8300_0100, 0x2800_0000, 1).is_err()); // a file is 512-byte aligned
    assert!(MsiPte::mrif(0x8300_0200, 0x2800_0000, 0).is_err());
    assert!(MsiPte::mrif(0x8300_0200, 0x2800_0000, 2048).is_err());
    assert_eq!(MsiPte::invalid().words(), [0, 0]);
    // The negative control only.
    assert_eq!(MsiPte::basic(0x2800_0000).words(), [0xa00_0007, 0]);
    // A16: a move changes only the second doubleword, so one store changes both the page and the identity.
    let a = MsiPte::mrif(0x8300_0200, 0x2800_0000, 5).unwrap();
    let b = MsiPte::mrif(0x8300_0200, 0x2800_1000, 1029).unwrap();
    assert_eq!(a.words()[0], b.words()[0]);
}

#[test]
fn msi_table_layout() {
    // iommu.c:658-670: entry k is at the table's base + 16 k.
    let mem = FakeMem::new();
    let t = MsiTable::new(&mem, 0x8200_0000, MsiWindow::new(1 << 40, 8).unwrap()).unwrap();
    let e = MsiPte::mrif(0x8300_0200, 0x2800_0000, 3).unwrap();
    t.set(&mem, 37, e).unwrap();
    assert_eq!(mem.read(0x8200_0000 + 37 * 16), e.words()[0]);
    assert_eq!(mem.read(0x8200_0000 + 37 * 16 + 8), e.words()[1]);
    assert_eq!(t.get(&mem, 37), Ok(e));
    assert!(t.set(&mem, 256, e).is_err());
    t.clear(&mem, 37).unwrap();
    assert_eq!(mem.read(0x8200_0000 + 37 * 16), 0);
}

#[test]
fn mrif_layout() {
    // The AIA specification's MRIF table, as QEMU reads it (iommu.c:760-806): identities 64 k to 64 k + 63 have their pending
    // bits at offset 16 k and their enable bits at 16 k + 8.
    let mem = FakeMem::new();
    let f = Mrif::new(0x8300_0200).unwrap();
    f.reset(&mem);
    f.enable(&mem, 70).unwrap();
    assert_eq!(mem.read(0x8300_0200 + 0x18), 1 << 6);
    f.enable(&mem, 1).unwrap();
    assert_eq!(mem.read(0x8300_0200 + 0x08), 1 << 1);
    f.enable(&mem, 2047).unwrap();
    assert_eq!(mem.read(0x8300_0200 + 0x1f8), 1 << 63);
    mem.write(0x8300_0200 + 0x10, 1 << 6);
    assert!(f.is_pending(&mem, 70).unwrap());
    assert!(f.take_pending(&mem, 70).unwrap());
    assert_eq!(mem.read(0x8300_0200 + 0x10), 0);
    assert!(!f.take_pending(&mem, 70).unwrap());
    f.disable(&mem, 70).unwrap();
    assert_eq!(mem.read(0x8300_0200 + 0x18), 0);
    assert!(f.enable(&mem, 2048).is_err());
    assert!(Mrif::new(0x8300_0100).is_err());
    assert!(Mrif::new(1 << 56).is_err()); // an MSI entry cannot name it
    assert_eq!(msi::MRIF_BYTES, 512);
    // IOMMU-SPEC 8.4 and 8.5: a fired vector (identity 1, enable clear) keeps its pending bit; the strays (5, enabled or
    // not, and 1000) are taken.
    f.reset(&mem);
    f.enable(&mem, 5).unwrap();
    mem.write(0x8300_0200, (1 << 1) | (1 << 5));
    mem.write(0x8300_0200 + 15 * 16, 1 << 40);
    let mut want = [0u64; 32];
    want[0] = 1 << 5;
    want[15] = 1 << 40;
    assert_eq!(f.stray(&mem, 1), Ok(want));
    assert_eq!(mem.read(0x8300_0200), 1 << 1);
    assert_eq!(mem.read(0x8300_0200 + 15 * 16), 0);
    assert!(f.stray(&mem, 0).is_err());
}

// ---- the frames a removal never asks for ------------------------------------------------------------------------------------

#[test]
fn removal_asks_for_no_frame() {
    let mem = FakeMem::new();
    let mut frames = FakeFrames::new(&mem, 0x9000_0000, 64);
    let mut t = iopt::Iopt::new(&mem, 0x8100_0000, PtMode::Sv39x4).unwrap();
    for p in 0..8u64 {
        t.map(
            &mem,
            &mut frames,
            (1 << 40) + p * 0x20_0000,
            0x8800_0000 + p * 4096,
            Access::Read,
        )
        .unwrap();
    }
    let allocs = frames.allocs;
    frames.budget = Some(0); // any allocation now fails
    for p in 0..4u64 {
        assert!(t
            .unmap(&mem, &mut frames, (1 << 40) + p * 0x20_0000)
            .is_some());
    }
    assert_eq!(frames.allocs, allocs);
    t.destroy(&mem, &mut frames);
    assert_eq!(frames.allocs, allocs);
    assert_eq!(frames.live(), 0);
    let mut d = Ddt::new(&mem, 0x8000_0000, Levels::Three, Format::Extended).unwrap();
    frames.budget = None;
    let root = TableRoot::new(0x8100_0000, PtMode::Sv39x4, 0).unwrap();
    d.set(&mem, &mut frames, 0x1234, &DeviceContext::deny(root))
        .unwrap();
    frames.budget = Some(0);
    d.clear(&mem, &mut frames, 0x1234);
    assert_eq!(frames.live(), 0);
    let _ = frames.alloc(); // (the budget is respected)
    assert_eq!(frames.live(), 0);
}

// ---- tables the crate did not write -------------------------------------------------------------------------------------

#[test]
fn removal_stops_at_a_superpage_it_did_not_write() {
    // A 2 MiB leaf (read-only, or execute-only) put in by hand at level 1: unmapping a page inside it finds no 4 KiB leaf and
    // touches neither the superpage nor the memory it maps (which must not be read as a table).
    for leaf_bits in [0x53u64, 0x59] {
        let mem = FakeMem::new();
        let mut frames = FakeFrames::new(&mem, 0x9000_0000, 8);
        let mut t = iopt::Iopt::new(&mem, 0x8100_0000, PtMode::Sv39x4).unwrap();
        t.map(&mem, &mut frames, 0x1000, 0x8765_4000, Access::Read)
            .unwrap();
        let l1 = (mem.read(0x8100_0000) >> 10) << 12;
        let big = 0x4000_0000u64;
        mem.write(l1 + 8, (big >> 12) << 10 | leaf_bits);
        mem.write(big + 8, 0x1); // looks like a valid entry if `big` were a table
        assert_eq!(t.unmap(&mem, &mut frames, 0x20_1000), None);
        assert_eq!(mem.read(big + 8), 0x1);
        assert_eq!(mem.read(l1 + 8), (big >> 12) << 10 | leaf_bits);
        assert_eq!(
            t.map(&mem, &mut frames, 0x20_1000, 0x8765_5000, Access::Read),
            Err(iopt::MapError::AlreadyMapped)
        );
    }
}

#[test]
fn a_pointer_at_the_last_level_is_not_a_leaf_and_not_followed() {
    let mem = FakeMem::new();
    let mut frames = FakeFrames::new(&mem, 0x9000_0000, 8);
    let mut t = iopt::Iopt::new(&mem, 0x8100_0000, PtMode::Sv39x4).unwrap();
    t.map(&mem, &mut frames, 0x1000, 0x8765_4000, Access::Read)
        .unwrap();
    let l1 = (mem.read(0x8100_0000) >> 10) << 12;
    let l0 = (mem.read(l1) >> 10) << 12;
    mem.write(l0 + 2 * 8, (0x9000_0000u64 >> 12) << 10 | 1); // V only, at level 0
    let mut seen = Vec::new();
    t.for_each_leaf(&mem, |iova, pa, _| seen.push((iova, pa)));
    assert_eq!(seen, [(0x1000, 0x8765_4000)]);
}

#[test]
fn an_mrif_reset_clears_all_512_bytes() {
    let mem = FakeMem::new();
    for i in 0..64 {
        mem.write(0x8300_0200 + i * 8, !0);
    }
    Mrif::new(0x8300_0200).unwrap().reset(&mem);
    for i in 0..64 {
        assert_eq!(mem.read(0x8300_0200 + i * 8), 0, "word {i}");
    }
    assert_eq!(mem.read(0x8300_0200 + 512), 0);
}
