//! The device windows (PL5, D1): built from the device tree and failing closed. Expectations for the real QEMU `virt` tree come from `dtc`'s
//! own listing of it, not from the code under test; the synthetic trees cover what the real one does not: a device the system does not
//! know, a near-miss compatible string, a disabled node, a bus that translates addresses, a device sharing a page with an interrupt
//! controller, ranges cut in several places, one-cell addresses.
mod common;
use common::Dt;
use kcore::devwin::{device_windows, in_window};
use kcore::fdt;

const VIRT_AIA: &[u8] = include_bytes!("fixtures/virt-aia.dtb");
const CONSOLE: (u64, u64) = (0x1000_0000, 0x100);

fn windows_of(blob: &[u8], excluded: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let nodes = fdt::parse(blob).unwrap();
    device_windows(&nodes, excluded, &fdt::memory_ranges(&nodes))
}

#[test]
fn the_real_virt_tree_gives_the_rtc_and_the_virtio_slots_and_nothing_else() {
    let w = windows_of(VIRT_AIA, &[CONSOLE]);
    // dtc's listing of the tree: the goldfish RTC page, and the eight virtio-mmio slots 0x10001000 .. 0x10008000 as one run
    assert_eq!(w, vec![(0x10_1000, 0x10_2000), (0x1000_1000, 0x1000_9000)]);
    // the boundaries the suite probes (device_caps_need_the_resource)
    assert!(in_window(&w, 0x10_1000, 4096), "the RTC");
    assert!(in_window(&w, 0x1000_1000, 4096));
    assert!(in_window(&w, 0x1000_8000, 4096));
    assert!(!in_window(&w, 0x1000_0000, 4096), "the console's page");
    assert!(
        !in_window(&w, 0x1000_9000, 4096),
        "just above the virtio slots"
    );
    assert!(!in_window(&w, 0x1000_8000, 8192), "straddling the end");
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
        (0x3000_0000, "the PCI ECAM window"),
    ] {
        assert!(!in_window(&w, pa, 4096), "{what} at {pa:#x}");
    }
    assert!(!in_window(&w, 0x1000_1001, 4096), "misaligned");
    assert!(!in_window(&w, 0x1000_1000, 0), "empty");
    assert!(!in_window(&w, 0x1000_1000, 100), "not a whole page");
    assert!(!in_window(&w, u64::MAX - 4095, 8192), "wraps");
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

fn virtio(name: &'static str, a: u64, s: u64) -> Dev {
    (name, "virtio,mmio", "", vec![(a, s)])
}

#[test]
fn an_admitted_device_gets_its_pages_and_neighbours_merge() {
    let w = windows_of(
        &machine(&[
            virtio("v@50000100", 0x5000_0100, 0x100), // inside one page, unaligned
            virtio("v@50001000", 0x5000_1000, 0x1800), // touches the first page's neighbour
            virtio("v@60000000", 0x6000_0000, 0x4000),
            (
                "rtc@70000000",
                "google,goldfish-rtc",
                "okay",
                vec![(0x7000_0000, 0x1000)],
            ),
            (
                "multi@80000000",
                "virtio,mmio",
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
                "virtio,mmio-foo",
                "",
                vec![(0x4000_0000, 0x1000)],
            ), // a near miss is not the string
            (
                "near@41000000",
                "xvirtio,mmio",
                "",
                vec![(0x4100_0000, 0x1000)],
            ),
            virtio("ok@50000000", 0x5000_0000, 0x1000),
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
            (
                "off@50000000",
                "virtio,mmio",
                "disabled",
                vec![(0x5000_0000, 0x1000)],
            ),
            (
                "res@51000000",
                "virtio,mmio",
                "reserved",
                vec![(0x5100_0000, 0x1000)],
            ),
            (
                "on@52000000",
                "virtio,mmio",
                "okay",
                vec![(0x5200_0000, 0x1000)],
            ),
        ]),
        &[],
    );
    assert_eq!(w, vec![(0x5200_0000, 0x5200_1000)]);
    // a bus whose `ranges` translate (child address 0 -> parent 0x4000000, 0x2000000 long): the child's reg is bus-local, not physical
    let mut tr = Vec::new();
    for x in [0u32, 0, 0, 0x400_0000, 0, 0x200_0000] {
        tr.extend(x.to_be_bytes());
    }
    let dev = [virtio("v@5000", 0x5000, 0x1000)];
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
    let mut all = vec![virtio("big@40000000", 0x4000_0000, 0x10000)];
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
    all.push(virtio("onram@88000000", 0x8800_0000, 0x1000));
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
        &machine(&[virtio("big@40000000", 0x4000_0000, 0x10000)]),
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
    d.prop("compatible", b"virtio,mmio\0");
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
