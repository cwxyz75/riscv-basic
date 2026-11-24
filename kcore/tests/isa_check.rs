//! The boot-time extension check (PL1) against real QEMU device trees (`-machine virt,aia=aplic-imsic,dumpdtb`, two harts): the
//! `rva23s64` CPU has everything, plain `rv64` lacks what the profile adds, and the `riscv,isa` string form reads the same.
use kcore::fdt::{self, Node};
use kcore::isa::{extensions_of, missing_extensions, REQUIRED_EXTENSIONS};

const RVA23: &[u8] = include_bytes!("fixtures/virt-rva23s64.dtb");
const RV64: &[u8] = include_bytes!("fixtures/virt-rv64.dtb");

fn cpu(nodes: &[Node], n: usize) -> &Node {
    nodes
        .iter()
        .find(|c| c.path == format!("/cpus/cpu@{n}"))
        .unwrap()
}

#[test]
fn the_profile_cpu_passes_on_every_hart() {
    let nodes = fdt::parse(RVA23).unwrap();
    assert_eq!(missing_extensions(&nodes), vec![]);
    for h in 0..2 {
        let have = extensions_of(cpu(&nodes, h));
        for want in REQUIRED_EXTENSIONS {
            assert!(have.iter().any(|e| e == want), "hart {h}: {want}");
        }
    }
}

#[test]
fn a_plain_rv64_cpu_is_refused_and_the_missing_names_are_those_dtc_shows() {
    let nodes = fdt::parse(RV64).unwrap();
    let miss = missing_extensions(&nodes);
    let names: Vec<&str> = miss.iter().map(|m| m.1).collect();
    // QEMU's rv64 lists neither V nor Zicond nor Svade (it has Svadu) in its device tree
    assert_eq!(names, vec!["svade", "v", "zicond"], "{miss:?}");
    assert!(miss.iter().all(|m| m.0 == 0), "the first hart is reported");
}

#[test]
fn the_isa_string_form_gives_the_same_extensions() {
    let nodes = fdt::parse(RVA23).unwrap();
    let mut c = cpu(&nodes, 0).clone();
    let list = extensions_of(&c);
    c.props.retain(|(n, _)| n != "riscv,isa-extensions");
    let isa = c.prop("riscv,isa").unwrap();
    assert!(isa.starts_with(b"rv64imafdcbvh_"));
    let mut from_string = extensions_of(&c);
    let mut from_list = list;
    from_string.sort();
    from_list.sort();
    assert_eq!(from_string, from_list);
}

#[test]
fn a_tree_without_harts_or_with_one_deficient_hart_is_refused() {
    let nodes = fdt::parse(RVA23).unwrap();
    let none: Vec<Node> = nodes
        .iter()
        .filter(|n| !n.path.starts_with("/cpus/"))
        .cloned()
        .collect();
    assert_eq!(missing_extensions(&none), vec![(0, "cpu")]);
    // hart 1 loses Zicond: reported with its own id, and hart 0 does not hide it
    let mut broken = nodes.clone();
    for n in broken.iter_mut().filter(|n| n.path == "/cpus/cpu@1") {
        for (name, v) in n.props.iter_mut() {
            if name == "riscv,isa-extensions" {
                let s: Vec<u8> = v
                    .split(|&b| b == 0)
                    .filter(|w| !w.is_empty() && *w != b"zicond")
                    .flat_map(|w| w.iter().copied().chain(core::iter::once(0)))
                    .collect();
                *v = s;
            }
        }
    }
    assert_eq!(missing_extensions(&broken), vec![(1, "zicond")]);
}

/// T188 item 5: the kernel enables the cache-block instructions for user mode through `senvcfg` (`enable_cache_block_ops`), and a read-back of
/// the bits proves only that they stuck. The profile check is the other half of the evidence: a machine that lacks Zicbom or Zicboz does not boot.
#[test]
fn the_cache_block_extensions_are_required_to_boot() {
    for ext in ["zicbom", "zicboz"] {
        assert!(
            kcore::isa::REQUIRED_EXTENSIONS.contains(&ext),
            "{ext} is part of RVA23 and the user build uses its instructions"
        );
    }
}
