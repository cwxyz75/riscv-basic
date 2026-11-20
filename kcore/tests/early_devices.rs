//! The console and the test finisher found without allocating (PL5b): the real QEMU `virt` trees (expectations from `dtc`'s listing: the
//! UART at 0x1000_0000 named by `/chosen/stdout-path`, the SiFive test device at 0x10_0000) and synthetic trees for the other ways a
//! tree can name a console: an alias, options after the path, a node that is not a 16550, a missing node, a malformed tree.
mod common;
use common::Dt;
use kcore::fdt::{console_base, test_finisher_base};

const TREES: &[&[u8]] = &[
    include_bytes!("fixtures/virt.dtb"),
    include_bytes!("fixtures/virt-aia.dtb"),
    include_bytes!("fixtures/virt-rva23s64.dtb"),
];

#[test]
fn the_real_virt_trees_name_the_uart_and_the_finisher() {
    for (i, t) in TREES.iter().enumerate() {
        assert_eq!(console_base(t), Some(0x1000_0000), "tree {i}");
        assert_eq!(test_finisher_base(t), Some(0x10_0000), "tree {i}");
    }
}

/// A tree with `/chosen/stdout-path = chosen_path`, an alias `serial0` for the UART node and a UART at `reg` with `compat`.
fn tree(stdout: Option<&str>, compat: &str, cells: u32, reg: &[u32], finisher: bool) -> Vec<u8> {
    let mut d = Dt::new();
    d.begin("");
    d.u32s("#address-cells", &[cells]);
    d.u32s("#size-cells", &[cells.clamp(1, 2)]);
    d.begin("chosen");
    if let Some(p) = stdout {
        let mut b = p.as_bytes().to_vec();
        b.push(0);
        d.prop("stdout-path", &b);
    }
    d.end();
    d.begin("aliases");
    d.prop("serial0", b"/soc/uart@2000\0");
    d.end();
    d.begin("soc");
    d.u32s("#address-cells", &[cells]);
    d.u32s("#size-cells", &[cells.clamp(1, 2)]);
    d.begin("uart@2000");
    // `reg` before `compatible`, as QEMU writes them
    d.u32s("reg", reg);
    let mut c = compat.as_bytes().to_vec();
    c.push(0);
    d.prop("compatible", &c);
    d.end();
    if finisher {
        d.begin("test@5000");
        d.u32s(
            "reg",
            &[0x5000, 0x1000][..2.min(reg.len())]
                .iter()
                .copied()
                .chain(std::iter::empty())
                .collect::<Vec<_>>(),
        );
        d.prop("compatible", b"sifive,test0\0syscon\0");
        d.end();
    }
    d.end();
    d.end();
    d.finish()
}

#[test]
fn an_absolute_path_an_alias_and_options_all_name_the_same_console() {
    for stdout in [
        "/soc/uart@2000",
        "serial0",
        "serial0:115200n8",
        "/soc/uart@2000:115200",
    ] {
        let t = tree(Some(stdout), "ns16550a", 1, &[0x2000, 0x100], false);
        assert_eq!(console_base(&t), Some(0x2000), "stdout-path {stdout}");
    }
    // two-cell addresses
    let t = tree(
        Some("serial0"),
        "ns16550a",
        2,
        &[0, 0x2000, 0, 0x100],
        false,
    );
    assert_eq!(console_base(&t), Some(0x2000));
    let t = tree(
        Some("serial0"),
        "ns16550a",
        2,
        &[1, 0x2000, 0, 0x100],
        false,
    );
    assert_eq!(
        console_base(&t),
        Some(0x1_0000_2000),
        "the high cell counts"
    );
    // the older compatible
    assert_eq!(
        console_base(&tree(
            Some("serial0"),
            "ns16550",
            1,
            &[0x2000, 0x100],
            false
        )),
        Some(0x2000)
    );
}

#[test]
fn no_console_a_console_that_is_not_a_16550_and_bad_names_give_none() {
    assert_eq!(
        console_base(&tree(None, "ns16550a", 1, &[0x2000, 0x100], false)),
        None,
        "no stdout-path"
    );
    assert_eq!(
        console_base(&tree(
            Some("serial0"),
            "sifive,uart0",
            1,
            &[0x2000, 0x100],
            false
        )),
        None,
        "not a 16550"
    );
    assert_eq!(
        console_base(&tree(
            Some("serial1"),
            "ns16550a",
            1,
            &[0x2000, 0x100],
            false
        )),
        None,
        "an alias that does not exist"
    );
    assert_eq!(
        console_base(&tree(
            Some("/soc/uart@3000"),
            "ns16550a",
            1,
            &[0x2000, 0x100],
            false
        )),
        None,
        "a path that does not exist"
    );
    assert_eq!(
        console_base(&tree(Some("/soc"), "ns16550a", 1, &[0x2000, 0x100], false)),
        None,
        "a node with no 16550 compatible"
    );
    assert_eq!(
        console_base(&tree(Some("serial0"), "ns16550a", 1, &[], false)),
        None,
        "no reg"
    );
    assert_eq!(console_base(&[]), None);
    assert_eq!(
        console_base(&[0xd0, 0x0d, 0xfe, 0xed, 0, 0, 0, 8]),
        None,
        "a header and nothing else"
    );
}

#[test]
fn a_hostile_tree_ends_the_walk_without_a_panic() {
    let good = tree(Some("serial0"), "ns16550a", 1, &[0x2000, 0x100], true);
    assert_eq!(console_base(&good), Some(0x2000));
    assert_eq!(
        test_finisher_base(&good),
        Some(0x5000),
        "reg before compatible, one-cell addresses"
    );
    // every truncation and a corrupted byte anywhere: a result or None, never a panic
    for cut in 0..good.len() {
        let _ = console_base(&good[..cut]);
        let _ = test_finisher_base(&good[..cut]);
    }
    for i in 0..good.len() {
        let mut bad = good.clone();
        bad[i] ^= 0xff;
        let _ = console_base(&bad);
        let _ = test_finisher_base(&bad);
    }
}

#[test]
fn a_tree_without_the_test_device_has_none() {
    assert_eq!(
        test_finisher_base(&tree(
            Some("serial0"),
            "ns16550a",
            1,
            &[0x2000, 0x100],
            false
        )),
        None
    );
    assert_eq!(test_finisher_base(&[]), None);
}

/// A console tree with extra properties on the UART node and a number of other aliases (the wanted one comes last).
fn tree_with(
    extra: &[(&str, Vec<u8>)],
    uart_compat: &[u8],
    aliases: usize,
    name: &str,
    bootargs: &str,
) -> Vec<u8> {
    let mut d = Dt::new();
    d.begin("");
    d.u32s("#address-cells", &[1]);
    d.u32s("#size-cells", &[1]);
    d.begin("chosen");
    d.prop("stdout-path", b"serial0:115200\0");
    if !bootargs.is_empty() {
        let mut b = bootargs.as_bytes().to_vec();
        b.push(0);
        d.prop("bootargs", &b);
    }
    d.end();
    d.begin("aliases");
    for i in 0..aliases {
        d.prop(&format!("ethernet{i}"), b"/soc/eth@9000\0");
    }
    d.prop("serial0", format!("/soc/{name}\0").as_bytes());
    d.end();
    d.begin("other"); // a node with the same name elsewhere, before the real one
    d.begin("uart@2000");
    d.u32s("reg", &[0x9999, 0x100]);
    d.prop("compatible", b"ns16550a\0");
    d.end();
    d.end();
    d.begin("soc");
    d.u32s("#address-cells", &[1]);
    d.u32s("#size-cells", &[1]);
    d.begin("uart@2000");
    d.u32s("reg", &[0x2000, 0x100]);
    d.prop("compatible", uart_compat);
    for (k, v) in extra {
        d.prop(k, v);
    }
    d.end();
    d.end();
    d.end();
    d.finish()
}

#[test]
fn many_aliases_the_second_compatible_and_a_same_named_node_elsewhere() {
    // fifty aliases, the wanted one last: no fixed table to overflow
    let t = tree_with(&[], b"ns16550a\0", 50, "uart@2000", "");
    assert_eq!(console_base(&t), Some(0x2000));
    // the 16550 compatible is the second of two
    let t = tree_with(&[], b"acme,uart\0ns16550a\0", 0, "uart@2000", "");
    assert_eq!(console_base(&t), Some(0x2000));
    // the path matches the node at /soc, not the same-named one under /other (which comes first and has another address)
    assert_ne!(console_base(&t), Some(0x9999));
}

#[test]
fn a_path_without_the_unit_address_names_the_node_that_has_one() {
    let t = tree_with(&[], b"ns16550a\0", 0, "uart", "");
    assert_eq!(console_base(&t), Some(0x2000), "/soc/uart names uart@2000");
    let t = tree_with(&[], b"ns16550a\0", 0, "uart@2001", "");
    assert_eq!(
        console_base(&t),
        None,
        "a unit address that differs is a different node"
    );
}

#[test]
fn a_uart_the_kernel_cannot_drive_is_not_the_console() {
    let be = |v: u32| v.to_be_bytes().to_vec();
    assert_eq!(
        console_base(&tree_with(
            &[("status", b"disabled\0".to_vec())],
            b"ns16550a\0",
            0,
            "uart@2000",
            ""
        )),
        None,
        "disabled"
    );
    assert_eq!(
        console_base(&tree_with(
            &[("status", b"okay\0".to_vec())],
            b"ns16550a\0",
            0,
            "uart@2000",
            ""
        )),
        Some(0x2000),
        "okay"
    );
    assert_eq!(
        console_base(&tree_with(
            &[("reg-shift", be(2))],
            b"ns16550a\0",
            0,
            "uart@2000",
            ""
        )),
        None,
        "registers four bytes apart"
    );
    assert_eq!(
        console_base(&tree_with(
            &[("reg-shift", be(0))],
            b"ns16550a\0",
            0,
            "uart@2000",
            ""
        )),
        Some(0x2000),
        "shift 0 is the plain kind"
    );
    assert_eq!(
        console_base(&tree_with(
            &[("reg-io-width", be(4))],
            b"ns16550a\0",
            0,
            "uart@2000",
            ""
        )),
        None,
        "word-wide registers"
    );
    assert_eq!(
        console_base(&tree_with(
            &[("reg-io-width", be(1))],
            b"ns16550a\0",
            0,
            "uart@2000",
            ""
        )),
        Some(0x2000)
    );
}

#[test]
fn boot_words_are_found_without_allocating() {
    use kcore::fdt::bootarg_present;
    let t = tree_with(
        &[],
        b"ns16550a\0",
        0,
        "uart@2000",
        "shard=k console=sbi ram=256M",
    );
    assert!(bootarg_present(&t, "console=sbi"));
    assert!(bootarg_present(&t, "shard=k"));
    assert!(
        !bootarg_present(&t, "console=s"),
        "a prefix is not the word"
    );
    assert!(!bootarg_present(&t, "sbi"));
    assert!(
        !bootarg_present(
            &tree_with(&[], b"ns16550a\0", 0, "uart@2000", ""),
            "console=sbi"
        ),
        "no bootargs at all"
    );
    assert!(!bootarg_present(&[], "console=sbi"));
}
