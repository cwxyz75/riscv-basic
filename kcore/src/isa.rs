//! The boot-time extension check (PL1, OD47, OD127): the kernel is RVA23S64 plus what OD127 adds (the AIA's supervisor part), and
//! it refuses to run on a hart that lacks one of the extensions it relies on. The logic is pure: it reads the device tree's
//! `cpu` nodes (`riscv,isa-extensions`, or the older `riscv,isa` string, and `mmu-type`) and names what is missing.

use crate::fdt::Node;
use alloc::string::String;
use alloc::vec::Vec;

/// The extensions every hart must have, by the name the device tree gives them. Each was confirmed against the RVA23 profile text
/// (`riscv/riscv-profiles`, `rva23-profile.adoc`, 2026-11-20): Sv39 (checked through `mmu-type`), Svade, Sstc, Zicbom, Zicboz, Zicntr,
/// V and Zicond are mandatory in RVA23S64 (the unprivileged ones inherited from RVA23U64); `B` is Zba, Zbb and Zbs. `ssaia` is not
/// part of the profile: OD127 adds it (the AIA replaces the PLIC).
pub const REQUIRED_EXTENSIONS: &[&str] = &[
    "svade", "sstc", "zicbom", "zicboz", "zba", "zbb", "zbs", "v", "zicond", "zicntr", "ssaia",
];

/// The extension names a `cpu` node advertises: the NUL-separated list of `riscv,isa-extensions`, else the parsed `riscv,isa` string
/// (the base letters after `rv64` or `rv32` as single-letter extensions, then each `_`-separated name).
#[allow(clippy::large_stack_frames)] // KT13: boot or test only, on a stack nothing else is under (the harts' extensions, read at boot)
pub fn extensions_of(cpu: &Node) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(v) = cpu.prop("riscv,isa-extensions") {
        for w in v.split(|&b| b == 0).filter(|w| !w.is_empty()) {
            if let Ok(s) = core::str::from_utf8(w) {
                out.push(String::from(s));
            }
        }
        return out;
    }
    if let Some(v) = cpu.prop("riscv,isa") {
        let s = core::str::from_utf8(v).unwrap_or("");
        let s = s.trim_end_matches('\0');
        let mut parts = s.split('_');
        if let Some(base) = parts.next() {
            let letters = base
                .strip_prefix("rv64")
                .or_else(|| base.strip_prefix("rv32"))
                .unwrap_or(base);
            for c in letters.chars() {
                let mut one = String::new();
                one.push(c);
                out.push(one);
            }
        }
        for p in parts {
            out.push(String::from(p));
        }
    }
    out
}

/// Does the node's `mmu-type` give Sv39? Sv48 and Sv57 both require Sv39 to be implemented, so they count.
fn has_sv39(cpu: &Node) -> bool {
    cpu.prop("mmu-type").is_some_and(|v| {
        let v = v.split(|&b| b == 0).next().unwrap_or(&[]);
        v == b"riscv,sv39" || v == b"riscv,sv48" || v == b"riscv,sv57"
    })
}

/// Is this node a hart (`device_type = "cpu"` under `/cpus`)?
fn is_cpu(n: &Node) -> bool {
    n.path.starts_with("/cpus/")
        && n.prop("device_type")
            .is_some_and(|v| v.split(|&b| b == 0).next() == Some(b"cpu"))
}

/// What the first deficient hart lacks: `(hart's reg, name)` for each missing extension of that hart, or empty when every hart has all of
/// [`REQUIRED_EXTENSIONS`] and Sv39. A tree with no hart node at all is reported as `(0, "cpu")` missing: the kernel cannot tell what it
/// runs on.
pub fn missing_extensions(nodes: &[Node]) -> Vec<(u32, &'static str)> {
    let mut missing = Vec::new();
    let mut harts = 0usize;
    for cpu in nodes.iter().filter(|n| is_cpu(n)) {
        harts = harts.saturating_add(1);
        let id = cpu.u32_prop("reg").unwrap_or(u32::MAX);
        let have = extensions_of(cpu);
        if !has_sv39(cpu) {
            missing.push((id, "sv39"));
        }
        for &want in REQUIRED_EXTENSIONS {
            if !have.iter().any(|h| h == want) {
                missing.push((id, want));
            }
        }
        if !missing.is_empty() {
            return missing;
        }
    }
    if harts == 0 {
        missing.push((0, "cpu"));
    }
    missing
}
