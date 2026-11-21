//! The harts of the machine (SC1, KERNEL-V1-SPEC 2.3): which firmware hart ids the device tree lists under `/cpus` (any values, in any
//! order) and the hart masks the firmware's remote-fence call wants. Pure: nodes and ids in, numbers out. A hart has a dense **index** (the
//! position in the list this module returns, the boot hart first) and a firmware **id**; only the SBI and AIA layers use ids.

use crate::fdt::Node;
use alloc::vec::Vec;

/// The firmware ids of the harts the tree lists under `/cpus` (`cpu@*` nodes whose `compatible` names a RISC-V hart and whose `status` is not
/// disabled), the boot hart `boot_id` first and the others in tree order. `None` when the tree does not list the boot hart (the caller then
/// has only the boot hart).
pub fn hart_ids(nodes: &[Node], boot_id: u32) -> Option<Vec<u32>> {
    let mut ids: Vec<u32> = Vec::new();
    for n in nodes {
        let Some(name) = n.path.strip_prefix("/cpus/") else {
            continue;
        };
        if name.contains('/') || !(name == "cpu" || name.starts_with("cpu@")) {
            continue;
        }
        let riscv = n
            .prop("compatible")
            .is_some_and(|c| c.split(|&b| b == 0).any(|s| s.starts_with(b"riscv")));
        let usable = n
            .prop("status")
            .is_none_or(|s| s.starts_with(b"okay") || s.starts_with(b"ok"));
        // `reg` is one cell (the hart id) under `#address-cells = <1>`, which the RISC-V binding fixes
        let cells = n.cells("reg");
        let &[id] = cells.as_slice() else {
            continue;
        };
        if riscv && usable && !ids.contains(&id) {
            ids.push(id);
        }
    }
    let at = ids.iter().position(|&i| i == boot_id)?;
    ids.remove(at);
    ids.insert(0, boot_id);
    Some(ids)
}

/// Call `each(hart_mask, hart_mask_base)` for the harts `ids`, the pairs the firmware's remote-fence calls want: one per group of 64
/// consecutive ids, the base being the first id of the group (a multiple of 64) and bit `k` of the mask hart `base + k`. Pairs come in
/// ascending base order; ids may repeat or come in any order (`ids` is sorted in place). A hart id above 63 therefore needs a call of its own
/// with a non-zero base, not a shifted mask. Allocation-free: the kernel calls it with a lock held.
pub fn for_each_hart_mask(ids: &mut [usize], mut each: impl FnMut(usize, usize)) {
    ids.sort_unstable();
    let mut cur: Option<(usize, usize)> = None; // (mask, base)
    for &id in ids.iter() {
        let base = id & !63usize;
        let bit = 1usize << (id & 63);
        cur = match cur {
            Some((mask, b)) if b == base => Some((mask | bit, b)),
            Some((mask, b)) => {
                each(mask, b);
                Some((bit, base))
            }
            None => Some((bit, base)),
        };
    }
    if let Some((mask, base)) = cur {
        each(mask, base);
    }
}

/// The pairs of [`for_each_hart_mask`] as a list.
pub fn sbi_hart_masks(ids: &[usize]) -> Vec<(usize, usize)> {
    let mut sorted: Vec<usize> = ids.to_vec();
    let mut out = Vec::new();
    for_each_hart_mask(&mut sorted, |mask, base| out.push((mask, base)));
    out
}
