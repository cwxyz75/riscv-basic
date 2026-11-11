//! The boot image (S4-SPEC section 3): a flat archive of the user programs and fixture
//! files that the kernel turns into one VMO for `init`.
//!
//! Layout: `"BIMG"`, `count:u32`, then `count` entries of `name_len:u16, name, offset:u32,
//! size:u32`, then the data; every entry's data starts at a 16-byte aligned offset.

use crate::{rd_u16, rd_u32};
use alloc::vec::Vec;

pub const MAGIC: [u8; 4] = *b"BIMG";
pub const MAX_ENTRIES: usize = 256;
pub const MAX_NAME: usize = 255;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: Vec<u8>,
    pub offset: u32,
    pub size: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootImageError {
    BadMagic,
    Truncated,
    TooMany,
    BadName,
    DuplicateName,
    /// Data outside the image, before the table's end, or not 16-byte aligned.
    BadExtent,
    /// Building: a size does not fit 32 bits.
    TooBig,
}

/// Build an image from `(name, data)` pairs, in order.
pub fn build(files: &[(&[u8], &[u8])]) -> Result<Vec<u8>, BootImageError> {
    if files.len() > MAX_ENTRIES {
        return Err(BootImageError::TooMany);
    }
    let mut table = 8usize;
    for (name, _) in files {
        if name.is_empty() || name.len() > MAX_NAME {
            return Err(BootImageError::BadName);
        }
        table += 2 + name.len() + 8;
    }
    let mut offset = align16(table);
    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&(files.len() as u32).to_le_bytes());
    let mut offsets = Vec::new();
    for (name, data) in files {
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(name);
        let off = u32::try_from(offset).map_err(|_| BootImageError::TooBig)?;
        let size = u32::try_from(data.len()).map_err(|_| BootImageError::TooBig)?;
        out.extend_from_slice(&off.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        offsets.push(offset);
        offset = align16(offset + data.len());
    }
    for ((_, data), off) in files.iter().zip(offsets) {
        out.resize(off, 0);
        out.extend_from_slice(data);
    }
    out.resize(offset, 0);
    Ok(out)
}

fn align16(n: usize) -> usize {
    (n + 15) & !15
}

/// Parse and validate the table of an image.
pub fn parse(img: &[u8]) -> Result<Vec<Entry>, BootImageError> {
    if img.get(0..4).ok_or(BootImageError::Truncated)? != MAGIC {
        return Err(BootImageError::BadMagic);
    }
    let count = rd_u32(img, 4).ok_or(BootImageError::Truncated)? as usize;
    if count > MAX_ENTRIES {
        return Err(BootImageError::TooMany);
    }
    let mut at = 8usize;
    let mut entries: Vec<Entry> = Vec::new();
    for _ in 0..count {
        let n = rd_u16(img, at).ok_or(BootImageError::Truncated)? as usize;
        if n == 0 || n > MAX_NAME {
            return Err(BootImageError::BadName);
        }
        let name = img
            .get(at + 2..at + 2 + n)
            .ok_or(BootImageError::Truncated)?
            .to_vec();
        let offset = rd_u32(img, at + 2 + n).ok_or(BootImageError::Truncated)?;
        let size = rd_u32(img, at + 6 + n).ok_or(BootImageError::Truncated)?;
        at += 10 + n;
        if entries.iter().any(|e| e.name == name) {
            return Err(BootImageError::DuplicateName);
        }
        entries.push(Entry { name, offset, size });
    }
    for e in &entries {
        let end = (e.offset as usize)
            .checked_add(e.size as usize)
            .ok_or(BootImageError::BadExtent)?;
        if (e.offset as usize) < at || e.offset % 16 != 0 || end > img.len() {
            return Err(BootImageError::BadExtent);
        }
    }
    Ok(entries)
}

/// The bytes of the entry called `name`.
pub fn find<'a>(img: &'a [u8], entries: &[Entry], name: &[u8]) -> Option<&'a [u8]> {
    let e = entries.iter().find(|e| e.name == name)?;
    img.get(e.offset as usize..(e.offset as usize).checked_add(e.size as usize)?)
}
