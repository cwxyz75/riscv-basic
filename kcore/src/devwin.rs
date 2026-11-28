//! The device windows a physical VMO may name (PL5, KERNEL-V1-SPEC 2.3, work item D1): built at boot from the device tree instead of
//! a constant table. A window is a page-aligned range of physical addresses that the `reg` of a device the system knows how to use names.
//! The rule is a security boundary, so it fails closed: a node is admitted only if its `compatible` is on [`ALLOWED`] (a new kind of device
//! is unmappable until someone decides it should be: an earlier version admitted everything but a denylist, and fw-cfg's DMA interface and
//! the flash banks were then mappable), its `status` is not disabled, and every bus above it passes addresses through unchanged. From the
//! admitted ranges are cut the kernel's own devices, RAM, and the ranges of every node the kernel or firmware drives ([`NEVER`]), so that
//! a device sharing a page with an interrupt controller does not make the controller mappable.
//!
//! **No DMA-capable device outside the IOMMU** (IOMMU-SPEC 2.6, IOMMU12): a device that can issue memory transactions of its own is
//! admitted only behind the IOMMU (the PCI host, with its `iommu-map`). `virtio,mmio` slots are DMA-capable and the tree puts them behind
//! no IOMMU, so they are not admitted (Q-IOMMU-MODEL 7); fw-cfg and the flash banks never were. What remains outside the PCI host is
//! the RTC, which has no DMA ability.

use crate::fdt::Node;
use alloc::vec::Vec;

/// The devices a physical VMO may name, by `compatible` (exact match of one string of the property): what the device manager drives.
/// Adding a device here is a deliberate decision about what a holder of the resource handle may touch, and a DMA-capable one may be
/// added only behind the IOMMU (IOMMU-SPEC 2.6): `virtio,mmio` left this list in IOMMU12.
///
/// The PCI host (`pci-host-ecam-generic`, IOMMU-SPEC 2.6) is admitted only when it is behind the IOMMU (an `iommu-map`): its windows
/// are its `reg` (the configuration window, ECAM) and the memory windows of its `ranges`, never its I/O-port window. That is safe
/// only because the IOMMU denies every device id before any window exists (IO1; the kernel asserts the boot order).
pub const ALLOWED: &[&str] = &["google,goldfish-rtc", crate::iommutree::PCI_HOST];

/// Compatible strings of nodes the kernel (or the firmware) drives or that are not memory-mapped devices at all: interrupt controllers
/// and timers, the test finisher, the power and reset syscons. Their `reg` ranges are cut from every window.
const NEVER: &[&str] = &[
    "riscv,iommu",
    "riscv,aplic",
    "riscv,imsics",
    "riscv,plic0",
    "sifive,plic-1.0.0",
    "riscv,clint0",
    "sifive,clint0",
    "riscv,aclint-mtimer",
    "riscv,aclint-mswi",
    "riscv,aclint-sswi",
    "sifive,test0",
    "sifive,test1",
    "syscon-poweroff",
    "syscon-reboot",
];

/// The nodes of a tree that are admitted devices: `compatible` on [`ALLOWED`], not disabled, every bus above them passing addresses through.
/// (Their `reg` ranges are the windows; their `interrupts` are the sources a process may take, `kcore::aiatree`.)
pub fn admitted(nodes: &[Node]) -> impl Iterator<Item = &Node> {
    nodes.iter().filter(move |n| {
        !NEVER.iter().any(|c| n.is_compatible(c))
            && ALLOWED.iter().any(|c| n.is_compatible(c))
            && !disabled(n)
            && physical(n, nodes)
            && behind_the_iommu_if_pci(n, nodes)
    })
}

/// A PCI host is admitted only with every function behind the IOMMU (`iommutree::fully_behind_the_iommu`); every other admitted
/// kind has no such condition.
fn behind_the_iommu_if_pci(n: &Node, nodes: &[Node]) -> bool {
    !n.is_compatible(crate::iommutree::PCI_HOST)
        || crate::iommutree::fully_behind_the_iommu(n, nodes)
}

/// A node that is disabled (`status` other than `okay`) is not in use: firmware marks the devices it owns that way.
pub(crate) fn disabled(n: &Node) -> bool {
    n.prop("status").is_some_and(|v| {
        let s = v.split(|&b| b == 0).next().unwrap_or(&[]);
        s != b"okay" && s != b"ok"
    })
}

/// Does every ancestor of `n` pass addresses through unchanged (a `ranges` property that is empty)? A bus with translating `ranges`, or
/// none, makes the child's `reg` a bus-local address, not a physical one: such a node is refused rather than translated.
pub(crate) fn physical(n: &Node, nodes: &[Node]) -> bool {
    let mut path = n.path.as_str();
    while let Some(slash) = path.rfind('/') {
        path = &path[..slash];
        if path.is_empty() {
            return true; // reached the root
        }
        let Some(parent) = nodes.iter().find(|p| p.path == path) else {
            return false;
        };
        match parent.prop("ranges") {
            Some([]) => {}
            _ => return false,
        }
    }
    true
}

/// The page range `[start, end)` that the byte range `(address, size)` touches (widened outward to whole pages); `None` for an empty or
/// overflowing range.
fn pages(addr: u64, size: u64) -> Option<(u64, u64)> {
    if size == 0 {
        return None;
    }
    let end = addr.checked_add(size)?;
    let end = end.checked_next_multiple_of(4096)?;
    Some((addr & !4095, end))
}

/// The device windows: every `reg` range of every admitted node (see the module comment), minus `excluded` byte ranges (what the kernel
/// drives: the console UART, and anything else the caller names), minus `ram` byte ranges (a device range that overlaps RAM or a reserved
/// range is not a window) and minus the ranges of the nodes the kernel or firmware drives, as sorted, non-overlapping page ranges
/// `(start, end)` in bytes, adjacent ones merged.
#[allow(clippy::large_stack_frames)] // KT13: boot or test only, on a stack nothing else is under (the device windows, built once at boot)
pub fn device_windows(
    nodes: &[Node],
    excluded: &[(u64, u64)],
    ram: &[(u64, u64)],
) -> Vec<(u64, u64)> {
    let mut want: Vec<(u64, u64)> = Vec::new();
    let mut cut: Vec<(u64, u64)> = excluded
        .iter()
        .chain(ram.iter())
        .filter_map(|&(a, s)| pages(a, s))
        .collect();
    for n in nodes {
        // what the kernel or firmware drives is cut from every window, wherever its range sits
        if NEVER.iter().any(|c| n.is_compatible(c)) {
            cut.extend(n.regs().iter().filter_map(|&(a, s)| pages(a, s)));
            continue;
        }
        if ALLOWED.iter().any(|c| n.is_compatible(c)) && !disabled(n) && physical(n, nodes) {
            if n.is_compatible(crate::iommutree::PCI_HOST) {
                if !crate::iommutree::fully_behind_the_iommu(n, nodes) {
                    cut.extend(n.regs().iter().filter_map(|&(a, s)| pages(a, s)));
                }
                continue; // (below: only behind the IOMMU, and with its ranges)
            }
            want.extend(n.regs().iter().filter_map(|&(a, s)| pages(a, s)));
        } else if physical(n, nodes) {
            // a device that is not admitted (or disabled) is cut too, page-widened like a window: an admitted node whose `reg` is less
            // than a page never opens the page to a neighbour that shares it (IOMMU12: a `virtio,mmio` slot beside an admitted device)
            cut.extend(n.regs().iter().filter_map(|&(a, s)| pages(a, s)));
        }
    }
    want.extend(
        crate::iommutree::pci_windows(nodes)
            .into_iter()
            .filter_map(|(a, s)| pages(a, s)),
    );
    cut.sort_unstable();
    want.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::new();
    for (mut a, b) in want {
        for &(ca, cb) in &cut {
            if ca >= b {
                break; // sorted by start: nothing further can overlap
            }
            if cb <= a {
                continue;
            }
            if ca > a {
                push(&mut out, a, ca);
            }
            a = a.max(cb);
            if a >= b {
                break;
            }
        }
        if a < b {
            push(&mut out, a, b);
        }
    }
    out
}

/// Add `[a, b)` to a sorted list, merging with the last range when they touch or overlap.
fn push(out: &mut Vec<(u64, u64)>, a: u64, b: u64) {
    match out.last_mut() {
        Some(last) if a <= last.1 => last.1 = last.1.max(b),
        _ => out.push((a, b)),
    }
}

/// Is `[pa, pa + bytes)` page aligned, non-empty and inside one window?
pub fn in_window(windows: &[(u64, u64)], pa: u64, bytes: u64) -> bool {
    let Some(end) = pa.checked_add(bytes) else {
        return false;
    };
    pa.is_multiple_of(4096)
        && bytes != 0
        && bytes.is_multiple_of(4096)
        && windows.iter().any(|&(s, e)| pa >= s && end <= e)
}
