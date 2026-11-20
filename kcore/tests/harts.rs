//! The harts from the device tree and the SBI hart masks (SC1). The real `virt` four-hart tree lists cpu@0 .. cpu@3; synthetic trees cover
//! ids that are not dense or not in order, a disabled hart, and ids above 63.
mod common;
use common::Dt;
use kcore::fdt;
use kcore::harts::{hart_ids, sbi_hart_masks};

const FOUR: &[u8] = include_bytes!("fixtures/virt-aia-smp4.dtb");

fn cpus(list: &[(u32, &str, &str)]) -> Vec<u8> {
    let mut d = Dt::new();
    d.begin("");
    d.begin("cpus");
    d.u32s("#address-cells", &[1]);
    for (id, compat, status) in list {
        d.begin(&format!("cpu@{id}"));
        d.u32s("reg", &[*id]);
        d.prop("compatible", format!("{compat}\0").as_bytes());
        if !status.is_empty() {
            d.prop("status", format!("{status}\0").as_bytes());
        }
        d.end();
    }
    d.end();
    d.end();
    d.finish()
}

#[test]
fn the_real_virt_tree_lists_four_harts_and_the_boot_hart_comes_first() {
    let nodes = fdt::parse(FOUR).unwrap();
    assert_eq!(hart_ids(&nodes, 0), Some(vec![0, 1, 2, 3]));
    assert_eq!(
        hart_ids(&nodes, 2),
        Some(vec![2, 0, 1, 3]),
        "the boot hart is index 0, the rest in tree order"
    );
    assert_eq!(
        hart_ids(&nodes, 9),
        None,
        "a boot hart the tree does not list"
    );
}

#[test]
fn ids_need_not_be_dense_in_order_or_below_64_and_unusable_cpus_are_left_out() {
    let t = cpus(&[
        (200, "riscv", ""),
        (3, "sifive,u74\0riscv", "okay"),
        (64, "riscv", "disabled"),
        (7, "arm,cortex-a53", ""),
        (63, "riscv", ""),
    ]);
    let nodes = fdt::parse(&t).unwrap();
    assert_eq!(hart_ids(&nodes, 3), Some(vec![3, 200, 63]));
    assert_eq!(hart_ids(&nodes, 200), Some(vec![200, 3, 63]));
    assert_eq!(hart_ids(&nodes, 64), None, "a disabled hart is not listed");
    assert_eq!(hart_ids(&nodes, 7), None, "not a RISC-V cpu");
}

#[test]
fn the_hart_masks_group_ids_by_sixty_four_with_the_base_of_each_group() {
    assert_eq!(sbi_hart_masks(&[]), vec![]);
    assert_eq!(sbi_hart_masks(&[0]), vec![(1, 0)]);
    assert_eq!(sbi_hart_masks(&[63]), vec![(1 << 63, 0)]);
    assert_eq!(sbi_hart_masks(&[64]), vec![(1, 64)]);
    assert_eq!(
        sbi_hart_masks(&[200, 63, 0, 64, 1, 200]),
        vec![(1 | 2 | 1 << 63, 0), (1, 64), (1 << 8, 192)],
        "any order, duplicates once, ascending bases"
    );
}
