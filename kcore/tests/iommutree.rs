//! The platform IOMMU from the device tree (IOMMU5; IOMMU-SPEC 2.2, 2.5, 2.6, 3.3). Expectations for the real QEMU `virt` tree
//! (`-machine virt,aia=aplic-imsic,iommu-sys=on -cpu rva23s64 -smp 4 -m 512M`, dumped with `dumpdtb` on 2026-11-26) come from
//! `dtc`'s listing of it, not from the code under test: `iommu@3010000` with `reg = <0 0x3010000 0 0x1000>`,
//! `interrupts = <0x24 1 0x25 1 0x26 1 0x27 1>` on the supervisor APLIC (phandle 0x0c), phandle 0x8000; `pci@30000000` with
//! `iommu-map = <0 0x8000 0 0x10000>`, ECAM `reg = <0 0x30000000 0 0x10000000>` and `ranges` holding an I/O window at 0x3000000
//! (0x10000), 32-bit memory at 0x40000000 (0x40000000) and 64-bit memory at 0x4_0000_0000 (0x4_0000_0000). The hostile cases are
//! the real tree with one property changed.
use kcore::devwin::device_windows;
use kcore::fdt::{self, Node};
use kcore::iommutree::{self, discover, highest_address, Absent, Cause, MapEntry, Source};

const IOMMU_TREE: &[u8] = include_bytes!("fixtures/virt-aia-iommu.dtb");
const NO_IOMMU_TREE: &[u8] = include_bytes!("fixtures/virt-aia.dtb");
const CONSOLE: (u64, u64) = (0x1000_0000, 0x100);

fn nodes() -> Vec<Node> {
    fdt::parse(IOMMU_TREE).unwrap()
}

fn be(cells: &[u32]) -> Vec<u8> {
    cells.iter().flat_map(|c| c.to_be_bytes()).collect()
}

/// The real tree with property `name` of the node at `path` replaced (or removed, for `None`).
fn with(path: &str, name: &str, value: Option<Vec<u8>>) -> Vec<Node> {
    let mut n = nodes();
    let node = n.iter_mut().find(|x| x.path == path).unwrap();
    node.props.retain(|(k, _)| k != name);
    if let Some(v) = value {
        node.props.push((name.into(), v));
    }
    n
}

const IOMMU: &str = "/soc/iommu@3010000";
const PCI: &str = "/soc/pci@30000000";

#[test]
fn the_real_tree_gives_the_registers_the_sources_and_the_map() {
    let i = discover(&nodes()).unwrap();
    assert_eq!((i.base, i.size), (0x301_0000, 0x1000));
    assert_eq!(
        i.sources,
        [0x24, 0x25, 0x26, 0x27].map(|number| Source { number, trigger: 1 })
    );
    assert_eq!(i.source(Cause::CommandQueue).unwrap().number, 36);
    assert_eq!(i.source(Cause::FaultQueue).unwrap().number, 37);
    assert_eq!(i.pci.len(), 1);
    assert_eq!(i.pci[0].ecam, (0x3000_0000, 0x1000_0000));
    assert_eq!(
        i.pci[0].memory,
        vec![(0x4000_0000, 0x4000_0000), (0x4_0000_0000, 0x4_0000_0000)],
        "the two memory windows of ranges; not the I/O window at 0x3000000"
    );
    assert_eq!(
        i.pci[0].map,
        vec![MapEntry {
            rid_base: 0,
            did_base: 0,
            len: 0x10000
        }]
    );
    assert_eq!(i.device_ids, 0x10000);
}

#[test]
fn a_tree_without_the_iommu_is_the_refusal() {
    let n = fdt::parse(NO_IOMMU_TREE).unwrap();
    assert_eq!(discover(&n), Err(Absent::None));
    assert!(Absent::None.to_string().contains("no IOMMU"));
    // two nodes: only one is supported
    let mut n = nodes();
    let mut twin = n.iter().find(|x| x.path == IOMMU).unwrap().clone();
    twin.path = "/soc/iommu@3020000".into();
    n.push(twin);
    assert_eq!(discover(&n), Err(Absent::Several));
    assert!(Absent::Several.to_string().contains("no IOMMU"));
    // disabled: not there
    assert_eq!(
        discover(&with(IOMMU, "status", Some(b"disabled\0".to_vec()))),
        Err(Absent::None)
    );
}

#[test]
fn a_malformed_node_is_the_refusal_not_a_guess() {
    let bad = |n: Vec<Node>, what: &str| {
        let e = discover(&n).expect_err(what);
        assert!(matches!(e, Absent::Malformed(_)), "{what}: {e:?}");
        assert!(e.to_string().contains("no IOMMU"), "{what}");
    };
    bad(with(IOMMU, "reg", None), "no reg");
    bad(
        with(IOMMU, "reg", Some(be(&[0, 0x301_0800, 0, 0x1000]))),
        "unaligned registers",
    );
    bad(
        with(IOMMU, "reg", Some(be(&[0, 0x301_0000, 0, 0x800]))),
        "half a page",
    );
    bad(with(IOMMU, "phandle", None), "no phandle");
    bad(
        with(IOMMU, "interrupt-parent", Some(be(&[0x0b]))),
        "the M-level APLIC",
    );
    bad(
        with(IOMMU, "interrupts", Some(be(&[0x24, 1]))),
        "only one source",
    );
    bad(
        with(IOMMU, "interrupts", Some(be(&[0x24, 1, 0x25]))),
        "odd cells",
    );
    bad(
        with(IOMMU, "interrupts", Some(be(&[0x24, 1, 0x24, 1]))),
        "the same source twice",
    );
    bad(
        with(IOMMU, "interrupts", Some(be(&[0, 1, 0x25, 1]))),
        "source 0",
    );
    bad(
        with(IOMMU, "interrupts", Some(be(&[0x24, 3, 0x25, 1]))),
        "an unknown trigger",
    );
    bad(
        with(IOMMU, "interrupts", Some(be(&[2000, 1, 0x25, 1]))),
        "past 1023",
    );
    bad(
        with(PCI, "iommu-map", Some(be(&[0, 0x1234, 0, 0x10000]))),
        "a map naming another IOMMU",
    );
    bad(
        with(
            PCI,
            "iommu-map",
            Some(be(&[0, 0x8000, 0xff_0000, 0x10_0000])),
        ),
        "device ids past 24 bits",
    );
    bad(
        with(PCI, "iommu-map", Some(be(&[0, 0x8000, 0]))),
        "a short map",
    );
    bad(
        with(PCI, "ranges", Some(be(&[0x0200_0000, 0, 0x4000_0000]))),
        "short ranges",
    );
}

#[test]
fn a_pci_host_without_a_map_is_not_behind_the_iommu_and_opens_no_window() {
    let n = with(PCI, "iommu-map", None);
    assert!(discover(&n).unwrap().pci.is_empty());
    assert!(iommutree::pci_windows(&n).is_empty());
    let w = device_windows(&n, &[CONSOLE], &fdt::memory_ranges(&n));
    assert_eq!(w, vec![(0x10_1000, 0x10_2000), (0x1000_1000, 0x1000_9000)]);
}

/// IOMMU-SPEC 2.6: behind the IOMMU, the ECAM window and the two memory windows join the RTC and the virtio slots; the I/O-port
/// window, the IOMMU's register page and the interrupt controllers stay out.
#[test]
fn the_real_tree_opens_the_pci_windows_and_never_the_iommu() {
    let n = nodes();
    let w = device_windows(&n, &[CONSOLE], &fdt::memory_ranges(&n));
    assert_eq!(
        w,
        vec![
            (0x10_1000, 0x10_2000),
            (0x1000_1000, 0x1000_9000),
            (0x3000_0000, 0x8000_0000),
            (0x4_0000_0000, 0x8_0000_0000),
        ],
        "the ECAM (0x30000000..0x40000000) and the 32-bit window (0x40000000..0x80000000) touch, and merge"
    );
    for pa in [
        0x301_0000u64,
        0x300_0000,
        0x300_f000,
        0xd00_0000,
        0x2800_0000,
    ] {
        assert!(!kcore::devwin::in_window(&w, pa, 4096), "{pa:#x}");
    }
}

#[test]
fn the_highest_address_is_the_top_of_the_64_bit_window() {
    assert_eq!(highest_address(&nodes()), Some(0x8_0000_0000));
    // a 64-bit window that reaches IO_BASE (2^40) is what the boot refuses
    let n = with(
        PCI,
        "ranges",
        Some(be(&[0x0300_0000, 0x100, 0, 0x100, 0, 0x1, 0])),
    );
    assert_eq!(highest_address(&n), Some((1 << 40) + (1 << 32)));
    // RAM above it too
    let n = with("/memory@80000000", "reg", Some(be(&[0x100, 0, 0, 0x1000])));
    assert!(highest_address(&n).unwrap() > 1 << 40);
}

#[test]
fn the_pci_hosts_intx_and_the_iommus_sources_are_never_a_processs() {
    let a = kcore::aiatree::discover(&nodes()).unwrap();
    assert_eq!(a.sources, vec![1, 2, 3, 4, 5, 6, 7, 8, 11]);
}
