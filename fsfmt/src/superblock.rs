//! The superblock (FS-SPEC section 3): one block holding the volume's identity and the root of the newest commit. Eight
//! slots in two rings (four at the start of the device, four at the end) are written round robin; a slot torn by a crash
//! simply fails its checksum and mount takes the newest slot that verifies.

use crate::crc::crc32c;
use crate::device::BLOCK;
use crate::node::Ptr;

pub const MAGIC: &[u8; 8] = b"DOOFSSB1";
pub const VERSION: u32 = 1;
/// Check algorithm id: CRC-32C.
pub const CSUM_CRC32C: u32 = 1;
pub const RING: u64 = 4;
pub const SLOTS: u64 = 2 * RING;
pub const LABEL_MAX: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Superblock {
    pub uuid: [u8; 16],
    pub label: [u8; LABEL_MAX],
    /// The commit this slot records (1 is the empty volume `mkfs` writes).
    pub generation: u64,
    /// The device's size in blocks when the slot was written.
    pub blocks: u64,
    pub root: Option<Ptr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SbError {
    Magic,
    Version,
    Checksum,
    /// Fields that cannot be right (a label that is not UTF-8, a zero generation).
    Field,
}

/// The tree's owner id: it is in every node header, so a block of another volume is caught.
pub fn owner_of(uuid: &[u8; 16]) -> u64 {
    u64::from_le_bytes(uuid[..8].try_into().expect("8 bytes"))
        ^ u64::from_le_bytes(uuid[8..].try_into().expect("8 bytes"))
}

/// Block address of slot `i` (0 to 7) on a device of `blocks` blocks.
pub fn slot_addr(i: u64, blocks: u64) -> u64 {
    if i < RING {
        i
    } else {
        blocks - SLOTS + i
    }
}

/// The first and one-past-last block of the area trees may use.
pub fn data_region(blocks: u64) -> (u64, u64) {
    (RING, blocks - RING)
}

/// Smallest device a volume fits on.
pub const MIN_BLOCKS: u64 = 2 * RING + 8;

impl Superblock {
    pub fn label_str(&self) -> &str {
        let n = self.label.iter().position(|&b| b == 0).unwrap_or(LABEL_MAX);
        core::str::from_utf8(&self.label[..n]).unwrap_or("")
    }

    pub fn encode(&self) -> [u8; BLOCK] {
        let mut b = [0u8; BLOCK];
        b[..8].copy_from_slice(MAGIC);
        b[8..12].copy_from_slice(&VERSION.to_le_bytes());
        b[12..16].copy_from_slice(&CSUM_CRC32C.to_le_bytes());
        b[16..32].copy_from_slice(&self.uuid);
        b[32..64].copy_from_slice(&self.label);
        b[64..72].copy_from_slice(&self.generation.to_le_bytes());
        b[72..80].copy_from_slice(&self.blocks.to_le_bytes());
        match &self.root {
            Some(p) => {
                b[80] = 1;
                p.encode(&mut b[88..120]);
            }
            None => b[80] = 0,
        }
        let c = crc32c(&b[..BLOCK - 4]);
        b[BLOCK - 4..].copy_from_slice(&c.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8; BLOCK]) -> Result<Superblock, SbError> {
        if &b[..8] != MAGIC {
            return Err(SbError::Magic);
        }
        let stored = u32::from_le_bytes(b[BLOCK - 4..].try_into().expect("4 bytes"));
        if crc32c(&b[..BLOCK - 4]) != stored {
            return Err(SbError::Checksum);
        }
        let ver = u32::from_le_bytes(b[8..12].try_into().expect("4 bytes"));
        let alg = u32::from_le_bytes(b[12..16].try_into().expect("4 bytes"));
        if ver != VERSION || alg != CSUM_CRC32C {
            return Err(SbError::Version);
        }
        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(&b[16..32]);
        let mut label = [0u8; LABEL_MAX];
        label.copy_from_slice(&b[32..64]);
        let generation = u64::from_le_bytes(b[64..72].try_into().expect("8 bytes"));
        let blocks = u64::from_le_bytes(b[72..80].try_into().expect("8 bytes"));
        let root = match b[80] {
            0 => None,
            1 => Some(Ptr::decode(&b[88..120])),
            _ => return Err(SbError::Field),
        };
        let n = label.iter().position(|&c| c == 0).unwrap_or(LABEL_MAX);
        if generation == 0
            || core::str::from_utf8(&label[..n]).is_err()
            || label[n..].iter().any(|&c| c != 0)
            || b[81..88].iter().any(|&c| c != 0)
            || b[120..BLOCK - 4].iter().any(|&c| c != 0)
        {
            return Err(SbError::Field);
        }
        Ok(Superblock {
            uuid,
            label,
            generation,
            blocks,
            root,
        })
    }
}
