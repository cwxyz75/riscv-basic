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
    /// Two entries share bytes.
    Overlap,
}

/// An image name is a relative path of non-empty components, none of them `.` or `..`, with
/// no NUL (memfsd builds `/bin` and `/home` from these names).
fn valid_name(n: &[u8]) -> bool {
    !n.is_empty()
        && n.len() <= MAX_NAME
        && !n.contains(&0)
        && n.split(|&b| b == b'/')
            .all(|c| !c.is_empty() && c != b"." && c != b"..")
}

/// Build an image from `(name, data)` pairs, in order.
pub fn build(files: &[(&[u8], &[u8])]) -> Result<Vec<u8>, BootImageError> {
    if files.len() > MAX_ENTRIES {
        return Err(BootImageError::TooMany);
    }
    let mut table = 8usize;
    for (name, _) in files {
        if !valid_name(name) {
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
    parse_prefix(img, img.len())
}

/// Like [`parse`] when only a *prefix* of the image is in memory (at least the whole table) and
/// the image is `total_len` bytes long: the extents are checked against `total_len`.
pub fn parse_prefix(img: &[u8], total_len: usize) -> Result<Vec<Entry>, BootImageError> {
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
        if !valid_name(&name) {
            return Err(BootImageError::BadName);
        }
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
        if (e.offset as usize) < at || e.offset % 16 != 0 || end > total_len {
            return Err(BootImageError::BadExtent);
        }
    }
    // No two non-empty extents may share bytes.
    let mut spans: Vec<(u32, u32)> = entries
        .iter()
        .filter(|e| e.size > 0)
        .map(|e| (e.offset, e.offset + e.size))
        .collect();
    spans.sort();
    if spans.windows(2).any(|w| w[0].1 > w[1].0) {
        return Err(BootImageError::Overlap);
    }
    Ok(entries)
}

/// The bytes of the entry called `name`.
pub fn find<'a>(img: &'a [u8], entries: &[Entry], name: &[u8]) -> Option<&'a [u8]> {
    let e = entries.iter().find(|e| e.name == name)?;
    img.get(e.offset as usize..(e.offset as usize).checked_add(e.size as usize)?)
}
