//! The block pool: a device, a block allocator and the bookkeeping that keeps a block out of reuse while a retained
//! snapshot can still reach it (FS-SPEC section 3: "blocks freed in commit N are reused only when no retained superblock slot
//! can reach them"; invariant FI2).
//!
//! Generations: the tree's *open* generation is the one its next writes are born in; a commit closes it. A block born in
//! generation `b` and retired while generation `r` was open (the tree stopped using it) is reachable from exactly the
//! snapshots `s` with `b <= s < r`; it may be reused once none of those is retained.

use crate::device::{DevError, Device, BLOCK};
use crate::node::{Node, NodeError, Ptr};
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolError {
    Dev(DevError),
    Node(NodeError),
    /// No free block.
    NoSpace,
}

impl From<DevError> for PoolError {
    fn from(e: DevError) -> PoolError {
        PoolError::Dev(e)
    }
}

impl From<NodeError> for PoolError {
    fn from(e: NodeError) -> PoolError {
        PoolError::Node(e)
    }
}

#[derive(Debug, Clone, Copy)]
struct Retired {
    addr: u64,
    birth: u64,
    retired_in: u64,
}

pub struct Pool<D: Device> {
    pub dev: D,
    /// The tree these blocks belong to (in every header).
    pub owner: u64,
    /// The generation the next writes are born in.
    pub open_gen: u64,
    next: u64,
    /// One past the last block the pool may hand out.
    limit: u64,
    free: Vec<u64>,
    retired: Vec<Retired>,
    /// Retained snapshots: generation to how many hold it.
    live: BTreeMap<u64, u32>,
}

impl<D: Device> Pool<D> {
    /// A pool over `dev`; blocks `first..` are available (the blocks before are for the superblock rings, step 2).
    pub fn new(dev: D, owner: u64, first: u64) -> Pool<D> {
        let limit = dev.blocks();
        Pool {
            dev,
            owner,
            open_gen: 1,
            next: first,
            limit,
            free: Vec::new(),
            retired: Vec::new(),
            live: BTreeMap::new(),
        }
    }

    /// A pool for a mounted volume: blocks `first..limit` belong to it, `free` are the ones nothing reaches, and the next
    /// writes are born in generation `open_gen`.
    pub fn mounted(dev: D, owner: u64, open_gen: u64, limit: u64, mut free: Vec<u64>) -> Pool<D> {
        // the lowest address is handed out first
        free.sort_unstable_by(|a, b| b.cmp(a));
        Pool {
            dev,
            owner,
            open_gen,
            next: limit,
            limit,
            free,
            retired: Vec::new(),
            live: BTreeMap::new(),
        }
    }

    /// Replace the free list (mount: everything the newest root does not reach).
    pub fn set_free(&mut self, mut free: Vec<u64>) {
        free.sort_unstable_by(|a, b| b.cmp(a));
        self.free = free;
    }

    pub fn free_blocks(&self) -> usize {
        self.free.len()
    }

    pub fn retired_blocks(&self) -> usize {
        self.retired.len()
    }

    /// The addresses on the free list (test hook: none may be reachable from a retained snapshot).
    pub fn free_list(&self) -> &[u64] {
        &self.free
    }

    /// Blocks handed out so far (high-water mark of the bump allocator).
    pub fn high_water(&self) -> u64 {
        self.next
    }

    /// A snapshot committed at generation `gen` is retained (its blocks stay out of reuse) until `unpin`.
    pub fn pin(&mut self, gen: u64) {
        *self.live.entry(gen).or_insert(0) += 1;
    }

    pub fn unpin(&mut self, gen: u64) {
        if let Some(n) = self.live.get_mut(&gen) {
            *n -= 1;
            if *n == 0 {
                self.live.remove(&gen);
            }
        }
    }

    fn take(&mut self) -> Result<u64, PoolError> {
        if let Some(a) = self.free.pop() {
            return Ok(a);
        }
        if self.next >= self.limit {
            return Err(PoolError::NoSpace);
        }
        self.next += 1;
        Ok(self.next - 1)
    }

    /// Write `node` to a fresh block born in the open generation; returns its pointer.
    pub fn write_node(&mut self, node: &Node) -> Result<Ptr, PoolError> {
        let addr = self.take()?;
        let block = node.encode(addr, self.open_gen, self.owner);
        if let Err(e) = self.dev.write(addr, &block) {
            self.free.push(addr);
            return Err(e.into());
        }
        Ok(Ptr {
            addr,
            gen: self.open_gen,
            check: Ptr::check_of(&block),
        })
    }

    /// Read and verify the node `ptr` names.
    pub fn read_node(&mut self, ptr: &Ptr) -> Result<Node, PoolError> {
        let mut block = [0u8; BLOCK];
        self.dev.read(ptr.addr, &mut block)?;
        Ok(Node::decode(&block, ptr, self.owner)?)
    }

    /// The tree has stopped using `ptr`'s block as of the open generation.
    pub fn retire(&mut self, ptr: &Ptr) {
        self.retired.push(Retired {
            addr: ptr.addr,
            birth: ptr.gen,
            retired_in: self.open_gen,
        });
    }

    /// Move to the free list every retired block no retained snapshot can reach. Returns how many.
    pub fn reclaim(&mut self) -> usize {
        let live = &self.live;
        let mut freed = 0;
        let mut keep = Vec::new();
        for r in self.retired.drain(..) {
            if live.range(r.birth..r.retired_in).next().is_some() {
                keep.push(r);
            } else {
                self.free.push(r.addr);
                freed += 1;
            }
        }
        self.retired = keep;
        freed
    }
}
