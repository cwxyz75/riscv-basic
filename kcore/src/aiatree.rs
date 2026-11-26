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
    /// Physical address of the first hart's supervisor interrupt file; the file of hart index `i` is the page at `imsic_base + i * 0x1000`.
    pub imsic_base: u64,
    /// The firmware ids of the harts in the order of the IMSIC's interrupt files: the position of a hart here is its hart index in an
    /// APLIC target register.
    pub hart_order: Vec<u32>,
    /// The identities of each hart's interrupt file (`riscv,num-ids`: 1 to this, 63 to 2047). Every identity the kernel uses is
    /// allocated per hart (`kcore::msiid`, IOMMU-SPEC 8.1); none is derived from a source number.
    pub num_ids: u32,
    /// The sources a process may take (sorted, unique, each from 1 to `num_sources`).
    pub sources: Vec<u32>,
    /// Sources an admitted device on this APLIC names that the APLIC does not have (0, or above `num_sources`): not usable. The
    /// kernel says so at boot instead of dropping them quietly (T183).
    pub dropped: Vec<u32>,
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
        // an interrupt controller or an interrupt nexus above the device is its own interrupt domain: the root's parent is not the device's
        if ancestor.prop("interrupt-controller").is_some()
            || ancestor.prop("interrupt-map").is_some()
        {
            return None;
        }
        if path.is_empty() {
            return None; // the root was the last place to look
        }
    }
    None
}

fn phandle(n: &Node) -> Option<u32> {
    // (`linux,phandle` is the older spelling of the same property)
    n.cells("phandle")
        .first()
        .copied()
        .or_else(|| n.cells("linux,phandle").first().copied())
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
    let regs = aplic.regs();
    let (aplic_base, aplic_size) = *regs.first()?;
    // the registers the kernel writes reach 0x3004 + 4 * (sources - 1): the domain's four pages
    if aplic_size < 0x4000 {
        return None;
    }
    let num_sources = *aplic.cells("riscv,num-sources").first()?;
    // an APLIC has at most 1023 sources (the AIA specification); a tree that says more is wrong
    if num_sources == 0 || num_sources > 1023 {
        return None;
    }
    let imsic_ph = *aplic.cells("msi-parent").first()?;
    let imsic = nodes.iter().find(|n| phandle(n) == Some(imsic_ph))?;
    // IMSIC groups (several sockets) change how a hart index is formed (`(group << riscv,hart-index-bits) | index in the group`): this reads
    // only the single-group layout, where the index is the position in the list, and refuses a controller that says otherwise
    if imsic.regs().len() != 1
        || imsic
            .cells("riscv,group-index-bits")
            .first()
            .is_some_and(|&b| b != 0)
        || imsic
            .cells("riscv,guest-index-bits")
            .first()
            .is_some_and(|&b| b != 0)
    {
        return None;
    }
    let (imsic_base, imsic_size) = *imsic.regs().first()?;
    // the identities of each file: the IMSIC binding requires the property, and the AIA specification allows 63 to 2047
    let num_ids = *imsic.cells("riscv,num-ids").first()?;
    if !(63..=2047).contains(&num_ids) {
        return None;
    }
    // interrupts-extended: (cpu interrupt controller phandle, interrupt number) per hart, in hart-index order; the interrupt number must be
    // 9, the supervisor external interrupt (11 is the machine's file: not ours to use)
    let ext = imsic.cells("interrupts-extended");
    let mut hart_order = Vec::new();
    for (k, ph) in ext.iter().step_by(2).take(ext.len() / 2).enumerate() {
        let irq = ext.get(k.checked_mul(2)?.checked_add(1)?)?;
        if *irq != 9 {
            return None;
        }
        hart_order.push(hart_of_intc(nodes, *ph)?);
    }
    // one page per file, all inside the node's register window; a hart index is 14 bits in an APLIC target word
    if hart_order.is_empty()
        || hart_order.len() > 1 << 14
        || imsic_size < (hart_order.len() as u64).checked_mul(0x1000)?
    {
        return None;
    }
    let aplic_ph = phandle(aplic);
    let mut sources: Vec<u32> = Vec::new();
    let mut dropped: Vec<u32> = Vec::new();
    for n in admitted(nodes) {
        // only devices whose interrupts go to this APLIC (a device that names its parents in `interrupts-extended` is not read), and only
        // sources it has
        let parent = inherited(nodes, n, "interrupt-parent");
        if parent.is_none() || parent != aplic_ph || n.prop("interrupts-extended").is_some() {
            continue;
        }
        if let Some(&src) = n.cells("interrupts").first() {
            if (1..=num_sources).contains(&src) {
                sources.push(src);
            } else {
                dropped.push(src);
            }
        }
    }
    sources.sort_unstable();
    sources.dedup();
    dropped.sort_unstable();
    dropped.dedup();
    Some(Aia {
        aplic_base,
        num_sources,
        imsic_base,
        hart_order,
        num_ids,
        sources,
        dropped,
    })
}
