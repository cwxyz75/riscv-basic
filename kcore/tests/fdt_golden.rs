//! The device-tree reader against real QEMU trees (`-machine virt,dumpdtb`), with expectations taken
//! from `dtc` (an independent reader) rather than from this parser, plus hostile-input checks.
use kcore::fdt::{self, FdtError};

const VIRT: &[u8] = include_bytes!("fixtures/virt.dtb");
const AIA: &[u8] = include_bytes!("fixtures/virt-aia.dtb");

#[test]
fn the_default_board_has_a_plic_and_eight_virtio_slots() {
    let nodes = fdt::parse(VIRT).unwrap();
    assert_eq!(nodes.len(), 31, "dtc counts 31 nodes");
    assert_eq!(nodes[0].path, "/");
    let soc = nodes.iter().find(|n| n.path == "/soc").unwrap();
    assert!(soc.is_compatible("simple-bus"));
    assert!(nodes.iter().any(|n| n.is_compatible("riscv,plic0")));
    let slots = fdt::virtio_mmio(&nodes);
    assert_eq!(slots.len(), 8);
    for (k, s) in slots.iter().rev().enumerate() {
        // the tree lists them from the highest address down
        assert_eq!(s.base, 0x1000_1000 + k as u64 * 0x1000, "slot {k}");
        assert_eq!(s.size, 0x1000);
        assert_eq!(s.irq, k as u32 + 1);
    }
    let mem = nodes
        .iter()
        .find(|n| n.path.starts_with("/memory@"))
        .unwrap();
    assert_eq!(mem.reg(), Some((0x8000_0000, 128 * 1024 * 1024)));
    assert!(fdt::has_compatible(VIRT, "riscv,plic0"));
}

#[test]
fn the_aia_board_has_no_plic_but_the_same_slots() {
    let nodes = fdt::parse(AIA).unwrap();
    assert_eq!(nodes.len(), 34, "dtc counts 34 nodes");
    assert!(!nodes.iter().any(|n| n.is_compatible("riscv,plic0")));
    assert!(!fdt::has_compatible(AIA, "riscv,plic0"));
    assert!(nodes.iter().any(|n| n.is_compatible("riscv,aplic")));
    assert_eq!(fdt::virtio_mmio(&nodes).len(), 8);
}

#[test]
fn header_errors_are_reported() {
    assert_eq!(fdt::parse(&[]).err(), Some(FdtError::BadHeader));
    assert_eq!(fdt::parse(&VIRT[..20]).err(), Some(FdtError::BadHeader));
    let mut bad = VIRT.to_vec();
    bad[0] ^= 1;
    assert_eq!(fdt::parse(&bad).err(), Some(FdtError::BadHeader));
    let mut short = VIRT.to_vec();
    short.truncate(VIRT.len() - 100);
    assert!(
        fdt::parse(&short).is_err(),
        "the header claims more than there is"
    );
    assert_eq!(fdt::total_size(VIRT), Some(VIRT.len()));
}

#[test]
fn every_truncation_and_corruption_is_an_error_or_a_tree_never_a_panic() {
    for n in 0..VIRT.len() {
        let _ = fdt::parse(&VIRT[..n]);
    }
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for _ in 0..4000 {
        let mut c = VIRT.to_vec();
        for _ in 0..1 + next() % 6 {
            let i = (next() % c.len() as u64) as usize;
            c[i] = next() as u8;
        }
        if let Ok(nodes) = fdt::parse(&c) {
            let _ = fdt::virtio_mmio(&nodes); // whatever came out is safe to use
            for n in &nodes {
                let _ = n.reg();
            }
        }
    }
}

#[test]
fn the_limits_bound_a_hostile_tree() {
    // 5000 empty nodes under the root: refused as too big, not parsed
    let mut st: Vec<u8> = vec![];
    let w = |v: &mut Vec<u8>, x: u32| v.extend(x.to_be_bytes());
    w(&mut st, 1);
    st.extend([0u8; 4]);
    for _ in 0..5000 {
        w(&mut st, 1);
        st.extend(b"n\0\0\0");
        w(&mut st, 2);
    }
    w(&mut st, 2);
    w(&mut st, 9);
    let mut b = vec![];
    let total = 40 + st.len();
    for x in [
        0xd00d_feedu32,
        total as u32,
        40,
        total as u32,
        0,
        17,
        16,
        0,
        0,
        st.len() as u32,
    ] {
        b.extend(x.to_be_bytes());
    }
    b.extend(&st);
    assert_eq!(fdt::parse(&b).err(), Some(FdtError::TooBig));
}

fn tree(strings: &[u8], structure: &[u8]) -> Vec<u8> {
    let off_struct = 40usize;
    let off_strings = off_struct + structure.len();
    let total = off_strings + strings.len();
    let mut b = vec![];
    for x in [
        0xd00d_feedu32,
        total as u32,
        off_struct as u32,
        off_strings as u32,
        0,
        17,
        16,
        0,
        strings.len() as u32,
        structure.len() as u32,
    ] {
        b.extend(x.to_be_bytes());
    }
    b.extend(structure);
    b.extend(strings);
    b
}

#[test]
fn a_long_name_cannot_multiply_into_a_huge_allocation() {
    // one node with a 100 KiB name and thousands of children: every child's path would copy it
    let w = |v: &mut Vec<u8>, x: u32| v.extend(x.to_be_bytes());
    let mut st = vec![];
    w(&mut st, 1);
    st.extend([0u8; 4]);
    w(&mut st, 1);
    let name = vec![b'a'; 100_000];
    st.extend(&name);
    st.push(0);
    while st.len() % 4 != 0 {
        st.push(0);
    }
    for _ in 0..3000 {
        w(&mut st, 1);
        st.extend(b"c\0\0\0");
        w(&mut st, 2);
    }
    w(&mut st, 2);
    w(&mut st, 2);
    w(&mut st, 9);
    assert_eq!(fdt::parse(&tree(b"x\0", &st)).err(), Some(FdtError::TooBig));
}

#[test]
fn many_properties_sharing_one_huge_name_are_refused() {
    let w = |v: &mut Vec<u8>, x: u32| v.extend(x.to_be_bytes());
    let mut strings = vec![b'p'; 100_000];
    strings.push(0);
    let mut st = vec![];
    w(&mut st, 1);
    st.extend([0u8; 4]);
    for _ in 0..10_000 / 64 + 1 {
        w(&mut st, 1);
        st.extend(b"n\0\0\0");
        for _ in 0..64 {
            w(&mut st, 3);
            w(&mut st, 0);
            w(&mut st, 0);
        }
        w(&mut st, 2);
    }
    w(&mut st, 2);
    w(&mut st, 9);
    assert_eq!(
        fdt::parse(&tree(&strings, &st)).err(),
        Some(FdtError::TooBig)
    );
}
