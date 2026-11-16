//! A file (an image, or a block device) as an `fsfmt::Device`, and the small helpers the three tools share.

use fsfmt::{DevError, Device, BLOCK};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

pub struct FileDevice {
    f: File,
    blocks: u64,
}

impl FileDevice {
    pub fn open(path: &str, write: bool) -> std::io::Result<FileDevice> {
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(write)
            .open(path)?;
        let len = f.seek(SeekFrom::End(0))?;
        Ok(FileDevice {
            f,
            blocks: len / BLOCK as u64,
        })
    }

    /// Create (or truncate) an image of `blocks` blocks.
    pub fn create(path: &str, blocks: u64) -> std::io::Result<FileDevice> {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        f.set_len(blocks * BLOCK as u64)?;
        Ok(FileDevice { f, blocks })
    }
}

impl Device for FileDevice {
    fn blocks(&self) -> u64 {
        self.blocks
    }
    fn read(&mut self, addr: u64, buf: &mut [u8; BLOCK]) -> Result<(), DevError> {
        if addr >= self.blocks {
            return Err(DevError::Range);
        }
        self.f
            .seek(SeekFrom::Start(addr * BLOCK as u64))
            .map_err(|_| DevError::Io)?;
        self.f.read_exact(buf).map_err(|_| DevError::Io)
    }
    fn write(&mut self, addr: u64, data: &[u8; BLOCK]) -> Result<(), DevError> {
        if addr >= self.blocks {
            return Err(DevError::Range);
        }
        self.f
            .seek(SeekFrom::Start(addr * BLOCK as u64))
            .map_err(|_| DevError::Io)?;
        self.f.write_all(data).map_err(|_| DevError::Io)
    }
    fn flush(&mut self) -> Result<(), DevError> {
        self.f.sync_all().map_err(|_| DevError::Io)
    }
}

pub fn parse_uuid(s: &str) -> Option<[u8; 16]> {
    let h: String = s.chars().filter(|c| *c != '-').collect();
    if h.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(h.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// 16 random bytes from the host (a version-4 uuid).
pub fn random_uuid() -> [u8; 16] {
    let mut u = [0u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut u))
        .expect("/dev/urandom");
    u[6] = (u[6] & 0x0f) | 0x40;
    u[8] = (u[8] & 0x3f) | 0x80;
    u
}
