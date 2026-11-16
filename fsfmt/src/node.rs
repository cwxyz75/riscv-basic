//! Pointers and tree nodes (FS-SPEC section 3).
//!
//! A **pointer** is 32 bytes: the block address, the generation the block was born in, and a 16-byte check field (the CRC-32C
//! of the whole block in its first four bytes, little-endian, the other twelve zero: sized for an AEAD tag later). A node is
//! one block: a 32-byte header (magic, kind, level, entry count, its own address, birth generation, owner) then packed
//! entries; the rest of the block is zero. Every read checks the pointer's check field and then that the header says what the
//! pointer said (a misdirected write is caught twice, FS-SPEC section 3).

use crate::crc::crc32c;
use crate::device::BLOCK;
use crate::{MAX_KEY, MAX_VALUE};
use alloc::vec::Vec;

pub const MAGIC: u32 = 0xD00F_5E01;
pub const HEADER: usize = 32;
/// Bytes of a block available to entries.
pub const CAP: usize = BLOCK - HEADER;
pub const PTR_LEN: usize = 32;

const KIND_LEAF: u8 = 1;
const KIND_INTERNAL: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ptr {
    pub addr: u64,
    pub gen: u64,
    pub check: [u8; 16],
}

impl Ptr {
    /// The check field of a block's bytes.
    pub fn check_of(block: &[u8; BLOCK]) -> [u8; 16] {
        let mut c = [0u8; 16];
        c[..4].copy_from_slice(&crc32c(block).to_le_bytes());
        c
    }

    pub fn encode(&self, out: &mut [u8]) {
        out[..8].copy_from_slice(&self.addr.to_le_bytes());
        out[8..16].copy_from_slice(&self.gen.to_le_bytes());
        out[16..32].copy_from_slice(&self.check);
    }

    pub fn decode(b: &[u8]) -> Ptr {
        let mut check = [0u8; 16];
        check.copy_from_slice(&b[16..32]);
        Ptr {
            addr: u64::from_le_bytes(b[..8].try_into().expect("8 bytes")),
            gen: u64::from_le_bytes(b[8..16].try_into().expect("8 bytes")),
            check,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeError {
    /// The block does not match the pointer's check field.
    Check,
    /// The header disagrees with the pointer or the tree (magic, address, generation, owner).
    Header,
    /// The entries are malformed: out of bounds, too long, not in key order, bytes left over.
    Layout,
}

/// One node in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    /// Sorted `(key, value)` entries.
    Leaf(Vec<(Vec<u8>, Vec<u8>)>),
    /// Sorted `(lower bound, child)` entries; `level` is the children's level plus one (1 above the leaves). Child `i` holds
    /// keys at or above its bound and below the next entry's; the first bound is not used for routing.
    Internal {
        level: u8,
        kids: Vec<(Vec<u8>, Ptr)>,
    },
}

impl Node {
    pub fn level(&self) -> u8 {
        match self {
            Node::Leaf(_) => 0,
            Node::Internal { level, .. } => *level,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Node::Leaf(e) => e.len(),
            Node::Internal { kids, .. } => kids.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes the entries take in a block.
    pub fn size(&self) -> usize {
        match self {
            Node::Leaf(e) => e.iter().map(|(k, v)| 4 + k.len() + v.len()).sum(),
            Node::Internal { kids, .. } => kids.iter().map(|(k, _)| 2 + PTR_LEN + k.len()).sum(),
        }
    }

    /// The first key in the node (a leaf's smallest key; an internal node's first bound).
    pub fn first_key(&self) -> Option<&[u8]> {
        match self {
            Node::Leaf(e) => e.first().map(|(k, _)| k.as_slice()),
            Node::Internal { kids, .. } => kids.first().map(|(k, _)| k.as_slice()),
        }
    }

    /// The block for this node, born in `gen`, stored at `addr`, owned by tree `owner`.
    pub fn encode(&self, addr: u64, gen: u64, owner: u64) -> [u8; BLOCK] {
        debug_assert!(self.size() <= CAP, "a node must fit its block");
        let mut b = [0u8; BLOCK];
        b[..4].copy_from_slice(&MAGIC.to_le_bytes());
        b[4] = match self {
            Node::Leaf(_) => KIND_LEAF,
            Node::Internal { .. } => KIND_INTERNAL,
        };
        b[5] = self.level();
        b[6..8].copy_from_slice(&(self.len() as u16).to_le_bytes());
        b[8..16].copy_from_slice(&addr.to_le_bytes());
        b[16..24].copy_from_slice(&gen.to_le_bytes());
        b[24..32].copy_from_slice(&owner.to_le_bytes());
        let mut at = HEADER;
        match self {
            Node::Leaf(entries) => {
                for (k, v) in entries {
                    b[at..at + 2].copy_from_slice(&(k.len() as u16).to_le_bytes());
                    b[at + 2..at + 4].copy_from_slice(&(v.len() as u16).to_le_bytes());
                    b[at + 4..at + 4 + k.len()].copy_from_slice(k);
                    b[at + 4 + k.len()..at + 4 + k.len() + v.len()].copy_from_slice(v);
                    at += 4 + k.len() + v.len();
                }
            }
            Node::Internal { kids, .. } => {
                for (k, p) in kids {
                    b[at..at + 2].copy_from_slice(&(k.len() as u16).to_le_bytes());
                    p.encode(&mut b[at + 2..at + 2 + PTR_LEN]);
                    b[at + 2 + PTR_LEN..at + 2 + PTR_LEN + k.len()].copy_from_slice(k);
                    at += 2 + PTR_LEN + k.len();
                }
            }
        }
        b
    }

    /// Check `block` against `ptr` and decode it. `owner` is the tree the pointer belongs to.
    pub fn decode(block: &[u8; BLOCK], ptr: &Ptr, owner: u64) -> Result<Node, NodeError> {
        if Ptr::check_of(block) != ptr.check {
            return Err(NodeError::Check);
        }
        let magic = u32::from_le_bytes(block[..4].try_into().expect("4"));
        let (kind, level) = (block[4], block[5]);
        let count = u16::from_le_bytes([block[6], block[7]]) as usize;
        let addr = u64::from_le_bytes(block[8..16].try_into().expect("8"));
        let gen = u64::from_le_bytes(block[16..24].try_into().expect("8"));
        let own = u64::from_le_bytes(block[24..32].try_into().expect("8"));
        if magic != MAGIC || addr != ptr.addr || gen != ptr.gen || own != owner {
            return Err(NodeError::Header);
        }
        let mut at = HEADER;
        let node = match (kind, level) {
            (KIND_LEAF, 0) => {
                let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
                for _ in 0..count {
                    let head = block.get(at..at + 4).ok_or(NodeError::Layout)?;
                    let (kl, vl) = (
                        u16::from_le_bytes([head[0], head[1]]) as usize,
                        u16::from_le_bytes([head[2], head[3]]) as usize,
                    );
                    if kl == 0 || kl > MAX_KEY || vl > MAX_VALUE {
                        return Err(NodeError::Layout);
                    }
                    let k = block.get(at + 4..at + 4 + kl).ok_or(NodeError::Layout)?;
                    let v = block
                        .get(at + 4 + kl..at + 4 + kl + vl)
                        .ok_or(NodeError::Layout)?;
                    if entries.last().is_some_and(|(p, _)| p.as_slice() >= k) {
                        return Err(NodeError::Layout);
                    }
                    entries.push((k.to_vec(), v.to_vec()));
                    at += 4 + kl + vl;
                }
                Node::Leaf(entries)
            }
            (KIND_INTERNAL, l) if l >= 1 => {
                let mut kids: Vec<(Vec<u8>, Ptr)> = Vec::new();
                for _ in 0..count {
                    let head = block.get(at..at + 2 + PTR_LEN).ok_or(NodeError::Layout)?;
                    let kl = u16::from_le_bytes([head[0], head[1]]) as usize;
                    if kl > MAX_KEY {
                        return Err(NodeError::Layout);
                    }
                    let p = Ptr::decode(&head[2..]);
                    let k = block
                        .get(at + 2 + PTR_LEN..at + 2 + PTR_LEN + kl)
                        .ok_or(NodeError::Layout)?;
                    // (bounds strictly increase from the second entry on; the first is not used for routing)
                    if kids.len() >= 2 && kids.last().is_some_and(|(p, _)| p.as_slice() >= k) {
                        return Err(NodeError::Layout);
                    }
                    kids.push((k.to_vec(), p));
                    at += 2 + PTR_LEN + kl;
                }
                Node::Internal { level: l, kids }
            }
            _ => return Err(NodeError::Header),
        };
        // nothing but zeros after the entries
        if block[at..].iter().any(|&x| x != 0) {
            return Err(NodeError::Layout);
        }
        Ok(node)
    }
}
