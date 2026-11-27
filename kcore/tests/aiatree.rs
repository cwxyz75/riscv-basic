//! The interrupt controller from the device tree (PL5c). Expectations for the real QEMU `virt` trees come from `dtc`'s listing: the
//! supervisor APLIC at 0xd000000 with 96 sources, the harts in the order of the IMSIC's `interrupts-extended` (phandles 8, 6, 4, 2 are the
//! interrupt controllers of cpu@0 .. cpu@3: the order of the list, not the numbering of the phandles, is the hart index), and the sources
//! the admitted devices name: virtio-mmio 1 to 8 and the RTC, 11. Synthetic trees cover hart ids that are not dense or not in order.
mod common;
use common::Dt;
use kcore::aiatree::discover;
use kcore::fdt;

const ONE: &[u8] = include_bytes!("fixtures/virt-aia.dtb");
const FOUR: &[u8] = include_bytes!("fixtures/virt-aia-smp4.dtb");
const NO_AIA: &[u8] = include_bytes!("fixtures/virt.dtb");

#[test]
fn the_real_virt_trees_give_the_supervisor_aplic_the_hart_order_and_the_device_sources() {
    let a = discover(&fdt::parse(ONE).unwrap()).unwrap();
    assert_eq!(a.aplic_base, 0xd00_0000);
    assert_eq!(a.num_sources, 96);
    assert_eq!(a.imsic_base, 0x2800_0000);
    assert_eq!(a.hart_order, vec![0]);
    assert_eq!(a.num_ids, 255, "QEMU 11.1.3: riscv,num-ids = <0xff>");
    assert_eq!(a.sources, vec![1, 2, 3, 4, 5, 6, 7, 8, 11]);
    assert!(a.dropped.is_empty());
    let a = discover(&fdt::parse(FOUR).unwrap()).unwrap();
    assert_eq!(a.aplic_base, 0xd00_0000);
    assert_eq!(
        a.hart_order,
        vec![0, 1, 2, 3],
        "phandles 8, 6, 4, 2 name cpu0 .. cpu3"
    );
    assert_eq!(a.num_ids, 255);
    assert_eq!(a.sources, vec![1, 2, 3, 4, 5, 6, 7, 8, 11]);
}

#[test]
fn a_machine_with_a_plic_and_no_aplic_has_none() {
    assert_eq!(discover(&fdt::parse(NO_AIA).unwrap()), None);
}

/// A machine with an APLIC in MSI mode and an IMSIC listing harts `order` (hart ids; the cpu nodes are written in id order), and devices
/// `(compat, source, status, parent_ok)`.
fn machine(
    hart_ids: &[u32],
    order: &[u32],
    devices: &[(&str, u32, &str, bool)],
    num_sources: u32,
) -> Vec<u8> {
    let mut d = Dt::new();
    d.begin("");
    d.u32s("#address-cells", &[2]);
    d.u32s("#size-cells", &[2]);
    d.u32s("interrupt-parent", &[0x10]);
    d.begin("cpus");
    for (i, id) in hart_ids.iter().enumerate() {
        d.begin(&format!("cpu@{id}"));
        d.u32s("reg", &[*id]);
        d.u32s("phandle", &[0x100 + i as u32 * 2]);
        d.begin("interrupt-controller");
        d.u32s("phandle", &[0x101 + i as u32 * 2]);
        d.end();
        d.end();
    }
    d.end();
    d.begin("soc");
    d.u32s("#address-cells", &[2]);
    d.u32s("#size-cells", &[2]);
    d.prop("ranges", &[]);
    d.begin("imsic@28000000");
    d.u32s("phandle", &[0x20]);
    let ext: Vec<u32> = order
        .iter()
        .flat_map(|id| {
            let i = hart_ids.iter().position(|h| h == id).unwrap() as u32;
            [0x101 + i * 2, 9]
        })
        .collect();
    d.u32s("interrupts-extended", &ext);
    d.reg(&[(0x2800_0000, 0x4000)]);
    d.u32s("riscv,num-ids", &[255]);
    d.prop("compatible", b"riscv,imsics\0");
    d.end();
    d.begin("aplic@d000000");
    d.u32s("phandle", &[0x10]);
    d.reg(&[(0xd00_0000, 0x8000)]);
    d.u32s("riscv,num-sources", &[num_sources]);
    d.u32s("msi-parent", &[0x20]);
    d.prop("compatible", b"riscv,aplic\0");
    d.end();
    for (i, (compat, src, status, parent_ok)) in devices.iter().enumerate() {
        d.begin(&format!("dev@{:x}", 0x1000_0000 + i * 0x1000));
        let mut c = compat.as_bytes().to_vec();
        c.push(0);
        d.prop("compatible", &c);
        d.reg(&[(0x1000_0000 + i as u64 * 0x1000, 0x1000)]);
        d.u32s("interrupts", &[*src, 4]);
        if !parent_ok {
            d.u32s("interrupt-parent", &[0x99]);
        }
        if !status.is_empty() {
            let mut st = status.as_bytes().to_vec();
            st.push(0);
            d.prop("status", &st);
        }
        d.end();
    }
    d.end();
    d.end();
    d.finish()
}

#[test]
fn hart_ids_need_not_be_dense_or_in_order_and_the_imsic_list_is_the_hart_index() {
    // ids 0, 2, 5; the IMSIC lists them 5, 0, 2: hart 5 has index 0
    let t = machine(&[0, 2, 5], &[5, 0, 2], &[], 96);
    let a = discover(&fdt::parse(&t).unwrap()).unwrap();
    assert_eq!(a.hart_order, vec![5, 0, 2]);
    let t = machine(&[0, 1, 2, 3], &[0, 1, 2, 3], &[], 96);
    assert_eq!(
        discover(&fdt::parse(&t).unwrap()).unwrap().hart_order,
        vec![0, 1, 2, 3]
    );
}

#[test]
fn only_admitted_devices_on_this_aplic_and_within_its_sources_are_interrupts_a_process_may_take() {
    let devices = [
        ("virtio,mmio", 3, "", true),
        ("virtio,mmio", 3, "", true), // the same source twice: once
        ("google,goldfish-rtc", 11, "okay", true),
        ("virtio,mmio", 4, "disabled", true), // disabled
        ("virtio,mmio", 5, "", false),        // interrupts go to some other controller
        ("acme,unknown", 6, "", true),        // not an admitted device
        ("virtio,mmio", 90, "", true), // above 63: as good as any (IOMMU4: no identity is source + 1)
        ("virtio,mmio", 0, "", true),  // source 0 does not exist
        ("virtio,mmio", 70, "", true), // above 63
        ("virtio,mmio", 97, "", true), // past the controller's 96: said at boot, not used
    ];
    let t = machine(&[0], &[0], &devices, 96);
    let a = discover(&fdt::parse(&t).unwrap()).unwrap();
    assert_eq!(a.sources, vec![3, 11, 70, 90]);
    assert_eq!(a.dropped, vec![0, 97]);
    // beyond the controller's own source count
    let t = machine(
        &[0],
        &[0],
        &[("virtio,mmio", 20, "", true), ("virtio,mmio", 7, "", true)],
        10,
    );
    let a = discover(&fdt::parse(&t).unwrap()).unwrap();
    assert_eq!(a.sources, vec![7]);
    assert_eq!(a.dropped, vec![20]);
}

#[test]
fn a_controller_without_message_delivery_or_without_harts_is_not_usable() {
    let mut d = Dt::new();
    d.begin("");
    d.begin("soc");
    d.begin("aplic@d000000");
    d.u32s("phandle", &[0x10]);
    d.reg(&[(0xd00_0000, 0x8000)]);
    d.u32s("riscv,num-sources", &[96]);
    d.prop("compatible", b"riscv,aplic\0"); // no msi-parent: direct mode
    d.end();
    d.end();
    d.end();
    // (reg with the root's default cells would not decode: this tree only needs to be refused)
    assert_eq!(discover(&fdt::parse(&d.finish()).unwrap()), None);
    // an IMSIC that lists no harts
    let t = machine(&[0], &[], &[], 96);
    assert_eq!(discover(&fdt::parse(&t).unwrap()), None);
}

/// A tree whose M-level APLIC (it has children) and M-level IMSIC come first in the blob, with another base and another hart order, then the
/// S-level pair: the S-level controller and its own IMSIC must be the ones used. Extra knobs for the odd shapes below.
fn two_levels(
    imsic_extra: &dyn Fn(&mut Dt),
    aplic_sources: u32,
    s_irq_cell: u32,
    devices: &[(u32, bool)],
) -> Vec<u8> {
    let mut d = Dt::new();
    d.begin("");
    d.u32s("#address-cells", &[2]);
    d.u32s("#size-cells", &[2]);
    d.u32s("interrupt-parent", &[0x10]);
    d.begin("cpus");
    for id in 0..3u32 {
        d.begin(&format!("cpu@{id}"));
        d.u32s("reg", &[id]);
        d.u32s("phandle", &[0x100 + id * 2]);
        d.begin("interrupt-controller");
        d.u32s("phandle", &[0x101 + id * 2]);
        d.end();
        d.end();
    }
    d.end();
    d.begin("soc");
    d.u32s("#address-cells", &[2]);
    d.u32s("#size-cells", &[2]);
    d.prop("ranges", &[]);
    // machine level first: another base, children, a different order and the machine external interrupt (11)
    d.begin("imsic@24000000");
    d.u32s("phandle", &[0x21]);
    d.u32s("interrupts-extended", &[0x105, 11, 0x103, 11, 0x101, 11]);
    d.reg(&[(0x2400_0000, 0x4000)]);
    d.u32s("riscv,num-ids", &[255]);
    d.prop("compatible", b"riscv,imsics\0");
    d.end();
    d.begin("aplic@c000000");
    d.u32s("phandle", &[0x11]);
    d.reg(&[(0xc00_0000, 0x8000)]);
    d.u32s("riscv,num-sources", &[96]);
    d.u32s("msi-parent", &[0x21]);
    d.u32s("riscv,children", &[0x10]);
    d.prop("compatible", b"riscv,aplic\0");
    d.end();
    // supervisor level
    d.begin("imsic@28000000");
    d.u32s("phandle", &[0x20]);
    d.u32s(
        "interrupts-extended",
        &[0x101, s_irq_cell, 0x103, s_irq_cell, 0x105, s_irq_cell],
    );
    d.reg(&[(0x2800_0000, 0x4000)]);
    d.u32s("riscv,num-ids", &[255]);
    imsic_extra(&mut d);
    d.prop("compatible", b"riscv,imsics\0");
    d.end();
    d.begin("aplic@d000000");
    d.u32s("phandle", &[0x10]);
    d.reg(&[(0xd00_0000, 0x8000)]);
    d.u32s("riscv,num-sources", &[aplic_sources]);
    d.u32s("msi-parent", &[0x20]);
    d.prop("compatible", b"riscv,aplic\0");
    d.end();
    for (i, (src, with_ext)) in devices.iter().enumerate() {
        d.begin(&format!("dev@{:x}", 0x1000_0000 + i * 0x1000));
        d.prop("compatible", b"virtio,mmio\0");
        d.reg(&[(0x1000_0000 + i as u64 * 0x1000, 0x1000)]);
        d.u32s("interrupts", &[*src, 4]);
        if *with_ext {
            d.u32s("interrupts-extended", &[0x10, *src, 4]);
        }
        d.end();
    }
    d.end();
    d.end();
    d.finish()
}

#[test]
fn the_supervisor_controller_and_its_own_imsic_are_chosen_over_the_machine_level_pair() {
    let t = two_levels(&|_| {}, 96, 9, &[(5, false)]);
    let a = discover(&fdt::parse(&t).unwrap()).unwrap();
    assert_eq!(a.aplic_base, 0xd00_0000, "the one without children");
    assert_eq!(
        a.hart_order,
        vec![0, 1, 2],
        "its own IMSIC's order, not the machine level's (2, 1, 0)"
    );
    assert_eq!(a.sources, vec![5]);
    // an IMSIC that lists the machine external interrupt (11) is not the supervisor's file
    let t = two_levels(&|_| {}, 96, 11, &[(5, false)]);
    assert_eq!(discover(&fdt::parse(&t).unwrap()), None);
}

#[test]
fn the_source_limits_are_exact() {
    // each of: the controller's own count as a source, one past it; nothing about 62 or 63 any more (the identities are allocated)
    let t = two_levels(&|_| {}, 62, 9, &[(62, false), (63, false), (61, false)]);
    let a = discover(&fdt::parse(&t).unwrap()).unwrap();
    assert_eq!(a.sources, vec![61, 62]);
    assert_eq!(a.dropped, vec![63]);
    let t = two_levels(&|_| {}, 62, 9, &[(62, false)]);
    assert_eq!(
        discover(&fdt::parse(&t).unwrap()).unwrap().sources,
        vec![62],
        "source == num-sources is a source"
    );
    let t = two_levels(&|_| {}, 61, 9, &[(62, false), (61, false)]);
    assert_eq!(
        discover(&fdt::parse(&t).unwrap()).unwrap().sources,
        vec![61],
        "one past the count is not"
    );
    // on a big controller every source it has is usable: 63, 64 and the last
    let t = two_levels(
        &|_| {},
        1023,
        9,
        &[(63, false), (62, false), (64, false), (1023, false)],
    );
    assert_eq!(
        discover(&fdt::parse(&t).unwrap()).unwrap().sources,
        vec![62, 63, 64, 1023]
    );
    // an APLIC has at most 1023 sources
    assert_eq!(
        discover(&fdt::parse(&two_levels(&|_| {}, 1024, 9, &[(5, false)])).unwrap()),
        None
    );
    assert!(discover(&fdt::parse(&two_levels(&|_| {}, 1023, 9, &[(5, false)])).unwrap()).is_some());
    assert_eq!(
        discover(&fdt::parse(&two_levels(&|_| {}, 0, 9, &[(5, false)])).unwrap()),
        None
    );
    assert_eq!(
        discover(&fdt::parse(&two_levels(&|_| {}, 0xffff_ffff, 9, &[(5, false)])).unwrap()),
        None
    );
}

#[test]
fn imsic_groups_are_refused_and_a_device_that_names_its_parents_elsewhere_is_not_read() {
    // several groups change how the hart index is formed: not read, fail closed
    let t = two_levels(
        &|d| d.u32s("riscv,group-index-bits", &[1]),
        96,
        9,
        &[(5, false)],
    );
    assert_eq!(discover(&fdt::parse(&t).unwrap()), None);
    let t = two_levels(
        &|d| d.u32s("riscv,group-index-bits", &[0]),
        96,
        9,
        &[(5, false)],
    );
    assert!(
        discover(&fdt::parse(&t).unwrap()).is_some(),
        "zero group bits is the single-group layout"
    );
    let t = two_levels(
        &|d| d.reg(&[(0x2800_0000, 0x4000), (0x2900_0000, 0x4000)]),
        96,
        9,
        &[(5, false)],
    );
    // (a second `reg` property replaces the first in this builder; the real two-region case is the one `regs()` counts)
    let _ = t;
    // a device with interrupts-extended names its parents itself
    let t = two_levels(&|_| {}, 96, 9, &[(5, true), (6, false)]);
    assert_eq!(discover(&fdt::parse(&t).unwrap()).unwrap().sources, vec![6]);
}

#[test]
fn a_device_behind_an_interrupt_nexus_does_not_inherit_the_roots_parent() {
    let mut d = Dt::new();
    d.begin("");
    d.u32s("#address-cells", &[2]);
    d.u32s("#size-cells", &[2]);
    d.u32s("interrupt-parent", &[0x10]);
    d.begin("cpus");
    d.begin("cpu@0");
    d.u32s("reg", &[0]);
    d.u32s("phandle", &[0x100]);
    d.begin("interrupt-controller");
    d.u32s("phandle", &[0x101]);
    d.end();
    d.end();
    d.end();
    d.begin("soc");
    d.u32s("#address-cells", &[2]);
    d.u32s("#size-cells", &[2]);
    d.prop("ranges", &[]);
    d.begin("imsic@28000000");
    d.u32s("phandle", &[0x20]);
    d.u32s("interrupts-extended", &[0x101, 9]);
    d.reg(&[(0x2800_0000, 0x4000)]);
    d.u32s("riscv,num-ids", &[255]);
    d.prop("compatible", b"riscv,imsics\0");
    d.end();
    d.begin("aplic@d000000");
    d.u32s("phandle", &[0x10]);
    d.reg(&[(0xd00_0000, 0x8000)]);
    d.u32s("riscv,num-sources", &[96]);
    d.u32s("msi-parent", &[0x20]);
    d.prop("compatible", b"riscv,aplic\0");
    d.end();
    d.begin("bridge"); // an interrupt nexus: its children's interrupts are local numbers, not the APLIC's sources
    d.u32s("#address-cells", &[2]);
    d.u32s("#size-cells", &[2]);
    d.prop("interrupt-map", &[]);
    d.prop("ranges", &[]);
    d.begin("dev@1000");
    d.prop("compatible", b"virtio,mmio\0");
    d.reg(&[(0x1000, 0x1000)]);
    d.u32s("interrupts", &[3]);
    d.end();
    d.end();
    d.begin("dev@2000");
    d.prop("compatible", b"virtio,mmio\0");
    d.reg(&[(0x2000, 0x1000)]);
    d.u32s("interrupts", &[4]);
    d.end();
    d.end();
    d.end();
    let a = discover(&fdt::parse(&d.finish()).unwrap()).unwrap();
    assert_eq!(
        a.sources,
        vec![4],
        "the device under the nexus is not the APLIC's"
    );
}

#[test]
fn the_imsic_window_must_hold_a_page_per_listed_hart() {
    // five harts listed, a window of four pages (the `machine` builder writes 0x4000)
    let ids = [0, 1, 2, 3, 4];
    let ok = machine(&ids[..4], &ids[..4], &[], 96);
    assert!(discover(&fdt::parse(&ok).unwrap()).is_some());
    let small = machine(&ids, &ids, &[], 96);
    assert_eq!(discover(&fdt::parse(&small).unwrap()), None);
}

#[test]
fn the_identities_of_a_file_come_from_num_ids_which_is_required_and_bounded() {
    // the IMSIC binding requires `riscv,num-ids` (63 to 2047): without it, or out of range, the controller is not used. The supervisor
    // IMSIC of the two-level tree is the one changed (the machine-level one keeps its 255).
    let with = |n: Option<u32>| {
        let mut nodes = fdt::parse(&two_levels(&|_| {}, 96, 9, &[(5, false)])).unwrap();
        let imsic = nodes
            .iter_mut()
            .find(|x| x.path.ends_with("imsic@28000000"))
            .unwrap();
        imsic.props.retain(|(name, _)| name != "riscv,num-ids");
        if let Some(n) = n {
            imsic
                .props
                .push(("riscv,num-ids".into(), n.to_be_bytes().to_vec()));
        }
        discover(&nodes).map(|a| a.num_ids)
    };
    for (n, ok) in [
        (63, true),
        (64, true),
        (255, true),
        (2047, true),
        (62, false),
        (2048, false),
        (0, false),
    ] {
        assert_eq!(with(Some(n)), ok.then_some(n), "num-ids {n}");
    }
    assert_eq!(with(None), None, "no riscv,num-ids: not used");
}

/// IOMMU8 (IOMMU-SPEC 8.6; Q-IOMMU-IRQ B): PCI INTx is never admitted. The PCI host's `interrupt-map` is decoded here from the tree's
/// own cell counts (the host's `#address-cells` and `#interrupt-cells`, then each parent's `#address-cells` and `#interrupt-cells`, as
/// the device-tree specification lays the property out), not by any code under test: on QEMU `virt` with the IOMMU it names sources
/// 32 to 35 on the supervisor APLIC (phandle 0x0c, `dtc`'s listing), and none of them may be in the list `interrupt_create` admits.
#[test]
fn no_source_of_the_pci_hosts_interrupt_map_is_admitted() {
    let nodes = fdt::parse(include_bytes!("fixtures/virt-aia-iommu.dtb")).unwrap();
    let host = nodes
        .iter()
        .find(|n| n.is_compatible("pci-host-ecam-generic"))
        .expect("a PCI host");
    let map = host.cells("interrupt-map");
    let child_addr = host.u32_prop("#address-cells").unwrap() as usize;
    let child_int = host.u32_prop("#interrupt-cells").unwrap() as usize;
    let mut wired = Vec::new();
    let mut i = 0;
    while i < map.len() {
        i += child_addr + child_int;
        let phandle = map[i];
        i += 1;
        let parent = nodes
            .iter()
            .find(|n| n.u32_prop("phandle") == Some(phandle))
            .expect("the interrupt parent");
        assert_eq!(
            phandle, 0x0c,
            "every INTx line goes to the supervisor APLIC"
        );
        i += parent.u32_prop("#address-cells").unwrap_or(0) as usize;
        wired.push(map[i]);
        i += parent.u32_prop("#interrupt-cells").unwrap() as usize;
    }
    wired.sort_unstable();
    wired.dedup();
    assert_eq!(
        wired,
        vec![32, 33, 34, 35],
        "INTA to INTD of the four slot groups"
    );
    let a = discover(&nodes).unwrap();
    for s in &wired {
        assert!(
            !a.sources.contains(s),
            "PCI INTx source {s} is admitted ({:?})",
            a.sources
        );
    }
    assert_eq!(a.sources, vec![1, 2, 3, 4, 5, 6, 7, 8, 11]);
}
