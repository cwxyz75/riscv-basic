//! The filesystem proper (FS-SPEC section 8, step 3): the operations of `memfs` (create, mkdir, unlink, rename, read, write,
//! truncate, symlinks, directory listing, open-but-unlinked nodes, quotas) on an `fsfmt` volume. `memfs` is the oracle: the
//! error for every case is the same, and the differential test runs one operation sequence through both.
//!
//! Key space (all keys sort bytewise; integers are big-endian so ids and block numbers sort numerically):
//!
//! * `01 id` inode: kind, flags, links, parent, mtime, size, cost.
//! * `02 dir name` directory entry: the child's id.
//! * `03 id block` extent: the pointer of one 4 KiB block of file data (a hole has no entry and reads as zeros).
//! * `04 id` symlink target.
//! * `05 id` orphan record: unlinked but still open; a mount reaps them (nothing is open after a mount).
//! * `06` counters: live nodes, bytes of file data, bytes of bookkeeping (the quota state).
//! * `07` the next node id (ids are never reused).
//!
//! Every operation is a transaction: if it fails part way (the device is full or broken) the tree root, the pool and the
//! counters go back to where they were, so a later commit never records half an operation.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
pub use attrs::{AttrType, Hit, QueryResult, Term};
use fsfmt::node::Ptr;
use fsfmt::pool::PoolError;
use fsfmt::tree::TreeError;
use fsfmt::volume::{VolError, Volume};
use fsfmt::{Device, Tree, BLOCK, KEY_EXTENT};
pub use memfs::{Error, Kind, Limits};

pub mod attrs;
pub mod fsck;

pub type NodeId = u64;
pub const ROOT: NodeId = 0;

/// What a node costs against `max_meta` beyond its name (the same number as `memfs`).
const NODE_OVERHEAD: usize = 96;

/// Blocks kept back from ordinary writes so that unlink, shrink and release still work on a full volume.
pub const RESERVE: u64 = 64;

const K_INODE: u8 = 1;
const K_DIRENT: u8 = 2;
const K_SYMLINK: u8 = 4;
const K_ORPHAN: u8 = 5;
const K_COUNTERS: u8 = 6;
const K_NEXTID: u8 = 7;
/// `08 id name`: an attribute (type byte, value).
const K_ATTR: u8 = 8;
/// `09 name`: an index definition (type byte, index number).
const K_IDXDEF: u8 = 9;
/// `0a index escaped-key id`: an index entry (empty value).
const K_INDEX: u8 = 10;
/// `0b id`: the node's name in its directory (empty for the root).
const K_NAMEOF: u8 = 11;

/// An error of the operation (`Fs`, the same as `memfs`'s), or of the disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsError {
    Fs(Error),
    /// The volume is full.
    NoSpace,
    /// A block failed its check or the structure is wrong: the volume is damaged.
    Corrupt,
    /// The device failed.
    Io,
}

impl From<Error> for FsError {
    fn from(e: Error) -> FsError {
        FsError::Fs(e)
    }
}

impl From<PoolError> for FsError {
    fn from(e: PoolError) -> FsError {
        match e {
            PoolError::NoSpace => FsError::NoSpace,
            PoolError::Dev(_) => FsError::Io,
            PoolError::Node(_) => FsError::Corrupt,
        }
    }
}

impl From<TreeError> for FsError {
    fn from(e: TreeError) -> FsError {
        match e {
            TreeError::Pool(p) => p.into(),
            _ => FsError::Corrupt,
        }
    }
}

impl From<VolError> for FsError {
    fn from(e: VolError) -> FsError {
        match e {
            VolError::Dev(_) => FsError::Io,
            VolError::Pool(p) => p.into(),
            VolError::Tree(t) => t.into(),
            _ => FsError::Corrupt,
        }
    }
}

type R<T> = Result<T, FsError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counters {
    /// Live nodes, including unlinked ones that are still open (and the root).
    pub nodes: u64,
    /// Bytes of file data (file sizes, holes included).
    pub bytes: u64,
    /// Bytes of names and node bookkeeping.
    pub meta: u64,
}

#[derive(Debug, Clone, Copy)]
struct Inode {
    kind: Kind,
    readonly: bool,
    links: u8,
    parent: NodeId,
    mtime: u64,
    /// File: bytes. Directory: entries. Symlink: target length.
    size: u64,
    cost: u32,
    /// Bytes of attributes charged to the metadata quota (names, values and a fixed overhead each).
    attr_cost: u32,
}

fn kind_code(k: Kind) -> u8 {
    match k {
        Kind::File => 1,
        Kind::Dir => 2,
        Kind::Symlink => 4,
        Kind::Image => 3,
    }
}

impl Inode {
    fn encode(&self) -> [u8; 40] {
        let mut b = [0u8; 40];
        b[0] = kind_code(self.kind);
        b[1] = self.readonly as u8;
        b[2] = self.links;
        b[8..16].copy_from_slice(&self.parent.to_be_bytes());
        b[16..24].copy_from_slice(&self.mtime.to_be_bytes());
        b[24..32].copy_from_slice(&self.size.to_be_bytes());
        b[32..36].copy_from_slice(&self.cost.to_be_bytes());
        b[36..40].copy_from_slice(&self.attr_cost.to_be_bytes());
        b
    }

    fn decode(b: &[u8]) -> R<Inode> {
        if b.len() != 40 {
            return Err(FsError::Corrupt);
        }
        let kind = match b[0] {
            1 => Kind::File,
            2 => Kind::Dir,
            4 => Kind::Symlink,
            _ => return Err(FsError::Corrupt),
        };
        let u = |r: core::ops::Range<usize>| u64::from_be_bytes(b[r].try_into().expect("8 bytes"));
        Ok(Inode {
            kind,
            readonly: b[1] != 0,
            links: b[2],
            parent: u(8..16),
            mtime: u(16..24),
            size: u(24..32),
            cost: u32::from_be_bytes(b[32..36].try_into().expect("4 bytes")),
            attr_cost: u32::from_be_bytes(b[36..40].try_into().expect("4 bytes")),
        })
    }
}

fn k_inode(id: NodeId) -> [u8; 9] {
    let mut k = [K_INODE; 9];
    k[1..].copy_from_slice(&id.to_be_bytes());
    k
}

fn k_dirent(dir: NodeId, name: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(9 + name.len());
    k.push(K_DIRENT);
    k.extend_from_slice(&dir.to_be_bytes());
    k.extend_from_slice(name);
    k
}

fn k_extent(id: NodeId, block: u64) -> [u8; 17] {
    let mut k = [KEY_EXTENT; 17];
    k[1..9].copy_from_slice(&id.to_be_bytes());
    k[9..].copy_from_slice(&block.to_be_bytes());
    k
}

fn k_id(tag: u8, id: NodeId) -> [u8; 9] {
    let mut k = [tag; 9];
    k[1..].copy_from_slice(&id.to_be_bytes());
    k
}

fn u64_of(v: &[u8]) -> R<u64> {
    Ok(u64::from_be_bytes(
        v.try_into().map_err(|_| FsError::Corrupt)?,
    ))
}

pub struct Fs<D: Device> {
    pub vol: Volume<D>,
    limits: Limits,
    counters: Counters,
    /// Open connections per node (not on disk: a mount starts with none).
    refs: BTreeMap<NodeId, u32>,
    now: u64,
}

fn check_name(n: &[u8]) -> Result<(), Error> {
    proto::file::valid_name(n).map_err(|_| Error::BadName)
}

impl<D: Device> Fs<D> {
    /// Open the filesystem on a mounted volume: make the root if the volume is new, and reap the orphans (nodes unlinked
    /// while open, which no connection holds any more).
    pub fn open(vol: Volume<D>, limits: Limits) -> R<Fs<D>> {
        let mut vol = vol;
        vol.pool.set_reserve(RESERVE);
        let mut fs = Fs {
            vol,
            limits,
            counters: Counters::default(),
            refs: BTreeMap::new(),
            now: 0,
        };
        match fs.get(&[K_COUNTERS])? {
            Some(v) if v.len() == 24 => {
                fs.counters = Counters {
                    nodes: u64_of(&v[..8])?,
                    bytes: u64_of(&v[8..16])?,
                    meta: u64_of(&v[16..])?,
                };
            }
            Some(_) => return Err(FsError::Corrupt),
            None => {
                fs.txn(|fs| {
                    fs.put_inode(
                        ROOT,
                        &Inode {
                            kind: Kind::Dir,
                            readonly: false,
                            links: 1,
                            parent: ROOT,
                            mtime: 0,
                            size: 0,
                            cost: 0,
                            attr_cost: 0,
                        },
                    )?;
                    fs.counters.nodes = 1;
                    fs.put(&[K_NEXTID], &1u64.to_be_bytes())?;
                    Ok(())
                })?;
            }
        }
        let orphans = Tree::range_at(
            &mut fs.vol.pool,
            fs.vol.tree.root(),
            Some(&[K_ORPHAN]),
            Some(&[K_ORPHAN + 1]),
        )?;
        if !orphans.is_empty() {
            fs.txn_reserve(|fs| {
                for (k, _) in &orphans {
                    let id = u64_of(&k[1..])?;
                    fs.reap(id)?;
                }
                Ok(())
            })?;
        }
        fs.vol.commit()?;
        Ok(fs)
    }

    pub fn counters(&self) -> Counters {
        self.counters
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Make everything so far durable.
    pub fn commit(&mut self) -> R<()> {
        Ok(self.vol.commit()?)
    }

    pub fn into_volume(self) -> Volume<D> {
        self.vol
    }

    pub fn set_time(&mut self, now: u64) {
        self.now = now;
    }

    // ---------------------------------------------------------------------------------------------------- plumbing

    fn get(&mut self, key: &[u8]) -> R<Option<Vec<u8>>> {
        Ok(self.vol.tree.get(&mut self.vol.pool, key)?)
    }

    fn put(&mut self, key: &[u8], value: &[u8]) -> R<()> {
        self.vol.tree.insert(&mut self.vol.pool, key, value)?;
        Ok(())
    }

    fn del(&mut self, key: &[u8]) -> R<()> {
        self.vol.tree.delete(&mut self.vol.pool, key)?;
        Ok(())
    }

    fn inode(&mut self, id: NodeId) -> Result<Inode, FsError> {
        match self.get(&k_inode(id))? {
            Some(v) => Inode::decode(&v),
            None => Err(Error::NotFound.into()),
        }
    }

    /// Store `n`; the built-in size and mtime index entries follow it (a node is in them while it is linked).
    fn put_inode(&mut self, id: NodeId, n: &Inode) -> R<()> {
        let old = match self.get(&k_inode(id))? {
            Some(v) => Some(Inode::decode(&v)?),
            None => None,
        };
        self.put(&k_inode(id), &n.encode())?;
        if id != ROOT {
            let want = attrs::builtin_keys(n);
            let had = old.as_ref().map(attrs::builtin_keys).unwrap_or_default();
            for (idx, key) in &had {
                if !want.contains(&(*idx, key.clone())) {
                    self.del(&attrs::k_index(*idx, key, id))?;
                }
            }
            for (idx, key) in &want {
                if !had.contains(&(*idx, key.clone())) {
                    self.put(&attrs::k_index(*idx, key, id), b"")?;
                }
            }
        }
        Ok(())
    }

    /// Record `id`'s name (and, while it is linked, its entry in the built-in name index).
    fn set_name(&mut self, id: NodeId, name: &[u8]) -> R<()> {
        if let Some(old) = self.get(&k_id(K_NAMEOF, id))? {
            self.del(&attrs::k_index(attrs::IDX_NAME, &old, id))?;
        }
        self.put(&k_id(K_NAMEOF, id), name)?;
        self.put(&attrs::k_index(attrs::IDX_NAME, name, id), b"")
    }

    /// The node is no longer named: its name leaves the index (the record goes when the node does).
    fn unname(&mut self, id: NodeId) -> R<()> {
        if let Some(old) = self.get(&k_id(K_NAMEOF, id))? {
            self.del(&attrs::k_index(attrs::IDX_NAME, &old, id))?;
        }
        Ok(())
    }

    fn save_counters(&mut self) -> R<()> {
        let mut v = [0u8; 24];
        v[..8].copy_from_slice(&self.counters.nodes.to_be_bytes());
        v[8..16].copy_from_slice(&self.counters.bytes.to_be_bytes());
        v[16..].copy_from_slice(&self.counters.meta.to_be_bytes());
        self.put(&[K_COUNTERS], &v)
    }

    /// Run `f` as one transaction: on error the tree, the pool and the counters are as they were.
    fn txn<T>(&mut self, f: impl FnMut(&mut Self) -> R<T>) -> R<T> {
        self.txn_with(false, f)
    }

    /// A transaction on a deleting path: it may use the space kept in reserve.
    fn txn_reserve<T>(&mut self, f: impl FnMut(&mut Self) -> R<T>) -> R<T> {
        self.txn_with(true, f)
    }

    fn txn_with<T>(&mut self, reserve: bool, mut f: impl FnMut(&mut Self) -> R<T>) -> R<T> {
        let mut retried = false;
        loop {
            let root = self.vol.tree.root();
            let counters = self.counters;
            self.vol.pool.begin();
            self.vol.pool.allow_reserve(reserve);
            let r = f(self).and_then(|t| self.save_counters().map(|()| t));
            self.vol.pool.allow_reserve(false);
            match r {
                Ok(t) => {
                    self.vol.pool.end();
                    return Ok(t);
                }
                Err(e) => {
                    self.vol.pool.rollback();
                    self.vol.tree = Tree::from_root(root);
                    self.counters = counters;
                    // Space freed by earlier operations comes back only when a commit makes the old root unneeded: commit
                    // and try once more before telling the caller the volume is full.
                    if e == FsError::NoSpace && !retried {
                        retried = true;
                        if self.vol.commit().is_ok() {
                            continue;
                        }
                    }
                    return Err(e);
                }
            }
        }
    }

    // ----------------------------------------------------------------------------------------------------- queries

    pub fn kind(&mut self, id: NodeId) -> R<Kind> {
        Ok(self.inode(id)?.kind)
    }

    /// Size of a file in bytes (a directory has the number of its entries, a symlink its target length).
    pub fn size(&mut self, id: NodeId) -> R<u64> {
        Ok(self.inode(id)?.size)
    }

    pub fn is_readonly(&mut self, id: NodeId) -> R<bool> {
        Ok(self.inode(id)?.readonly)
    }

    pub fn set_readonly(&mut self, id: NodeId) -> R<()> {
        self.txn(|fs| {
            let mut n = fs.inode(id)?;
            n.readonly = true;
            fs.put_inode(id, &n)
        })
    }

    pub fn parent(&mut self, id: NodeId) -> R<NodeId> {
        Ok(self.inode(id)?.parent)
    }

    pub fn mtime(&mut self, id: NodeId) -> R<u64> {
        Ok(self.inode(id)?.mtime)
    }

    pub fn lookup(&mut self, dir: NodeId, name: &[u8]) -> R<NodeId> {
        check_name(name)?;
        if self.inode(dir)?.kind != Kind::Dir {
            return Err(Error::NotDirectory.into());
        }
        match self.get(&k_dirent(dir, name))? {
            Some(v) => u64_of(&v),
            None => Err(Error::NotFound.into()),
        }
    }

    pub fn walk(&mut self, start: NodeId, components: &[&[u8]]) -> R<NodeId> {
        let mut at = start;
        for c in components {
            at = self.lookup(at, c)?;
        }
        Ok(at)
    }

    /// How many directories below `root` `dir` is (0 for `root` itself); `Invalid` if `dir` is not at or below it.
    pub fn depth_below(&mut self, dir: NodeId, root: NodeId) -> R<usize> {
        let mut at = dir;
        let mut depth = 0;
        while at != root {
            if at == ROOT {
                return Err(Error::Invalid.into());
            }
            at = self.inode(at)?.parent;
            depth += 1;
        }
        Ok(depth)
    }

    /// Entries of `dir` in bytewise name order, strictly after `after`, at most `max` of them.
    pub fn read_dir(
        &mut self,
        dir: NodeId,
        after: Option<&[u8]>,
        max: usize,
    ) -> R<Vec<(Vec<u8>, Kind)>> {
        if self.inode(dir)?.kind != Kind::Dir {
            return Err(Error::NotDirectory.into());
        }
        let lo = match after {
            Some(a) => {
                let mut k = k_dirent(dir, a);
                k.push(0); // names have no NUL: this is the first key after `a`
                k
            }
            None => k_dirent(dir, b""),
        };
        let hi = k_dirent(dir + 1, b"");
        let root = self.vol.tree.root();
        let ents = Tree::range_limited(&mut self.vol.pool, root, Some(&lo), Some(&hi), max)?;
        let mut out = Vec::new();
        for (k, v) in ents {
            let child = u64_of(&v)?;
            out.push((k[9..].to_vec(), self.inode(child)?.kind));
        }
        Ok(out)
    }

    pub fn read_link(&mut self, id: NodeId) -> R<Vec<u8>> {
        match self.inode(id)?.kind {
            Kind::Symlink => Ok(self.get(&k_id(K_SYMLINK, id))?.ok_or(FsError::Corrupt)?),
            Kind::Dir => Err(Error::IsDirectory.into()),
            _ => Err(Error::Invalid.into()),
        }
    }

    // ----------------------------------------------------------------------------------------------------- creation

    fn next_id(&mut self) -> R<NodeId> {
        let id = u64_of(&self.get(&[K_NEXTID])?.ok_or(FsError::Corrupt)?)?;
        self.put(&[K_NEXTID], &(id + 1).to_be_bytes())?;
        Ok(id)
    }

    fn insert(&mut self, dir: NodeId, name: &[u8], kind: Kind, extra_cost: usize) -> R<NodeId> {
        check_name(name)?;
        let d = self.inode(dir)?;
        if d.kind != Kind::Dir {
            return Err(Error::NotDirectory.into());
        }
        if self.get(&k_dirent(dir, name))?.is_some() {
            return Err(Error::Exists.into());
        }
        if d.readonly {
            return Err(Error::ReadOnly.into());
        }
        // a directory unlinked while open takes no entries
        if d.links == 0 {
            return Err(Error::NotFound.into());
        }
        let cost = name.len() + NODE_OVERHEAD;
        if self.counters.nodes >= self.limits.max_nodes as u64
            || self.counters.meta + (cost + extra_cost) as u64 > self.limits.max_meta as u64
        {
            return Err(Error::Quota.into());
        }
        self.txn(|fs| {
            let id = fs.next_id()?;
            fs.put_inode(
                id,
                &Inode {
                    kind,
                    readonly: false,
                    links: 1,
                    parent: dir,
                    mtime: fs.now,
                    size: 0,
                    cost: (cost + extra_cost) as u32,
                    attr_cost: 0,
                },
            )?;
            fs.set_name(id, name)?;
            fs.put(&k_dirent(dir, name), &id.to_be_bytes())?;
            let mut d = d;
            d.mtime = fs.now;
            d.size += 1;
            fs.put_inode(dir, &d)?;
            fs.counters.nodes += 1;
            fs.counters.meta += (cost + extra_cost) as u64;
            Ok(id)
        })
    }

    pub fn create_file(&mut self, dir: NodeId, name: &[u8]) -> R<NodeId> {
        self.insert(dir, name, Kind::File, 0)
    }

    pub fn mkdir(&mut self, dir: NodeId, name: &[u8]) -> R<NodeId> {
        self.insert(dir, name, Kind::Dir, 0)
    }

    /// Create the symlink `name` in `dir`; the target may never climb above the connection root `root` (FS-SPEC 9.1).
    pub fn symlink(&mut self, dir: NodeId, name: &[u8], target: &[u8], root: NodeId) -> R<NodeId> {
        let depth = self.depth_below(dir, root)?;
        proto::file::check_link_target(target, depth).map_err(|_| Error::BadTarget)?;
        let id = self.insert(dir, name, Kind::Symlink, target.len())?;
        // (insert's transaction is complete; the target is a second one, which cannot fail on quota: it was charged above)
        let made = self.txn(|fs| {
            fs.put(&k_id(K_SYMLINK, id), target)?;
            let mut n = fs.inode(id)?;
            n.size = target.len() as u64;
            fs.put_inode(id, &n)
        });
        if let Err(e) = made {
            // the link is made whole or not at all
            let _ = self.unlink(dir, name);
            return Err(e);
        }
        Ok(id)
    }

    // ------------------------------------------------------------------------------------------------------ removal

    /// Remove `id`'s data, records and counters.
    fn reap(&mut self, id: NodeId) -> R<()> {
        let n = self.inode(id)?;
        match n.kind {
            Kind::File => {
                self.truncate_blocks(id, 0)?;
                self.counters.bytes -= n.size;
            }
            Kind::Symlink => self.del(&k_id(K_SYMLINK, id))?,
            _ => {}
        }
        self.drop_attrs(id)?;
        self.del(&k_inode(id))?;
        self.del(&k_id(K_ORPHAN, id))?;
        self.del(&k_id(K_NAMEOF, id))?;
        self.counters.nodes -= 1;
        self.counters.meta -= n.cost as u64 + n.attr_cost as u64;
        Ok(())
    }

    /// The last name is gone: free the node now, or keep it as an orphan while a connection holds it.
    fn drop_link(&mut self, id: NodeId) -> R<()> {
        let mut n = self.inode(id)?;
        n.links = 0;
        self.unname(id)?;
        if self.refs.get(&id).copied().unwrap_or(0) > 0 {
            self.put_inode(id, &n)?;
            self.put(&k_id(K_ORPHAN, id), b"")
        } else {
            self.put_inode(id, &n)?;
            self.reap(id)
        }
    }

    pub fn unlink(&mut self, dir: NodeId, name: &[u8]) -> R<()> {
        check_name(name)?;
        let d = self.inode(dir)?;
        if d.readonly {
            return Err(Error::ReadOnly.into());
        }
        if d.kind != Kind::Dir {
            return Err(Error::NotDirectory.into());
        }
        let target = u64_of(&self.get(&k_dirent(dir, name))?.ok_or(Error::NotFound)?)?;
        let t = self.inode(target)?;
        if t.kind == Kind::Dir && t.size != 0 {
            return Err(Error::NotEmpty.into());
        }
        self.txn_reserve(|fs| {
            fs.del(&k_dirent(dir, name))?;
            let mut d = d;
            d.mtime = fs.now;
            d.size -= 1;
            fs.put_inode(dir, &d)?;
            fs.drop_link(target)
        })
    }

    /// `rename(2)` as `memfs` has it: an existing target is replaced if both are files (or links), or both directories and
    /// the target is empty; the node is the same afterwards; a directory cannot move below itself.
    pub fn rename(
        &mut self,
        from_dir: NodeId,
        from_name: &[u8],
        to_dir: NodeId,
        to_name: &[u8],
    ) -> R<()> {
        check_name(from_name)?;
        check_name(to_name)?;
        let src = self.lookup(from_dir, from_name)?;
        for d in [from_dir, to_dir] {
            let n = self.inode(d)?;
            if n.kind != Kind::Dir {
                return Err(Error::NotDirectory.into());
            }
            if n.readonly {
                return Err(Error::ReadOnly.into());
            }
            if n.links == 0 {
                return Err(Error::NotFound.into());
            }
        }
        let dst = match self.lookup(to_dir, to_name) {
            Ok(id) => Some(id),
            Err(FsError::Fs(Error::NotFound)) => None,
            Err(e) => return Err(e),
        };
        if dst == Some(src) {
            return Ok(());
        }
        let s = self.inode(src)?;
        let src_is_dir = s.kind == Kind::Dir;
        if src_is_dir && self.is_within(src, to_dir)? {
            return Err(Error::Invalid.into());
        }
        let mut credit = 0u64;
        if let Some(d) = dst {
            let dn = self.inode(d)?;
            match (dn.kind == Kind::Dir, src_is_dir) {
                (true, true) if dn.size != 0 => return Err(Error::NotEmpty.into()),
                (true, false) => return Err(Error::IsDirectory.into()),
                (false, true) => return Err(Error::NotDirectory.into()),
                _ => {}
            }
            if self.refs.get(&d).copied().unwrap_or(0) == 0 {
                credit = dn.cost as u64;
            }
        }
        // a symlink's cost includes its target: a rename changes only the name part
        let target_len = if s.kind == Kind::Symlink {
            s.size as usize
        } else {
            0
        };
        let old_cost = s.cost as usize;
        let new_cost = to_name.len() + NODE_OVERHEAD + target_len;
        if new_cost > old_cost
            && self.counters.meta - credit + (new_cost - old_cost) as u64
                > self.limits.max_meta as u64
        {
            return Err(Error::Quota.into());
        }
        self.txn(|fs| {
            if let Some(d) = dst {
                fs.del(&k_dirent(to_dir, to_name))?;
                let mut t = fs.inode(to_dir)?;
                t.size -= 1;
                fs.put_inode(to_dir, &t)?;
                fs.drop_link(d)?;
            }
            fs.del(&k_dirent(from_dir, from_name))?;
            let mut f = fs.inode(from_dir)?;
            f.mtime = fs.now;
            f.size -= 1;
            fs.put_inode(from_dir, &f)?;
            fs.put(&k_dirent(to_dir, to_name), &src.to_be_bytes())?;
            let mut t = fs.inode(to_dir)?;
            t.mtime = fs.now;
            t.size += 1;
            fs.put_inode(to_dir, &t)?;
            fs.counters.meta = fs.counters.meta - old_cost as u64 + new_cost as u64;
            let mut m = fs.inode(src)?;
            m.cost = new_cost as u32;
            m.parent = to_dir;
            fs.put_inode(src, &m)?;
            fs.set_name(src, to_name)
        })
    }

    /// Is `target` the directory `root` itself or somewhere below it?
    fn is_within(&mut self, root: NodeId, target: NodeId) -> R<bool> {
        let mut at = target;
        loop {
            if at == root {
                return Ok(true);
            }
            if at == ROOT {
                return Ok(false);
            }
            at = self.inode(at)?.parent;
        }
    }

    /// A connection starts holding `id`.
    pub fn retain(&mut self, id: NodeId) -> R<()> {
        self.inode(id)?;
        *self.refs.entry(id).or_insert(0) += 1;
        Ok(())
    }

    /// A connection lets go; an unlinked node nobody holds is freed.
    pub fn release(&mut self, id: NodeId) -> R<()> {
        if let Some(n) = self.refs.get_mut(&id) {
            *n -= 1;
            if *n == 0 {
                self.refs.remove(&id);
            }
        }
        if self.refs.contains_key(&id) {
            return Ok(());
        }
        match self.inode(id) {
            Ok(n) if n.links == 0 && id != ROOT => self.txn_reserve(|fs| fs.reap(id)),
            _ => Ok(()),
        }
    }

    // ------------------------------------------------------------------------------------------------------ content

    /// Up to `len` bytes of a file at `offset` (fewer at the end; empty past it).
    pub fn read(&mut self, id: NodeId, offset: u64, len: usize) -> R<Vec<u8>> {
        let n = self.inode(id)?;
        match n.kind {
            Kind::File => {}
            Kind::Dir => return Err(Error::IsDirectory.into()),
            Kind::Image => return Err(Error::ReadOnly.into()),
            Kind::Symlink => return Err(Error::Invalid.into()),
        }
        let start = offset.min(n.size);
        let end = start.saturating_add(len as u64).min(n.size);
        let mut out = Vec::with_capacity((end - start) as usize);
        let mut at = start;
        while at < end {
            let (b, o) = (at / BLOCK as u64, (at % BLOCK as u64) as usize);
            let take = (BLOCK - o).min((end - at) as usize);
            match self.get(&k_extent(id, b))? {
                Some(v) => {
                    if v.len() != 32 {
                        return Err(FsError::Corrupt);
                    }
                    let blk = self.vol.pool.read_data(&Ptr::decode(&v))?;
                    out.extend_from_slice(&blk[o..o + take]);
                }
                None => out.resize(out.len() + take, 0),
            }
            at += take as u64;
        }
        Ok(out)
    }

    fn file_for_write(&mut self, id: NodeId) -> R<Inode> {
        let n = self.inode(id)?;
        match n.kind {
            Kind::Dir => return Err(Error::IsDirectory.into()),
            Kind::Image => return Err(Error::ReadOnly.into()),
            Kind::Symlink => return Err(Error::Invalid.into()),
            Kind::File => {}
        }
        if n.readonly {
            return Err(Error::ReadOnly.into());
        }
        Ok(n)
    }

    /// Write `data` at `offset`, growing the file (a gap reads as zeros and counts against the quota). Nothing changes on
    /// error. Returns the bytes written.
    pub fn write(&mut self, id: NodeId, offset: u64, data: &[u8]) -> R<usize> {
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or(Error::BadRange)?;
        let n = self.file_for_write(id)?;
        if data.is_empty() {
            return Ok(0);
        }
        if end > self.limits.max_file as u64 {
            return Err(Error::TooBig.into());
        }
        let grow = end.saturating_sub(n.size);
        if self.counters.bytes + grow > self.limits.max_bytes as u64 {
            return Err(Error::Quota.into());
        }
        self.txn(|fs| {
            let mut at = offset;
            let mut rest = data;
            while !rest.is_empty() {
                let (b, o) = (at / BLOCK as u64, (at % BLOCK as u64) as usize);
                let take = (BLOCK - o).min(rest.len());
                fs.write_block(id, b, o, &rest[..take])?;
                at += take as u64;
                rest = &rest[take..];
            }
            let mut n = fs.inode(id)?;
            n.size = n.size.max(end);
            n.mtime = fs.now;
            fs.put_inode(id, &n)?;
            fs.counters.bytes += grow;
            Ok(data.len())
        })
    }

    /// Replace `data.len()` bytes at `o` of block `b` (read-modify-write into a fresh block).
    fn write_block(&mut self, id: NodeId, b: u64, o: usize, data: &[u8]) -> R<()> {
        let old = self.get(&k_extent(id, b))?.map(|v| Ptr::decode(&v));
        let mut blk = match &old {
            Some(p) if data.len() < BLOCK => self.vol.pool.read_data(p)?,
            _ => [0u8; BLOCK],
        };
        blk[o..o + data.len()].copy_from_slice(data);
        let p = self.vol.pool.write_data(&blk)?;
        let mut v = [0u8; 32];
        p.encode(&mut v);
        self.put(&k_extent(id, b), &v)?;
        if let Some(old) = old {
            self.vol.pool.retire(&old);
        }
        Ok(())
    }

    /// Drop every block of `id` from block number `from` on.
    fn truncate_blocks(&mut self, id: NodeId, from: u64) -> R<()> {
        let root = self.vol.tree.root();
        let (lo, hi) = (k_extent(id, from), k_extent(id + 1, 0));
        let ents = Tree::range_at(&mut self.vol.pool, root, Some(&lo), Some(&hi))?;
        for (k, v) in ents {
            if v.len() != 32 {
                return Err(FsError::Corrupt);
            }
            self.vol.pool.retire(&Ptr::decode(&v));
            self.del(&k)?;
        }
        Ok(())
    }

    /// Make the file `size` bytes long (zero-filled growth, truncating shrink).
    pub fn set_size(&mut self, id: NodeId, size: u64) -> R<()> {
        let n = self.file_for_write(id)?;
        if size > self.limits.max_file as u64 {
            return Err(Error::TooBig.into());
        }
        if size > n.size && self.counters.bytes + (size - n.size) > self.limits.max_bytes as u64 {
            return Err(Error::Quota.into());
        }
        let shrinking = size < n.size;
        self.txn_with(shrinking, |fs| {
            if size < n.size {
                // whole blocks past the new end go; the tail of the last block is zeroed so a later growth reads zeros
                fs.truncate_blocks(id, size.div_ceil(BLOCK as u64))?;
                let o = (size % BLOCK as u64) as usize;
                if o != 0 {
                    let b = size / BLOCK as u64;
                    if fs.get(&k_extent(id, b))?.is_some() {
                        let zeros = [0u8; BLOCK];
                        fs.write_block(id, b, o, &zeros[o..])?;
                    }
                }
                fs.counters.bytes -= n.size - size;
            } else {
                fs.counters.bytes += size - n.size;
            }
            let mut m = fs.inode(id)?;
            m.size = size;
            m.mtime = fs.now;
            fs.put_inode(id, &m)
        })
    }
}
