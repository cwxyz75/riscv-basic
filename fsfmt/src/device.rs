//! The block device the format sits on: whole 4 KiB blocks, addressed by block number.

use alloc::vec::Vec;

pub const BLOCK: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevError {
    /// A block number beyond the device.
    Range,
    /// The device reported an I/O error.
    Io,
}

pub trait Device {
    /// Number of blocks.
    fn blocks(&self) -> u64;
    fn read(&mut self, addr: u64, buf: &mut [u8; BLOCK]) -> Result<(), DevError>;
    fn write(&mut self, addr: u64, data: &[u8; BLOCK]) -> Result<(), DevError>;
    /// Everything written so far is durable before this returns (a write barrier).
    fn flush(&mut self) -> Result<(), DevError>;
}

impl<D: Device + ?Sized> Device for &mut D {
    fn blocks(&self) -> u64 {
        (**self).blocks()
    }
    fn read(&mut self, addr: u64, buf: &mut [u8; BLOCK]) -> Result<(), DevError> {
        (**self).read(addr, buf)
    }
    fn write(&mut self, addr: u64, data: &[u8; BLOCK]) -> Result<(), DevError> {
        (**self).write(addr, data)
    }
    fn flush(&mut self) -> Result<(), DevError> {
        (**self).flush()
    }
}

/// A device in memory. A block never written reads as zeros.
#[derive(Clone)]
pub struct MemDevice {
    blocks: Vec<Option<alloc::boxed::Box<[u8; BLOCK]>>>,
    /// Counts, for tests.
    pub reads: u64,
    pub writes: u64,
    pub flushes: u64,
}

impl MemDevice {
    pub fn new(blocks: u64) -> MemDevice {
        MemDevice {
            blocks: (0..blocks).map(|_| None).collect(),
            reads: 0,
            writes: 0,
            flushes: 0,
        }
    }

    /// The bytes of a block as stored (test hook: flip a bit, look at a block).
    pub fn raw(&self, addr: u64) -> Option<&[u8; BLOCK]> {
        self.blocks.get(addr as usize)?.as_deref()
    }

    pub fn raw_mut(&mut self, addr: u64) -> Option<&mut [u8; BLOCK]> {
        Some(
            &mut **self
                .blocks
                .get_mut(addr as usize)?
                .get_or_insert_with(|| alloc::boxed::Box::new([0; BLOCK])),
        )
    }
}

impl Device for MemDevice {
    fn blocks(&self) -> u64 {
        self.blocks.len() as u64
    }

    fn read(&mut self, addr: u64, buf: &mut [u8; BLOCK]) -> Result<(), DevError> {
        self.reads += 1;
        match self.blocks.get(addr as usize).ok_or(DevError::Range)? {
            Some(b) => buf.copy_from_slice(&**b),
            None => buf.fill(0),
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DevError> {
        self.flushes += 1;
        Ok(())
    }

    fn write(&mut self, addr: u64, data: &[u8; BLOCK]) -> Result<(), DevError> {
        self.writes += 1;
        let slot = self.blocks.get_mut(addr as usize).ok_or(DevError::Range)?;
        match slot {
            Some(b) => b.copy_from_slice(data),
            None => *slot = Some(alloc::boxed::Box::new(*data)),
        }
        Ok(())
    }
}
