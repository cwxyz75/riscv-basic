//! A volume: the tree plus the superblock rings (FS-SPEC section 3). `mkfs` writes an empty volume; `mount` finds the newest
//! superblock slot whose tree verifies and rebuilds the free map by walking that tree (reachability is the truth); `commit`
//! writes the new blocks, flushes, writes the next slot, flushes. `check` is the same walk as a report (`fsck.doofs`).
//!
//! Space is reused only after the commit that freed it is durable: the volume retains the last committed snapshot until the
//! next one is on disk, so a torn slot always leaves the previous root intact (FI2).

use crate::device::{DevError, Device, BLOCK};
use crate::node::Ptr;
use crate::pool::{Pool, PoolError};
use crate::superblock::{self as sb, SbError, Superblock, LABEL_MAX, MIN_BLOCKS, SLOTS};
use crate::tree::{Snapshot, Tree, TreeError, Verified};

/// Key type byte of an extent entry: its value is the 32-byte pointer of a data block (the layer above owns the rest of the
/// key). The volume must know it: the free map is what the tree and the extents do not reach.
pub const KEY_EXTENT: u8 = 3;

/// `Tree::verify_at`, plus the data blocks the extent entries point at (in `reachable`, sorted, each once).
pub fn verify_all<D: Device>(
    pool: &mut Pool<D>,
    root: Option<Ptr>,
    gen: u64,
) -> Result<Verified, TreeError> {
    let mut v = Tree::verify_at(pool, root, gen)?;
    for (_, val) in Tree::range_at(pool, root, Some(&[KEY_EXTENT]), Some(&[KEY_EXTENT + 1]))? {
        if val.len() != 32 {
            return Err(TreeError::Structure("an extent value is not a pointer"));
        }
        let p = Ptr::decode(&val);
        if p.gen > gen {
            return Err(TreeError::Structure("a data block born after the snapshot"));
        }
        v.reachable.push(p.addr);
    }
    v.reachable.sort_unstable();
    if v.reachable.windows(2).any(|w| w[0] == w[1]) {
        return Err(TreeError::Structure("a block is reached twice"));
    }
    Ok(v)
}
use alloc::vec::Vec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolError {
    Dev(DevError),
    Pool(PoolError),
    Tree(TreeError),
    /// The device is below the minimum size.
    TooSmall,
    LabelTooLong,
    /// No slot holds a superblock whose tree verifies.
    NoValidSuperblock,
    /// The device is not the size the volume was made for.
    SizeMismatch,
}

impl From<DevError> for VolError {
    fn from(e: DevError) -> VolError {
        VolError::Dev(e)
    }
}
impl From<PoolError> for VolError {
    fn from(e: PoolError) -> VolError {
        VolError::Pool(e)
    }
}
impl From<TreeError> for VolError {
    fn from(e: TreeError) -> VolError {
        VolError::Tree(e)
    }
}

pub struct Volume<D: Device> {
    pub pool: Pool<D>,
    pub tree: Tree,
    pub uuid: [u8; 16],
    pub label: [u8; LABEL_MAX],
    /// The last commit that is durable.
    pub last: Snapshot,
}

fn label_bytes(label: &str) -> Result<[u8; LABEL_MAX], VolError> {
    if label.len() > LABEL_MAX || label.bytes().any(|b| b == 0) {
        return Err(VolError::LabelTooLong);
    }
    let mut l = [0u8; LABEL_MAX];
    l[..label.len()].copy_from_slice(label.as_bytes());
    Ok(l)
}

/// Write an empty volume: every slot cleared, then generation 1 (an empty tree) in its slot.
pub fn mkfs<D: Device>(dev: &mut D, uuid: [u8; 16], label: &str) -> Result<(), VolError> {
    let blocks = dev.blocks();
    if blocks < MIN_BLOCKS {
        return Err(VolError::TooSmall);
    }
    let label = label_bytes(label)?;
    let zero = [0u8; BLOCK];
    for i in 0..SLOTS {
        dev.write(sb::slot_addr(i, blocks), &zero)?;
    }
    dev.flush()?;
    let s = Superblock {
        uuid,
        label,
        generation: 1,
        blocks,
        root: None,
    };
    dev.write(sb::slot_addr(1, blocks), &s.encode())?;
    dev.flush()?;
    Ok(())
}

/// What `check` found on one slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotReport {
    pub slot: u64,
    pub addr: u64,
    pub sb: Result<Superblock, SbError>,
    /// For a readable slot: did its tree verify (and how many entries and nodes).
    pub tree: Option<Result<(usize, usize), TreeError>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub slots: Vec<SlotReport>,
    /// The slot mount would use.
    pub chosen: Option<u64>,
    /// Blocks of the data region the chosen tree reaches / does not reach.
    pub reachable: u64,
    pub unreachable: u64,
    /// Problems beyond a torn slot (a tree that does not verify, a block outside the data region, mixed uuids...).
    pub problems: Vec<&'static str>,
}

fn owner(uuid: &[u8; 16]) -> u64 {
    sb::owner_of(uuid)
}

/// Read all slots, and walk each readable slot's tree. Changes nothing.
pub fn check<D: Device>(dev: &mut D) -> Report {
    let blocks = dev.blocks();
    let mut r = Report {
        slots: Vec::new(),
        chosen: None,
        reachable: 0,
        unreachable: 0,
        problems: Vec::new(),
    };
    if blocks < MIN_BLOCKS {
        r.problems.push("device too small");
        return r;
    }
    let (lo, hi) = sb::data_region(blocks);
    let mut best: Option<(u64, u64, Vec<u64>)> = None; // (generation, slot, reachable)
    let mut uuid0: Option<[u8; 16]> = None;
    for i in 0..SLOTS {
        let addr = sb::slot_addr(i, blocks);
        let mut b = [0u8; BLOCK];
        let sbr = match dev.read(addr, &mut b) {
            Ok(()) => Superblock::decode(&b),
            Err(_) => Err(SbError::Checksum),
        };
        let mut tree = None;
        if let Ok(s) = &sbr {
            if s.blocks != blocks {
                r.problems.push("a slot records another device size");
            }
            match uuid0 {
                None => uuid0 = Some(s.uuid),
                Some(u) if u != s.uuid => r.problems.push("slots of different volumes"),
                _ => {}
            }
            let mut pool =
                Pool::mounted(&mut *dev, owner(&s.uuid), s.generation + 1, hi, Vec::new());
            let v = verify_all(&mut pool, s.root, s.generation);
            tree = Some(match &v {
                Ok(v) => Ok((v.entries, v.nodes)),
                Err(e) => Err(*e),
            });
            if let Ok(v) = v {
                if v.reachable.iter().any(|&a| a < lo || a >= hi) {
                    r.problems
                        .push("a tree reaches a block outside the data region");
                } else if best.as_ref().is_none_or(|(g, _, _)| s.generation > *g) {
                    best = Some((s.generation, i, v.reachable));
                }
            }
        }
        r.slots.push(SlotReport {
            slot: i,
            addr,
            sb: sbr,
            tree,
        });
    }
    match best {
        Some((_, slot, reach)) => {
            r.chosen = Some(slot);
            r.reachable = reach.len() as u64;
            r.unreachable = (hi - lo) - r.reachable;
        }
        None => r.problems.push("no slot holds a verifying tree"),
    }
    r
}

/// Mount: the newest slot whose tree verifies; the free map is everything that tree does not reach.
pub fn mount<D: Device>(mut dev: D) -> Result<Volume<D>, VolError> {
    let blocks = dev.blocks();
    if blocks < MIN_BLOCKS {
        return Err(VolError::TooSmall);
    }
    let (lo, hi) = sb::data_region(blocks);
    let mut cands: Vec<Superblock> = Vec::new();
    for i in 0..SLOTS {
        let mut b = [0u8; BLOCK];
        if dev.read(sb::slot_addr(i, blocks), &mut b).is_ok() {
            if let Ok(s) = Superblock::decode(&b) {
                cands.push(s);
            }
        }
    }
    cands.sort_by_key(|s| core::cmp::Reverse(s.generation));
    let Some(newest) = cands.first().cloned() else {
        return Err(VolError::NoValidSuperblock);
    };
    if newest.blocks != blocks {
        return Err(VolError::SizeMismatch);
    }
    for s in cands
        .iter()
        .filter(|s| s.uuid == newest.uuid && s.blocks == blocks)
    {
        let mut pool = Pool::mounted(dev, owner(&s.uuid), s.generation + 1, hi, Vec::new());
        let walked = verify_all(&mut pool, s.root, s.generation);
        match walked {
            Ok(v) if v.reachable.iter().all(|&a| a >= lo && a < hi) => {
                // reachable is sorted: the free map is the gaps
                let mut free = Vec::new();
                let mut it = v.reachable.iter().peekable();
                for a in lo..hi {
                    if it.peek() == Some(&&a) {
                        it.next();
                    } else {
                        free.push(a);
                    }
                }
                pool.set_free(free);
                let last = Snapshot {
                    root: s.root,
                    gen: s.generation,
                };
                pool.pin(last.gen);
                return Ok(Volume {
                    pool,
                    tree: Tree::from_snapshot(&last),
                    uuid: s.uuid,
                    label: s.label,
                    last,
                });
            }
            _ => dev = pool.dev,
        }
    }
    Err(VolError::NoValidSuperblock)
}

impl<D: Device> Volume<D> {
    /// Give the device back (an unmount, or a crash in a test: anything uncommitted is gone).
    pub fn into_device(self) -> D {
        self.pool.dev
    }

    pub fn label(&self) -> &str {
        let n = self.label.iter().position(|&b| b == 0).unwrap_or(LABEL_MAX);
        core::str::from_utf8(&self.label[..n]).unwrap_or("")
    }

    /// True if the tree has changed since the last commit.
    pub fn dirty(&self) -> bool {
        self.tree.root() != self.last.root
    }

    /// Make the tree as it is now durable: flush the new blocks, write the next slot, flush. On error nothing is lost: the
    /// previous commit is still the newest slot, and a later `commit` retries.
    pub fn commit(&mut self) -> Result<(), VolError> {
        if !self.dirty() {
            return Ok(());
        }
        self.pool.dev.flush()?;
        let snap = self.tree.commit(&mut self.pool);
        let blocks = self.pool.dev.blocks();
        let s = Superblock {
            uuid: self.uuid,
            label: self.label,
            generation: snap.gen,
            blocks,
            root: snap.root,
        };
        let written = self
            .pool
            .dev
            .write(sb::slot_addr(snap.gen % SLOTS, blocks), &s.encode())
            .and_then(|()| self.pool.dev.flush());
        if let Err(e) = written {
            Tree::release(&mut self.pool, &snap);
            return Err(e.into());
        }
        Tree::release(&mut self.pool, &self.last);
        self.last = snap;
        self.pool.reclaim();
        Ok(())
    }
}
