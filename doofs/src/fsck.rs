//! The consistency check of the filesystem layer (what `fsck.doofs` adds to the volume check): every entry names an existing
//! node whose parent is that directory, every linked node is named exactly once, directory sizes are their entry counts,
//! extents belong to files and lie inside them, symlink targets exist, orphans are unlinked, and the quota counters are what
//! the nodes add up to. Read-only.

use crate::{
    Counters, Fs, Inode, K_COUNTERS, K_DIRENT, K_INODE, K_ORPHAN, K_SYMLINK, NODE_OVERHEAD,
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
        meta += n.cost as u64;
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
    let have = Counters { nodes, bytes, meta };
    if stored != Some(have) || have != fs.counters() {
        bad.push(format!(
            "counters say {stored:?} (memory {:?}), the nodes add up to {have:?}",
            fs.counters()
        ));
    }
    bad
}
