//! The memory filesystem's tree (S4-SPEC section 7), with no I/O: directories as ordered
//! maps, files as byte vectors, boot-image files as read-only `(offset, length)` extents,
//! quota accounting, read-only subtrees, and reference counts so a node that is unlinked while
//! a connection has it open lives on until the last connection lets go. `no_std`,
//! `forbid(unsafe_code)`, host-tested against an independent path-keyed model.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

pub type NodeId = u32;

/// The root directory.
pub const ROOT: NodeId = 0;

/// Server-wide ceilings (S4-SPEC section 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_nodes: usize,
    /// Bytes of file data in all files together.
    pub max_bytes: usize,
    /// Bytes in one file.
    pub max_file: usize,
    /// Bytes of bookkeeping (names and per-node overhead) in the whole tree: names are client
    /// data too, and 8192 nodes with 255-byte names are megabytes.
    pub max_meta: usize,
}

/// What a node costs against `max_meta` beyond its name.
const NODE_OVERHEAD: usize = 96;

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            max_nodes: 8192,
            max_bytes: 8 * 1024 * 1024,
            max_file: 4 * 1024 * 1024,
            max_meta: 2 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    /// A read-only file whose bytes are an extent of the boot image.
    Image,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    NotFound,
    Exists,
    NotEmpty,
    IsDirectory,
    NotDirectory,
    /// The node-count or total-bytes quota.
    Quota,
    /// One file would exceed its size limit.
    TooBig,
    /// A read-only node or a read-only directory.
    ReadOnly,
    /// A name that is empty, too long, `.`/`..`, or contains `/` or NUL.
    BadName,
    /// An offset or length that overflows.
    BadRange,
    /// A directory cannot be moved into itself or below itself.
    Invalid,
}

/// File contents: fixed-size chunks, so a file holds (about) the bytes it counts. One growing
/// `Vec` either doubled its capacity (twice the counted memory) or, grown exactly, copied the
/// whole file on every append and needed old plus new at once.
struct Blob {
    len: usize,
    chunks: Vec<alloc::boxed::Box<[u8]>>,
}

/// Bytes per chunk.
const CHUNK: usize = 16 * 1024;

impl Blob {
    fn new() -> Blob {
        Blob {
            len: 0,
            chunks: Vec::new(),
        }
    }

    /// Heap bytes held.
    fn capacity(&self) -> usize {
        self.chunks.len() * CHUNK
    }

    fn read(&self, offset: usize, len: usize) -> Option<Vec<u8>> {
        let start = offset.min(self.len);
        let end = start.saturating_add(len).min(self.len);
        let mut out = Vec::new();
        out.try_reserve_exact(end - start).ok()?;
        let mut at = start;
        while at < end {
            let (c, o) = (at / CHUNK, at % CHUNK);
            let n = (CHUNK - o).min(end - at);
            out.extend_from_slice(&self.chunks[c][o..o + n]);
            at += n;
        }
        Some(out)
    }

    /// Copy `data` in at `offset` (the blob is already long enough).
    fn write_at(&mut self, offset: usize, data: &[u8]) {
        let mut at = offset;
        let mut rest = data;
        while !rest.is_empty() {
            let (c, o) = (at / CHUNK, at % CHUNK);
            let n = (CHUNK - o).min(rest.len());
            self.chunks[c][o..o + n].copy_from_slice(&rest[..n]);
            at += n;
            rest = &rest[n..];
        }
    }

    /// Grow to `new_len` (zero-filled) or shrink. `Err` if memory ran out, with nothing changed.
    fn resize(&mut self, new_len: usize) -> Result<(), ()> {
        let need = new_len.div_ceil(CHUNK);
        if need > self.chunks.len() {
            self.chunks
                .try_reserve_exact(need - self.chunks.len())
                .map_err(|_| ())?;
            let mut fresh: Vec<alloc::boxed::Box<[u8]>> = Vec::new();
            fresh
                .try_reserve_exact(need - self.chunks.len())
                .map_err(|_| ())?;
            for _ in self.chunks.len()..need {
                let mut c: Vec<u8> = Vec::new();
                c.try_reserve_exact(CHUNK).map_err(|_| ())?;
                c.resize(CHUNK, 0);
                fresh.push(c.into_boxed_slice());
            }
            self.chunks.extend(fresh);
        } else {
            self.chunks.truncate(need);
            // the unused tail of the last chunk must read as zeros if the file grows again
            if !new_len.is_multiple_of(CHUNK) {
                if let Some(last) = self.chunks.last_mut() {
                    last[new_len % CHUNK..].fill(0);
                }
            }
        }
        self.len = new_len;
        Ok(())
    }
}

enum Data {
    File(Blob),
    Dir(BTreeMap<Vec<u8>, NodeId>),
    Image { offset: u64, len: u64 },
}

struct Node {
    /// Name length plus overhead, charged to `max_meta` while the node lives.
    cost: usize,
    data: Data,
    /// Directory entries that name this node (0 once unlinked, 1 while linked).
    links: u32,
    /// Open connections holding this node.
    refs: u32,
    readonly: bool,
}

pub struct Fs {
    limits: Limits,
    nodes: Vec<Option<Node>>,
    free: Vec<NodeId>,
    live: usize,
    bytes: usize,
    meta: usize,
}

fn check_name(n: &[u8]) -> Result<(), Error> {
    proto::file::valid_name(n).map_err(|_| Error::BadName)
}

impl Fs {
    pub fn new(limits: Limits) -> Fs {
        let root = Node {
            data: Data::Dir(BTreeMap::new()),
            links: 1,
            refs: 0,
            readonly: false,
            cost: 0,
        };
        Fs {
            limits,
            nodes: alloc::vec![Some(root)],
            free: Vec::new(),
            live: 1,
            bytes: 0,
            meta: 0,
        }
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Live nodes (including unlinked ones that are still open).
    pub fn node_count(&self) -> usize {
        self.live
    }

    /// Bytes of file data held.
    pub fn bytes_used(&self) -> usize {
        self.bytes
    }

    /// Bytes of name and node bookkeeping charged so far.
    pub fn meta_used(&self) -> usize {
        self.meta
    }

    /// Heap bytes the file data really occupies (capacity, not length): what the server's own
    /// memory use depends on.
    pub fn data_capacity(&self) -> usize {
        self.nodes
            .iter()
            .flatten()
            .map(|n| match &n.data {
                Data::File(b) => b.capacity(),
                _ => 0,
            })
            .sum()
    }

    fn node(&self, id: NodeId) -> Result<&Node, Error> {
        self.nodes
            .get(id as usize)
            .and_then(|n| n.as_ref())
            .ok_or(Error::NotFound)
    }

    fn node_mut(&mut self, id: NodeId) -> Result<&mut Node, Error> {
        self.nodes
            .get_mut(id as usize)
            .and_then(|n| n.as_mut())
            .ok_or(Error::NotFound)
    }

    pub fn kind(&self, id: NodeId) -> Result<Kind, Error> {
        Ok(match self.node(id)?.data {
            Data::File(_) => Kind::File,
            Data::Dir(_) => Kind::Dir,
            Data::Image { .. } => Kind::Image,
        })
    }

    /// Size of a file or image extent in bytes (a directory has the number of its entries).
    pub fn size(&self, id: NodeId) -> Result<u64, Error> {
        Ok(match &self.node(id)?.data {
            Data::File(b) => b.len as u64,
            Data::Dir(m) => m.len() as u64,
            Data::Image { len, .. } => *len,
        })
    }

    pub fn is_readonly(&self, id: NodeId) -> Result<bool, Error> {
        let n = self.node(id)?;
        Ok(n.readonly || matches!(n.data, Data::Image { .. }))
    }

    /// Mark a directory or file read-only: no entries may be created in or removed from a
    /// read-only directory, and a read-only file cannot be written. (Image files always are.)
    pub fn set_readonly(&mut self, id: NodeId) -> Result<(), Error> {
        self.node_mut(id)?.readonly = true;
        Ok(())
    }

    /// The entry called `name` in directory `dir`.
    pub fn lookup(&self, dir: NodeId, name: &[u8]) -> Result<NodeId, Error> {
        check_name(name)?;
        match &self.node(dir)?.data {
            Data::Dir(m) => m.get(name).copied().ok_or(Error::NotFound),
            _ => Err(Error::NotDirectory),
        }
    }

    /// Follow `components` from `start` (each must be a directory except the last).
    pub fn walk(&self, start: NodeId, components: &[&[u8]]) -> Result<NodeId, Error> {
        let mut at = start;
        for c in components {
            at = self.lookup(at, c)?;
        }
        Ok(at)
    }

    fn insert(&mut self, dir: NodeId, name: &[u8], data: Data) -> Result<NodeId, Error> {
        check_name(name)?;
        {
            let d = self.node(dir)?;
            match &d.data {
                Data::Dir(m) => {
                    if m.contains_key(name) {
                        return Err(Error::Exists);
                    }
                }
                _ => return Err(Error::NotDirectory),
            }
            if d.readonly {
                return Err(Error::ReadOnly);
            }
            // A directory that was unlinked while a connection held it is an orphan: nothing may
            // be created in it (its children would outlive it and leak quota).
            if d.links == 0 {
                return Err(Error::NotFound);
            }
        }
        let cost = name.len() + NODE_OVERHEAD;
        if self.live >= self.limits.max_nodes || self.meta + cost > self.limits.max_meta {
            return Err(Error::Quota);
        }
        let node = Node {
            cost,
            data,
            links: 1,
            refs: 0,
            readonly: false,
        };
        let id = if let Some(id) = self.free.pop() {
            self.nodes[id as usize] = Some(node);
            id
        } else {
            self.nodes.try_reserve(1).map_err(|_| Error::Quota)?;
            self.nodes.push(Some(node));
            (self.nodes.len() - 1) as NodeId
        };
        self.live += 1;
        self.meta += cost;
        if let Data::Dir(m) = &mut self.node_mut(dir)?.data {
            m.insert(name.to_vec(), id);
        }
        Ok(id)
    }

    pub fn create_file(&mut self, dir: NodeId, name: &[u8]) -> Result<NodeId, Error> {
        self.insert(dir, name, Data::File(Blob::new()))
    }

    pub fn mkdir(&mut self, dir: NodeId, name: &[u8]) -> Result<NodeId, Error> {
        self.insert(dir, name, Data::Dir(BTreeMap::new()))
    }

    /// A read-only file whose contents are `len` bytes at `offset` of the boot image.
    pub fn add_image_file(
        &mut self,
        dir: NodeId,
        name: &[u8],
        offset: u64,
        len: u64,
    ) -> Result<NodeId, Error> {
        offset.checked_add(len).ok_or(Error::BadRange)?;
        self.insert(dir, name, Data::Image { offset, len })
    }

    /// The `(offset, length)` of an image file.
    pub fn image_extent(&self, id: NodeId) -> Result<(u64, u64), Error> {
        match self.node(id)?.data {
            Data::Image { offset, len } => Ok((offset, len)),
            Data::Dir(_) => Err(Error::IsDirectory),
            Data::File(_) => Err(Error::NotFound),
        }
    }

    /// Remove the entry `name` of `dir`. A directory must be empty. The node itself lives on
    /// while connections hold it.
    pub fn unlink(&mut self, dir: NodeId, name: &[u8]) -> Result<(), Error> {
        check_name(name)?;
        let target = {
            let d = self.node(dir)?;
            if d.readonly {
                return Err(Error::ReadOnly);
            }
            match &d.data {
                Data::Dir(m) => *m.get(name).ok_or(Error::NotFound)?,
                _ => return Err(Error::NotDirectory),
            }
        };
        if let Data::Dir(m) = &self.node(target)?.data {
            if !m.is_empty() {
                return Err(Error::NotEmpty);
            }
        }
        if let Data::Dir(m) = &mut self.node_mut(dir)?.data {
            m.remove(name);
        }
        self.node_mut(target)?.links = 0;
        self.collect(target);
        Ok(())
    }

    /// Move the entry `from_name` of `from_dir` to `to_dir` as `to_name` (`rename(2)`): an existing
    /// target is replaced if it is a file and the source is a file, or an empty directory and the
    /// source is a directory; the node (and so any open connection to it) is the same afterwards.
    /// Both directories must be writable. A directory cannot be moved below itself.
    pub fn rename(
        &mut self,
        from_dir: NodeId,
        from_name: &[u8],
        to_dir: NodeId,
        to_name: &[u8],
    ) -> Result<(), Error> {
        check_name(from_name)?;
        check_name(to_name)?;
        let src = self.lookup(from_dir, from_name)?;
        for d in [from_dir, to_dir] {
            let n = self.node(d)?;
            if !matches!(n.data, Data::Dir(_)) {
                return Err(Error::NotDirectory);
            }
            if n.readonly {
                return Err(Error::ReadOnly);
            }
            if n.links == 0 {
                return Err(Error::NotFound); // an orphaned directory takes no entries
            }
        }
        let dst = match self.lookup(to_dir, to_name) {
            Ok(id) => Some(id),
            Err(Error::NotFound) => None,
            Err(e) => return Err(e),
        };
        if dst == Some(src) {
            return Ok(()); // the same node under the same name: nothing to do
        }
        let src_is_dir = matches!(self.node(src)?.data, Data::Dir(_));
        if src_is_dir && self.is_within(src, to_dir)? {
            return Err(Error::Invalid);
        }
        let mut freed = 0;
        if let Some(d) = dst {
            match (&self.node(d)?.data, src_is_dir) {
                (Data::Dir(m), true) if !m.is_empty() => return Err(Error::NotEmpty),
                (Data::Dir(_), false) => return Err(Error::IsDirectory),
                (Data::Dir(_), true) => {}
                (_, true) => return Err(Error::NotDirectory),
                (_, false) => {}
            }
            freed = self.node(d)?.cost;
        }
        let old_cost = self.node(src)?.cost;
        let new_cost = to_name.len() + NODE_OVERHEAD;
        // the replaced target's cost comes back once nothing holds it (not before: a connection
        // may keep it alive), so count it only if it is freed at once
        let credit = match dst {
            Some(d) if self.node(d)?.refs == 0 => freed,
            _ => 0,
        };
        if new_cost > old_cost && self.meta - credit + (new_cost - old_cost) > self.limits.max_meta
        {
            return Err(Error::Quota);
        }
        if let Some(d) = dst {
            if let Data::Dir(m) = &mut self.node_mut(to_dir)?.data {
                m.remove(to_name);
            }
            self.node_mut(d)?.links = 0;
            self.collect(d);
        }
        if let Data::Dir(m) = &mut self.node_mut(from_dir)?.data {
            m.remove(from_name);
        }
        if let Data::Dir(m) = &mut self.node_mut(to_dir)?.data {
            m.insert(to_name.to_vec(), src);
        }
        self.meta = self.meta - old_cost + new_cost;
        self.node_mut(src)?.cost = new_cost;
        Ok(())
    }

    /// Is `target` the directory `root` itself or somewhere below it?
    fn is_within(&self, root: NodeId, target: NodeId) -> Result<bool, Error> {
        let mut work = alloc::vec![root];
        while let Some(d) = work.pop() {
            if d == target {
                return Ok(true);
            }
            if let Data::Dir(m) = &self.node(d)?.data {
                work.extend(m.values().copied());
            }
        }
        Ok(false)
    }

    /// A connection starts holding `id`.
    pub fn retain(&mut self, id: NodeId) -> Result<(), Error> {
        self.node_mut(id)?.refs += 1;
        Ok(())
    }

    /// A connection lets go; an unlinked node nobody holds is freed.
    pub fn release(&mut self, id: NodeId) {
        if let Ok(n) = self.node_mut(id) {
            n.refs = n.refs.saturating_sub(1);
        }
        self.collect(id);
    }

    fn collect(&mut self, id: NodeId) {
        let mut work = alloc::vec![id];
        while let Some(id) = work.pop() {
            let dead = matches!(self.nodes.get(id as usize), Some(Some(n)) if n.links == 0 && n.refs == 0)
                && id != ROOT;
            if !dead {
                continue;
            }
            if let Some(Some(n)) = self.nodes.get_mut(id as usize).map(|s| s.take()) {
                self.meta -= n.cost;
                match &n.data {
                    Data::File(b) => self.bytes -= b.len,
                    // Defence in depth: whatever a dying directory still names dies with it.
                    Data::Dir(m) => {
                        for &child in m.values() {
                            if let Some(Some(c)) = self.nodes.get_mut(child as usize) {
                                c.links = 0;
                            }
                            work.push(child);
                        }
                    }
                    Data::Image { .. } => {}
                }
            }
            self.free.push(id);
            self.live -= 1;
        }
    }

    /// Up to `len` bytes of a file at `offset` (fewer at the end; empty past it).
    pub fn read(&self, id: NodeId, offset: u64, len: usize) -> Result<Vec<u8>, Error> {
        match &self.node(id)?.data {
            Data::File(b) => b
                .read(usize::try_from(offset).unwrap_or(usize::MAX), len)
                .ok_or(Error::Quota),
            Data::Dir(_) => Err(Error::IsDirectory),
            Data::Image { .. } => Err(Error::ReadOnly),
        }
    }

    /// Write `data` at `offset`, growing the file (a gap is zero-filled and counts against the
    /// quota). Nothing changes on error. Returns the bytes written.
    pub fn write(&mut self, id: NodeId, offset: u64, data: &[u8]) -> Result<usize, Error> {
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or(Error::BadRange)?;
        let n = self.node(id)?;
        match &n.data {
            Data::Dir(_) => return Err(Error::IsDirectory),
            Data::Image { .. } => return Err(Error::ReadOnly),
            Data::File(_) => {}
        }
        if n.readonly {
            return Err(Error::ReadOnly);
        }
        if data.is_empty() {
            return Ok(0);
        }
        let end = usize::try_from(end).map_err(|_| Error::TooBig)?;
        self.resize_for(id, end)?;
        let offset = offset as usize;
        if let Data::File(b) = &mut self.node_mut(id)?.data {
            b.write_at(offset, data);
        }
        Ok(data.len())
    }

    /// Make the file `size` bytes long (zero-filled growth, truncating shrink).
    pub fn set_size(&mut self, id: NodeId, size: u64) -> Result<(), Error> {
        let n = self.node(id)?;
        match &n.data {
            Data::Dir(_) => return Err(Error::IsDirectory),
            Data::Image { .. } => return Err(Error::ReadOnly),
            Data::File(_) => {}
        }
        if n.readonly {
            return Err(Error::ReadOnly);
        }
        let size = usize::try_from(size).map_err(|_| Error::TooBig)?;
        if size > self.limits.max_file {
            return Err(Error::TooBig);
        }
        let cur = match &self.node(id)?.data {
            Data::File(b) => b.len,
            _ => 0,
        };
        if size > cur {
            self.resize_for(id, size)
        } else {
            if let Data::File(b) = &mut self.node_mut(id)?.data {
                let _ = b.resize(size); // shrinking frees; it cannot fail
            }
            self.bytes -= cur - size;
            Ok(())
        }
    }

    /// Grow the file to at least `new_len` bytes, checking limits before changing anything.
    fn resize_for(&mut self, id: NodeId, new_len: usize) -> Result<(), Error> {
        let cur = match &self.node(id)?.data {
            Data::File(b) => b.len,
            _ => return Err(Error::IsDirectory),
        };
        if new_len <= cur {
            return Ok(());
        }
        if new_len > self.limits.max_file {
            return Err(Error::TooBig);
        }
        let grow = new_len - cur;
        if self.bytes + grow > self.limits.max_bytes {
            return Err(Error::Quota);
        }
        if let Data::File(b) = &mut self.node_mut(id)?.data {
            b.resize(new_len).map_err(|_| Error::Quota)?;
        }
        self.bytes += grow;
        Ok(())
    }

    /// Entries of `dir` in bytewise name order, strictly after `after` (the last name the caller
    /// saw; `None` starts at the beginning), at most `max` of them.
    pub fn read_dir(
        &self,
        dir: NodeId,
        after: Option<&[u8]>,
        max: usize,
    ) -> Result<Vec<(Vec<u8>, Kind)>, Error> {
        let m = match &self.node(dir)?.data {
            Data::Dir(m) => m,
            _ => return Err(Error::NotDirectory),
        };
        let mut out = Vec::new();
        let iter: alloc::boxed::Box<dyn Iterator<Item = (&Vec<u8>, &NodeId)>> = match after {
            Some(a) => alloc::boxed::Box::new(
                m.range::<[u8], _>((core::ops::Bound::Excluded(a), core::ops::Bound::Unbounded)),
            ),
            None => alloc::boxed::Box::new(m.iter()),
        };
        for (name, id) in iter.take(max) {
            out.push((name.clone(), self.kind(*id)?));
        }
        Ok(out)
    }
}
