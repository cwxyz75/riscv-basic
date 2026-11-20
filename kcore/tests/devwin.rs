//! The device windows (PL5, D1): built from the device tree. Expectations for the real QEMU `virt` tree come from `dtc`'s own listing of
//! it, not from the code under test; the synthetic trees add a device, an unaligned one, one on top of RAM and one on the console.
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
fn the_real_virt_tree_gives_exactly_the_devices_the_kernel_does_not_drive() {
    let w = windows_of(VIRT_AIA, &[CONSOLE]);
    // dtc's listing of the tree: the goldfish RTC page; fw-cfg; the eight virtio-mmio slots (one run of 0x1000 .. 0x9000 after the
    // console's page is cut out of its own node); the two flash banks (adjacent, so one range); the PCI ECAM window.
    // Not in it: the console (excluded), the APLICs, the IMSICs, the CLINT and the test finisher (by what they are), the CPUs and the RAM.
    assert_eq!(
        w,
        vec![
            (0x10_1000, 0x10_2000),     // rtc
            (0x1000_1000, 0x1000_9000), // virtio_mmio@10001000 .. @10008000
            (0x1010_0000, 0x1010_1000), // fw-cfg
            (0x2000_0000, 0x2400_0000), // flash bank 0 and 1
            (0x3000_0000, 0x4000_0000), // pci ecam
        ]
    );
    // the boundaries the suite probes (device_windows_only): the console's page below, the page after the last virtio slot
    assert!(!in_window(&w, 0x1000_0000, 4096), "the console's page");
    assert!(in_window(&w, 0x1000_1000, 4096));
    assert!(in_window(&w, 0x1000_8000, 4096));
    assert!(
        !in_window(&w, 0x1000_9000, 4096),
        "just above the virtio slots"
    );
    assert!(!in_window(&w, 0x1000_8000, 8192), "straddling the end");
    assert!(!in_window(&w, 0x8000_0000, 4096), "RAM");
    assert!(!in_window(&w, 0x10_0000, 4096), "the test finisher");
    assert!(!in_window(&w, 0xd00_0000, 4096), "the APLIC");
    assert!(!in_window(&w, 0x2400_0000, 4096), "the supervisor IMSIC");
    assert!(!in_window(&w, 0x200_0000, 4096), "the CLINT");
    assert!(!in_window(&w, 0x1000_1001, 4096), "misaligned");
    assert!(!in_window(&w, 0x1000_1000, 0), "empty");
    assert!(!in_window(&w, 0x1000_1000, 100), "not a whole page");
    assert!(!in_window(&w, u64::MAX - 4095, 8192), "wraps");
}

fn machine(devices: &[(&str, &str, u64, u64)]) -> Vec<u8> {
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
    for &(name, compat, a, s) in devices {
        d.begin(name);
        let mut c = compat.as_bytes().to_vec();
        c.push(0);
        d.prop("compatible", &c);
        d.reg(&[(a, s)]);
        d.end();
    }
    d.end();
    d.end();
    d.finish()
}

#[test]
fn an_extra_device_gets_a_window_widened_to_its_pages_and_neighbours_merge() {
    let blob = machine(&[
        ("acme@50000100", "acme,widget", 0x5000_0100, 0x100), // inside one page, unaligned
        ("acme@50001000", "acme,gadget", 0x5000_1000, 0x1800), // touches the first page's neighbour
        ("far@60000000", "acme,far", 0x6000_0000, 0x4000),
    ]);
    let w = windows_of(&blob, &[]);
    assert_eq!(
        w,
        vec![(0x5000_0000, 0x5000_3000), (0x6000_0000, 0x6000_4000)]
    );
}

#[test]
fn nothing_the_kernel_drives_and_no_ram_ever_becomes_a_window() {
    let blob = machine(&[
        ("uart@10000000", "ns16550a", 0x1000_0000, 0x100),
        (
            "aplic@d000000",
            "qemu,aplic\0riscv,aplic",
            0xd00_0000,
            0x8000,
        ),
        (
            "imsics@24000000",
            "qemu,imsics\0riscv,imsics",
            0x2400_0000,
            0x1000,
        ),
        ("clint@2000000", "sifive,clint0", 0x200_0000, 0x1_0000),
        (
            "test@100000",
            "sifive,test1\0sifive,test0\0syscon",
            0x10_0000,
            0x1000,
        ),
        ("onram@88000000", "acme,confused", 0x8800_0000, 0x1000), // a device node that sits on RAM: not a window
        ("ok@30000000", "acme,ok", 0x3000_0000, 0x1000),
    ]);
    // the console is named by the caller (the chosen stdout-path in the kernel); a second device on the console's page is cut as well
    let w = windows_of(&blob, &[(0x1000_0000, 0x100)]);
    assert_eq!(w, vec![(0x3000_0000, 0x3000_1000)]);
    // a device range that only partly overlaps what is cut keeps the rest
    let blob = machine(&[("big@40000000", "acme,big", 0x4000_0000, 0x10000)]);
    let w = windows_of(&blob, &[(0x4000_4000, 0x1000)]);
    assert_eq!(
        w,
        vec![(0x4000_0000, 0x4000_4000), (0x4000_5000, 0x4001_0000)]
    );
}

#[test]
fn a_tree_with_no_devices_gives_no_windows() {
    assert_eq!(windows_of(&machine(&[]), &[]), vec![]);
    assert!(!in_window(&[], 0x1000_1000, 4096));
}
