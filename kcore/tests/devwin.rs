//! The device windows (PL5, D1): built from the device tree and failing closed. Expectations for the real QEMU `virt` tree come from `dtc`'s
//! own listing of it, not from the code under test; the synthetic trees cover what the real one does not: a device the system does not
//! know, a near-miss compatible string, a disabled node, a bus that translates addresses, a device sharing a page with an interrupt
//! controller, ranges cut in several places, one-cell addresses. Since IOMMU12 (IOMMU-SPEC 2.6, Q-IOMMU-MODEL 7) `virtio,mmio` is not
//! admitted: its slots are DMA-capable and not behind the IOMMU, so the synthetic trees use the RTC's compatible for an admitted device.
mod common;
use common::Dt;
use kcore::devwin::{device_windows, in_window};
use kcore::fdt;

const VIRT_AIA: &[u8] = include_bytes!("fixtures/virt-aia.dtb");
const VIRT_IOMMU: &[u8] = include_bytes!("fixtures/virt-aia-iommu.dtb");
/// The compatible of the one admitted platform device that is not a PCI host (IOMMU-SPEC 2.6: no DMA ability).
const RTC: &str = "google,goldfish-rtc";
const CONSOLE: (u64, u64) = (0x1000_0000, 0x100);

fn windows_of(blob: &[u8], excluded: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let nodes = fdt::parse(blob).unwrap();
    device_windows(&nodes, excluded, &fdt::memory_ranges(&nodes))
}

#[test]
fn the_real_virt_tree_gives_the_rtc_and_nothing_else() {
    let w = windows_of(VIRT_AIA, &[CONSOLE]);
    // dtc's listing of the tree: the goldfish RTC page; the eight virtio-mmio slots 0x10001000 .. 0x10008000 are not behind the IOMMU
    // and are no window (IOMMU12), and this tree's PCI host has no `iommu-map`
    assert_eq!(w, vec![(0x10_1000, 0x10_2000)]);
    // the boundaries the suite probes (device_caps_need_the_resource, no_dma_device_outside_the_iommu)
    assert!(in_window(&w, 0x10_1000, 4096), "the RTC");
    for slot in 0..8u64 {
        assert!(
            !in_window(&w, 0x1000_1000 + slot * 0x1000, 4096),
            "virtio-mmio slot {slot}"
        );
    }
    assert!(!in_window(&w, 0x1000_0000, 4096), "the console's page");
    assert!(!in_window(&w, 0x10_1000, 8192), "straddling the RTC's end");
    assert!(!in_window(&w, 0x8000_0000, 4096), "RAM");
    // everything else the live tree holds is refused: the test finisher, the CLINT, both APLICs (the M-level one too), both IMSIC files,
    // fw-cfg (its DMA interface writes guest RAM), both flash banks (writable), the PCI ECAM window
    for (pa, what) in [
        (0x10_0000, "the test finisher"),
        (0x200_0000, "the CLINT"),
        (0xc00_0000, "the M-level APLIC"),
        (0xd00_0000, "the S-level APLIC"),
        (0xd00_4000, "the S-level APLIC's upper half"),
        (0x2400_0000, "the M-level IMSIC file"),
        (0x2800_0000, "the S-level IMSIC file"),
        (0x1010_0000, "fw-cfg"),
        (0x2000_0000, "flash bank 0"),
        (0x2200_0000, "flash bank 1"),
        (
            0x3000_0000,
            "the PCI ECAM window (this tree has no IOMMU: its host has no iommu-map)",
        ),
    ] {
        assert!(!in_window(&w, pa, 4096), "{what} at {pa:#x}");
    }
    assert!(!in_window(&w, 0x10_1001, 4096), "misaligned");
    assert!(!in_window(&w, 0x10_1000, 0), "empty");
    assert!(!in_window(&w, 0x10_1000, 100), "not a whole page");
    assert!(!in_window(&w, u64::MAX - 4095, 8192), "wraps");
}

/// IOMMU12 (IOMMU-SPEC 2.6, the conformance row "No DMA device outside the IOMMU"): on the machine the system runs on (QEMU `virt` with
/// the IOMMU), the windows are exactly the RTC page, the ECAM window and the PCI host's two memory windows; no virtio-mmio slot, fw-cfg,
/// flash bank or the IOMMU's own registers. The expectation is `dtc`'s listing of the tree.
#[test]
fn on_the_iommu_machine_the_windows_are_the_rtc_the_ecam_and_the_pci_memory_windows() {
    let w = windows_of(VIRT_IOMMU, &[CONSOLE]);
    assert_eq!(
        w,
        vec![
            (0x10_1000, 0x10_2000),
            (0x3000_0000, 0x8000_0000),
            (0x4_0000_0000, 0x8_0000_0000),
        ],
        "the RTC; the ECAM (0x30000000..0x40000000) and the 32-bit window (0x40000000..0x80000000), which touch; the 64-bit window"
    );
    for slot in 0..8u64 {
        assert!(
            !in_window(&w, 0x1000_1000 + slot * 0x1000, 4096),
            "virtio-mmio slot {slot}"
        );
    }
    for (pa, what) in [
        (0x1010_0000u64, "fw-cfg"),
        (0x301_0000, "the IOMMU's registers"),
        (0x300_0000, "the PCI I/O-port window"),
        (0x2000_0000, "flash bank 0"),
        (0x2800_0000, "the S-level IMSIC file"),
    ] {
        assert!(!in_window(&w, pa, 4096), "{what} at {pa:#x}");
    }
}

/// IOMMU12 (Q-IOMMU-MODEL 7): `virtio,mmio` is never admitted, however the node looks: enabled, on a bus that passes addresses
/// through, alone or beside an admitted device.
#[test]
fn virtio_mmio_yields_no_window() {
    let w = windows_of(
        &machine(&[
            (
                "virtio_mmio@50000000",
                "virtio,mmio",
                "okay",
                vec![(0x5000_0000, 0x1000)],
            ),
            (
                "virtio_mmio@50001000",
                "virtio,mmio",
                "",
                vec![(0x5000_1000, 0x1000)],
            ),
            dev("rtc@60000000", 0x6000_0000, 0x1000),
        ]),
        &[],
    );
    assert_eq!(w, vec![(0x6000_0000, 0x6000_1000)]);
    assert!(!kcore::devwin::ALLOWED.contains(&"virtio,mmio"));
    let nodes = fdt::parse(VIRT_IOMMU).unwrap();
    assert!(
        nodes.iter().any(|n| n.is_compatible("virtio,mmio")),
        "the real tree has virtio-mmio slots"
    );
    assert!(
        kcore::devwin::admitted(&nodes).all(|n| !n.is_compatible("virtio,mmio")),
        "no virtio-mmio node of the real tree is admitted"
    );
}

/// A device in a test tree: `(name, compatible, status, reg)`.
type Dev = (&'static str, &'static str, &'static str, Vec<(u64, u64)>);

/// A machine with a `/soc` that passes addresses through (`ranges;`) holding `(name, compatible, status, reg)` devices.
fn machine(devices: &[Dev]) -> Vec<u8> {
    soc(b"", true, devices)
}

fn soc(soc_ranges: &[u8], has_ranges: bool, devices: &[Dev]) -> Vec<u8> {
    let mut d = Dt::new();
    d.begin("");
    d.u32s("#address-cells", &[2]);
    d.u32s("#size-cells", &[2]);
    d.begin("memory@80000000");
    d.prop("device_type", b"memory\0");
    d.reg(&[(0x8000_0000, 0x1000_0000)]);
    d.end();
    d.begin("cpus");
    d.begin("cpu@0");
    d.reg(&[(0, 0)]);
    d.end();
    d.end();
    d.begin("soc");
    d.u32s("#address-cells", &[2]);
    d.u32s("#size-cells", &[2]);
    if has_ranges {
        d.prop("ranges", soc_ranges);
    }
    for (name, compat, status, regs) in devices {
        d.begin(name);
        let mut c = compat.as_bytes().to_vec();
        c.push(0);
        d.prop("compatible", &c);
        if !status.is_empty() {
            let mut st = status.as_bytes().to_vec();
            st.push(0);
            d.prop("status", &st);
        }
        d.reg(regs);
        d.end();
    }
    d.end();
    d.end();
    d.finish()
}

/// An admitted device (the RTC's compatible) at `(a, s)`.
fn dev(name: &'static str, a: u64, s: u64) -> Dev {
    (name, RTC, "", vec![(a, s)])
}

#[test]
fn an_admitted_device_gets_its_pages_and_neighbours_merge() {
    let w = windows_of(
        &machine(&[
            dev("v@50000100", 0x5000_0100, 0x100), // inside one page, unaligned
            dev("v@50001000", 0x5000_1000, 0x1800), // touches the first page's neighbour
            dev("v@60000000", 0x6000_0000, 0x4000),
            (
                "rtc@70000000",
                "google,goldfish-rtc",
                "okay",
                vec![(0x7000_0000, 0x1000)],
            ),
            (
                "multi@80000000",
                RTC,
                "",
                vec![(0x9000_0000, 0x1000), (0x9000_8000, 0x1000)],
            ), // two ranges in one reg
        ]),
        &[],
    );
    assert_eq!(
        w,
        vec![
            (0x5000_0000, 0x5000_3000),
            (0x6000_0000, 0x6000_4000),
            (0x7000_0000, 0x7000_1000),
            (0x9000_0000, 0x9000_1000),
            (0x9000_8000, 0x9000_9000),
        ]
    );
}

#[test]
fn a_device_nobody_decided_on_is_never_a_window() {
    let w = windows_of(
        &machine(&[
            (
                "fw@10100000",
                "qemu,fw-cfg-mmio",
                "",
                vec![(0x1010_0000, 0x18)],
            ),
            (
                "flash@20000000",
                "cfi-flash",
                "",
                vec![(0x2000_0000, 0x200_0000)],
            ),
            (
                "pci@30000000",
                "pci-host-ecam-generic",
                "",
                vec![(0x3000_0000, 0x1000_0000)],
            ),
            (
                "near@40000000",
                "google,goldfish-rtc-foo",
                "",
                vec![(0x4000_0000, 0x1000)],
            ), // a near miss is not the string
            (
                "near@41000000",
                "xgoogle,goldfish-rtc",
                "",
                vec![(0x4100_0000, 0x1000)],
            ),
            dev("ok@50000000", 0x5000_0000, 0x1000),
        ]),
        &[],
    );
    assert_eq!(w, vec![(0x5000_0000, 0x5000_1000)]);
    // a node with no compatible is not a device either
    let mut d = Dt::new();
    d.begin("");
    d.u32s("#address-cells", &[2]);
    d.u32s("#size-cells", &[2]);
    d.begin("mystery@50000000");
    d.reg(&[(0x5000_0000, 0x1000)]);
    d.end();
    d.end();
    assert_eq!(windows_of(&d.finish(), &[]), vec![]);
}

#[test]
fn a_disabled_node_and_a_bus_that_translates_or_has_no_ranges_give_nothing() {
    // firmware marks the devices it owns disabled
    let w = windows_of(
        &machine(&[
            ("off@50000000", RTC, "disabled", vec![(0x5000_0000, 0x1000)]),
            ("res@51000000", RTC, "reserved", vec![(0x5100_0000, 0x1000)]),
            ("on@52000000", RTC, "okay", vec![(0x5200_0000, 0x1000)]),
        ]),
        &[],
    );
    assert_eq!(w, vec![(0x5200_0000, 0x5200_1000)]);
    // a bus whose `ranges` translate (child address 0 -> parent 0x4000000, 0x2000000 long): the child's reg is bus-local, not physical
    let mut tr = Vec::new();
    for x in [0u32, 0, 0, 0x400_0000, 0, 0x200_0000] {
        tr.extend(x.to_be_bytes());
    }
    let dev = [dev("v@5000", 0x5000, 0x1000)];
    assert_eq!(
        windows_of(&soc(&tr, true, &dev), &[]),
        vec![],
        "translating ranges"
    );
    assert_eq!(
        windows_of(&soc(&[], false, &dev), &[]),
        vec![],
        "no ranges at all"
    );
    assert_eq!(
        windows_of(&soc(&[], true, &dev), &[]),
        vec![(0x5000, 0x6000)],
        "identity ranges: the control"
    );
}

#[test]
fn what_the_kernel_drives_and_ram_are_cut_from_every_window() {
    // a device range that spans an interrupt controller and a console page, and one on RAM
    let mut all = vec![dev("big@40000000", 0x4000_0000, 0x10000)];
    all.push((
        "imsic@40004000",
        "riscv,imsics",
        "",
        vec![(0x4000_4000, 0x1000)],
    ));
    all.push((
        "clint@40008800",
        "riscv,clint0",
        "",
        vec![(0x4000_8800, 0x100)],
    )); // a small controller inside a page: the whole page goes
    all.push(dev("onram@88000000", 0x8800_0000, 0x1000));
    let w = windows_of(&machine(&all), &[(0x4000_2800, 0x100)]); // the caller's console, unaligned
    assert_eq!(
        w,
        vec![
            (0x4000_0000, 0x4000_2000),
            (0x4000_3000, 0x4000_4000),
            (0x4000_5000, 0x4000_8000),
            (0x4000_9000, 0x4001_0000)
        ]
    );
    // overlapping and duplicate cuts, several holes in one range
    let w = windows_of(
        &machine(&[dev("big@40000000", 0x4000_0000, 0x10000)]),
        &[
            (0x4000_2000, 0x2000),
            (0x4000_3000, 0x1000),
            (0x4000_3000, 0x1000),
            (0x4000_9000, 0x1000),
        ],
    );
    assert_eq!(
        w,
        vec![
            (0x4000_0000, 0x4000_2000),
            (0x4000_4000, 0x4000_9000),
            (0x4000_a000, 0x4001_0000)
        ]
    );
}

#[test]
fn a_tree_with_one_cell_addresses_and_one_with_no_devices() {
    let mut d = Dt::new();
    d.begin("");
    d.u32s("#address-cells", &[1]);
    d.u32s("#size-cells", &[1]);
    d.begin("soc");
    d.u32s("#address-cells", &[1]);
    d.u32s("#size-cells", &[1]);
    d.prop("ranges", &[]);
    d.begin("v@10001000");
    d.prop("compatible", b"google,goldfish-rtc\0");
    d.reg_1cell(0x1000_1000, 0x1000);
    d.end();
    d.end();
    d.end();
    assert_eq!(
        windows_of(&d.finish(), &[]),
        vec![(0x1000_1000, 0x1000_2000)]
    );
    assert_eq!(windows_of(&machine(&[]), &[]), vec![]);
    assert!(!in_window(&[], 0x1000_1000, 4096));
}

#[test]
fn a_cut_that_starts_at_a_windows_start_leaves_no_empty_range() {
    // (KT10: the mutant "ca > a gives ca >= a" in `device_windows` survived the first mutation run: it pushes the empty range [a, a))
    let w = windows_of(VIRT_IOMMU, &[CONSOLE, (0x3000_0000, 0x1000)]);
    assert_eq!(
        w,
        vec![
            (0x10_1000, 0x10_2000),
            (0x3000_1000, 0x8000_0000),
            (0x4_0000_0000, 0x8_0000_0000)
        ],
        "the first ECAM page is cut away from the front of its run, and nothing empty is left behind"
    );
    assert!(w.iter().all(|&(a, b)| a < b), "every range is non-empty");
}

#[test]
fn a_cut_that_ends_at_a_windows_end_leaves_no_empty_range() {
    // (KT10: the mutant "a < b gives a <= b" after the cut loop of `device_windows`: it pushes the empty range [b, b))
    let w = windows_of(VIRT_IOMMU, &[CONSOLE, (0x7_ffff_f000, 0x1000)]);
    assert_eq!(
        w,
        vec![
            (0x10_1000, 0x10_2000),
            (0x3000_0000, 0x8000_0000),
            (0x4_0000_0000, 0x7_ffff_f000)
        ],
        "the last page of the 64-bit window is cut away from the end of its run, and nothing empty is left behind"
    );
    assert!(w.iter().all(|&(a, b)| a < b), "every range is non-empty");
}

/// IOMMU12 review: an admitted device whose `reg` is less than a page is widened to the page, and a device that is not admitted in the
/// same page (here a `virtio,mmio` slot, whose DMA is outside the IOMMU) must not become mappable through it: the neighbour's page is
/// cut, so the page is no window at all; a neighbour in another page leaves the admitted page alone.
#[test]
fn a_sub_page_device_never_opens_its_page_to_a_neighbour_that_is_not_admitted() {
    let w = windows_of(
        &machine(&[
            dev("rtc@50000100", 0x5000_0100, 0x100),
            (
                "virtio_mmio@50000800",
                "virtio,mmio",
                "",
                vec![(0x5000_0800, 0x200)],
            ),
            dev("rtc@60000000", 0x6000_0000, 0x100),
            (
                "virtio_mmio@60001000",
                "virtio,mmio",
                "",
                vec![(0x6000_1000, 0x200)],
            ),
            dev("rtc@70000000", 0x7000_0000, 0x100),
            ("rtc@70000800", RTC, "disabled", vec![(0x7000_0800, 0x100)]), // a disabled one is not admitted either
        ]),
        &[],
    );
    assert_eq!(w, vec![(0x6000_0000, 0x6000_1000)]);
}
