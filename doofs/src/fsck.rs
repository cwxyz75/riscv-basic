//! The consistency check of the filesystem layer (what `fsck.doofs` adds to the volume check): every entry names an existing
//! node whose parent is that directory, every linked node is named exactly once, directory sizes are their entry counts,
//! extents belong to files and lie inside them, symlink targets exist, orphans are unlinked, and the quota counters are what
//! the nodes add up to. Read-only.

use crate::attrs::{builtin_keys, k_index, parse_entry, ATTR_OVERHEAD, IDX_NAME, INDEX_OVERHEAD};
use crate::{
    Counters, Fs, Inode, K_ATTR, K_COUNTERS, K_DIRENT, K_IDXDEF, K_INDEX, K_INODE, K_NAMEOF,
    K_ORPHAN, K_SYMLINK, NODE_OVERHEAD,
};
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use fsfmt::{Device, Tree, BLOCK, KEY_EXTENT};
use memfs::Kind;

fn u64_at(b: &[u8]) -> u64 {
    u64::from_be_bytes(b.try_into().unwrap_or([0; 8]))
}

/// All the problems found (empty if the filesystem is consistent).
pub fn check_fs<D: Device>(fs: &mut Fs<D>) -> Vec<String> {
    let mut bad: Vec<String> = Vec::new();
    let root = fs.vol.tree.root();
    let all = match Tree::range_at(&mut fs.vol.pool, root, None, None) {
        Ok(a) => a,
        Err(e) => return alloc::vec![format!("the tree cannot be read: {e:?}")],
    };
    let mut inodes: BTreeMap<u64, Inode> = BTreeMap::new();
    let mut dirents: Vec<(u64, Vec<u8>, u64)> = Vec::new();
    let mut extents: Vec<(u64, u64)> = Vec::new();
    let mut symlinks: BTreeMap<u64, usize> = BTreeMap::new();
    let mut orphans: Vec<u64> = Vec::new();
    let mut stored: Option<Counters> = None;
    let mut attrs: Vec<(u64, Vec<u8>, u8, usize)> = Vec::new();
    let mut defs: BTreeMap<Vec<u8>, (u8, u8)> = BTreeMap::new();
    let mut entries: Vec<(u8, Vec<u8>, u64)> = Vec::new();
    let mut names: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    for (k, v) in &all {
        match k[0] {
            K_INODE if k.len() == 9 => match Inode::decode(v) {
                Ok(n) => {
                    inodes.insert(u64_at(&k[1..9]), n);
                }
                Err(_) => bad.push(format!("inode {} is malformed", u64_at(&k[1..9]))),
            },
            K_DIRENT if k.len() > 9 && v.len() == 8 => {
                dirents.push((u64_at(&k[1..9]), k[9..].to_vec(), u64_at(v)))
            }
            KEY_EXTENT if k.len() == 17 => extents.push((u64_at(&k[1..9]), u64_at(&k[9..17]))),
            K_SYMLINK if k.len() == 9 => {
                symlinks.insert(u64_at(&k[1..9]), v.len());
            }
            K_ORPHAN if k.len() == 9 => orphans.push(u64_at(&k[1..9])),
            K_COUNTERS if k.len() == 1 && v.len() == 24 => {
                stored = Some(Counters {
                    nodes: u64_at(&v[..8]),
                    bytes: u64_at(&v[8..16]),
                    meta: u64_at(&v[16..]),
                });
            }
            crate::K_NEXTID if k.len() == 1 => {}
            K_ATTR if k.len() > 9 && !v.is_empty() => {
                attrs.push((u64_at(&k[1..9]), k[9..].to_vec(), v[0], v.len() - 1));
            }
            K_IDXDEF if k.len() > 1 && v.len() == 2 => {
                defs.insert(k[1..].to_vec(), (v[0], v[1]));
            }
            K_INDEX if k.len() > 2 => match parse_entry(k) {
                Some((key, id)) => entries.push((k[1], key, id)),
                None => bad.push("a malformed index entry".into()),
            },
            K_NAMEOF if k.len() == 9 => {
                names.insert(u64_at(&k[1..9]), v.clone());
            }
            _ => bad.push(format!("an unknown or malformed record, key type {}", k[0])),
        }
    }
    if !inodes.contains_key(&0) {
        bad.push("no root inode".into());
    }
    // entries
    let mut named: BTreeMap<u64, u32> = BTreeMap::new();
    let mut counts: BTreeMap<u64, u64> = BTreeMap::new();
    for (dir, name, child) in &dirents {
        match inodes.get(dir) {
            Some(d) if d.kind == Kind::Dir => *counts.entry(*dir).or_insert(0) += 1,
            _ => bad.push(format!(
                "entry {name:?} is in {dir}, which is not a directory"
            )),
        }
        match inodes.get(child) {
            None => bad.push(format!(
                "entry {name:?} of {dir} names a missing node {child}"
            )),
            Some(c) => {
                if c.parent != *dir {
                    bad.push(format!(
                        "node {child} has parent {} but is named in {dir}",
                        c.parent
                    ));
                }
                if c.links != 1 {
                    bad.push(format!("node {child} is named but has {} links", c.links));
                }
                *named.entry(*child).or_insert(0) += 1;
            }
        }
    }
    let (mut nodes, mut bytes, mut meta) = (0u64, 0u64, 0u64);
    for (id, n) in &inodes {
        nodes += 1;
        meta += n.cost as u64 + n.attr_cost as u64;
        if n.kind == Kind::File {
            bytes += n.size;
        }
        let times = named.get(id).copied().unwrap_or(0);
        if *id == 0 {
            if times != 0 || n.kind != Kind::Dir || n.parent != 0 {
                bad.push("the root is malformed".into());
            }
        } else if n.links == 1 && times != 1 {
            bad.push(format!("node {id} is linked but named {times} times"));
        } else if n.links == 0 && !orphans.contains(id) {
            bad.push(format!("node {id} is unlinked but has no orphan record"));
        }
        if n.kind == Kind::Dir && n.size != counts.get(id).copied().unwrap_or(0) {
            bad.push(format!(
                "directory {id} says {} entries, has {}",
                n.size,
                counts.get(id).copied().unwrap_or(0)
            ));
        }
        if n.kind == Kind::Symlink && symlinks.get(id).copied() != Some(n.size as usize) {
            bad.push(format!("symlink {id} has no matching target record"));
        }
        if *id != 0 && (n.cost as usize) < NODE_OVERHEAD {
            bad.push(format!("node {id} has a cost below the overhead"));
        }
    }
    for id in &orphans {
        if inodes.get(id).is_none_or(|n| n.links != 0) {
            bad.push(format!(
                "orphan record for {id}, which is missing or linked"
            ));
        }
    }
    for id in symlinks.keys() {
        if inodes.get(id).is_none_or(|n| n.kind != Kind::Symlink) {
            bad.push(format!("a symlink record for {id}, which is not a symlink"));
        }
    }
    for (id, block) in &extents {
        match inodes.get(id) {
            Some(n) if n.kind == Kind::File => {
                if block * BLOCK as u64 >= n.size {
                    bad.push(format!(
                        "file {id} has an extent at block {block} past its size {}",
                        n.size
                    ));
                }
            }
            _ => bad.push(format!("an extent of {id}, which is not a file")),
        }
    }
    // attributes: each belongs to a node, and a node's attribute cost is what they add up to
    let mut attr_cost: BTreeMap<u64, u64> = BTreeMap::new();
    for (id, name, _, len) in &attrs {
        if !inodes.contains_key(id) {
            bad.push(format!("an attribute of the missing node {id}"));
        }
        *attr_cost.entry(*id).or_insert(0) += (name.len() + len + ATTR_OVERHEAD) as u64;
    }
    for (id, n) in &inodes {
        if n.attr_cost as u64 != attr_cost.get(id).copied().unwrap_or(0) {
            bad.push(format!(
                "node {id} says {} bytes of attributes, has {}",
                n.attr_cost,
                attr_cost.get(id).copied().unwrap_or(0)
            ));
        }
    }
    for name in defs.keys() {
        meta += (name.len() + INDEX_OVERHEAD) as u64;
    }
    // names: every node but the root has one record
    for id in inodes.keys() {
        if *id != 0 && !names.contains_key(id) {
            bad.push(format!("node {id} has no name record"));
        }
    }
    for id in names.keys() {
        if !inodes.contains_key(id) {
            bad.push(format!("a name record for the missing node {id}"));
        }
    }
    // index entries: exactly the ones the nodes and attributes call for
    let mut want: Vec<(u8, Vec<u8>, u64)> = Vec::new();
    for (id, n) in &inodes {
        if *id == 0 {
            continue;
        }
        for (idx, key) in builtin_keys(n) {
            want.push((idx, key, *id));
        }
        if n.links == 1 {
            if let Some(name) = names.get(id) {
                want.push((IDX_NAME, name.clone(), *id));
            }
        }
    }
    for (id, name, ty, len) in &attrs {
        if let Some((dty, idx)) = defs.get(name) {
            if dty == ty {
                let value = all
                    .iter()
                    .find(|(k, _)| k.as_slice() == crate::attrs::k_attr(*id, name).as_slice())
                    .map(|(_, v)| v[1..].to_vec())
                    .unwrap_or_default();
                let _ = len;
                if let Some(t) = memfs::AttrType::from_code(*ty) {
                    if let Ok(key) = t.key(&value) {
                        want.push((*idx, key, *id));
                    }
                }
            }
        }
    }
    want.sort();
    entries.sort();
    if want != entries {
        let missing = want.iter().filter(|w| !entries.contains(w)).count();
        let extra = entries.iter().filter(|e| !want.contains(e)).count();
        bad.push(format!("the index entries differ from what the nodes and attributes call for ({missing} missing, {extra} extra)"));
    }
    let _ = k_index;
    let have = Counters { nodes, bytes, meta };
    if stored != Some(have) || have != fs.counters() {
        bad.push(format!(
            "counters say {stored:?} (memory {:?}), the nodes add up to {have:?}",
            fs.counters()
        ));
    }
    bad
}
