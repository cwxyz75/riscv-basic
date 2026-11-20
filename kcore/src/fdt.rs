//! A device-tree reader (S6a-2, S6c): `has_compatible`, an allocation-free scan the kernel uses to
//! tell a PLIC machine from an AIA one, and `parse`, the whole tree for the device manager.
//! Hostile input never panics and costs a bounded amount: every offset is bounds-checked, and the
//! limits below cap nodes, depth, properties, name lengths and the bytes the parse retains.

const MAGIC: u32 = 0xd00d_feed;

#[allow(clippy::arithmetic_side_effects)] // INVARIANT: `get(at..at + 4)` is a checked slice and `at` is a table offset inside the blob
fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// Does the flattened device tree `blob` contain a `compatible` property with the string
/// `name` among its (NUL-separated) values? `false` for anything that is not a well-formed tree.
pub fn has_compatible(blob: &[u8], name: &str) -> bool {
    scan(blob, name).unwrap_or(false)
}

#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: every access is through checked `get` slices; `at` advances by small fixed steps inside the structure block
fn scan(blob: &[u8], name: &str) -> Option<bool> {
    if be32(blob, 0)? != MAGIC {
        return None;
    }
    let total = be32(blob, 4)? as usize;
    let off_struct = be32(blob, 8)? as usize;
    let off_strings = be32(blob, 12)? as usize;
    let size_struct = be32(blob, 36)? as usize;
    let blob = blob.get(..total)?;
    let strings = blob.get(off_strings..)?;
    let structure = blob.get(off_struct..off_struct.checked_add(size_struct)?)?;
    let mut at = 0usize;
    loop {
        match be32(structure, at)? {
            1 => {
                // FDT_BEGIN_NODE: a NUL-terminated name, padded to 4
                let rest = structure.get(at + 4..)?;
                let len = rest.iter().position(|&b| b == 0)?;
                at = at.checked_add(4 + (len + 1).next_multiple_of(4))?;
            }
            2 => at += 4, // FDT_END_NODE
            3 => {
                // FDT_PROP: length, name offset, value
                let len = be32(structure, at + 4)? as usize;
                let nameoff = be32(structure, at + 8)? as usize;
                let value = structure.get(at + 12..at.checked_add(12)?.checked_add(len)?)?;
                let pname = strings.get(nameoff..)?;
                let pend = pname.iter().position(|&b| b == 0)?;
                if &pname[..pend] == b"compatible"
                    && value.split(|&b| b == 0).any(|s| s == name.as_bytes())
                {
                    return Some(true);
                }
                at = at.checked_add(12 + len.next_multiple_of(4))?;
            }
            4 => at += 4,            // FDT_NOP
            9 => return Some(false), // FDT_END
            _ => return None,
        }
    }
}

// ---- the early devices (PL5b): allocation-free, because the console is needed before the heap exists ----

/// Deepest nesting the allocation-free walker follows (the same bound as the whole-tree parse).
const WALK_DEPTH: usize = MAX_DEPTH;
const MAX_DEPTH_EARLY: usize = MAX_DEPTH;

/// What `walk_props` calls for each property: the node's path components, its parent's cell counts, the property name and value.
type PropVisitor<'a, 'f> = dyn FnMut(&[&'a str], (u32, u32), &'a str, &'a [u8]) + 'f;

/// What `node_at` collects about a node: its `reg` (the first range), `compatible`, and whether anything about it makes it unusable as a
/// plain byte-register device (a disabled `status`, a `reg-shift` other than 0, a `reg-io-width` other than 1).
struct NodeFacts<'a> {
    reg: Option<(u64, u64)>,
    compat: Option<&'a [u8]>,
    unusable: bool,
}

/// Walk every property of the tree without allocating: `f(path, cells, name, value)` is called for each, where `path` is the node's name
/// components from the root (`["soc", "serial@10000000"]`; the root itself is `[]`) and `cells` is the pair `(#address-cells,
/// #size-cells)` its parent gave its children (the specification's defaults, 2 and 1, until a parent says otherwise; properties come
/// before child nodes in the format, so the counts are known when the children are met). A malformed tree ends the walk.
#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: every access is through checked `get` slices or a depth checked against WALK_DEPTH; `at` advances inside the structure block
fn walk_props<'a>(blob: &'a [u8], f: &mut PropVisitor<'a, '_>) -> Option<()> {
    if be32(blob, 0)? != MAGIC {
        return None;
    }
    let total = be32(blob, 4)? as usize;
    let off_struct = be32(blob, 8)? as usize;
    let off_strings = be32(blob, 12)? as usize;
    let size_struct = be32(blob, 36)? as usize;
    let blob = blob.get(..total)?;
    let strings = blob.get(off_strings..)?;
    let structure = blob.get(off_struct..off_struct.checked_add(size_struct)?)?;
    let mut names: [&str; WALK_DEPTH] = [""; WALK_DEPTH];
    // cells[d]: the counts the node at depth d gives its children
    let mut cells = [(2u32, 1u32); WALK_DEPTH + 1];
    let mut depth = 0usize; // 0 before the root; the root is depth 1
    let mut at = 0usize;
    loop {
        match be32(structure, at)? {
            1 => {
                let rest = structure.get(at + 4..)?;
                let len = rest.iter().position(|&b| b == 0)?;
                let name = core::str::from_utf8(&rest[..len]).ok()?;
                at = at.checked_add(4 + (len + 1).next_multiple_of(4))?;
                if depth >= WALK_DEPTH {
                    return None;
                }
                if depth >= 1 {
                    names[depth - 1] = name;
                }
                depth += 1;
                cells[depth] = (2, 1);
            }
            2 => {
                depth = depth.checked_sub(1)?;
                at += 4;
            }
            3 => {
                if depth == 0 {
                    return None; // a property outside any node
                }
                let len = be32(structure, at + 4)? as usize;
                let nameoff = be32(structure, at + 8)? as usize;
                let value = structure.get(at + 12..at.checked_add(12)?.checked_add(len)?)?;
                let pname = strings.get(nameoff..)?;
                let pend = pname.iter().position(|&b| b == 0)?;
                let pname = core::str::from_utf8(&pname[..pend]).ok()?;
                if pname == "#address-cells" {
                    cells[depth] = (be32(value, 0)?, cells[depth].1);
                } else if pname == "#size-cells" {
                    cells[depth] = (cells[depth].0, be32(value, 0)?);
                }
                // the parent's counts apply to this node's `reg`
                let parent = cells[depth.saturating_sub(1)];
                let path: &[&str] = names.get(..depth.saturating_sub(1))?;
                f(path, parent, pname, value);
                at = at.checked_add(12 + len.next_multiple_of(4))?;
            }
            4 => at += 4,
            9 => return Some(()),
            _ => return None,
        }
    }
}

/// The first `(address, size)` of a `reg` value written with the given cell counts (one or two cells each; a size of zero cells reads as 0).
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: `get` is checked; the cell counts are at most 2, so the offsets are at most 16
fn first_reg(v: &[u8], (a, s): (u32, u32)) -> Option<(u64, u64)> {
    if a == 0 || a > 2 || s > 2 {
        return None;
    }
    let cell = |at: usize, n: u32| -> Option<u64> {
        let mut x = 0u64;
        for k in 0..n as usize {
            let w = u32::from_be_bytes(v.get(at + 4 * k..at + 4 * k + 4)?.try_into().ok()?);
            x = (x << 32) | u64::from(w);
        }
        Some(x)
    };
    Some((cell(0, a)?, cell(4 * a as usize, s)?))
}

/// Does one node name match one component of a path string? A component written without a unit address (`serial`) names the node
/// `serial@10000000` (the specification's rule); one written with it must match exactly.
fn component_is(name: &str, want: &str) -> bool {
    name == want
        || (!want.contains('@') && name.contains('@') && name.split('@').next() == Some(want))
}

/// Does `path` (a node's name components from the root) equal the absolute path string `want` (`/soc/serial@10000000`, or `/soc/serial`)?
/// No wildcards.
fn path_is(path: &[&str], want: &str) -> bool {
    let Some(rest) = want.strip_prefix('/') else {
        return false;
    };
    let mut parts = rest.split('/').filter(|p| !p.is_empty());
    for c in path {
        match parts.next() {
            Some(p) if component_is(c, p) => {}
            _ => return false,
        }
    }
    parts.next().is_none()
}

/// The facts about the node at an absolute path (collected by walking the whole tree: properties arrive node by node).
fn node_at<'a>(blob: &'a [u8], want: &str) -> Option<NodeFacts<'a>> {
    let mut f = NodeFacts {
        reg: None,
        compat: None,
        unusable: false,
    };
    walk_props(blob, &mut |path, cells, name, value| {
        if path_is(path, want) {
            match name {
                "reg" => f.reg = first_reg(value, cells),
                "compatible" => f.compat = Some(value),
                "status" => {
                    let st = value.split(|&b| b == 0).next().unwrap_or(&[]);
                    f.unusable |= st != b"okay" && st != b"ok";
                }
                "reg-shift" => f.unusable |= be32(value, 0) != Some(0),
                "reg-io-width" => f.unusable |= be32(value, 0) != Some(1),
                _ => {}
            }
        }
    })?;
    Some(f)
}

/// The base address of the console UART the tree names: `/chosen/stdout-path` (an absolute path or an alias from `/aliases`, either with
/// `:options` after it) naming a node whose `compatible` is an NS16550, whose `status` is not disabled, whose registers are the plain
/// byte-wide, stride-one kind the kernel drives (`reg-shift` and `reg-io-width` absent or 0 and 1), and whose `reg` gives the base. `None`
/// when the tree names no console, names one the kernel cannot drive or is malformed: the caller falls back to the firmware's console.
pub fn console_base(blob: &[u8]) -> Option<u64> {
    let mut stdout: Option<&str> = None;
    walk_props(blob, &mut |path, _, name, value| {
        if path == ["chosen"] && name == "stdout-path" {
            stdout = core::str::from_utf8(value.split(|&b| b == 0).next().unwrap_or(&[])).ok();
        }
    })?;
    let want = stdout?.split(':').next()?;
    let want = if want.starts_with('/') {
        want
    } else {
        // an alias: its value is the path (no table of aliases: one more walk for this one key)
        let mut target: Option<&str> = None;
        walk_props(blob, &mut |path, _, name, value| {
            if path == ["aliases"] && name == want {
                target = core::str::from_utf8(value.split(|&b| b == 0).next().unwrap_or(&[])).ok();
            }
        })?;
        target?
    };
    let f = node_at(blob, want)?;
    let is_16550 = f
        .compat?
        .split(|&b| b == 0)
        .any(|c| c == b"ns16550a" || c == b"ns16550");
    if is_16550 && !f.unusable {
        f.reg.map(|r| r.0)
    } else {
        None
    }
}

/// Is `word` one of the space-separated words of `/chosen/bootargs`? Allocation-free (the boot words that must act before the heap exists).
pub fn bootarg_present(blob: &[u8], word: &str) -> bool {
    let mut found = false;
    let _ = walk_props(blob, &mut |path, _, name, value| {
        if path == ["chosen"] && name == "bootargs" {
            found |= value
                .split(|&b| b == 0 || b == b' ')
                .any(|w| w == word.as_bytes());
        }
    });
    found
}

/// The base address of the test finisher, a SiFive test device (`sifive,test1` or `sifive,test0`): the way a headless test run says pass or
/// fail. `None` when the tree has none (real hardware): the kernel then ends through the firmware.
pub fn test_finisher_base(blob: &[u8]) -> Option<u64> {
    // pass one: the path of the first node whose compatible names a SiFive test device (`reg` may come before `compatible` in a node, so
    // the address is read in a second pass)
    let mut found: Option<([&str; MAX_DEPTH_EARLY], usize)> = None;
    walk_props(blob, &mut |path, _, name, value| {
        let matches = name == "compatible"
            && value
                .split(|&b| b == 0)
                .any(|c| c == b"sifive,test1" || c == b"sifive,test0");
        if matches && found.is_none() {
            let mut copy = [""; MAX_DEPTH_EARLY];
            for (d, c) in copy.iter_mut().zip(path.iter()) {
                *d = c;
            }
            found = Some((copy, path.len()));
        }
    })?;
    let (comps, len) = found?;
    let mut reg: Option<u64> = None;
    walk_props(blob, &mut |path, cells, name, value| {
        if name == "reg" && path == comps.get(..len).unwrap_or(&[]) {
            reg = first_reg(value, cells).map(|r| r.0);
        }
    })?;
    reg
}

// ---- the whole tree (S6c) ----

use alloc::string::String;
use alloc::vec::Vec;

/// Limits that bound what a hostile tree can cost.
pub const MAX_NODES: usize = 4096;
pub const MAX_DEPTH: usize = 16;
pub const MAX_PROPS_PER_NODE: usize = 64;
/// Longest node name and property name accepted (the specification says 31; QEMU's are shorter).
pub const MAX_NAME: usize = 128;
/// Most bytes the parse may retain in all (paths, property names and values together).
pub const MAX_RETAINED: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FdtError {
    /// Not a flattened device tree (magic, version, sizes, offsets).
    BadHeader,
    /// The structure block is malformed or runs off the end.
    BadStructure,
    /// More nodes, deeper nesting or more properties than the limits allow.
    TooBig,
}

/// One node: its full path (`/soc/virtio_mmio@10001000`), its properties, and the cell counts its
/// own `reg` is written in (the parent's `#address-cells` and `#size-cells`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub path: String,
    pub props: Vec<(String, Vec<u8>)>,
    pub addr_cells: u32,
    pub size_cells: u32,
}

impl Node {
    pub fn prop(&self, name: &str) -> Option<&[u8]> {
        self.props
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_slice())
    }

    pub fn u32_prop(&self, name: &str) -> Option<u32> {
        let v = self.prop(name)?;
        Some(u32::from_be_bytes(v.get(..4)?.try_into().ok()?))
    }

    /// Does `compatible` list `name`?
    pub fn is_compatible(&self, name: &str) -> bool {
        self.prop("compatible")
            .is_some_and(|v| v.split(|&b| b == 0).any(|s| s == name.as_bytes()))
    }

    /// The first `(address, size)` of `reg`, decoded with the parent's cell counts.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: `get` is checked; k is below the cell count read from the blob
    pub fn reg(&self) -> Option<(u64, u64)> {
        let v = self.prop("reg")?;
        let cell = |at: usize, n: u32| -> Option<u64> {
            let mut x = 0u64;
            for k in 0..n as usize {
                let w = u32::from_be_bytes(v.get(at + 4 * k..at + 4 * k + 4)?.try_into().ok()?);
                x = x.checked_shl(32)? | u64::from(w);
            }
            Some(x)
        };
        if self.addr_cells == 0 || self.addr_cells > 2 || self.size_cells > 2 {
            return None;
        }
        let a = cell(0, self.addr_cells)?;
        let s = cell(4 * self.addr_cells as usize, self.size_cells)?;
        Some((a, s))
    }
}

impl Node {
    /// Every `(address, size)` pair of `reg`, decoded with the parent's cell counts (a node with several ranges, like a machine with two
    /// banks of RAM, lists them all). Empty if there is no `reg` or the cell counts are unusable.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: `get` is checked; k is below the cell count read from the blob; the entry size is at most 16 bytes
    pub fn regs(&self) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        let Some(v) = self.prop("reg") else {
            return out;
        };
        if self.addr_cells == 0 || self.addr_cells > 2 || self.size_cells > 2 {
            return out;
        }
        let cell = |at: usize, n: u32| -> Option<u64> {
            let mut x = 0u64;
            for k in 0..n as usize {
                let w = u32::from_be_bytes(v.get(at + 4 * k..at + 4 * k + 4)?.try_into().ok()?);
                x = x.checked_shl(32)? | u64::from(w);
            }
            Some(x)
        };
        let entry = 4 * (self.addr_cells + self.size_cells) as usize;
        let mut at = 0;
        while at + entry <= v.len() {
            let (Some(a), Some(s)) = (
                cell(at, self.addr_cells),
                cell(at + 4 * self.addr_cells as usize, self.size_cells),
            ) else {
                break;
            };
            out.push((a, s));
            at += entry;
        }
        out
    }
}

/// The RAM the tree describes: every `reg` range of a node whose `device_type` is `memory`.
pub fn memory_ranges(nodes: &[Node]) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    for n in nodes {
        let is_memory = n
            .prop("device_type")
            .is_some_and(|v| v.split(|&b| b == 0).next() == Some(b"memory"));
        if is_memory {
            out.extend(n.regs());
        }
    }
    out
}

/// The ranges `/reserved-memory` reserves: every `reg` range of a child of that node (firmware, a framebuffer, a ramdisk). A child with
/// no `reg` (a dynamic allocation request) reserves nothing the kernel can name.
pub fn reserved_ranges(nodes: &[Node]) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    for n in nodes {
        if n.path.starts_with("/reserved-memory/") {
            out.extend(n.regs());
        }
    }
    out
}

/// The memory reservation block of the header: `(address, size)` pairs the loader wants kept (the tree's own copy, OpenSBI's region on some
/// firmware), up to the all-zero terminator. Bounded and bounds-checked like the rest of the reader.
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: `get` is checked; `at` advances by 16 inside the blob; the count is capped
pub fn reservation_block(blob: &[u8]) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    if be32(blob, 0) != Some(MAGIC) {
        return out;
    }
    let Some(mut at) = be32(blob, 16).map(|o| o as usize) else {
        return out;
    };
    let be64 = |at: usize| -> Option<u64> {
        Some(u64::from_be_bytes(blob.get(at..at + 8)?.try_into().ok()?))
    };
    while out.len() < 64 {
        let (Some(a), Some(s)) = (be64(at), be64(at + 8)) else {
            break;
        };
        if a == 0 && s == 0 {
            break;
        }
        out.push((a, s));
        at += 16;
    }
    out
}

/// `/cpus/timebase-frequency`: the rate of the `time` counter in hertz (one cell, or two for a very fast counter).
pub fn timebase_frequency(nodes: &[Node]) -> Option<u64> {
    let v = nodes
        .iter()
        .find(|n| n.path == "/cpus")?
        .prop("timebase-frequency")?;
    match v.len() {
        4 => Some(u64::from(u32::from_be_bytes(v.try_into().ok()?))),
        8 => Some(u64::from_be_bytes(v.try_into().ok()?)),
        _ => None,
    }
}

/// A virtio-mmio slot found in the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtioMmio {
    pub base: u64,
    pub size: u64,
    /// The interrupt source number (`interrupts`, first cell).
    pub irq: u32,
}

/// Parse the whole tree. Everything is bounds-checked; limits above bound the cost.
#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: the blob is boot-loader data: every access is through checked `get` slices and offsets advance inside the structure block (a bad blob is an Err, fuzzed in fdt_golden)
pub fn parse(blob: &[u8]) -> Result<Vec<Node>, FdtError> {
    if be32(blob, 0) != Some(MAGIC) {
        return Err(FdtError::BadHeader);
    }
    let total = be32(blob, 4).ok_or(FdtError::BadHeader)? as usize;
    let off_struct = be32(blob, 8).ok_or(FdtError::BadHeader)? as usize;
    let off_strings = be32(blob, 12).ok_or(FdtError::BadHeader)? as usize;
    let size_strings = be32(blob, 32).ok_or(FdtError::BadHeader)? as usize;
    let size_struct = be32(blob, 36).ok_or(FdtError::BadHeader)? as usize;
    let blob = blob.get(..total).ok_or(FdtError::BadHeader)?;
    let strings = blob
        .get(
            off_strings
                ..off_strings
                    .checked_add(size_strings)
                    .ok_or(FdtError::BadHeader)?,
        )
        .ok_or(FdtError::BadHeader)?;
    let structure = blob
        .get(
            off_struct
                ..off_struct
                    .checked_add(size_struct)
                    .ok_or(FdtError::BadHeader)?,
        )
        .ok_or(FdtError::BadHeader)?;
    let bad = FdtError::BadStructure;
    let mut nodes: Vec<Node> = Vec::new();
    // per open node: (index in `nodes`, the cell counts its children use)
    let mut stack: Vec<(usize, u32, u32)> = Vec::new();
    let mut retained = 0usize;
    let mut at = 0usize;
    loop {
        match be32(structure, at).ok_or(bad)? {
            1 => {
                let rest = structure.get(at + 4..).ok_or(bad)?;
                let len = rest.iter().position(|&b| b == 0).ok_or(bad)?;
                if len > MAX_NAME {
                    return Err(FdtError::TooBig);
                }
                let name = core::str::from_utf8(&rest[..len]).map_err(|_| bad)?;
                at = at
                    .checked_add(4 + (len + 1).next_multiple_of(4))
                    .ok_or(bad)?;
                if nodes.len() >= MAX_NODES || stack.len() >= MAX_DEPTH {
                    return Err(FdtError::TooBig);
                }
                let (path, ac, sc) = match stack.last() {
                    None => (String::from("/"), 2, 1), // the root; the spec's defaults
                    Some(&(p, pac, psc)) => {
                        let parent = &nodes[p].path;
                        let mut path = parent.clone();
                        if !path.ends_with('/') {
                            path.push('/');
                        }
                        path.push_str(name);
                        (path, pac, psc)
                    }
                };
                retained += path.len();
                if retained > MAX_RETAINED {
                    return Err(FdtError::TooBig);
                }
                nodes.push(Node {
                    path,
                    props: Vec::new(),
                    addr_cells: ac,
                    size_cells: sc,
                });
                stack.push((nodes.len() - 1, 2, 1));
            }
            2 => {
                stack.pop().ok_or(bad)?;
                at += 4;
            }
            3 => {
                let len = be32(structure, at + 4).ok_or(bad)? as usize;
                let nameoff = be32(structure, at + 8).ok_or(bad)? as usize;
                let end = at
                    .checked_add(12)
                    .and_then(|x| x.checked_add(len))
                    .ok_or(bad)?;
                let value = structure.get(at + 12..end).ok_or(bad)?;
                let pname = strings.get(nameoff..).ok_or(bad)?;
                let pend = pname.iter().position(|&b| b == 0).ok_or(bad)?;
                if pend > MAX_NAME {
                    return Err(FdtError::TooBig);
                }
                retained += pend + len;
                if retained > MAX_RETAINED {
                    return Err(FdtError::TooBig);
                }
                let pname = core::str::from_utf8(&pname[..pend]).map_err(|_| bad)?;
                let &(ni, _, _) = stack.last().ok_or(bad)?;
                if nodes[ni].props.len() >= MAX_PROPS_PER_NODE {
                    return Err(FdtError::TooBig);
                }
                // a node's own cell counts apply to its children
                if pname == "#address-cells" || pname == "#size-cells" {
                    if let Some(w) = value.get(..4) {
                        let v = u32::from_be_bytes([w[0], w[1], w[2], w[3]]);
                        let top = stack.last_mut().ok_or(bad)?;
                        if pname == "#address-cells" {
                            top.1 = v;
                        } else {
                            top.2 = v;
                        }
                    }
                }
                nodes[ni].props.push((String::from(pname), value.to_vec()));
                at = at.checked_add(12 + len.next_multiple_of(4)).ok_or(bad)?;
            }
            4 => at += 4,
            9 => {
                return if stack.is_empty() {
                    Ok(nodes)
                } else {
                    Err(bad)
                };
            }
            _ => return Err(bad),
        }
    }
}

/// Every `virtio,mmio` slot in the tree, in tree order.
pub fn virtio_mmio(nodes: &[Node]) -> Vec<VirtioMmio> {
    nodes
        .iter()
        .filter(|n| n.is_compatible("virtio,mmio"))
        .filter_map(|n| {
            let (base, size) = n.reg()?;
            Some(VirtioMmio {
                base,
                size,
                irq: n.u32_prop("interrupts")?,
            })
        })
        .collect()
}

/// The total size the header claims (clamped by the caller to what it holds), if it is a tree.
pub fn total_size(head: &[u8]) -> Option<usize> {
    if be32(head, 0)? != MAGIC {
        return None;
    }
    Some(be32(head, 4)? as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    /// A tiny tree: root { plic { compatible = "riscv,plic0" } soc { compatible = "simple-bus", "x" } }
    fn tree(compat: &[&[u8]]) -> Vec<u8> {
        let strings = b"compatible\0";
        let mut st: Vec<u8> = vec![];
        let w = |v: &mut Vec<u8>, x: u32| v.extend(x.to_be_bytes());
        w(&mut st, 1);
        st.extend(b"\0\0\0\0"); // root: empty name, padded
        w(&mut st, 1);
        st.extend(b"plic\0\0\0\0");
        let mut value = vec![];
        for c in compat {
            value.extend(*c);
            value.push(0);
        }
        w(&mut st, 3);
        w(&mut st, value.len() as u32);
        w(&mut st, 0);
        st.extend(&value);
        while !st.len().is_multiple_of(4) {
            st.push(0);
        }
        w(&mut st, 2);
        w(&mut st, 2);
        w(&mut st, 9);
        let off_struct = 40usize;
        let off_strings = off_struct + st.len();
        let total = off_strings + strings.len();
        let mut b = vec![];
        for x in [
            MAGIC,
            total as u32,
            off_struct as u32,
            off_strings as u32,
            0,
            17,
            16,
            0,
            strings.len() as u32,
            st.len() as u32,
        ] {
            b.extend(x.to_be_bytes());
        }
        b.extend(&st);
        b.extend(strings);
        b
    }

    #[test]
    fn finds_a_compatible_string_among_several() {
        let t = tree(&[b"sifive,plic-1.0.0", b"riscv,plic0"]);
        assert!(has_compatible(&t, "riscv,plic0"));
        assert!(has_compatible(&t, "sifive,plic-1.0.0"));
        assert!(!has_compatible(&t, "riscv,aplic"));
        assert!(!has_compatible(&t, "riscv,plic"), "no prefix matches");
    }

    #[test]
    fn hostile_input_never_panics() {
        let t = tree(&[b"riscv,plic0"]);
        // every truncation and every single-byte corruption
        for n in 0..t.len() {
            let _ = has_compatible(&t[..n], "riscv,plic0");
        }
        for i in 0..t.len() {
            for v in [0u8, 1, 0x7f, 0xff] {
                let mut c = t.clone();
                c[i] = v;
                let _ = has_compatible(&c, "riscv,plic0");
            }
        }
        assert!(!has_compatible(&[], "x"));
        assert!(!has_compatible(&[0xd0, 0x0d, 0xfe], "x"));
    }
}
