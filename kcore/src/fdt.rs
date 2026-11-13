//! The least of a device-tree reader: is a given `compatible` string present? (S6a-2 uses it to
//! tell a PLIC machine from an AIA one before the device manager and its full parser exist, S6c.)
//! Hostile input never panics: every offset is bounds-checked.

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
