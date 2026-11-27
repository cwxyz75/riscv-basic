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
    bad(with(IOMMU, "#iommu-cells", None), "no #iommu-cells");
    bad(
        with(IOMMU, "#iommu-cells", Some(be(&[2]))),
        "a two-cell specifier",
    );
    bad(
        with(IOMMU, "#iommu-cells", Some(be(&[1, 1]))),
        "#iommu-cells of two cells",
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

/// A map that leaves some requester ids out, folds them with a mask, or names another IOMMU does not put the whole host behind
/// the IOMMU: no PCI window opens (a function outside the map would reach memory untranslated).
#[test]
fn a_pci_host_only_partly_behind_the_iommu_opens_no_window() {
    let only = |n: &[Node]| device_windows(n, &[CONSOLE], &fdt::memory_ranges(n));
    let closed = vec![(0x10_1000, 0x10_2000), (0x1000_1000, 0x1000_9000)];
    for (what, n) in [
        (
            "half the ids",
            with(PCI, "iommu-map", Some(be(&[0, 0x8000, 0, 0x8000]))),
        ),
        (
            "a hole",
            with(
                PCI,
                "iommu-map",
                Some(be(&[0, 0x8000, 0, 0x100, 0x200, 0x8000, 0x200, 0xfe00])),
            ),
        ),
        (
            "another IOMMU",
            with(PCI, "iommu-map", Some(be(&[0, 0x7777, 0, 0x10000]))),
        ),
        ("a mask", with(PCI, "iommu-map-mask", Some(be(&[0xff00])))),
        (
            "two halves on one set of device ids (0x5 and 0x8005 are both device 5)",
            with(
                PCI,
                "iommu-map",
                Some(be(&[0, 0x8000, 0, 0x8000, 0x8000, 0x8000, 0, 0x8000])),
            ),
        ),
        (
            "an entry ahead of the whole map that shifts ids (first match: 0x0 is device 0x100)",
            with(
                PCI,
                "iommu-map",
                Some(be(&[0, 0x8000, 0x100, 0x10, 0, 0x8000, 0, 0x10000])),
            ),
        ),
        (
            "a malformed map",
            with(PCI, "iommu-map", Some(be(&[0, 0x8000, 0]))),
        ),
    ] {
        assert!(iommutree::pci_windows(&n).is_empty(), "{what}");
        assert_eq!(only(&n), closed, "{what}");
    }
    // two entries that together cover every id are a whole map
    let n = with(
        PCI,
        "iommu-map",
        Some(be(&[0, 0x8000, 0, 0x8000, 0x8000, 0x8000, 0x8000, 0x8000])),
    );
    assert_eq!(iommutree::pci_windows(&n), iommutree::pci_windows(&nodes()));
}

/// The tree CI's QEMU (Ubuntu 26.04's 10.2.1; 11.0 writes the same) gives the machine: `iommu-map = <0 0x8000 0 0>, <0 0x8000 0
/// 0xffff>` (its `virt.c` at v10.2.1; 11.1 writes the one entry `<0 0x8000 0 0x10000>` of the fixture). The empty first entry
/// maps nothing, as the binding reads it, and is no refusal: the epic's smoke at 29a950e panicked on it ("device ids past 24
/// bits"). The second leaves requester id 0xffff outside the IOMMU, so the host is only partly behind it and opens no window.
#[test]
fn qemu_10_2s_map_is_an_iommu_whose_pci_host_misses_one_requester_id() {
    let n = with(
        PCI,
        "iommu-map",
        Some(be(&[0, 0x8000, 0, 0, 0, 0x8000, 0, 0xffff])),
    );
    let i = discover(&n).expect("an empty entry is no refusal");
    assert_eq!(
        i.pci[0].map,
        vec![MapEntry {
            rid_base: 0,
            did_base: 0,
            len: 0xffff
        }]
    );
    assert_eq!(i.device_ids, 0xffff);
    assert!(i.covers(0) && i.covers(0xfffe));
    assert!(!i.covers(0xffff) && !i.covers(0x1_0000));
    assert!(iommutree::pci_windows(&n).is_empty());
    let closed = vec![(0x10_1000, 0x10_2000), (0x1000_1000, 0x1000_9000)];
    assert_eq!(
        device_windows(&n, &[CONSOLE], &fdt::memory_ranges(&n)),
        closed
    );
    // an empty entry beside a whole one changes nothing
    let n = with(
        PCI,
        "iommu-map",
        Some(be(&[0x20, 0x8000, 0x20, 0, 0, 0x8000, 0, 0x10000])),
    );
    assert_eq!(discover(&n), discover(&nodes()));
    assert_eq!(iommutree::pci_windows(&n), iommutree::pci_windows(&nodes()));
    // an empty entry naming another IOMMU maps nothing either
    let n = with(
        PCI,
        "iommu-map",
        Some(be(&[0, 0x1234, 0, 0, 0, 0x8000, 0, 0x10000])),
    );
    assert_eq!(discover(&n), discover(&nodes()));
    assert_eq!(iommutree::pci_windows(&n), iommutree::pci_windows(&nodes()));
    // a map of nothing but empty entries, or an empty property, puts no id behind the IOMMU (and is no refusal)
    for v in [be(&[0, 0x8000, 0, 0]), vec![]] {
        let n = with(PCI, "iommu-map", Some(v.clone()));
        assert!(discover(&n).unwrap().pci.is_empty(), "{v:x?}");
        assert!(iommutree::pci_windows(&n).is_empty(), "{v:x?}");
    }
    // a mask that folds nothing does not make a partial map whole
    let mut n = with(
        PCI,
        "iommu-map",
        Some(be(&[0, 0x8000, 0, 0, 0, 0x8000, 0, 0xffff])),
    );
    let pci = n.iter_mut().find(|x| x.path == PCI).unwrap();
    pci.props.push(("iommu-map-mask".into(), be(&[0xffff])));
    assert!(iommutree::pci_windows(&n).is_empty());
}

/// A map may send some requester ids to another IOMMU (the binding allows it): this IOMMU is still usable and owns its own
/// entries, but the host is not wholly behind it and opens no window.
#[test]
fn a_map_naming_another_iommu_too_keeps_this_one_but_opens_no_window() {
    let n = with(
        PCI,
        "iommu-map",
        Some(be(&[0, 0x8000, 0, 0x8000, 0x8000, 0x1234, 0, 0x8000])),
    );
    let i = discover(&n).unwrap();
    assert_eq!(
        i.pci[0].map,
        vec![MapEntry {
            rid_base: 0,
            did_base: 0,
            len: 0x8000
        }]
    );
    assert!(i.covers(0x7fff) && !i.covers(0x8000));
    assert!(iommutree::pci_windows(&n).is_empty());
    // a hole below the highest id: an id in it is none of this IOMMU's (`iodomain_attach` refuses it)
    let n = with(
        PCI,
        "iommu-map",
        Some(be(&[0, 0x8000, 0, 0x100, 0x200, 0x8000, 0x200, 0xfe00])),
    );
    let i = discover(&n).unwrap();
    assert_eq!(i.device_ids, 0x1_0000);
    assert!(i.covers(0xff) && i.covers(0x200) && i.covers(0xffff));
    assert!(!i.covers(0x100) && !i.covers(0x1ff));
    // wholly another's: nothing of this host is behind this IOMMU
    let n = with(PCI, "iommu-map", Some(be(&[0, 0x1234, 0, 0x10000])));
    assert!(discover(&n).unwrap().pci.is_empty());
    assert!(iommutree::pci_windows(&n).is_empty());
}

/// `iommu-map-mask` is ANDed with the requester id before the lookup: a mask that keeps all 16 bits is the identity and the
/// windows open; one that folds functions together, or is not one cell, opens none.
#[test]
fn a_mask_that_folds_nothing_is_the_identity() {
    for m in [0xffff_u32, 0xffff_ffff] {
        let n = with(PCI, "iommu-map-mask", Some(be(&[m])));
        assert_eq!(
            iommutree::pci_windows(&n),
            iommutree::pci_windows(&nodes()),
            "{m:#x}"
        );
    }
    for m in [&[0xfff8_u32][..], &[0xff00], &[0, 0xffff], &[]] {
        let n = with(PCI, "iommu-map-mask", Some(be(m)));
        assert!(iommutree::pci_windows(&n).is_empty(), "{m:x?}");
    }
}

/// The kernel programs rising-edge and level-high sources only: a falling-edge or level-low source is a refusal.
#[test]
fn a_source_the_kernel_cannot_program_is_the_refusal() {
    for trigger in [2u32, 8] {
        let n = with(
            IOMMU,
            "interrupts",
            Some(be(&[0x24, trigger, 0x25, 1, 0x26, 1, 0x27, 1])),
        );
        assert!(
            matches!(discover(&n), Err(Absent::Malformed(_))),
            "trigger {trigger}"
        );
    }
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
