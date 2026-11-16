//! Attributes, indexes and queries (FS-SPEC 9.1) on the disk: the same operations, errors and results as `memfs`.
//!
//! * An attribute is the record `08 id name -> type, value`. A user index is the definition `09 name -> type, number` and one
//!   entry `0a number key id` per node that has an attribute of that name and type; the built-in indexes (number 0 name, 1
//!   size, 2 mtime) have entries for every linked node but the root, kept up to date by `put_inode` and `set_name`.
//! * The index key in an entry is the **escaped** key (a zero byte becomes `00 ff`, the end is `00 01`), so that the entries
//!   sort by `(key, id)` whatever the keys' lengths.
//! * A query is a range scan over one index, filtered to the nodes strictly below the directory.

use crate::{k_id, u64_of, Fs, FsError, Inode, K_ATTR, K_IDXDEF, K_INDEX, K_NAMEOF, R, ROOT};
use alloc::vec::Vec;
use fsfmt::{Device, Tree};
pub use memfs::{AttrType, Term};
use memfs::{
    Error, Kind, BUILTIN_MTIME, BUILTIN_NAME, BUILTIN_SIZE, MAX_ATTR_NAME, MAX_ATTR_VALUE,
    MAX_INDEXES, MAX_INDEX_VALUE, MAX_QUERY,
};

/// One query result: the node, its key, and its path relative to the directory queried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub id: u64,
    pub key: Vec<u8>,
    pub path: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryResult {
    pub hits: Vec<Hit>,
    /// There were more matches than `max`.
    pub more: bool,
}

pub const IDX_NAME: u8 = 0;
pub const IDX_SIZE: u8 = 1;
pub const IDX_MTIME: u8 = 2;
/// The first number a user index gets.
const FIRST_USER: u8 = 3;
/// What an attribute costs against the metadata quota beyond its name and value, and an index beyond its name.
pub(crate) const ATTR_OVERHEAD: usize = 32;
pub(crate) const INDEX_OVERHEAD: usize = 64;

/// A key with every zero byte doubled to `00 ff` and a `00 01` at the end: prefix-free, and bytewise order is preserved.
pub fn esc(key: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(key.len() + 2);
    for &b in key {
        out.push(b);
        if b == 0 {
            out.push(0xff);
        }
    }
    out.extend_from_slice(&[0, 1]);
    out
}

/// The key and id of an index entry's tree key `0a idx escaped id`.
pub(crate) fn parse_entry(k: &[u8]) -> Option<(Vec<u8>, u64)> {
    if k.len() < 2 + 2 + 8 {
        return None;
    }
    let id = u64::from_be_bytes(k[k.len() - 8..].try_into().ok()?);
    let body = &k[2..k.len() - 8];
    let mut key = Vec::new();
    let mut i = 0;
    while i < body.len() {
        if body[i] == 0 {
            match body.get(i + 1) {
                Some(0xff) => {
                    key.push(0);
                    i += 2;
                }
                Some(1) if i + 2 == body.len() => return Some((key, id)),
                _ => return None,
            }
        } else {
            key.push(body[i]);
            i += 1;
        }
    }
    None
}

pub fn k_index(idx: u8, key: &[u8], id: u64) -> Vec<u8> {
    let mut k = alloc::vec![K_INDEX, idx];
    k.extend_from_slice(&esc(key));
    k.extend_from_slice(&id.to_be_bytes());
    k
}

pub(crate) fn k_attr(id: u64, name: &[u8]) -> Vec<u8> {
    let mut k = alloc::vec![K_ATTR];
    k.extend_from_slice(&id.to_be_bytes());
    k.extend_from_slice(name);
    k
}

fn k_idxdef(name: &[u8]) -> Vec<u8> {
    let mut k = alloc::vec![K_IDXDEF];
    k.extend_from_slice(name);
    k
}

/// The built-in index entries a node has (size and mtime; the name has its own record): none unless it is linked.
pub(crate) fn builtin_keys(n: &Inode) -> Vec<(u8, Vec<u8>)> {
    if n.links != 1 {
        return Vec::new();
    }
    alloc::vec![
        (IDX_SIZE, n.size.to_be_bytes().to_vec()),
        (IDX_MTIME, n.mtime.to_be_bytes().to_vec()),
    ]
}

fn check_attr_name(name: &[u8]) -> Result<(), Error> {
    if name.is_empty()
        || name.len() > MAX_ATTR_NAME
        || name.contains(&0)
        || name.contains(&b'/')
        || name == BUILTIN_NAME
        || name == BUILTIN_SIZE
        || name == BUILTIN_MTIME
    {
        return Err(Error::BadName);
    }
    Ok(())
}

fn ty_of(code: u8) -> R<AttrType> {
    AttrType::from_code(code).ok_or(FsError::Corrupt)
}

impl<D: Device> Fs<D> {
    /// The user indexes: name to (type, number).
    fn index_defs(&mut self) -> R<Vec<(Vec<u8>, AttrType, u8)>> {
        let root = self.vol.tree.root();
        let ents = Tree::range_at(
            &mut self.vol.pool,
            root,
            Some(&[K_IDXDEF]),
            Some(&[K_IDXDEF + 1]),
        )?;
        let mut out = Vec::new();
        for (k, v) in ents {
            if v.len() != 2 {
                return Err(FsError::Corrupt);
            }
            out.push((k[1..].to_vec(), ty_of(v[0])?, v[1]));
        }
        Ok(out)
    }

    /// The user indexes (name, type) in bytewise order. The built-in ones are not listed.
    pub fn indexes(&mut self) -> R<Vec<(Vec<u8>, AttrType)>> {
        Ok(self
            .index_defs()?
            .into_iter()
            .map(|(n, t, _)| (n, t))
            .collect())
    }

    fn index_def(&mut self, name: &[u8]) -> R<Option<(AttrType, u8)>> {
        match self.get(&k_idxdef(name))? {
            Some(v) if v.len() == 2 => Ok(Some((ty_of(v[0])?, v[1]))),
            Some(_) => Err(FsError::Corrupt),
            None => Ok(None),
        }
    }

    fn attr_record(&mut self, id: u64, name: &[u8]) -> R<Option<(AttrType, Vec<u8>)>> {
        match self.get(&k_attr(id, name))? {
            Some(v) if !v.is_empty() => Ok(Some((ty_of(v[0])?, v[1..].to_vec()))),
            Some(_) => Err(FsError::Corrupt),
            None => Ok(None),
        }
    }

    /// Set (replace) attribute `name` of `id`.
    pub fn attr_set(&mut self, id: u64, name: &[u8], ty: AttrType, value: &[u8]) -> R<()> {
        check_attr_name(name)?;
        if value.len() > MAX_ATTR_VALUE {
            return Err(Error::TooBig.into());
        }
        let key = ty.key(value)?; // type and length
        let n = self.inode(id)?;
        if n.readonly {
            return Err(Error::ReadOnly.into());
        }
        let def = self.index_def(name)?;
        if let Some((it, _)) = def {
            if it == ty && value.len() > MAX_INDEX_VALUE {
                return Err(Error::TooBig.into());
            }
        }
        let old = self.attr_record(id, name)?;
        let new_cost = name.len() + value.len() + ATTR_OVERHEAD;
        let old_cost = old
            .as_ref()
            .map_or(0, |(_, v)| name.len() + v.len() + ATTR_OVERHEAD);
        if new_cost > old_cost
            && self.counters.meta + (new_cost - old_cost) as u64 > self.limits.max_meta as u64
        {
            return Err(Error::Quota.into());
        }
        self.txn(|fs| {
            let mut rec = alloc::vec![ty as u8];
            rec.extend_from_slice(value);
            fs.put(&k_attr(id, name), &rec)?;
            if let Some((it, idx)) = def {
                if let Some((ot, ov)) = &old {
                    if *ot == it {
                        fs.del(&k_index(
                            idx,
                            &it.key(ov).map_err(|_| FsError::Corrupt)?,
                            id,
                        ))?;
                    }
                }
                if ty == it {
                    fs.put(&k_index(idx, &key, id), b"")?;
                }
            }
            let mut n = fs.inode(id)?;
            n.attr_cost = n.attr_cost - old_cost as u32 + new_cost as u32;
            fs.put_inode(id, &n)?;
            fs.counters.meta = fs.counters.meta - old_cost as u64 + new_cost as u64;
            Ok(())
        })
    }

    pub fn attr_get(&mut self, id: u64, name: &[u8]) -> R<(AttrType, Vec<u8>)> {
        check_attr_name(name)?;
        self.inode(id)?;
        self.attr_record(id, name)?.ok_or(Error::NoAttr.into())
    }

    pub fn attr_remove(&mut self, id: u64, name: &[u8]) -> R<()> {
        check_attr_name(name)?;
        let n = self.inode(id)?;
        if n.readonly {
            return Err(Error::ReadOnly.into());
        }
        let (ty, v) = self.attr_record(id, name)?.ok_or(Error::NoAttr)?;
        let def = self.index_def(name)?;
        let cost = name.len() + v.len() + ATTR_OVERHEAD;
        self.txn_reserve(|fs| {
            fs.del(&k_attr(id, name))?;
            if let Some((it, idx)) = def {
                if it == ty {
                    fs.del(&k_index(
                        idx,
                        &it.key(&v).map_err(|_| FsError::Corrupt)?,
                        id,
                    ))?;
                }
            }
            let mut n = fs.inode(id)?;
            n.attr_cost -= cost as u32;
            fs.put_inode(id, &n)?;
            fs.counters.meta -= cost as u64;
            Ok(())
        })
    }

    /// The attribute names of `id` in bytewise order.
    pub fn attr_list(&mut self, id: u64) -> R<Vec<Vec<u8>>> {
        self.inode(id)?;
        let (lo, hi) = (k_attr(id, b""), k_attr(id + 1, b""));
        let root = self.vol.tree.root();
        let ents = Tree::range_at(&mut self.vol.pool, root, Some(&lo), Some(&hi))?;
        Ok(ents.into_iter().map(|(k, _)| k[9..].to_vec()).collect())
    }

    /// Remove every attribute of a node that is going away, and its entries.
    pub(crate) fn drop_attrs(&mut self, id: u64) -> R<()> {
        let (lo, hi) = (k_attr(id, b""), k_attr(id + 1, b""));
        let root = self.vol.tree.root();
        let ents = Tree::range_at(&mut self.vol.pool, root, Some(&lo), Some(&hi))?;
        for (k, v) in ents {
            let name = &k[9..];
            if let Some((it, idx)) = self.index_def(name)? {
                if v.first().copied() == Some(it as u8) {
                    let key = it.key(&v[1..]).map_err(|_| FsError::Corrupt)?;
                    self.del(&k_index(idx, &key, id))?;
                }
            }
            self.del(&k)?;
        }
        Ok(())
    }

    /// Create an index on attribute `name` of type `ty` and back-fill it. Nothing changes on error.
    pub fn make_index(&mut self, name: &[u8], ty: AttrType) -> R<()> {
        check_attr_name(name)?;
        if self.index_def(name)?.is_some() {
            return Err(Error::Exists.into());
        }
        let defs = self.index_defs()?;
        if defs.len() >= MAX_INDEXES {
            return Err(Error::Quota.into());
        }
        // the back-fill: every value of that type, on any node, must be indexable
        let root = self.vol.tree.root();
        let attrs = Tree::range_at(
            &mut self.vol.pool,
            root,
            Some(&[K_ATTR]),
            Some(&[K_ATTR + 1]),
        )?;
        let mut fill: Vec<(Vec<u8>, u64)> = Vec::new();
        for (k, v) in &attrs {
            if k.len() > 9 && &k[9..] == name && v.first().copied() == Some(ty as u8) {
                if v.len() - 1 > MAX_INDEX_VALUE {
                    return Err(Error::TooBig.into());
                }
                fill.push((
                    ty.key(&v[1..]).map_err(|_| FsError::Corrupt)?,
                    u64_of(&k[1..9])?,
                ));
            }
        }
        let cost = name.len() + INDEX_OVERHEAD;
        if self.counters.meta + cost as u64 > self.limits.max_meta as u64 {
            return Err(Error::Quota.into());
        }
        let number = FIRST_USER + defs.len() as u8;
        self.txn(|fs| {
            fs.put(&k_idxdef(name), &[ty as u8, number])?;
            for (key, id) in &fill {
                fs.put(&k_index(number, key, *id), b"")?;
            }
            fs.counters.meta += cost as u64;
            Ok(())
        })
    }

    /// The path of `id` relative to `below`, if `id` is strictly below it and linked all the way.
    fn path_below(&mut self, id: u64, below: u64) -> R<Option<Vec<u8>>> {
        let mut parts: Vec<Vec<u8>> = Vec::new();
        let mut at = id;
        while at != below {
            let n = self.inode(at)?;
            if n.links == 0 || at == ROOT {
                return Ok(None);
            }
            match self.get(&k_id(K_NAMEOF, at))? {
                Some(name) => parts.push(name),
                None => return Ok(None),
            }
            at = n.parent;
        }
        if parts.is_empty() {
            return Ok(None);
        }
        parts.reverse();
        Ok(Some(parts.join(&b'/')))
    }

    /// The live, linked nodes strictly below `dir` whose key in `index` matches `term`, ordered by `(key, id)`, strictly after
    /// `after` when given, at most `min(max, MAX_QUERY)` of them (FS-SPEC 9.1).
    pub fn query(
        &mut self,
        dir: u64,
        index: &[u8],
        term: &Term,
        after: Option<(&[u8], u64)>,
        max: usize,
    ) -> R<QueryResult> {
        if self.inode(dir)?.kind != Kind::Dir {
            return Err(Error::NotDirectory.into());
        }
        let (ty, idx) = if index == BUILTIN_NAME {
            (AttrType::Bytes, IDX_NAME)
        } else if index == BUILTIN_SIZE {
            (AttrType::U64, IDX_SIZE)
        } else if index == BUILTIN_MTIME {
            (AttrType::U64, IDX_MTIME)
        } else {
            self.index_def(index)?.ok_or(Error::NoIndex)?
        };
        let (lo, hi, eq) = match term {
            Term::Eq(v) => (None, None, Some(ty.key(v)?)),
            Term::Range { lo, hi } => (
                lo.as_deref().map(|v| ty.key(v)).transpose()?,
                hi.as_deref().map(|v| ty.key(v)).transpose()?,
                None,
            ),
        };
        // the scan bounds in tree keys
        let prefix = [K_INDEX, idx];
        let mut start = match (&eq, &lo) {
            (Some(e), _) => [&prefix[..], &esc(e)].concat(),
            (None, Some(l)) => [&prefix[..], &esc(l)].concat(),
            (None, None) => prefix.to_vec(),
        };
        let end = match (&eq, &hi) {
            (Some(e), _) => [&prefix[..], &esc(e), &[0xff; 9]].concat(),
            (None, Some(h)) => [&prefix[..], &esc(h)].concat(),
            (None, None) => alloc::vec![K_INDEX, idx + 1],
        };
        if let Some((ak, aid)) = after {
            // the first key after the pair (ak, aid): the entry's own bytes with a zero appended
            let mut k = k_index(idx, ak, aid);
            k.push(0);
            if k > start {
                start = k;
            }
        }
        let max = max.min(MAX_QUERY);
        let mut hits: Vec<Hit> = Vec::new();
        let mut more = false;
        'scan: loop {
            let root = self.vol.tree.root();
            let chunk =
                Tree::range_limited(&mut self.vol.pool, root, Some(&start), Some(&end), 256)?;
            if chunk.is_empty() {
                break;
            }
            for (k, _) in &chunk {
                let (key, id) = parse_entry(k).ok_or(FsError::Corrupt)?;
                if id == ROOT || id == dir {
                    continue;
                }
                let Some(path) = self.path_below(id, dir)? else {
                    continue;
                };
                if hits.len() == max {
                    more = true;
                    break 'scan;
                }
                hits.push(Hit { id, key, path });
            }
            let mut next = chunk[chunk.len() - 1].0.clone();
            next.push(0);
            start = next;
        }
        Ok(QueryResult { hits, more })
    }
}
