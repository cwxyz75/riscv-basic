//! The device windows a physical VMO may name (PL5, KERNEL-V1-SPEC 2.3, work item D1): built at boot from the device tree instead of
//! a constant table. A window is a page-aligned range of physical addresses that a device node's `reg` names and the kernel does not
//! itself drive. The rule is a security boundary: a physical VMO is never RAM, never the console, the interrupt controllers, the
//! timer or the test finisher, and never a range the tree does not name.

use crate::fdt::Node;
use alloc::vec::Vec;

/// Compatible strings of nodes the kernel (or the firmware) drives or that are not memory-mapped devices at all: interrupt controllers
/// and timers, the test finisher, the power and reset syscons. A node listing any of them contributes no window.
const NEVER: &[&str] = &[
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

/// Nodes by path that are not devices: the CPUs (their `reg` is a hart id), the memory and the reserved memory (`reg` is RAM), the chosen
/// and alias nodes.
fn not_a_device(n: &Node) -> bool {
    let p = n.path.as_str();
    p == "/"
        || p == "/cpus"
        || p.starts_with("/cpus/")
        || p.starts_with("/memory")
        || p == "/reserved-memory"
        || p.starts_with("/reserved-memory/")
        || p == "/chosen"
        || p == "/aliases"
        || n.prop("device_type")
            .is_some_and(|v| v.split(|&b| b == 0).next() == Some(b"memory"))
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

/// The device windows: every `reg` range of every node that is a device, minus `excluded` byte ranges (what the kernel drives: the console
/// UART, and anything else the caller names) and minus `ram` byte ranges (a device range that overlaps RAM is not a window), as sorted,
/// non-overlapping page ranges `(start, end)` in bytes, adjacent ones merged.
pub fn device_windows(
    nodes: &[Node],
    excluded: &[(u64, u64)],
    ram: &[(u64, u64)],
) -> Vec<(u64, u64)> {
    let mut want: Vec<(u64, u64)> = Vec::new();
    for n in nodes {
        if not_a_device(n) || NEVER.iter().any(|c| n.is_compatible(c)) {
            continue;
        }
        want.extend(n.regs().iter().filter_map(|&(a, s)| pages(a, s)));
    }
    let mut cut: Vec<(u64, u64)> = excluded
        .iter()
        .chain(ram.iter())
        .filter_map(|&(a, s)| pages(a, s))
        .collect();
    cut.sort_unstable();
    want.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::new();
    for (mut a, b) in want {
        for &(ca, cb) in &cut {
            if cb <= a || ca >= b {
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
