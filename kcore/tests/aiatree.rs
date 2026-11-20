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
    assert_eq!(a.hart_order, vec![0]);
    assert_eq!(a.sources, vec![1, 2, 3, 4, 5, 6, 7, 8, 11]);
    let a = discover(&fdt::parse(FOUR).unwrap()).unwrap();
    assert_eq!(a.aplic_base, 0xd00_0000);
    assert_eq!(
        a.hart_order,
        vec![0, 1, 2, 3],
        "phandles 8, 6, 4, 2 name cpu0 .. cpu3"
    );
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
        ("virtio,mmio", 90, "", true),        // beyond the 64 an IMSIC register holds
        ("virtio,mmio", 0, "", true),         // source 0 does not exist
        ("virtio,mmio", 70, "", true),        // beyond 63
    ];
    let t = machine(&[0], &[0], &devices, 96);
    let a = discover(&fdt::parse(&t).unwrap()).unwrap();
    assert_eq!(a.sources, vec![3, 11]);
    // beyond the controller's own source count
    let t = machine(
        &[0],
        &[0],
        &[("virtio,mmio", 20, "", true), ("virtio,mmio", 7, "", true)],
        10,
    );
    assert_eq!(discover(&fdt::parse(&t).unwrap()).unwrap().sources, vec![7]);
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
