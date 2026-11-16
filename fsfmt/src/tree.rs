//! The copy-on-write B+tree (FS-SPEC section 3, F1): byte-string keys and values, every change writes new blocks along the
//! path from a leaf to the root and retires the old ones, and a **commit** closes a generation and yields a [`Snapshot`] that
//! stays readable (none of its blocks is overwritten or reused) until it is released. No block is ever modified in place.

use crate::device::Device;
use crate::node::{Node, Ptr, CAP};
use crate::pool::{Pool, PoolError};
use crate::{MAX_KEY, MAX_VALUE};
use alloc::vec::Vec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeError {
    Pool(PoolError),
    /// An empty key, or one over `MAX_KEY`.
    BadKey,
    /// A value over `MAX_VALUE`.
    BadValue,
    /// A structural rule broken (found by `verify`).
    Structure(&'static str),
}

impl From<PoolError> for TreeError {
    fn from(e: PoolError) -> TreeError {
        TreeError::Pool(e)
    }
}

/// A committed root: the tree as it was when generation `gen` closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    pub root: Option<Ptr>,
    pub gen: u64,
}

/// What `verify` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    pub entries: usize,
    pub nodes: usize,
    /// Levels above the leaves (0: the root is a leaf).
    pub height: u8,
    /// Every block address the tree reaches, sorted.
    pub reachable: Vec<u64>,
}

/// A right sibling made by a split, with the bound that routes to it.
/// Key/value pairs in key order.
pub type Entries = Vec<(Vec<u8>, Vec<u8>)>;

type Split = Option<(Vec<u8>, Ptr)>;

pub struct Tree {
    root: Option<Ptr>,
}

/// Result of inserting below a node: its replacement, and a right sibling if it had to split.
struct Ins {
    left: Ptr,
    split: Split,
    old: Option<Vec<u8>>,
    level: u8,
}

enum Del {
    Unchanged,
    Empty,
    Replaced(Ptr),
}

/// The child of an internal node that covers `key`: the last bound at or below it among the second entry on, else the first.
fn route(kids: &[(Vec<u8>, Ptr)], key: &[u8]) -> usize {
    kids[1..].partition_point(|(b, _)| b.as_slice() <= key)
}

/// Where to cut `sizes` so both halves are non-empty and the larger is as small as it can be (so that both fit a block).
fn cut(sizes: &[usize]) -> usize {
    let total: usize = sizes.iter().sum();
    let mut acc = 0;
    let (mut best, mut best_max) = (1, usize::MAX);
    for (i, s) in sizes.iter().enumerate().take(sizes.len() - 1) {
        acc += s;
        let m = acc.max(total - acc);
        if m < best_max {
            best_max = m;
            best = i + 1;
        }
    }
    best
}

impl Default for Tree {
    fn default() -> Self {
        Self::new()
    }
}

impl Tree {
    pub fn new() -> Tree {
        Tree { root: None }
    }

    /// Continue from a committed root (new changes copy on write from it).
    pub fn from_snapshot(s: &Snapshot) -> Tree {
        Tree { root: s.root }
    }

    /// A tree rooted at `root` (a pointer remembered earlier, to put a tree back).
    pub fn from_root(root: Option<Ptr>) -> Tree {
        Tree { root }
    }

    pub fn root(&self) -> Option<Ptr> {
        self.root
    }

    /// Close the open generation: the tree as it is now is the snapshot, retained until `release`.
    pub fn commit<D: Device>(&self, pool: &mut Pool<D>) -> Snapshot {
        let s = Snapshot {
            root: self.root,
            gen: pool.open_gen,
        };
        pool.pin(s.gen);
        pool.open_gen += 1;
        s
    }

    /// Let go of a snapshot; its blocks may be reused by the next `reclaim`.
    pub fn release<D: Device>(pool: &mut Pool<D>, s: &Snapshot) {
        pool.unpin(s.gen);
    }

    fn check_kv(key: &[u8], value: Option<&[u8]>) -> Result<(), TreeError> {
        if key.is_empty() || key.len() > MAX_KEY {
            return Err(TreeError::BadKey);
        }
        if value.is_some_and(|v| v.len() > MAX_VALUE) {
            return Err(TreeError::BadValue);
        }
        Ok(())
    }

    // ------------------------------------------------------------------------------------------------------- reading

    /// The value of `key` in the tree rooted at `root`.
    pub fn get_at<D: Device>(
        pool: &mut Pool<D>,
        root: Option<Ptr>,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, TreeError> {
        let mut at = root;
        while let Some(p) = at {
            match pool.read_node(&p)? {
                Node::Leaf(e) => {
                    return Ok(e
                        .binary_search_by(|(k, _)| k.as_slice().cmp(key))
                        .ok()
                        .map(|i| e[i].1.clone()))
                }
                Node::Internal { kids, .. } => at = Some(kids[route(&kids, key)].1),
            }
        }
        Ok(None)
    }

    pub fn get<D: Device>(
        &self,
        pool: &mut Pool<D>,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, TreeError> {
        Self::get_at(pool, self.root, key)
    }

    /// The entries with `lo <= key < hi` (either bound optional), in key order, from the tree rooted at `root`.
    pub fn range_at<D: Device>(
        pool: &mut Pool<D>,
        root: Option<Ptr>,
        lo: Option<&[u8]>,
        hi: Option<&[u8]>,
    ) -> Result<Entries, TreeError> {
        Self::range_limited(pool, root, lo, hi, usize::MAX)
    }

    /// As `range_at`, stopping after `limit` entries.
    pub fn range_limited<D: Device>(
        pool: &mut Pool<D>,
        root: Option<Ptr>,
        lo: Option<&[u8]>,
        hi: Option<&[u8]>,
        limit: usize,
    ) -> Result<Entries, TreeError> {
        let mut out = Vec::new();
        if let Some(p) = root {
            Self::range_node(pool, &p, lo, hi, limit, &mut out)?;
        }
        Ok(out)
    }

    pub fn range<D: Device>(
        &self,
        pool: &mut Pool<D>,
        lo: Option<&[u8]>,
        hi: Option<&[u8]>,
    ) -> Result<Entries, TreeError> {
        Self::range_at(pool, self.root, lo, hi)
    }

    fn range_node<D: Device>(
        pool: &mut Pool<D>,
        p: &Ptr,
        lo: Option<&[u8]>,
        hi: Option<&[u8]>,
        limit: usize,
        out: &mut Entries,
    ) -> Result<(), TreeError> {
        match pool.read_node(p)? {
            Node::Leaf(e) => {
                for (k, v) in e {
                    if out.len() >= limit {
                        break;
                    }
                    if lo.is_none_or(|l| k.as_slice() >= l) && hi.is_none_or(|h| k.as_slice() < h) {
                        out.push((k, v));
                    }
                }
            }
            Node::Internal { kids, .. } => {
                for i in 0..kids.len() {
                    if out.len() >= limit {
                        break;
                    }
                    // keys of child i are below the next bound: skip it when they all fall below `lo`
                    if let (Some(l), Some((nb, _))) = (lo, kids.get(i + 1)) {
                        if nb.as_slice() <= l {
                            continue;
                        }
                    }
                    // and when its own bound is already at or past `hi`
                    if i > 0 && hi.is_some_and(|h| kids[i].0.as_slice() >= h) {
                        break;
                    }
                    Self::range_node(pool, &kids[i].1, lo, hi, limit, out)?;
                }
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------------------------------------------ inserting

    /// Set `key` to `value`; returns the value it replaced.
    pub fn insert<D: Device>(
        &mut self,
        pool: &mut Pool<D>,
        key: &[u8],
        value: &[u8],
    ) -> Result<Option<Vec<u8>>, TreeError> {
        Self::check_kv(key, Some(value))?;
        let r = Self::ins(pool, self.root, key, value)?;
        self.root = Some(match r.split {
            None => r.left,
            Some((bound, right)) => pool.write_node(&Node::Internal {
                level: r.level + 1,
                kids: alloc::vec![(Vec::new(), r.left), (bound, right)],
            })?,
        });
        Ok(r.old)
    }

    fn ins<D: Device>(
        pool: &mut Pool<D>,
        at: Option<Ptr>,
        key: &[u8],
        value: &[u8],
    ) -> Result<Ins, TreeError> {
        let Some(p) = at else {
            let left = pool.write_node(&Node::Leaf(alloc::vec![(key.to_vec(), value.to_vec())]))?;
            return Ok(Ins {
                left,
                split: None,
                old: None,
                level: 0,
            });
        };
        match pool.read_node(&p)? {
            Node::Leaf(mut e) => {
                let old = match e.binary_search_by(|(k, _)| k.as_slice().cmp(key)) {
                    Ok(i) => Some(core::mem::replace(&mut e[i].1, value.to_vec())),
                    Err(i) => {
                        e.insert(i, (key.to_vec(), value.to_vec()));
                        None
                    }
                };
                let (left, split) = Self::write_leaf(pool, e)?;
                pool.retire(&p);
                Ok(Ins {
                    left,
                    split,
                    old,
                    level: 0,
                })
            }
            Node::Internal { level, mut kids } => {
                let i = route(&kids, key);
                let child = Self::ins(pool, Some(kids[i].1), key, value)?;
                kids[i].1 = child.left;
                if let Some((bound, right)) = child.split {
                    kids.insert(i + 1, (bound, right));
                }
                let (left, split) = Self::write_internal(pool, level, kids)?;
                pool.retire(&p);
                Ok(Ins {
                    left,
                    split,
                    old: child.old,
                    level,
                })
            }
        }
    }

    /// Write a leaf, or two if its entries do not fit one block; returns the left pointer and the right with its bound.
    fn write_leaf<D: Device>(
        pool: &mut Pool<D>,
        e: Vec<(Vec<u8>, Vec<u8>)>,
    ) -> Result<(Ptr, Split), TreeError> {
        let node = Node::Leaf(e);
        if node.size() <= CAP {
            return Ok((pool.write_node(&node)?, None));
        }
        let Node::Leaf(mut e) = node else {
            unreachable!()
        };
        let sizes: Vec<usize> = e.iter().map(|(k, v)| 4 + k.len() + v.len()).collect();
        let at = cut(&sizes);
        let right = e.split_off(at);
        let bound = right[0].0.clone();
        let l = pool.write_node(&Node::Leaf(e))?;
        let r = pool.write_node(&Node::Leaf(right))?;
        Ok((l, Some((bound, r))))
    }

    fn write_internal<D: Device>(
        pool: &mut Pool<D>,
        level: u8,
        kids: Vec<(Vec<u8>, Ptr)>,
    ) -> Result<(Ptr, Split), TreeError> {
        let node = Node::Internal { level, kids };
        if node.size() <= CAP {
            return Ok((pool.write_node(&node)?, None));
        }
        let Node::Internal { mut kids, .. } = node else {
            unreachable!()
        };
        let sizes: Vec<usize> = kids.iter().map(|(k, _)| 34 + k.len()).collect();
        let at = cut(&sizes);
        let right = kids.split_off(at);
        let bound = right[0].0.clone();
        let l = pool.write_node(&Node::Internal { level, kids })?;
        let r = pool.write_node(&Node::Internal { level, kids: right })?;
        Ok((l, Some((bound, r))))
    }

    // ------------------------------------------------------------------------------------------------------ deleting

    /// Remove `key`; returns the value it had.
    pub fn delete<D: Device>(
        &mut self,
        pool: &mut Pool<D>,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, TreeError> {
        Self::check_kv(key, None)?;
        let Some(root) = self.root else {
            return Ok(None);
        };
        let (d, old) = Self::del(pool, &root, key)?;
        match d {
            Del::Unchanged => {}
            Del::Empty => self.root = None,
            Del::Replaced(p) => self.root = Some(p),
        }
        // an internal root with one child is that child
        while let Some(r) = self.root {
            match pool.read_node(&r)? {
                Node::Internal { kids, .. } if kids.len() == 1 => {
                    pool.retire(&r);
                    self.root = Some(kids[0].1);
                }
                _ => break,
            }
        }
        Ok(old)
    }

    fn del<D: Device>(
        pool: &mut Pool<D>,
        p: &Ptr,
        key: &[u8],
    ) -> Result<(Del, Option<Vec<u8>>), TreeError> {
        match pool.read_node(p)? {
            Node::Leaf(mut e) => {
                let Ok(i) = e.binary_search_by(|(k, _)| k.as_slice().cmp(key)) else {
                    return Ok((Del::Unchanged, None));
                };
                let (_, old) = e.remove(i);
                pool.retire(p);
                if e.is_empty() {
                    return Ok((Del::Empty, Some(old)));
                }
                Ok((Del::Replaced(pool.write_node(&Node::Leaf(e))?), Some(old)))
            }
            Node::Internal { level, mut kids } => {
                let i = route(&kids, key);
                let (d, old) = Self::del(pool, &kids[i].1, key)?;
                match d {
                    Del::Unchanged => return Ok((Del::Unchanged, old)),
                    Del::Empty => {
                        kids.remove(i);
                    }
                    Del::Replaced(np) => {
                        kids[i].1 = np;
                        Self::merge_small(pool, &mut kids, i)?;
                    }
                }
                pool.retire(p);
                if kids.is_empty() {
                    return Ok((Del::Empty, old));
                }
                Ok((
                    Del::Replaced(pool.write_node(&Node::Internal { level, kids })?),
                    old,
                ))
            }
        }
    }

    /// If child `i` is small and fits together with a neighbour in one block, merge them (two blocks retired, one written).
    fn merge_small<D: Device>(
        pool: &mut Pool<D>,
        kids: &mut Vec<(Vec<u8>, Ptr)>,
        i: usize,
    ) -> Result<(), TreeError> {
        if kids.len() < 2 {
            return Ok(());
        }
        let node = pool.read_node(&kids[i].1)?;
        if node.size() >= CAP / 4 {
            return Ok(());
        }
        // merge with the right neighbour if there is one, else the left
        let (a, b) = if i + 1 < kids.len() {
            (i, i + 1)
        } else {
            (i - 1, i)
        };
        let (na, nb) = (pool.read_node(&kids[a].1)?, pool.read_node(&kids[b].1)?);
        if na.size() + nb.size() > CAP {
            return Ok(());
        }
        let merged = match (na, nb) {
            (Node::Leaf(mut x), Node::Leaf(y)) => {
                x.extend(y);
                Node::Leaf(x)
            }
            (Node::Internal { level, kids: mut x }, Node::Internal { kids: mut y, .. }) => {
                // the right half's first bound becomes the parent's separator for it
                y[0].0 = kids[b].0.clone();
                x.extend(y);
                Node::Internal { level, kids: x }
            }
            _ => return Err(TreeError::Structure("siblings of different kinds")),
        };
        // (an internal merge can grow by the parent's separator replacing a shorter first bound: check what is built)
        if merged.size() > CAP {
            return Ok(());
        }
        let p = pool.write_node(&merged)?;
        let (ra, rb) = (kids[a].1, kids[b].1);
        pool.retire(&ra);
        pool.retire(&rb);
        kids[a].1 = p;
        kids.remove(b);
        Ok(())
    }

    // ------------------------------------------------------------------------------------------------------ checking

    /// Walk the tree at `root` (committed at `gen`) and check every rule: every pointer's check field and header, node
    /// layout, keys in order and inside the bounds the parents give, all leaves at one depth, no empty node, no block born
    /// after `gen`.
    pub fn verify_at<D: Device>(
        pool: &mut Pool<D>,
        root: Option<Ptr>,
        gen: u64,
    ) -> Result<Verified, TreeError> {
        let mut v = Verified {
            entries: 0,
            nodes: 0,
            height: 0,
            reachable: Vec::new(),
        };
        if let Some(p) = root {
            let top = pool.read_node(&p)?;
            v.height = top.level();
            Self::verify_node(pool, &p, v.height, None, None, gen, &mut v)?;
        }
        v.reachable.sort_unstable();
        if v.reachable.windows(2).any(|w| w[0] == w[1]) {
            return Err(TreeError::Structure("a block is reached twice"));
        }
        Ok(v)
    }

    pub fn verify<D: Device>(&self, pool: &mut Pool<D>) -> Result<Verified, TreeError> {
        let gen = pool.open_gen;
        Self::verify_at(pool, self.root, gen)
    }

    fn verify_node<D: Device>(
        pool: &mut Pool<D>,
        p: &Ptr,
        level: u8,
        lo: Option<&[u8]>,
        hi: Option<&[u8]>,
        gen: u64,
        v: &mut Verified,
    ) -> Result<(), TreeError> {
        if p.gen > gen {
            return Err(TreeError::Structure("a block born after the snapshot"));
        }
        let node = pool.read_node(p)?;
        if node.level() != level {
            return Err(TreeError::Structure("a node at the wrong level"));
        }
        if node.is_empty() {
            return Err(TreeError::Structure("an empty node"));
        }
        v.nodes += 1;
        v.reachable.push(p.addr);
        match node {
            Node::Leaf(e) => {
                for (k, _) in &e {
                    if lo.is_some_and(|l| k.as_slice() < l) || hi.is_some_and(|h| k.as_slice() >= h)
                    {
                        return Err(TreeError::Structure("a key outside its parent's bounds"));
                    }
                }
                v.entries += e.len();
            }
            Node::Internal { kids, .. } => {
                for i in 0..kids.len() {
                    if i > 0 {
                        let b = kids[i].0.as_slice();
                        if lo.is_some_and(|l| b < l) || hi.is_some_and(|h| b >= h) {
                            return Err(TreeError::Structure(
                                "a bound outside its parent's bounds",
                            ));
                        }
                    }
                    let clo = if i == 0 {
                        lo
                    } else {
                        Some(kids[i].0.as_slice())
                    };
                    let chi = kids.get(i + 1).map(|(b, _)| b.as_slice()).or(hi);
                    Self::verify_node(pool, &kids[i].1, level - 1, clo, chi, gen, v)?;
                }
            }
        }
        Ok(())
    }
}
