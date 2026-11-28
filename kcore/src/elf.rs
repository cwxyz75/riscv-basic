//! The kernel's ELF loader front end: parse and *validate* a static RISC-V ELF64
//! executable into the segments to map (OD40). Pure and panic-free on any input: a
//! user program is hostile data. The policy is deliberately small:
//!
//! * `ET_EXEC` or `ET_DYN`, 64-bit little-endian, `EM_RISCV`, version 1; no interpreter. An `ET_EXEC` has no dynamic section. An `ET_DYN` is a
//!   **static-PIE** (HD2): link addresses start at 0, the image is placed at a base the loader chooses, and its dynamic section may hold only the
//!   `DT_RELA` family, whose entries are all `R_RISCV_RELATIVE` and name 8-byte words inside the file bytes of the image's segments, whichever the
//!   permissions: **the loader applies them** (`Image::relocate`) to its copy of the bytes before it maps them, so read-only data stays read-only and
//!   the program needs no startup code of its own (KERNEL-V1-SPEC amendment 2026-11-23). No `DT_NEEDED`, no symbol relocations, no `DT_REL`/`DT_JMPREL`.
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
    /// A static-PIE's relocation table is malformed, or holds a relocation that is not `R_RISCV_RELATIVE` on a word inside a segment's file bytes.
    Relocation,
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
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: vaddr and memsz were checked against the user limit when the segment was parsed
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
    /// A static-PIE (`ET_DYN`): `entry` and the segments' `vaddr` are offsets from a base the loader chooses.
    pub pie: bool,
    /// For a static-PIE, the bytes from the base to the end of its last segment, rounded up to a page (what one reservation must cover); 0 for `ET_EXEC`.
    pub span: u64,
    /// For a static-PIE, its `R_RISCV_RELATIVE` relocations (checked: each word lies inside one segment's file bytes).
    pub relocs: Vec<Reloc>,
}

/// One `R_RISCV_RELATIVE` relocation: the 8-byte word at link address `offset` becomes `base + addend`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reloc {
    pub offset: u64,
    pub addend: u64,
}

impl Image {
    /// Apply the relocations that fall in segment `seg`'s file bytes to `buf`, a copy of those `seg.filesz` bytes, for an image placed at `base`.
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: `parse` checked every word (8 bytes, offset..offset+8) to lie inside one segment's `vaddr..vaddr + filesz`; the slice is checked to be that long
    pub fn relocate(&self, seg: &Segment, buf: &mut [u8], base: u64) {
        if buf.len() as u64 != seg.filesz {
            return;
        }
        for r in &self.relocs {
            if r.offset >= seg.vaddr && r.offset + 8 <= seg.vaddr + seg.filesz {
                let at = (r.offset - seg.vaddr) as usize;
                buf[at..at + 8].copy_from_slice(&base.wrapping_add(r.addend).to_le_bytes());
            }
        }
    }
}

const DT_NULL: u64 = 0;
const DT_HASH: u64 = 4;
const DT_STRTAB: u64 = 5;
const DT_SYMTAB: u64 = 6;
const DT_RELA: u64 = 7;
const DT_RELASZ: u64 = 8;
const DT_RELAENT: u64 = 9;
const DT_STRSZ: u64 = 10;
const DT_SYMENT: u64 = 11;
const DT_DEBUG: u64 = 21;
const DT_TEXTREL: u64 = 22;
const DT_FLAGS: u64 = 30;
const DT_GNU_HASH: u64 = 0x6fff_fef5;
const DT_FLAGS_1: u64 = 0x6fff_fffb;
const DT_RELACOUNT: u64 = 0x6fff_fff9;
const R_RISCV_RELATIVE: u64 = 3;
/// Most bytes a static-PIE may span (the same bound as the segments together, plus what the gaps between them can cost).
pub const MAX_PIE_SPAN: u64 = 128 * 1024 * 1024;

#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: every caller checked `at + 2 <= b.len()` (the header and table sizes were checked against the file length first)
fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: every caller checked `at + 4 <= b.len()` first
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}
#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: every caller checked `at + 8 <= b.len()` first
fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(a)
}

#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
// INVARIANT: the file is hostile data: the header length (64 bytes) and every program header's range were checked against the file length before these reads, with checked_add on the offsets
#[allow(clippy::large_stack_frames)] // KT13: boot or test only, on a stack nothing else is under (the boot loader's images and the system library, read at boot)
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
    let pie = match u16_at(bytes, 16) {
        2 => false, // ET_EXEC
        3 => true,  // ET_DYN: a static-PIE
        _ => return Err(ElfError::BadType),
    };
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
    let mut dynamic: Option<(u64, u64)> = None; // (file offset, filesz) of a static-PIE's PT_DYNAMIC
    for i in 0..phnum {
        let at = phoff as usize + i * PHDR;
        let (kind, flags) = (u32_at(bytes, at), u32_at(bytes, at + 4));
        match kind {
            PT_INTERP => return Err(ElfError::Dynamic),
            PT_DYNAMIC => {
                if !pie || dynamic.is_some() {
                    return Err(ElfError::Dynamic);
                }
                let (offset, filesz) = (u64_at(bytes, at + 8), u64_at(bytes, at + 32));
                dynamic = Some((offset, filesz));
            }
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
                if pie {
                    // (offsets from the base: the span is what must fit the user half, checked below; the stack is the loader's to avoid)
                    if end > MAX_PIE_SPAN {
                        return Err(ElfError::SegmentOutsideUser);
                    }
                } else {
                    if vaddr < BASE || end > TOP {
                        return Err(ElfError::SegmentOutsideUser);
                    }
                    // The stack (and its guard page below) belongs to the kernel's process setup.
                    let stack_lo = abi::user::STACK_TOP - (abi::user::STACK_PAGES + 1) * PAGE_SIZE;
                    if vaddr < abi::user::STACK_TOP && end > stack_lo {
                        return Err(ElfError::OverlapsStack);
                    }
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
    let span = if pie {
        segments
            .iter()
            .map(|s| s.vaddr.saturating_add(s.memsz))
            .max()
            .unwrap_or(0)
            .next_multiple_of(PAGE_SIZE)
    } else {
        0
    };
    let relocs = match dynamic {
        Some((doff, dsz)) => check_relocations(bytes, &segments, doff, dsz, span)?,
        None => Vec::new(),
    };
    Ok(Image {
        entry,
        segments,
        pie,
        span,
        relocs,
    })
}

/// A static-PIE's dynamic section and relocation table (see the module comment): only the allowed tags, a table inside a loaded segment's file bytes, every
/// entry an `R_RISCV_RELATIVE` (symbol 0) on an 8-byte word inside a writable segment.
#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
// INVARIANT: every range is checked against the file length with checked_add before the reads; the loop counts below the table's checked size
#[allow(clippy::large_stack_frames)] // KT13: boot or test only, on a stack nothing else is under (as `parse`)
fn check_relocations(
    bytes: &[u8],
    segments: &[Segment],
    doff: u64,
    dsz: u64,
    span: u64,
) -> Result<Vec<Reloc>, ElfError> {
    let end = doff.checked_add(dsz).ok_or(ElfError::Dynamic)?;
    if end > bytes.len() as u64 || !dsz.is_multiple_of(16) {
        return Err(ElfError::Dynamic);
    }
    let (mut rela, mut relasz, mut relaent) = (None::<u64>, 0u64, 24u64);
    let mut terminated = false;
    for i in 0..(dsz / 16) as usize {
        let at = doff as usize + i * 16;
        let (tag, val) = (u64_at(bytes, at), u64_at(bytes, at + 8));
        match tag {
            DT_NULL => {
                terminated = true;
                break;
            }
            DT_RELA => rela = Some(val),
            DT_RELASZ => relasz = val,
            DT_RELAENT => relaent = val,
            // (tolerated and ignored: a static link may leave an empty symbol table and its hash, the flags, the count of relative relocations, an empty `DT_DEBUG` and `DT_TEXTREL`, which only says the loader writes relocations into read-only segments, as it does)
            DT_HASH | DT_GNU_HASH | DT_STRTAB | DT_SYMTAB | DT_STRSZ | DT_SYMENT | DT_FLAGS
            | DT_FLAGS_1 | DT_RELACOUNT | DT_DEBUG | DT_TEXTREL => {}
            _ => return Err(ElfError::Dynamic),
        }
    }
    if !terminated || relaent != 24 || !relasz.is_multiple_of(24) {
        return Err(ElfError::Dynamic);
    }
    let Some(rela) = rela.filter(|_| relasz > 0) else {
        return Ok(Vec::new()); // no relocations at all: a position-independent program that needs none
    };
    // the table lies in the file bytes of one loaded segment (its address is a link address: a vaddr)
    let table_end = rela.checked_add(relasz).ok_or(ElfError::Relocation)?;
    let seg = segments
        .iter()
        .find(|s| s.vaddr <= rela && table_end <= s.vaddr.saturating_add(s.filesz))
        .ok_or(ElfError::Relocation)?;
    let at0 = (seg.offset + (rela - seg.vaddr)) as usize; // (inside the file: the segment's file range was checked)
    let mut out = Vec::new();
    for i in 0..(relasz / 24) as usize {
        let at = at0 + i * 24;
        let (r_off, info, addend) = (
            u64_at(bytes, at),
            u64_at(bytes, at + 8),
            u64_at(bytes, at + 16),
        );
        if info != R_RISCV_RELATIVE {
            return Err(ElfError::Relocation); // (type 3 in the low word, symbol 0 in the high)
        }
        let word_end = r_off.checked_add(8).ok_or(ElfError::Relocation)?;
        // (the word must be copied from the file, so that the loader can patch its copy; the target must be inside the image)
        let in_file = r_off.is_multiple_of(8)
            && segments
                .iter()
                .any(|s| s.vaddr <= r_off && word_end <= s.vaddr + s.filesz);
        if !in_file || addend > span {
            return Err(ElfError::Relocation);
        }
        out.push(Reloc {
            offset: r_off,
            addend,
        });
    }
    Ok(out)
}
