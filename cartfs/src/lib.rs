//! The disk format behind `SAVE` / `LOAD` (see DESIGN.md, "The disk").
//!
//! An old-school floppy: a header, a free-sector map, a fixed-length directory,
//! then data sectors that chain to one another, so files go wherever there is
//! room and fragmentation never matters. Everything is 512-byte sectors over
//! the [`BlockDevice`] trait; the kernel supplies a virtio disk, tests supply
//! [`MemDisk`].
//!
//! ```text
//! sector 0      header: "RVDISK01", total sectors u32, directory entries u32
//! sector 1      free map: bit n (LSB first) set = sector n in use
//! sectors 2-5   directory: 64 entries of 32 bytes
//! sectors 6..   data: next-sector u16 (0 = last) + 510 payload bytes
//! entry         name[16] NUL padded | kind u8 (0 = free) | 0 | first u16 | len u32 | 8 zero
//! ```
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

pub const SECTOR: usize = 512;
pub const MAGIC: &[u8; 8] = b"RVDISK01";
pub const MAX_SECTORS: u32 = 4096;
pub const DIR_ENTRIES: usize = 64;
pub const NAME_MAX: usize = 16;
pub const MAX_FILE: usize = 65536;
const ENTRY: usize = 32;
const FIRST_DIR: u32 = 2;
const DIR_SECTORS: u32 = (DIR_ENTRIES * ENTRY / SECTOR) as u32;
const FIRST_DATA: u32 = FIRST_DIR + DIR_SECTORS;
const PAYLOAD: usize = SECTOR - 2;

/// What a file holds; stored as the directory's type byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Basic,
    Logo,
}

impl Kind {
    fn byte(self) -> u8 {
        match self {
            Kind::Basic => 1,
            Kind::Logo => 2,
        }
    }
    fn from_byte(b: u8) -> Option<Kind> {
        match b {
            1 => Some(Kind::Basic),
            2 => Some(Kind::Logo),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The device failed a read or write.
    Device,
    /// No valid header: the disk was never formatted (or is not ours).
    NotFormatted,
    /// The structures are inconsistent (bad chain, bad entry).
    Corrupt,
    NotFound,
    /// The name exists with the other kind.
    TypeMismatch,
    /// Empty, too long, or a character outside A-Z 0-9 . - _ (after upper-casing).
    BadName,
    /// No free sectors, or the directory is full.
    Full,
    /// Longer than [`MAX_FILE`].
    TooBig,
}

pub trait BlockDevice {
    fn sector_count(&self) -> u32;
    fn read(&mut self, sector: u32, buf: &mut [u8; SECTOR]) -> Result<(), Error>;
    fn write(&mut self, sector: u32, buf: &[u8; SECTOR]) -> Result<(), Error>;
}

/// A disk in memory, for tests and for building images on the host.
pub struct MemDisk {
    pub bytes: Vec<u8>,
}

impl MemDisk {
    pub fn new(sectors: u32) -> MemDisk {
        MemDisk {
            bytes: vec![0; sectors as usize * SECTOR],
        }
    }
}

impl BlockDevice for MemDisk {
    fn sector_count(&self) -> u32 {
        (self.bytes.len() / SECTOR) as u32
    }
    fn read(&mut self, sector: u32, buf: &mut [u8; SECTOR]) -> Result<(), Error> {
        let at = sector as usize * SECTOR;
        let src = self.bytes.get(at..at + SECTOR).ok_or(Error::Device)?;
        buf.copy_from_slice(src);
        Ok(())
    }
    fn write(&mut self, sector: u32, buf: &[u8; SECTOR]) -> Result<(), Error> {
        let at = sector as usize * SECTOR;
        let dst = self.bytes.get_mut(at..at + SECTOR).ok_or(Error::Device)?;
        dst.copy_from_slice(buf);
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub kind: Kind,
    pub len: u32,
}

pub struct Disk<D: BlockDevice> {
    dev: D,
    total: u32,
}

/// Upper-case and validate a file name.
pub fn canonical_name(name: &str) -> Result<String, Error> {
    if name.is_empty() || name.len() > NAME_MAX {
        return Err(Error::BadName);
    }
    let up = name.to_ascii_uppercase();
    if !up.bytes().all(|b| {
        b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'.' || b == b'-' || b == b'_'
    }) {
        return Err(Error::BadName);
    }
    Ok(up)
}

fn put_u16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}
fn put_u32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}
fn get_u16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn get_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

struct Raw {
    name: [u8; NAME_MAX],
    kind: u8,
    first: u16,
    len: u32,
}

impl Raw {
    fn name_str(&self) -> String {
        let n = self.name.iter().position(|&b| b == 0).unwrap_or(NAME_MAX);
        String::from_utf8_lossy(&self.name[..n]).into_owned()
    }
}

impl<D: BlockDevice> Disk<D> {
    /// Write an empty file system over the whole device.
    pub fn format(mut dev: D) -> Result<Disk<D>, Error> {
        let total = dev.sector_count();
        if !(FIRST_DATA + 1..=MAX_SECTORS).contains(&total) {
            return Err(Error::Device);
        }
        let zero = [0u8; SECTOR];
        for s in 0..total {
            dev.write(s, &zero)?;
        }
        let mut head = [0u8; SECTOR];
        head[..8].copy_from_slice(MAGIC);
        put_u32(&mut head, 8, total);
        put_u32(&mut head, 12, DIR_ENTRIES as u32);
        dev.write(0, &head)?;
        let mut disk = Disk { dev, total };
        let mut map = [0u8; SECTOR];
        for s in 0..FIRST_DATA {
            map[(s / 8) as usize] |= 1 << (s % 8);
        }
        // sectors past the end of the device count as in use, so they are never handed out
        for s in total..MAX_SECTORS {
            map[(s / 8) as usize] |= 1 << (s % 8);
        }
        disk.dev.write(1, &map)?;
        Ok(disk)
    }

    pub fn mount(mut dev: D) -> Result<Disk<D>, Error> {
        let mut head = [0u8; SECTOR];
        if dev.sector_count() < FIRST_DATA + 1 {
            return Err(Error::NotFormatted);
        }
        dev.read(0, &mut head)?;
        if &head[..8] != MAGIC {
            return Err(Error::NotFormatted);
        }
        let total = get_u32(&head, 8);
        if get_u32(&head, 12) != DIR_ENTRIES as u32
            || total > dev.sector_count()
            || !(FIRST_DATA + 1..=MAX_SECTORS).contains(&total)
        {
            return Err(Error::Corrupt);
        }
        Ok(Disk { dev, total })
    }

    pub fn into_device(self) -> D {
        self.dev
    }

    fn read_map(&mut self) -> Result<[u8; SECTOR], Error> {
        let mut map = [0u8; SECTOR];
        self.dev.read(1, &mut map)?;
        Ok(map)
    }

    fn read_dir(&mut self) -> Result<Vec<Option<Raw>>, Error> {
        let mut out = Vec::with_capacity(DIR_ENTRIES);
        let mut buf = [0u8; SECTOR];
        for s in 0..DIR_SECTORS {
            self.dev.read(FIRST_DIR + s, &mut buf)?;
            for e in buf.as_chunks::<ENTRY>().0 {
                if e[16] == 0 {
                    out.push(None);
                } else {
                    let mut name = [0u8; NAME_MAX];
                    name.copy_from_slice(&e[..NAME_MAX]);
                    out.push(Some(Raw {
                        name,
                        kind: e[16],
                        first: get_u16(e, 18),
                        len: get_u32(e, 20),
                    }));
                }
            }
        }
        Ok(out)
    }

    fn write_entry(&mut self, index: usize, raw: Option<&Raw>) -> Result<(), Error> {
        let per = SECTOR / ENTRY;
        let sector = FIRST_DIR + (index / per) as u32;
        let mut buf = [0u8; SECTOR];
        self.dev.read(sector, &mut buf)?;
        let at = (index % per) * ENTRY;
        let e = &mut buf[at..at + ENTRY];
        e.fill(0);
        if let Some(r) = raw {
            e[..NAME_MAX].copy_from_slice(&r.name);
            e[16] = r.kind;
            put_u16(e, 18, r.first);
            put_u32(e, 20, r.len);
        }
        self.dev.write(sector, &buf)
    }

    fn find(dir: &[Option<Raw>], name: &str) -> Option<usize> {
        dir.iter()
            .position(|e| e.as_ref().is_some_and(|r| r.name_str() == name))
    }

    /// The sectors of a file, following its chain; guards against loops.
    fn chain(&mut self, first: u16, len: u32) -> Result<Vec<u32>, Error> {
        let need = (len as usize).div_ceil(PAYLOAD).max(1);
        let mut sectors = Vec::with_capacity(need);
        let mut s = first as u32;
        let mut buf = [0u8; SECTOR];
        loop {
            if s < FIRST_DATA || s >= self.total || sectors.len() >= need {
                return Err(Error::Corrupt);
            }
            sectors.push(s);
            self.dev.read(s, &mut buf)?;
            let next = get_u16(&buf, 0);
            if next == 0 {
                break;
            }
            s = next as u32;
        }
        if sectors.len() != need {
            return Err(Error::Corrupt);
        }
        Ok(sectors)
    }

    pub fn dir(&mut self) -> Result<Vec<DirEntry>, Error> {
        let mut out = Vec::new();
        for r in self.read_dir()?.into_iter().flatten() {
            out.push(DirEntry {
                name: r.name_str(),
                kind: Kind::from_byte(r.kind).ok_or(Error::Corrupt)?,
                len: r.len,
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Sectors still available for data.
    pub fn free_sectors(&mut self) -> Result<u32, Error> {
        let map = self.read_map()?;
        Ok((FIRST_DATA..self.total)
            .filter(|s| map[(s / 8) as usize] & (1 << (s % 8)) == 0)
            .count() as u32)
    }

    pub fn load(&mut self, name: &str, kind: Kind) -> Result<Vec<u8>, Error> {
        let name = canonical_name(name)?;
        let dir = self.read_dir()?;
        let i = Self::find(&dir, &name).ok_or(Error::NotFound)?;
        let raw = dir[i].as_ref().ok_or(Error::Corrupt)?;
        if raw.kind != kind.byte() {
            return Err(Error::TypeMismatch);
        }
        let (first, len) = (raw.first, raw.len as usize);
        let sectors = self.chain(first, len as u32)?;
        let mut data = Vec::with_capacity(len);
        let mut buf = [0u8; SECTOR];
        for s in sectors {
            self.dev.read(s, &mut buf)?;
            let take = (len - data.len()).min(PAYLOAD);
            data.extend_from_slice(&buf[2..2 + take]);
        }
        Ok(data)
    }

    /// Store a file, replacing one of the same name and kind.
    pub fn save(&mut self, name: &str, kind: Kind, data: &[u8]) -> Result<(), Error> {
        let name = canonical_name(name)?;
        if data.len() > MAX_FILE {
            return Err(Error::TooBig);
        }
        let dir = self.read_dir()?;
        let old = Self::find(&dir, &name);
        if let Some(i) = old {
            if dir[i].as_ref().ok_or(Error::Corrupt)?.kind != kind.byte() {
                return Err(Error::TypeMismatch);
            }
        }
        let slot = match old {
            Some(i) => i,
            None => dir.iter().position(Option::is_none).ok_or(Error::Full)?,
        };
        let mut map = self.read_map()?;
        // the old copy's sectors become available to the new one
        if let Some(i) = old {
            let raw = dir[i].as_ref().ok_or(Error::Corrupt)?;
            for s in self.chain(raw.first, raw.len)? {
                map[(s / 8) as usize] &= !(1 << (s % 8));
            }
        }
        let need = data.len().div_ceil(PAYLOAD).max(1);
        let mut sectors = Vec::with_capacity(need);
        for s in FIRST_DATA..self.total {
            if sectors.len() == need {
                break;
            }
            if map[(s / 8) as usize] & (1 << (s % 8)) == 0 {
                sectors.push(s);
            }
        }
        if sectors.len() < need {
            return Err(Error::Full);
        }
        for &s in &sectors {
            map[(s / 8) as usize] |= 1 << (s % 8);
        }
        // data first, then the map, then the directory entry
        for (n, &s) in sectors.iter().enumerate() {
            let mut buf = [0u8; SECTOR];
            let next = sectors.get(n + 1).copied().unwrap_or(0);
            put_u16(&mut buf, 0, next as u16);
            let chunk = data.chunks(PAYLOAD).nth(n).unwrap_or(&[]);
            buf[2..2 + chunk.len()].copy_from_slice(chunk);
            self.dev.write(s, &buf)?;
        }
        self.dev.write(1, &map)?;
        let mut raw = Raw {
            name: [0; NAME_MAX],
            kind: kind.byte(),
            first: sectors[0] as u16,
            len: data.len() as u32,
        };
        raw.name[..name.len()].copy_from_slice(name.as_bytes());
        self.write_entry(slot, Some(&raw))
    }

    /// Remove a file of the given kind.
    pub fn delete(&mut self, name: &str, kind: Kind) -> Result<(), Error> {
        let name = canonical_name(name)?;
        let dir = self.read_dir()?;
        let i = Self::find(&dir, &name).ok_or(Error::NotFound)?;
        let raw = dir[i].as_ref().ok_or(Error::Corrupt)?;
        if raw.kind != kind.byte() {
            return Err(Error::TypeMismatch);
        }
        let sectors = self.chain(raw.first, raw.len)?;
        let mut map = self.read_map()?;
        for s in sectors {
            map[(s / 8) as usize] &= !(1 << (s % 8));
        }
        self.write_entry(i, None)?;
        self.dev.write(1, &map)
    }
}

#[cfg(test)]
mod tests;
