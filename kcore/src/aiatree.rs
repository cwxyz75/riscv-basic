//! The interrupt controller as the device tree describes it (PL5c, KERNEL-V1-SPEC 2.3): the supervisor-domain APLIC (where it is, how many
//! sources it has), the order of the harts in its IMSIC (the hart index its message-target registers want), and which of its sources a
//! process may take: only those the admitted devices (`devwin::admitted`: virtio-mmio and the RTC) name in `interrupts`, so, like the
//! device windows, an interrupt object fails closed. Pure: nodes in, numbers out.

use crate::devwin::admitted;
use crate::fdt::Node;
use alloc::vec::Vec;

/// What the kernel needs of the interrupt controller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Aia {
    /// Physical address of the supervisor-domain APLIC's registers.
    pub aplic_base: u64,
    /// The number of interrupt sources it has (`riscv,num-sources`).
    pub num_sources: u32,
    /// The firmware ids of the harts in the order of the IMSIC's interrupt files: the position of a hart here is its hart index in an
    /// APLIC target register.
    pub hart_order: Vec<u32>,
    /// The sources a process may take (sorted, unique, each at most 63: one IMSIC `eie` register holds ids 0 to 63).
    pub sources: Vec<u32>,
}

/// The first cell of a property of this node or, failing that, of the nearest ancestor that has it (how `interrupt-parent` is inherited).
fn inherited(nodes: &[Node], n: &Node, name: &str) -> Option<u32> {
    if let Some(v) = n.cells(name).first() {
        return Some(*v);
    }
    let mut path = n.path.as_str();
    while let Some(slash) = path.rfind('/') {
        path = &path[..slash];
        let want = if path.is_empty() { "/" } else { path };
        let ancestor = nodes.iter().find(|x| x.path == want)?;
        if let Some(v) = ancestor.cells(name).first() {
            return Some(*v);
        }
        if path.is_empty() {
            return None; // the root was the last place to look
        }
    }
    None
}

fn phandle(n: &Node) -> Option<u32> {
    n.cells("phandle").first().copied()
}

/// The hart id of the CPU whose interrupt-controller child has phandle `ph`: the `reg` of the parent of the node that has that phandle.
fn hart_of_intc(nodes: &[Node], ph: u32) -> Option<u32> {
    let intc = nodes
        .iter()
        .find(|n| n.path.starts_with("/cpus/") && phandle(n) == Some(ph))?;
    let cpu_path = &intc.path[..intc.path.rfind('/')?];
    nodes
        .iter()
        .find(|n| n.path == cpu_path)?
        .cells("reg")
        .first()
        .copied()
}

/// Find the supervisor-domain APLIC (the one without children, with `msi-parent`: message-signalled delivery needs an IMSIC) and what goes
/// with it. `None` for a tree with no such controller (an APLIC in direct mode only, a PLIC machine, no interrupt controller).
pub fn discover(nodes: &[Node]) -> Option<Aia> {
    let aplic = nodes
        .iter()
        .find(|n| n.is_compatible("riscv,aplic") && n.prop("riscv,children").is_none())?;
    let aplic_base = aplic.regs().first()?.0;
    let num_sources = *aplic.cells("riscv,num-sources").first()?;
    let imsic_ph = *aplic.cells("msi-parent").first()?;
    let imsic = nodes.iter().find(|n| phandle(n) == Some(imsic_ph))?;
    // interrupts-extended: (cpu interrupt controller phandle, interrupt number) per hart, in hart-index order
    let ext = imsic.cells("interrupts-extended");
    let mut hart_order = Vec::new();
    // (the phandle of each pair; a trailing odd cell is not a pair)
    for ph in ext.iter().step_by(2).take(ext.len() / 2) {
        hart_order.push(hart_of_intc(nodes, *ph)?);
    }
    if hart_order.is_empty() {
        return None;
    }
    let aplic_ph = phandle(aplic);
    let mut sources: Vec<u32> = Vec::new();
    for n in admitted(nodes) {
        // only devices whose interrupts go to this APLIC, and only sources it has
        let parent = inherited(nodes, n, "interrupt-parent");
        if parent.is_none() || parent != aplic_ph {
            continue;
        }
        if let Some(&src) = n.cells("interrupts").first() {
            if (1..=63).contains(&src) && src <= num_sources {
                sources.push(src);
            }
        }
    }
    sources.sort_unstable();
    sources.dedup();
    Some(Aia {
        aplic_base,
        num_sources,
        hart_order,
        sources,
    })
}
