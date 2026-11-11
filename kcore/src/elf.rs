//! The kernel's ELF loader front end: parse and *validate* a static RISC-V ELF64
//! executable into the segments to map (OD40). Pure and panic-free on any input: a
//! user program is hostile data. The policy is deliberately small:
//!
//! * `ET_EXEC`, 64-bit little-endian, `EM_RISCV`, version 1; no interpreter, no dynamic
//!   section (static binaries only).
//! * only `PT_LOAD` segments produce mappings; `PT_GNU_STACK` may not ask for an
//!   executable stack; other program headers are ignored.
//! * a segment must lie inside the user half, inside the file, with
//!   `filesz <= memsz`, and `vaddr` congruent to `offset` modulo the page size.
//! * permissions come from the segment flags: read is always granted, write and execute
//!   as flagged, **never both** (INVARIANTS M2).
//! * segments may not share a page (they would have to agree on permissions), and the
//!   entry point must lie in an executable segment.

use crate::frame::PAGE_SIZE;
use crate::pagetable::Perms;
use abi::user::{BASE, TOP};
use alloc::vec::Vec;

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;
const PT_GNU_STACK: u32 = 0x6474_e551;
const PF_X: u32 = 1;
const PF_W: u32 = 2;

const EHDR: usize = 64;
const PHDR: usize = 56;
/// Most program headers accepted.
pub const MAX_PHNUM: usize = 16;
/// Most bytes of memory all segments together may ask for.
pub const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfError {
    TooShort,
    BadMagic,
    NotElf64,
    NotLittleEndian,
    BadType,
    BadMachine,
    BadVersion,
    BadProgramHeaders,
    TooManySegments,
    Dynamic,
    ExecStack,
    SegmentOutsideFile,
    SegmentOutsideUser,
    SegmentMisaligned,
    BadSegmentSize,
    WriteExec,
    Overlap,
    /// A segment would overlap the stack the kernel maps for the process.
    OverlapsStack,
    TooBig,
    EntryNotExecutable,
    NoLoadSegments,
}

/// One segment to map: `memsz` bytes at `vaddr`, the first `filesz` of which come from
/// the file at `offset` (the rest is zero).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub vaddr: u64,
    pub memsz: u64,
    pub filesz: u64,
    pub offset: u64,
    /// The mapping's permissions, with the user bit set.
    pub perms: Perms,
}

impl Segment {
    /// The pages this segment occupies, as `first..end` page numbers.
    pub fn pages(&self) -> (u64, u64) {
        (
            self.vaddr / PAGE_SIZE,
            (self.vaddr + self.memsz).div_ceil(PAGE_SIZE),
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Image {
    pub entry: u64,
    pub segments: Vec<Segment>,
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}
fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(a)
}

pub fn parse(bytes: &[u8]) -> Result<Image, ElfError> {
    if bytes.len() < EHDR {
        return Err(ElfError::TooShort);
    }
    if &bytes[..4] != b"\x7fELF" {
        return Err(ElfError::BadMagic);
    }
    if bytes[4] != 2 {
        return Err(ElfError::NotElf64);
    }
    if bytes[5] != 1 {
        return Err(ElfError::NotLittleEndian);
    }
    if u16_at(bytes, 16) != 2 {
        return Err(ElfError::BadType); // ET_EXEC only
    }
    if u16_at(bytes, 18) != 243 {
        return Err(ElfError::BadMachine); // EM_RISCV
    }
    if u32_at(bytes, 20) != 1 || bytes[6] != 1 {
        return Err(ElfError::BadVersion);
    }
    let entry = u64_at(bytes, 24);
    let phoff = u64_at(bytes, 32);
    let phentsize = u16_at(bytes, 54) as usize;
    let phnum = u16_at(bytes, 56) as usize;
    if phentsize != PHDR {
        return Err(ElfError::BadProgramHeaders);
    }
    if phnum > MAX_PHNUM {
        return Err(ElfError::TooManySegments);
    }
    let table_end = (phnum as u64)
        .checked_mul(PHDR as u64)
        .and_then(|n| n.checked_add(phoff))
        .filter(|&e| e <= bytes.len() as u64)
        .ok_or(ElfError::BadProgramHeaders)?;
    let _ = table_end;
    let mut segments: Vec<Segment> = Vec::new();
    let mut total: u64 = 0;
    for i in 0..phnum {
        let at = phoff as usize + i * PHDR;
        let (kind, flags) = (u32_at(bytes, at), u32_at(bytes, at + 4));
        match kind {
            PT_INTERP | PT_DYNAMIC => return Err(ElfError::Dynamic),
            PT_GNU_STACK => {
                if flags & PF_X != 0 {
                    return Err(ElfError::ExecStack);
                }
            }
            PT_LOAD => {
                let (offset, vaddr) = (u64_at(bytes, at + 8), u64_at(bytes, at + 16));
                let (filesz, memsz) = (u64_at(bytes, at + 32), u64_at(bytes, at + 40));
                if memsz == 0 {
                    continue;
                }
                if filesz > memsz {
                    return Err(ElfError::BadSegmentSize);
                }
                let end = vaddr
                    .checked_add(memsz)
                    .ok_or(ElfError::SegmentOutsideUser)?;
                if vaddr < BASE || end > TOP {
                    return Err(ElfError::SegmentOutsideUser);
                }
                // The stack (and its guard page below) belongs to the kernel's process setup.
                let stack_lo = abi::user::STACK_TOP - (abi::user::STACK_PAGES + 1) * PAGE_SIZE;
                if vaddr < abi::user::STACK_TOP && end > stack_lo {
                    return Err(ElfError::OverlapsStack);
                }
                let file_end = offset
                    .checked_add(filesz)
                    .ok_or(ElfError::SegmentOutsideFile)?;
                if file_end > bytes.len() as u64 {
                    return Err(ElfError::SegmentOutsideFile);
                }
                if vaddr % PAGE_SIZE != offset % PAGE_SIZE {
                    return Err(ElfError::SegmentMisaligned);
                }
                let (w, x) = (flags & PF_W != 0, flags & PF_X != 0);
                if w && x {
                    return Err(ElfError::WriteExec);
                }
                total = total.saturating_add(memsz);
                if total > MAX_IMAGE_BYTES {
                    return Err(ElfError::TooBig);
                }
                let perms = Perms {
                    read: true,
                    write: w,
                    exec: x,
                    user: true,
                };
                segments.push(Segment {
                    vaddr,
                    memsz,
                    filesz,
                    offset,
                    perms,
                });
            }
            _ => {}
        }
    }
    if segments.is_empty() {
        return Err(ElfError::NoLoadSegments);
    }
    for (i, a) in segments.iter().enumerate() {
        let (a0, a1) = a.pages();
        for b in &segments[..i] {
            let (b0, b1) = b.pages();
            if a0 < b1 && b0 < a1 {
                return Err(ElfError::Overlap);
            }
        }
    }
    if !segments
        .iter()
        .any(|s| s.perms.exec && s.vaddr <= entry && entry < s.vaddr + s.memsz)
    {
        return Err(ElfError::EntryNotExecutable);
    }
    Ok(Image { entry, segments })
}
