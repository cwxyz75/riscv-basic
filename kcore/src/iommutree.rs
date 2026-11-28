//! The platform IOMMU as the device tree describes it (IOMMU5; IOMMU-SPEC 2.2, 2.5, 2.6, 3.3): the `riscv,iommu` node's register
//! page and its own wired interrupt sources on the supervisor-domain APLIC, the PCI host's `iommu-map` (which device ids exist and
//! that they are behind this IOMMU) and its memory windows, and the highest physical address the tree names anywhere. Pure: nodes
//! in, numbers out, safe on any tree (a malformed or hostile tree is a refusal, never a panic).

use crate::aiatree::{inherited, phandle};
use crate::devwin::{disabled, physical};
use crate::fdt::Node;
use alloc::vec::Vec;
use core::fmt;

/// The compatible of the platform IOMMU (a device on the system bus, not a PCI function: IOMMU-SPEC 2.1).
pub const COMPATIBLE: &str = "riscv,iommu";
/// The compatible of the PCI host whose windows the root resource may map (IOMMU-SPEC 2.6).
pub const PCI_HOST: &str = "pci-host-ecam-generic";

/// The cause of a wired IOMMU interrupt, by its position in the node's `interrupts` (the IOMMU specification's order, which
/// QEMU's `virt` follows: command queue, fault queue, performance monitor, page-request queue). The position is the vector the
/// kernel writes to `icvec` for that cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    CommandQueue = 0,
    FaultQueue = 1,
    PerfMonitor = 2,
    PageRequest = 3,
}

/// One of the IOMMU's own wired sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Source {
    /// The APLIC source number.
    pub number: u32,
    /// The trigger type of the tree's second cell: 1 rising edge or 4 level high (the two the kernel programs; a falling-edge or
    /// level-low source is a refusal, not a guess).
    pub trigger: u32,
}

/// One entry of the PCI host's `iommu-map`: requester ids `rid_base ..` (`len` of them) are device ids `did_base ..`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MapEntry {
    pub rid_base: u32,
    pub did_base: u32,
    pub len: u32,
}

/// A PCI host some of whose requester ids are behind the IOMMU (all of them only when `fully_behind_the_iommu` says so).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PciHost {
    /// The configuration window (ECAM): the host's `reg`.
    pub ecam: (u64, u64),
    /// The memory windows of its `ranges` (32- and 64-bit memory space), as physical (address, size). The I/O-port window is not
    /// one of them.
    pub memory: Vec<(u64, u64)>,
    /// Its `iommu-map`, every entry naming this IOMMU.
    pub map: Vec<MapEntry>,
}

/// What the kernel needs of the IOMMU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Iommu {
    /// The register page (`reg`): its base and size.
    pub base: u64,
    pub size: u64,
    /// Its wired sources in the tree's order ([`Cause`] is the index).
    pub sources: Vec<Source>,
    /// The PCI hosts with requester ids behind it (one on `virt`), wholly or in part.
    pub pci: Vec<PciHost>,
    /// One past the highest device id any `iommu-map` names (the directory must cover it).
    pub device_ids: u32,
}

impl Iommu {
    /// The source of `cause`, if the tree gives one.
    pub fn source(&self, cause: Cause) -> Option<Source> {
        self.sources.get(cause as usize).copied()
    }

    /// Is `device_id` one that some host's `iommu-map` names for this IOMMU? (A map may leave holes: one past the highest,
    /// `device_ids`, is not enough.)
    pub fn covers(&self, device_id: u64) -> bool {
        self.pci.iter().flat_map(|h| &h.map).any(|e| {
            let base = u64::from(e.did_base);
            device_id >= base && device_id < base.saturating_add(u64::from(e.len))
        })
    }
}

/// Why there is no usable IOMMU. Its `Display` contains `no IOMMU`, the refusal line of IOMMU-SPEC 2.5.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Absent {
    /// No `riscv,iommu` node.
    None,
    /// More than one (only one is supported).
    Several,
    /// The node is there but cannot be used: the reason.
    Malformed(&'static str),
}

impl fmt::Display for Absent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Absent::None => f.write_str("no IOMMU: the device tree has no riscv,iommu node"),
            Absent::Several => {
                f.write_str("no IOMMU: the device tree has more than one riscv,iommu node")
            }
            Absent::Malformed(why) => write!(f, "no IOMMU: the riscv,iommu node {why}"),
        }
    }
}

/// The supervisor-domain APLIC's phandle and its `#interrupt-cells` (the APLIC `aiatree` uses: no children, `msi-parent`).
fn s_aplic(nodes: &[Node]) -> Option<(u32, u32)> {
    let a = nodes.iter().find(|n| {
        n.is_compatible("riscv,aplic")
            && n.prop("riscv,children").is_none()
            && n.prop("msi-parent").is_some()
    })?;
    Some((phandle(a)?, *a.cells("#interrupt-cells").first()?))
}

/// The memory windows of a PCI host's `ranges`: entries of (child address: 3 cells, parent address: the parent's
/// `#address-cells`, size: the host's `#size-cells`), of which those whose space code (bits 25:24 of the first cell) is 32-bit
/// (2) or 64-bit (3) memory. `None` if the property does not divide into whole entries or a cell count is not one this reads.
#[allow(clippy::large_stack_frames)] // KT13: boot or test only, on a stack nothing else is under (the tree's IOMMU, read once at boot)
fn pci_memory_windows(host: &Node) -> Option<Vec<(u64, u64)>> {
    let child_cells = *host.cells("#address-cells").first()? as usize;
    let size_cells = *host.cells("#size-cells").first()? as usize;
    let parent_cells = host.addr_cells as usize;
    if child_cells != 3 || !(1..=2).contains(&size_cells) || !(1..=2).contains(&parent_cells) {
        return None;
    }
    let cells = host.cells("ranges");
    // (3 + at most 2 + at most 2: no overflow)
    let per = 3usize
        .saturating_add(parent_cells)
        .saturating_add(size_cells);
    if !cells.len().is_multiple_of(per) {
        return None;
    }
    let wide = |c: &[u32]| c.iter().fold(0u64, |acc, &x| (acc << 32) | u64::from(x));
    let mut out = Vec::new();
    for e in cells.chunks_exact(per) {
        let (child, rest) = e.split_at_checked(3)?;
        let (parent, size) = rest.split_at_checked(parent_cells)?;
        let space = (child.first()? >> 24) & 3;
        let (parent, size) = (wide(parent), wide(size));
        if (space == 2 || space == 3) && size != 0 {
            parent.checked_add(size)?;
            out.push((parent, size));
        }
    }
    Some(out)
}

/// One entry of an `iommu-map` as the binding writes it (`pci-iommu.yaml`): requester ids `rid_base ..` (`len` of them) are, on
/// the IOMMU whose phandle is `iommu`, the specifiers `did_base ..`.
struct RawEntry {
    rid_base: u32,
    iommu: u32,
    did_base: u32,
    len: u32,
}

/// The entries of `host`'s `iommu-map`: tuples of (rid-base, iommu phandle, iommu-base, length), one cell each (the binding's
/// specifier is one cell, as `riscv,iommu`'s `#iommu-cells = <1>` makes it). An entry of length 0 maps nothing and is dropped
/// (QEMU 10.2 and 11.0 write one ahead of their real entry); an empty property maps nothing too. `None` if the property does
/// not divide into whole entries.
fn map_entries(host: &Node) -> Option<Vec<RawEntry>> {
    let cells = host.cells("iommu-map");
    let (quads, rest) = cells.as_chunks::<4>();
    if !rest.is_empty() {
        return None;
    }
    Some(
        quads
            .iter()
            .filter(|q| q[3] != 0)
            .map(|&[rid_base, iommu, did_base, len]| RawEntry {
                rid_base,
                iommu,
                did_base,
                len,
            })
            .collect(),
    )
}

/// Find the one platform IOMMU, its sources (each on the supervisor-domain APLIC, with that controller's two-cell interrupt
/// specifiers) and the PCI hosts whose `iommu-map` names it.
#[allow(clippy::large_stack_frames)] // KT13: boot or test only, on a stack nothing else is under (the tree's IOMMU, read once at boot)
pub fn discover(nodes: &[Node]) -> Result<Iommu, Absent> {
    let mut found = nodes
        .iter()
        .filter(|n| n.is_compatible(COMPATIBLE) && !disabled(n));
    let node = found.next().ok_or(Absent::None)?;
    if found.next().is_some() {
        return Err(Absent::Several);
    }
    if !physical(node, nodes) {
        return Err(Absent::Malformed(
            "is not on a bus that passes addresses through",
        ));
    }
    let (base, size) = *node.regs().first().ok_or(Absent::Malformed("has no reg"))?;
    if size < 0x1000 || base % 0x1000 != 0 || base.checked_add(size).is_none() {
        return Err(Absent::Malformed(
            "has a register range that is not a whole page",
        ));
    }
    let me = phandle(node).ok_or(Absent::Malformed("has no phandle"))?;
    let (aplic, icells) = s_aplic(nodes).ok_or(Absent::Malformed(
        "has no supervisor-domain APLIC to deliver its interrupts",
    ))?;
    if inherited(nodes, node, "interrupt-parent") != Some(aplic) || icells != 2 {
        return Err(Absent::Malformed(
            "does not interrupt through the supervisor-domain APLIC",
        ));
    }
    let cells = node.cells("interrupts");
    let (pairs, rest) = cells.as_chunks::<2>();
    if !rest.is_empty() || pairs.len() < 2 || pairs.len() > 4 {
        return Err(Absent::Malformed(
            "does not name its command-queue and fault-queue interrupts",
        ));
    }
    let sources: Vec<Source> = pairs
        .iter()
        .map(|&[number, trigger]| Source { number, trigger })
        .collect();
    if sources
        .iter()
        .any(|s| s.number == 0 || s.number > 1023 || ![1, 4].contains(&s.trigger))
        || sources
            .iter()
            .enumerate()
            .any(|(i, s)| sources.iter().take(i).any(|t| t.number == s.number))
    {
        return Err(Absent::Malformed(
            "names an interrupt source that cannot be used",
        ));
    }
    if node.cells("#iommu-cells") != [1] {
        return Err(Absent::Malformed(
            "does not have #iommu-cells = <1> (one cell of device id)",
        ));
    }
    let mut pci = Vec::new();
    let mut device_ids = 0u32;
    for host in nodes.iter().filter(|n| n.is_compatible(PCI_HOST)) {
        if host.prop("iommu-map").is_none() {
            continue; // not behind this IOMMU: its windows are not opened (devwin)
        }
        let map = map_entries(host).ok_or(Absent::Malformed(
            "has a PCI host whose iommu-map cannot be read",
        ))?;
        // (an entry naming another IOMMU is that IOMMU's business: it puts the host only partly behind this one, which
        // `fully_behind_the_iommu` refuses windows for; it does not make this IOMMU unusable)
        let mut entries = Vec::new();
        for e in map.into_iter().filter(|e| e.iommu == me) {
            let end = u64::from(e.did_base).saturating_add(u64::from(e.len));
            if end > 1 << 24 {
                return Err(Absent::Malformed(
                    "has a PCI host whose iommu-map names device ids past 24 bits",
                ));
            }
            device_ids = device_ids.max(end as u32);
            entries.push(MapEntry {
                rid_base: e.rid_base,
                did_base: e.did_base,
                len: e.len,
            });
        }
        if entries.is_empty() {
            continue; // no requester id of this host is behind this IOMMU
        }
        let ecam = *host
            .regs()
            .first()
            .ok_or(Absent::Malformed("has a PCI host with no reg"))?;
        let memory = pci_memory_windows(host).ok_or(Absent::Malformed(
            "has a PCI host whose ranges cannot be read",
        ))?;
        pci.push(PciHost {
            ecam,
            memory,
            map: entries,
        });
    }
    Ok(Iommu {
        base,
        size,
        sources,
        pci,
        device_ids,
    })
}

/// Is every function of PCI host `host` behind the one platform IOMMU? Its `iommu-map` must name only that IOMMU, have no
/// `iommu-map-mask` that folds requester ids together (one that keeps all 16 bits is the identity), map each requester id to
/// the same device id (IOMMU-SPEC 2.2: device id = requester id; any other map can make two functions one device id, which one
/// domain per function cannot express), and cover every requester id the host can issue (0 to 0xffff: bus, device and
/// function). A host with no map, a partial map or a map naming anything else is not behind it (IOMMU-SPEC 2.6: fail closed; a
/// function outside the map would reach memory untranslated).
pub fn fully_behind_the_iommu(host: &Node, nodes: &[Node]) -> bool {
    let mut iommus = nodes
        .iter()
        .filter(|n| n.is_compatible(COMPATIBLE) && !disabled(n));
    let (Some(me), None) = (iommus.next().and_then(phandle), iommus.next()) else {
        return false;
    };
    // the mask is applied to a requester id before the lookup: one that keeps all 16 bits folds nothing, and any other makes
    // several functions one device id, which one domain per function cannot express
    if host.prop("iommu-map-mask").is_some() {
        match host.cells("iommu-map-mask").as_slice() {
            [m] if m & 0xffff == 0xffff => {}
            _ => return false,
        }
    }
    let Some(map) = map_entries(host) else {
        return false;
    };
    if map.is_empty()
        || map
            .iter()
            .any(|e| e.iommu != me || e.did_base != e.rid_base)
    {
        return false;
    }
    let mut spans: Vec<(u64, u64)> = map
        .iter()
        .map(|e| {
            (
                u64::from(e.rid_base),
                u64::from(e.rid_base).saturating_add(u64::from(e.len)),
            )
        })
        .collect();
    spans.sort_unstable();
    let mut covered = 0u64;
    for (start, end) in spans {
        if start > covered {
            return false; // a hole
        }
        covered = covered.max(end);
    }
    covered >= 0x1_0000
}

/// The PCI hosts fully behind the IOMMU (`fully_behind_the_iommu`), not disabled, on a bus that passes addresses through: their
/// ECAM and memory windows are device windows (IOMMU-SPEC 2.6; `kcore::devwin`). Any other host gets none.
pub fn pci_windows(nodes: &[Node]) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    for host in nodes.iter().filter(|n| {
        n.is_compatible(PCI_HOST)
            && fully_behind_the_iommu(n, nodes)
            && !disabled(n)
            && physical(n, nodes)
    }) {
        let (Some(ecam), Some(mem)) = (host.regs().first().copied(), pci_memory_windows(host))
        else {
            continue;
        };
        out.push(ecam);
        out.extend(mem);
    }
    out
}

/// One past the highest physical address the tree names: RAM, reserved memory, the `reg` of every node on a bus that passes
/// addresses through, and the parent side of every PCI host's `ranges` (IOMMU-SPEC 3.3: a tree that names anything at or above
/// `IO_BASE` is a boot refusal). `None` if some range overflows 64 bits.
#[allow(clippy::large_stack_frames)] // KT13: boot or test only, on a stack nothing else is under (read once at boot)
pub fn highest_address(nodes: &[Node]) -> Option<u64> {
    let mut top = 0u64;
    let mut see = |(a, s): (u64, u64)| -> Option<()> {
        top = top.max(a.checked_add(s)?);
        Some(())
    };
    for r in crate::fdt::memory_ranges(nodes) {
        see(r)?;
    }
    for r in crate::fdt::reserved_ranges(nodes) {
        see(r)?;
    }
    for n in nodes {
        if n.path.starts_with("/memory") || n.path.starts_with("/reserved-memory") {
            continue; // (counted above)
        }
        if physical(n, nodes) && n.path != "/" {
            for r in n.regs() {
                see(r)?;
            }
        }
        if n.is_compatible(PCI_HOST) {
            for r in pci_memory_windows(n).unwrap_or_default() {
                see(r)?;
            }
        }
    }
    Some(top)
}
