//! IOMMU8's acceptance vectors for `pcicore`: configuration-space images of QEMU 11.1.3's devices, parsed and checked against what
//! QEMU itself printed for the same machine.
//!
//! The images (`fixtures/*.cfg`) were taken on 2026-11-26 from `qemu-system-riscv64 -M virt,aia=aplic-imsic,iommu-sys=on -cpu
//! rva23s64` with `virtio-blk-pci` at slot 1, `virtio-net-pci` at 2, `virtio-rng-pci` at 3 (each `iommu_platform=on,disable-legacy=on`)
//! and `iommu-testdev` at 4: the monitor's `xp /64wx` of each function's ECAM page, and the six BAR registers read back by a probe
//! of a dozen M-mode instructions that wrote all ones to each and restored it.
//!
//! The **independent listing** is `fixtures/info-pci.txt`: the monitor's `info pci` for the same machine after a second probe had
//! placed the BARs at fixed addresses, so it prints each BAR's kind and range. The class codes, ids, BAR indexes, kinds and sizes
//! below are read from that listing by this file, not from the code under test. What `info pci` does not print (the MSI-X table
//! size and placement, the virtio structures) is from QEMU's source (`hw/virtio/virtio-pci.c` at v11.1.3: the modern BAR is 4,
//! holding the common, ISR, device and notify structures at 0x0, 0x1000, 0x2000 and 0x3000, 0x1000 bytes each, with a notify
//! multiplier of 4; MSI-X is in BAR 1, the table at 0 and the pending bits at 0x800; the default vector counts are 2 for the
//! block and entropy devices and 4 for the network device), stated here as numbers.
use pcicore::*;

fn fixture(name: &str) -> (Vec<u8>, [u32; 6]) {
    let path = format!("{}/tests/fixtures/{name}.cfg", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap();
    let mut cfg = vec![0u8; 256];
    let mut masks = [0u32; 6];
    for line in text.lines().filter(|l| !l.starts_with('#')) {
        let (head, rest) = line.split_once(':').unwrap();
        let words: Vec<u32> = rest
            .split_whitespace()
            .map(|w| u32::from_str_radix(w, 16).unwrap())
            .collect();
        if head == "masks" {
            masks.copy_from_slice(&words);
        } else {
            let at = usize::from_str_radix(head, 16).unwrap();
            for (k, w) in words.iter().enumerate() {
                cfg[at + 4 * k..at + 4 * k + 4].copy_from_slice(&w.to_le_bytes());
            }
        }
    }
    (cfg, masks)
}

/// One function of `info pci`: (slot, vendor, device, class name, [(BAR index, kind text, size)]).
type Listed = (u8, u16, u16, String, Vec<(u8, String, u64)>);

fn info_pci() -> Vec<Listed> {
    let path = format!("{}/tests/fixtures/info-pci.txt", env!("CARGO_MANIFEST_DIR"));
    let mut out: Vec<Listed> = Vec::new();
    for line in std::fs::read_to_string(path).unwrap().lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("Bus  0, device") {
            let slot = rest.split(',').next().unwrap().trim().parse().unwrap();
            out.push((slot, 0, 0, String::new(), Vec::new()));
        } else if let Some((class, ids)) = l.split_once(": PCI device ") {
            let f = out.last_mut().unwrap();
            let (v, d) = ids.split_once(':').unwrap();
            f.1 = u16::from_str_radix(v, 16).unwrap();
            f.2 = u16::from_str_radix(d, 16).unwrap();
            f.3 = class.to_string();
        } else if let Some(rest) = l.strip_prefix("BAR") {
            let (idx, desc) = rest.split_once(": ").unwrap();
            if let Some((kind, range)) = desc.split_once(" at ") {
                let (lo, hi) = range.split_once(" [").unwrap();
                let lo = u64::from_str_radix(lo.trim_start_matches("0x"), 16).unwrap();
                let hi = u64::from_str_radix(hi.trim_start_matches("0x").trim_end_matches(']'), 16)
                    .unwrap();
                out.last_mut().unwrap().4.push((
                    idx.parse().unwrap(),
                    kind.to_string(),
                    hi - lo + 1,
                ));
            }
        }
    }
    out
}

/// The class names QEMU prints, as class codes (PCI code and ID assignment specification: 01/00 SCSI, 02/00 Ethernet, 06/00 host
/// bridge; "Class 0255" is QEMU's way of printing 0x00ff, the "other" class).
fn class_of(name: &str) -> u32 {
    match name {
        "Host bridge" => 0x06_00,
        "SCSI controller" => 0x01_00,
        "Ethernet controller" => 0x02_00,
        "Class 0255" => 0x00_ff,
        other => panic!("unknown class {other}"),
    }
}

fn kind_text(k: BarKind) -> &'static str {
    match k {
        BarKind::Mem32 {
            prefetchable: false,
        } => "32 bit memory",
        BarKind::Mem32 { prefetchable: true } => "32 bit prefetchable memory",
        BarKind::Mem64 {
            prefetchable: false,
        } => "64 bit memory",
        BarKind::Mem64 { prefetchable: true } => "64 bit prefetchable memory",
        BarKind::Io => "I/O",
    }
}

const NAMES: [(u8, &str); 5] = [
    (0, "host-bridge"),
    (1, "virtio-blk-pci"),
    (2, "virtio-net-pci"),
    (3, "virtio-rng-pci"),
    (4, "iommu-testdev"),
];

#[test]
fn every_image_parses_to_what_info_pci_printed() {
    let listed = info_pci();
    assert_eq!(listed.len(), 5);
    for (slot, name) in NAMES {
        let (cfg, masks) = fixture(name);
        let bdf = Bdf::new(0, slot, 0).unwrap();
        let f = describe(bdf, &cfg, &masks).unwrap().unwrap();
        let l = listed.iter().find(|l| l.0 == slot).unwrap();
        assert_eq!(
            (f.header.vendor, f.header.device),
            (l.1, l.2),
            "{name}: ids"
        );
        assert_eq!(f.header.class >> 8, class_of(&l.3), "{name}: class");
        assert_eq!(f.header.header_type, 0, "{name}");
        // the BARs info pci printed with a range (the ROM, BAR6, is not a BAR register and is never assigned)
        let want: Vec<(u8, String, u64)> = l.4.iter().filter(|b| b.0 < 6).cloned().collect();
        let got: Vec<(u8, String, u64)> = f
            .bars
            .iter()
            .map(|b| (b.index, kind_text(b.kind).to_string(), b.size))
            .collect();
        assert_eq!(got, want, "{name}: BARs");
    }
}

#[test]
fn the_virtio_functions_have_their_structures_and_msix_where_qemu_puts_them() {
    for (slot, name, id, vectors) in [
        (1, "virtio-blk-pci", 2, 2),
        (2, "virtio-net-pci", 1, 4),
        (3, "virtio-rng-pci", 4, 2),
    ] {
        let (cfg, masks) = fixture(name);
        let f = describe(Bdf::new(0, slot, 0).unwrap(), &cfg, &masks)
            .unwrap()
            .unwrap();
        let (dev_id, l) = f.virtio.unwrap();
        assert_eq!(dev_id, id, "{name}");
        let r = |offset, len| Region {
            bar: 4,
            offset,
            len,
        };
        assert_eq!(l.bar, 4);
        assert_eq!(l.common, r(0, 0x1000), "{name}: common");
        assert_eq!(l.isr, r(0x1000, 0x1000), "{name}: ISR");
        assert_eq!(l.device, Some(r(0x2000, 0x1000)), "{name}: device");
        assert_eq!(l.notify, r(0x3000, 0x1000), "{name}: notify");
        assert_eq!(l.notify_multiplier, 4, "{name}");
        let m = f.msix.unwrap();
        assert_eq!(
            (
                m.table_size,
                m.table_bar,
                m.table_offset,
                m.pba_bar,
                m.pba_offset
            ),
            (vectors, 1, 0, 1, 0x800),
            "{name}: MSI-X"
        );
        assert_eq!(m.control_offset(), u16::from(m.cap) + 2);
        // the virtio capability list in list order: PCI-configuration access, notify, device, ISR, common
        let caps = capabilities(&cfg).unwrap();
        let ids: Vec<u8> = caps.iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![0x11, 9, 9, 9, 9, 9], "{name}");
        let kinds: Vec<u8> = virtio_caps(&cfg, &caps)
            .unwrap()
            .iter()
            .map(|c| c.cfg_type)
            .collect();
        assert_eq!(kinds, vec![5, 2, 4, 3, 1], "{name}");
        assert_eq!(
            f.flr, None,
            "IOMMU0: QEMU's virtio-pci functions advertise no function-level reset"
        );
        assert_eq!(
            f.header.interrupt_pin, 1,
            "{name}: INTA (listed, never routed)"
        );
    }
}

#[test]
fn the_test_device_and_the_host_bridge_have_no_msix() {
    for (slot, name) in [(0, "host-bridge"), (4, "iommu-testdev")] {
        let (cfg, masks) = fixture(name);
        let f = describe(Bdf::new(0, slot, 0).unwrap(), &cfg, &masks)
            .unwrap()
            .unwrap();
        assert!(
            f.caps.is_empty() && f.msix.is_none() && f.virtio.is_none(),
            "{name}"
        );
    }
}

#[test]
fn the_requester_id_and_ecam_offset_of_a_function() {
    let b = Bdf::new(0, 4, 0).unwrap();
    assert_eq!(
        b.rid(),
        0x20,
        "IOMMU0: the device at slot 4 is device id 0x20"
    );
    assert_eq!(b.ecam_offset(), 4 << 15);
    let b = Bdf::new(3, 31, 7).unwrap();
    assert_eq!(b.rid(), 0x3ff);
    assert_eq!(Bdf::from_rid(0x3ff), b);
    assert_eq!(b.ecam_offset(), (3 << 20) | (31 << 15) | (7 << 12));
    assert_eq!(format!("{b}"), "03:1f.7");
    assert_eq!(Bdf::new(0, 32, 0), None);
    assert_eq!(Bdf::new(0, 0, 8), None);
}

#[test]
fn an_absent_function_reads_all_ones() {
    let cfg = vec![0xffu8; 256];
    assert_eq!(header(&cfg), Ok(None));
    assert_eq!(
        describe(Bdf::new(0, 9, 0).unwrap(), &cfg, &[0; 6]),
        Ok(None)
    );
}

#[test]
fn the_images_assign_like_the_bus_does() {
    // the five functions' BARs in QEMU virt's 32-bit window (iommutree: 0x4000_0000, 1 GiB): 16 KiB BARs first, then the pages
    let mut all = Vec::new();
    for (slot, name) in NAMES {
        let (cfg, masks) = fixture(name);
        let f = describe(Bdf::new(0, slot, 0).unwrap(), &cfg, &masks)
            .unwrap()
            .unwrap();
        all.extend(f.bars.iter().map(|b| (f.bdf, *b)));
    }
    let a = assign((0x4000_0000, 0x4000_0000), &all).unwrap();
    let at: Vec<(u8, u8, u64)> = a.iter().map(|x| (x.bdf.dev, x.index, x.addr)).collect();
    assert_eq!(
        at,
        vec![
            (1, 4, 0x4000_0000),
            (2, 4, 0x4000_4000),
            (3, 4, 0x4000_8000),
            (1, 1, 0x4000_c000),
            (2, 1, 0x4000_d000),
            (3, 1, 0x4000_e000),
            (4, 0, 0x4000_f000),
        ]
    );
    assert_eq!(
        bar_writes(&a[0]),
        vec![(0x20, 0x4000_0000), (0x24, 0)],
        "a 64-bit BAR is two writes"
    );
    assert_eq!(bar_writes(&a[3]), vec![(0x14, 0x4000_c000)]);
}
