//! A device-tree reader (S6a-2, S6c): `has_compatible`, an allocation-free scan the kernel uses to
//! tell a PLIC machine from an AIA one, and `parse`, the whole tree for the device manager.
//! Hostile input never panics and costs a bounded amount: every offset is bounds-checked, and the
//! limits below cap nodes, depth, properties, name lengths and the bytes the parse retains.

const MAGIC: u32 = 0xd00d_feed;

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// Does the flattened device tree `blob` contain a `compatible` property with the string
/// `name` among its (NUL-separated) values? `false` for anything that is not a well-formed tree.
pub fn has_compatible(blob: &[u8], name: &str) -> bool {
    scan(blob, name).unwrap_or(false)
}

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

/// A virtio-mmio slot found in the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtioMmio {
    pub base: u64,
    pub size: u64,
    /// The interrupt source number (`interrupts`, first cell).
    pub irq: u32,
}

/// Parse the whole tree. Everything is bounds-checked; limits above bound the cost.
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
