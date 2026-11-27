//! IOMMU8: BAR sizing and assignment as properties over random device sets, and hostile configuration spaces (a function's bytes
//! are controlled by the device): every one ends in an error, never a hang or a panic.
use pcicore::*;

/// xorshift64*: a fixed seed per test, so a failure reproduces.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// The registers a BAR of `kind` and `size` reads back after all ones were written (the specification's sizing rule, written here
/// from the text: the address bits below the size read 0, the type bits are read-only).
fn masks_of(bars: &[(u8, BarKind, u64)]) -> [u32; 6] {
    let mut m = [0u32; 6];
    for &(i, k, size) in bars {
        let i = i as usize;
        match k {
            BarKind::Mem32 { prefetchable } => {
                m[i] = (!(size as u32 - 1) & !0xf) | if prefetchable { 8 } else { 0 };
            }
            BarKind::Mem64 { prefetchable } => {
                let a = !(size - 1) & !0xf;
                m[i] = a as u32 | 4 | if prefetchable { 8 } else { 0 };
                m[i + 1] = (a >> 32) as u32;
            }
            BarKind::Io => m[i] = (!(size as u32 - 1) & 0xfffc) | 1,
        }
    }
    m
}

fn random_bars(r: &mut Rng) -> Vec<(u8, BarKind, u64)> {
    let mut out = Vec::new();
    let mut i = 0u8;
    while i < 6 {
        match r.below(4) {
            0 => i += 1,
            1 => {
                out.push((
                    i,
                    BarKind::Mem32 {
                        prefetchable: r.below(2) == 1,
                    },
                    1 << (4 + r.below(21)),
                ));
                i += 1;
            }
            2 if i < 5 => {
                out.push((
                    i,
                    BarKind::Mem64 {
                        prefetchable: r.below(2) == 1,
                    },
                    1 << (4 + r.below(25)),
                ));
                i += 2;
            }
            _ => {
                out.push((i, BarKind::Io, 1 << (2 + r.below(6))));
                i += 1;
            }
        }
    }
    out
}

#[test]
fn sizing_decodes_what_the_registers_say() {
    let mut r = Rng(0x1234_5678_9abc_def1);
    for _ in 0..5000 {
        let spec = random_bars(&mut r);
        let got = decode_bars(&masks_of(&spec)).unwrap();
        let want: Vec<Bar> = spec
            .iter()
            .map(|&(index, kind, size)| Bar { index, kind, size })
            .collect();
        assert_eq!(got, want);
    }
}

#[test]
fn assignment_over_random_device_sets_never_overlaps_and_fits() {
    let mut r = Rng(0x0dd_ba11_cafe_f00d);
    let mut fitted = 0;
    let mut refused = 0;
    for _ in 0..3000 {
        // QEMU virt's 32-bit window, or smaller ones, so that both outcomes come up
        let window = (0x4000_0000u64, 1u64 << (20 + r.below(11)));
        let mut all = Vec::new();
        for dev in 0..r.below(12) as u8 {
            let bdf = Bdf::new(0, dev, 0).unwrap();
            for (index, kind, size) in random_bars(&mut r) {
                all.push((bdf, Bar { index, kind, size }));
            }
        }
        let need: u64 = all
            .iter()
            .filter(|(_, b)| b.is_memory())
            .map(|(_, b)| b.size.max(PAGE))
            .sum();
        match assign(window, &all) {
            Ok(a) => {
                fitted += 1;
                assert_eq!(a.len(), all.iter().filter(|(_, b)| b.is_memory()).count());
                for x in &a {
                    assert_eq!(x.addr % x.size.max(PAGE), 0, "natural alignment");
                    assert!(
                        x.addr >= window.0 && x.addr + x.span <= window.0 + window.1,
                        "inside the window"
                    );
                    assert!(x.span >= x.size && x.span % PAGE == 0);
                    if let BarKind::Mem32 { .. } = x.kind {
                        assert!(x.addr + x.span <= 1 << 32);
                    }
                }
                for (i, x) in a.iter().enumerate() {
                    for y in &a[i + 1..] {
                        assert!(
                            x.addr + x.span <= y.addr || y.addr + y.span <= x.addr,
                            "{x:?} overlaps {y:?}"
                        );
                    }
                }
                // deterministic: the same set in another order gets the same addresses
                let mut shuffled = all.clone();
                shuffled.reverse();
                assert_eq!(assign(window, &shuffled).unwrap(), a);
            }
            Err(e) => {
                refused += 1;
                assert_eq!(e, Error::NoSpace);
                // largest first with power-of-two spans packs without gaps: a refusal means it truly does not fit
                assert!(
                    need > window.1,
                    "refused {need:#x} bytes in a window of {:#x}",
                    window.1
                );
            }
        }
    }
    assert!(
        fitted > 100 && refused > 10,
        "both outcomes were exercised ({fitted}, {refused})"
    );
}

#[test]
fn a_32_bit_bar_is_never_placed_above_4_gib() {
    let bdf = Bdf::new(0, 1, 0).unwrap();
    let b32 = Bar {
        index: 1,
        kind: BarKind::Mem32 {
            prefetchable: false,
        },
        size: 0x1000,
    };
    let b64 = Bar {
        index: 4,
        kind: BarKind::Mem64 { prefetchable: true },
        size: 0x4000,
    };
    let high = (0x4_0000_0000, 0x4_0000_0000);
    assert_eq!(assign(high, &[(bdf, b32)]), Err(Error::NoSpace));
    assert_eq!(assign(high, &[(bdf, b64)]).unwrap()[0].addr, 0x4_0000_0000);
    // a window that straddles 4 GiB: the 32-bit BAR must not end above it
    assert_eq!(
        assign((0xffff_f000, 0x10_0000), &[(bdf, b64), (bdf, b32)]),
        Err(Error::NoSpace)
    );
    assert_eq!(
        assign((u64::MAX - 0xfff, 0x2000), &[(bdf, b64)]),
        Err(Error::NoSpace),
        "a window that wraps"
    );
}

#[test]
fn bars_of_size_zero_or_everything_or_no_shape_are_refused() {
    let one = |m0: u32, m1: u32| {
        let mut m = [0u32; 6];
        m[0] = m0;
        m[1] = m1;
        decode_bars(&m)
    };
    assert_eq!(
        one(0x0000_0008, 0),
        Err(Error::BarSize(0)),
        "32-bit, no address bits: the whole space"
    );
    assert_eq!(
        one(0x0000_000c, 0),
        Err(Error::BarSize(0)),
        "64-bit, no address bits"
    );
    assert_eq!(
        one(0xff00_f000, 0),
        Err(Error::BarSize(0)),
        "writable bits not contiguous"
    );
    assert_eq!(
        one(0x0000_000c, 0x0000_ffff),
        Err(Error::BarSize(0)),
        "64-bit, not contiguous"
    );
    assert_eq!(
        one(0xffff_f002, 0),
        Err(Error::BarType(1)),
        "type 1 (below 1 MiB, reserved)"
    );
    assert_eq!(one(0xffff_f006, 0), Err(Error::BarType(3)));
    assert_eq!(
        one(0x0000_0001, 0),
        Err(Error::BarSize(0)),
        "an I/O BAR with no address bits"
    );
    let mut m = [0u32; 6];
    m[5] = 0xffff_c00c;
    assert_eq!(decode_bars(&m), Err(Error::Bar64AtEnd(5)));
    // the largest legal ones decode, and then do not fit a window
    let huge = one(0x8000_0000, 0).unwrap();
    assert_eq!(huge[0].size, 0x8000_0000);
    let bdf = Bdf::new(0, 1, 0).unwrap();
    assert_eq!(
        assign((0x4000_0000, 0x4000_0000), &[(bdf, huge[0])]),
        Err(Error::NoSpace)
    );
    let huge64 = one(0x0000_000c, 0x8000_0000).unwrap();
    assert_eq!(huge64[0].size, 1 << 63);
    assert_eq!(
        assign((0x4000_0000, 0x4000_0000), &[(bdf, huge64[0])]),
        Err(Error::NoSpace)
    );
}

/// A plain type-0 function with a capability list starting at `first`, and `caps` written at their offsets: (offset, bytes).
fn image(first: u8, caps: &[(u8, &[u8])]) -> Vec<u8> {
    let mut c = vec![0u8; 256];
    c[0..2].copy_from_slice(&0x1af4u16.to_le_bytes());
    c[2..4].copy_from_slice(&0x1042u16.to_le_bytes());
    c[6] = 0x10; // capability list
    c[0x34] = first;
    for (at, bytes) in caps {
        c[*at as usize..*at as usize + bytes.len()].copy_from_slice(bytes);
    }
    c
}

#[test]
fn a_hostile_capability_list_ends_in_an_error() {
    // a loop: a capability pointing at itself, and two pointing at each other
    assert_eq!(
        capabilities(&image(0x40, &[(0x40, &[0x11, 0x40])])),
        Err(Error::CapLoop)
    );
    assert_eq!(
        capabilities(&image(
            0x40,
            &[(0x40, &[0x09, 0x50]), (0x50, &[0x09, 0x40])]
        )),
        Err(Error::CapLoop)
    );
    // pointers outside the capability area (the low two bits are masked, so 0x3f is 0x3c)
    assert_eq!(
        capabilities(&image(0x3f, &[])),
        Err(Error::CapPointer(0x3c))
    );
    assert_eq!(
        capabilities(&image(0x40, &[(0x40, &[0x09, 0x10])])),
        Err(Error::CapPointer(0x10))
    );
    // a pointer at the end, whose capability's body would run past the conventional space
    let c = image(0xfc, &[(0xfc, &[0x11, 0x00, 0x01, 0x00])]);
    let caps = capabilities(&c).unwrap();
    assert_eq!(msix_cap(&c, &caps), Err(Error::CapPointer(0xfc)));
    let c = image(0xf8, &[(0xf8, &[0x09, 0x00, 0x10, 0x01])]);
    assert_eq!(
        virtio_caps(&c, &capabilities(&c).unwrap()),
        Err(Error::CapPointer(0xf8))
    );
    // the longest legal list: one capability per doubleword from 0x40 to 0xfc
    let mut c = image(0x40, &[]);
    for p in (0x40..0x100).step_by(4) {
        c[p] = 0x0a;
        c[p + 1] = if p == 0xfc { 0 } else { (p + 4) as u8 };
    }
    assert_eq!(capabilities(&c).unwrap().len(), MAX_CAPS);
    // too short a buffer
    assert_eq!(capabilities(&[0u8; 100]), Err(Error::Short));
    assert_eq!(header(&[0u8; 255]), Err(Error::Short));
}

/// The blk image with its MSI-X capability (at 0x98) rewritten: table at `t` (BAR | offset), pending bits at `p`, `n` entries.
fn blk_with_msix(t: u32, p: u32, n: u16) -> (Vec<u8>, [u32; 6]) {
    let text = std::fs::read_to_string(format!(
        "{}/tests/fixtures/virtio-blk-pci.cfg",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
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
    cfg[0x9a..0x9c].copy_from_slice(&(n - 1).to_le_bytes());
    cfg[0x9c..0xa0].copy_from_slice(&t.to_le_bytes());
    cfg[0xa0..0xa4].copy_from_slice(&p.to_le_bytes());
    (cfg, masks)
}

#[test]
fn an_msix_table_in_the_wrong_place_is_refused() {
    let bdf = Bdf::new(0, 1, 0).unwrap();
    let d = |t, p, n| {
        let (c, m) = blk_with_msix(t, p, n);
        describe(bdf, &c, &m)
    };
    assert!(d(1, 0x801, 2).is_ok(), "QEMU's own placement");
    assert_eq!(
        d(1 | 0xff8, 0x801, 2),
        Err(Error::MsixPlacement),
        "the table runs past its 4 KiB BAR"
    );
    assert_eq!(
        d(1, 0x801, 257),
        Err(Error::MsixPlacement),
        "257 entries need 4112 bytes"
    );
    assert_eq!(
        d(1, 0x1, 2),
        Err(Error::MsixPlacement),
        "pending bits over the table"
    );
    assert_eq!(
        d(1, 0x011, 2),
        Err(Error::MsixPlacement),
        "pending bits over the second entry"
    );
    assert_eq!(
        d(1, 0xff8 | 1, 65),
        Err(Error::MsixPlacement),
        "pending bits past the BAR"
    );
    assert_eq!(
        d(4, 0x801, 2),
        Err(Error::MsixPlacement),
        "the table over the common configuration"
    );
    assert_eq!(
        d(4 | 0x3000, 0x801, 2),
        Err(Error::MsixPlacement),
        "the table over the notify structure"
    );
    assert_eq!(
        d(1, 4 | 0x1000, 2),
        Err(Error::MsixPlacement),
        "the pending bits over the ISR"
    );
    assert_eq!(d(4 | 0x3ff0, 0x801, 1), Err(Error::MsixPlacement));
    assert_eq!(
        d(0, 0x801, 2),
        Err(Error::MsixBar(0)),
        "BAR 0 is not implemented"
    );
    assert_eq!(
        d(5, 0x801, 2),
        Err(Error::MsixBar(5)),
        "BAR 5 is BAR 4's upper half"
    );
    assert_eq!(
        d(1, 0x800 | 7, 2),
        Err(Error::MsixBar(7)),
        "BIR 7 is reserved"
    );
    // separate BARs are fine when both fit: the table in BAR 4's unused tail is not over a structure
    // (QEMU's BAR 4 is exactly the four structures, so there is no tail: refused)
    assert_eq!(d(4 | 0x4000, 0x801, 1), Err(Error::MsixPlacement));
}

#[test]
fn a_virtio_function_without_its_structures_is_refused() {
    let (mut cfg, masks) = blk_with_msix(1, 0x801, 2);
    let bdf = Bdf::new(0, 1, 0).unwrap();
    // the common configuration (cap at 0x40, the end of the list) made a device configuration: no common structure
    cfg[0x43] = 4;
    assert_eq!(describe(bdf, &cfg, &masks), Err(Error::VirtioLayout));
    // the ISR structure (0x50) in BAR 1: the structures are split across BARs
    let (mut cfg, masks) = blk_with_msix(1, 0x801, 2);
    cfg[0x54] = 1;
    assert_eq!(describe(bdf, &cfg, &masks), Err(Error::VirtioLayout));
    // the notify structure (0x70) runs past its BAR
    let (mut cfg, masks) = blk_with_msix(1, 0x801, 2);
    cfg[0x78..0x7c].copy_from_slice(&0x3800u32.to_le_bytes());
    assert_eq!(describe(bdf, &cfg, &masks), Err(Error::VirtioCap(0x70)));
    // the notify capability too short for its multiplier
    let (mut cfg, masks) = blk_with_msix(1, 0x801, 2);
    cfg[0x72] = 16;
    assert_eq!(describe(bdf, &cfg, &masks), Err(Error::VirtioCap(0x70)));
    // a common configuration shorter than the registers a driver uses
    let (mut cfg, masks) = blk_with_msix(1, 0x801, 2);
    cfg[0x4c..0x50].copy_from_slice(&0x30u32.to_le_bytes());
    assert_eq!(describe(bdf, &cfg, &masks), Err(Error::VirtioCap(0x40)));
    // a bridge is not configured
    let (mut cfg, masks) = blk_with_msix(1, 0x801, 2);
    cfg[0x0e] = 0x81;
    assert_eq!(describe(bdf, &cfg, &masks), Err(Error::NotType0(1)));
}

#[test]
fn random_bytes_never_hang_or_panic() {
    let mut r = Rng(0xfeed_face_dead_beef);
    let mut errors = 0;
    for round in 0..20000 {
        let mut cfg: Vec<u8> = (0..256).map(|_| r.next() as u8).collect();
        if round % 2 == 0 {
            // make the walk likely: a capability list, a modern virtio id, plausible pointers
            cfg[0..4].copy_from_slice(&0x1042_1af4u32.to_le_bytes());
            cfg[6] |= 0x10;
            cfg[0x0e] = 0;
            for p in (0x34..0x100).step_by(4) {
                if r.below(3) != 0 {
                    cfg[p + 1] = (0x40 + r.below(0xc0) as u8) & !3;
                }
                if r.below(2) == 0 {
                    cfg[p] = [9, 0x11, 0x10, 0x13][r.below(4) as usize];
                }
            }
        }
        let masks: [u32; 6] = core::array::from_fn(|_| r.next() as u32);
        let bdf = Bdf::new(0, (round % 32) as u8, 0).unwrap();
        if describe(bdf, &cfg, &masks).is_err() {
            errors += 1;
        }
        let _ = capabilities(&cfg);
        if let Ok(b) = decode_bars(&masks) {
            let _ = assign(
                (0x4000_0000, 0x4000_0000),
                &b.iter().map(|b| (bdf, *b)).collect::<Vec<_>>(),
            );
        }
    }
    assert!(errors > 1000);
}

#[test]
fn msix_entries_and_message_control_encode_as_the_specification_lays_them_out() {
    // table entry: address low (0x0), address high (0x4), data (0x8), vector control (0xc; bit 0 masks)
    let e = MsixEntry {
        address: 0x100_0000_3000,
        data: 1,
        masked: true,
    };
    assert_eq!(e.encode(), [0x0000_3000, 0x100, 1, 1]);
    assert_eq!(MsixEntry::decode(e.encode()), e);
    assert!(!MsixEntry::decode([0, 0, 7, 0xfffe]).masked);
    // message control: enable is bit 15, the function mask bit 14; the table size field is kept
    assert_eq!(msix_control(0x0001, true, true), 0xc001);
    assert_eq!(msix_control(0xc001, true, false), 0x8001);
    assert_eq!(msix_control(0xc001, false, false), 0x0001);
    let m = Msix {
        cap: 0x98,
        table_size: 3,
        table_bar: 1,
        table_offset: 0x100,
        pba_bar: 1,
        pba_offset: 0x800,
    };
    assert_eq!(m.entry_offset(0), Some(0x100));
    assert_eq!(m.entry_offset(2), Some(0x120));
    assert_eq!(m.entry_offset(3), None);
    assert_eq!(m.pba_bytes(), 8);
    assert_eq!(
        Msix {
            table_size: 65,
            ..m
        }
        .pba_bytes(),
        16
    );
}

#[test]
fn the_function_level_reset_is_found_where_it_is_offered() {
    // PCI Express capability: Device Capabilities (cap + 4) bit 28; Device Control at cap + 8, bit 15 starts it
    let mut c = image(0x40, &[(0x40, &[0x10, 0x00, 0x02, 0x00])]);
    assert_eq!(flr(&c, &capabilities(&c).unwrap()), Ok(None));
    c[0x47] = 0x10;
    let f = flr(&c, &capabilities(&c).unwrap()).unwrap().unwrap();
    assert_eq!(f, Flr::Pcie { devctl: 0x48 });
    assert_eq!(f.start(0x2810), (0x48, 0xa810));
    // Advanced Features: capabilities byte (cap + 3) bit 1; control (cap + 4) bit 0
    let c = image(0x50, &[(0x50, &[0x13, 0x00, 0x06, 0x03])]);
    let f = flr(&c, &capabilities(&c).unwrap()).unwrap().unwrap();
    assert_eq!(f, Flr::Af { control: 0x54 });
    assert_eq!(f.start(0), (0x54, 1));
    // the command register around sizing and after enumeration: INTx always disabled, bus mastering never on
    assert_eq!(command_for_sizing(0x0007), 0x0400);
    assert_eq!(command_enabled(0x0005), 0x0402);
}
