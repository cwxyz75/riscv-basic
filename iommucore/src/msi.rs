//! MSI translation (IOMMU-SPEC 8.2): the address check and the interrupt-file number (the IOMMU specification's "Process to
//! translate addresses of MSIs"), the flat MSI page table, its 16-byte entries, and the 512-byte memory-resident interrupt file
//! (MRIF) of the AIA specification.
//!
//! An MSI page-table entry has two constructors, **invalid** and **mrif**: there is no basic-translate entry (IO11), except behind
//! the negative-control feature `fault-msi-basic`, which no real build enables. An MRIF entry's notice page and identity are both
//! in its second doubleword, so a move rewrites them with one store (A16; QEMU `riscv-iommu-bits.h` lines 466 to 469).

use crate::{ppn_addr, ppn_field, PhysMem64, PAGE_SHIFT, PAGE_SIZE};

/// The specification's bit-extract function (QEMU `riscv_iommu_pext_u64`): the bits of `val` at the positions set in `mask`,
/// packed into the low bits of the result in order.
pub const fn pext(val: u64, mask: u64) -> u64 {
    let mut ret = 0u64;
    let mut out = 1u64;
    let mut val = val;
    let mut mask = mask;
    while mask != 0 {
        if mask & 1 != 0 {
            if val & 1 != 0 {
                ret |= out;
            }
            out = out.wrapping_shl(1);
        }
        val >>= 1;
        mask >>= 1;
    }
    ret
}

/// The MSI window of a device context: `msi_addr_mask` and `msi_addr_pattern`, both in page-number units. An address whose page
/// number agrees with the pattern outside the mask is an MSI; the mask's bits of the page number select the interrupt file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsiWindow {
    mask: u64,
    pattern: u64,
}

/// The mask and pattern fields are 52 bits wide.
const WINDOW_FIELD: u64 = (1 << 52) - 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsiError {
    /// An address is not aligned as the format needs (the window's base to its size, a table to its page, a file to 512 bytes).
    Misaligned,
    /// The window has more files than one table page holds (256), or an index is outside the table.
    OutOfRange,
    /// A notice identity of 0 or above 2047.
    BadIdentity,
}

impl MsiWindow {
    /// The window of `2^files_log2` consecutive pages starting at `base_pa`, which must be aligned to the window's size: page `k`
    /// of it selects file `k` (IOMMU-SPEC 3.3, 8.2: `IO_BASE`, 256 pages).
    pub const fn new(base_pa: u64, files_log2: u32) -> Result<MsiWindow, MsiError> {
        if files_log2 > 8 {
            return Err(MsiError::OutOfRange);
        }
        let mask = (1u64 << files_log2).wrapping_sub(1);
        let base_page = base_pa >> PAGE_SHIFT;
        if base_pa & (PAGE_SIZE - 1) != 0 || base_page & mask != 0 || base_page > WINDOW_FIELD {
            return Err(MsiError::Misaligned);
        }
        Ok(MsiWindow {
            mask,
            pattern: base_page,
        })
    }
    pub const fn mask(&self) -> u64 {
        self.mask
    }
    pub const fn pattern(&self) -> u64 {
        self.pattern
    }
    /// The number of interrupt files (entries of the table).
    pub const fn files(&self) -> u64 {
        pext(u64::MAX, self.mask).wrapping_add(1)
    }
    /// `addr` is in the window: `(page ^ pattern) & !mask == 0` (QEMU `riscv_iommu_msi_check`).
    pub const fn matches(&self, addr: u64) -> bool {
        ((addr >> PAGE_SHIFT) ^ self.pattern) & !self.mask == 0
    }
    /// The interrupt-file number of `addr`: the mask's bits of its page number.
    pub const fn file_number(&self, addr: u64) -> u64 {
        pext(addr >> PAGE_SHIFT, self.mask)
    }
}

const PTE_V: u64 = 1 << 0;
const PTE_M_SHIFT: u32 = 1;
const PTE_M_MRIF: u64 = 1;
#[cfg(any(test, feature = "fault-msi-basic"))]
const PTE_M_BASIC: u64 = 3;
const MRIF_ADDR_SHIFT: u32 = 7; // MRIF address bits 55:9 in bits 53:7
const NID_LOW: u64 = 0x3ff; // bits 9:0
const NID_MSB_SHIFT: u32 = 60;

/// The highest interrupt identity an interrupt file holds.
pub const MAX_IDENTITY: u16 = 2047;

/// One MSI page-table entry (two doublewords).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsiPte {
    words: [u64; 2],
}

impl MsiPte {
    pub const fn invalid() -> MsiPte {
        MsiPte { words: [0, 0] }
    }

    /// An MRIF-mode entry: the device's message sets a pending bit in the file at `file_pa` (512-byte aligned) and, if that
    /// identity is enabled there, the IOMMU sends identity `notice_id` (1 to 2047) to the interrupt-file page at `notice_pa`.
    pub const fn mrif(file_pa: u64, notice_pa: u64, notice_id: u16) -> Result<MsiPte, MsiError> {
        if file_pa & (MRIF_BYTES - 1) != 0
            || notice_pa & (PAGE_SIZE - 1) != 0
            || file_pa >> 56 != 0
            || notice_pa >> 56 != 0
        {
            return Err(MsiError::Misaligned);
        }
        if notice_id == 0 || notice_id > MAX_IDENTITY {
            return Err(MsiError::BadIdentity);
        }
        let id = notice_id as u64;
        Ok(MsiPte {
            words: [
                ((file_pa >> 9) << MRIF_ADDR_SHIFT) | (PTE_M_MRIF << PTE_M_SHIFT) | PTE_V,
                ((id >> 10) << NID_MSB_SHIFT) | ppn_field(notice_pa) | (id & NID_LOW),
            ],
        })
    }

    /// A basic-translate entry, which forwards the device's message to the page at `target_pa` with the device's own data word:
    /// the forbidden state of IO11, for the negative-control kernel `fault-iommu-msi-basic` only.
    #[cfg(any(test, feature = "fault-msi-basic"))]
    pub const fn basic(target_pa: u64) -> MsiPte {
        MsiPte {
            words: [
                ppn_field(target_pa) | (PTE_M_BASIC << PTE_M_SHIFT) | PTE_V,
                0,
            ],
        }
    }

    pub const fn words(&self) -> [u64; 2] {
        self.words
    }
    pub const fn is_valid(&self) -> bool {
        self.words[0] & PTE_V != 0
    }
    /// The notice target of an MRIF entry: (page address, identity).
    pub const fn notice(&self) -> (u64, u16) {
        let w = self.words[1];
        (
            ppn_addr(w),
            ((w & NID_LOW) | (((w >> NID_MSB_SHIFT) & 1) << 10)) as u16,
        )
    }
}

/// A flat MSI page table: one page, one 16-byte entry per interrupt file of its window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsiTable {
    root: u64,
    window: MsiWindow,
}

impl MsiTable {
    /// A table in the page at `root_pa` for `window`; every entry is made invalid.
    pub fn new<M: PhysMem64>(
        mem: &M,
        root_pa: u64,
        window: MsiWindow,
    ) -> Result<MsiTable, MsiError> {
        if root_pa & (PAGE_SIZE - 1) != 0 || root_pa >> 56 != 0 {
            return Err(MsiError::Misaligned);
        }
        let t = MsiTable {
            root: root_pa,
            window,
        };
        for i in 0..window.files() {
            t.write_words(mem, i, [0, 0]);
        }
        Ok(t)
    }
    pub const fn root(&self) -> u64 {
        self.root
    }
    pub const fn window(&self) -> MsiWindow {
        self.window
    }

    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: callers check index < files <= 256, so the offset is below 4 KiB
    const fn entry_addr(&self, index: u64) -> u64 {
        self.root + index * 16
    }

    fn write_words<M: PhysMem64>(&self, mem: &M, index: u64, words: [u64; 2]) {
        let at = self.entry_addr(index);
        mem.write(at, words[0]);
        mem.write(at.wrapping_add(8), words[1]);
    }

    /// The entry for file `index`.
    pub fn get<M: PhysMem64>(&self, mem: &M, index: u64) -> Result<MsiPte, MsiError> {
        if index >= self.window.files() {
            return Err(MsiError::OutOfRange);
        }
        let at = self.entry_addr(index);
        Ok(MsiPte {
            words: [mem.read(at), mem.read(at.wrapping_add(8))],
        })
    }

    /// Write file `index`'s entry. A valid entry is written notice word first, then the word with the valid bit; an entry made
    /// invalid has its valid bit cleared first. When only the notice target changes (a move: IOMMU-SPEC 8.5) the entry is
    /// rewritten with **one** store of its second doubleword. When a valid entry's file changes, it is made invalid first, so
    /// the IOMMU never combines the new file with the old notice. (Every change still needs the invalidation and fence of 6.2.)
    pub fn set<M: PhysMem64>(&self, mem: &M, index: u64, entry: MsiPte) -> Result<(), MsiError> {
        let old = self.get(mem, index)?;
        let at = self.entry_addr(index);
        let [w0, w1] = entry.words;
        if !entry.is_valid() {
            mem.write(at, w0);
            mem.write(at.wrapping_add(8), w1);
            return Ok(());
        }
        if old.is_valid() && old.words[0] == w0 {
            mem.write(at.wrapping_add(8), w1);
            return Ok(());
        }
        if old.is_valid() {
            mem.write(at, 0);
        }
        mem.write(at.wrapping_add(8), w1);
        mem.write(at, w0);
        Ok(())
    }

    /// Make file `index`'s entry invalid.
    pub fn clear<M: PhysMem64>(&self, mem: &M, index: u64) -> Result<(), MsiError> {
        self.set(mem, index, MsiPte::invalid())
    }
}

/// The size of a memory-resident interrupt file: 32 pairs of (pending, enable) doublewords, for identities 0 to 2047.
pub const MRIF_BYTES: u64 = 512;

/// A memory-resident interrupt file (the AIA specification's format): for identities `64 k` to `64 k + 63`, the pending bits at
/// offset `16 k` and the enable bits at `16 k + 8`. Every change is an atomic operation on one doubleword, made from any hart and
/// without a lock while the IOMMU may write the file (IOMMU-SPEC 8.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mrif {
    pa: u64,
}

impl Mrif {
    /// The file at `pa`, which must be 512-byte aligned.
    pub const fn new(pa: u64) -> Result<Mrif, MsiError> {
        if pa & (MRIF_BYTES - 1) != 0 {
            return Err(MsiError::Misaligned);
        }
        Ok(Mrif { pa })
    }
    pub const fn pa(&self) -> u64 {
        self.pa
    }

    /// The pending doubleword of `id` and its bit.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: id <= 2047, so the offset is at most 0x1f0
    const fn pending_word(&self, id: u16) -> Result<(u64, u64), MsiError> {
        if id > MAX_IDENTITY {
            return Err(MsiError::BadIdentity);
        }
        Ok((self.pa + (id as u64 / 64) * 16, 1 << (id % 64)))
    }

    /// Clear every pending and enable bit (before the file is first named by an entry).
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: i < 64, so the offset is below 512
    pub fn reset<M: PhysMem64>(&self, mem: &M) {
        for i in 0..MRIF_BYTES / 8 {
            mem.write(self.pa + i * 8, 0);
        }
    }

    /// Set `id`'s enable bit: a message with that data word will send the notice.
    pub fn enable<M: PhysMem64>(&self, mem: &M, id: u16) -> Result<(), MsiError> {
        let (p, bit) = self.pending_word(id)?;
        mem.fetch_or(p.wrapping_add(8), bit);
        Ok(())
    }

    /// Clear `id`'s enable bit (the mask of IOMMU-SPEC 8.5).
    pub fn disable<M: PhysMem64>(&self, mem: &M, id: u16) -> Result<(), MsiError> {
        let (p, bit) = self.pending_word(id)?;
        mem.fetch_and(p.wrapping_add(8), !bit);
        Ok(())
    }

    pub fn is_enabled<M: PhysMem64>(&self, mem: &M, id: u16) -> Result<bool, MsiError> {
        let (p, bit) = self.pending_word(id)?;
        Ok(mem.read(p.wrapping_add(8)) & bit != 0)
    }

    /// Read and clear `id`'s pending bit in one atomic operation; whether it was set.
    pub fn take_pending<M: PhysMem64>(&self, mem: &M, id: u16) -> Result<bool, MsiError> {
        let (p, bit) = self.pending_word(id)?;
        Ok(mem.fetch_and(p, !bit) & bit != 0)
    }

    pub fn is_pending<M: PhysMem64>(&self, mem: &M, id: u16) -> Result<bool, MsiError> {
        let (p, bit) = self.pending_word(id)?;
        Ok(mem.read(p) & bit != 0)
    }

    /// Take every pending bit whose enable bit is clear (the stray messages of IOMMU-SPEC 8.4: a data word other than the one the
    /// kernel enabled) and return them, one bit per identity: word `k` holds identities `64 k` to `64 k + 63`.
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: k < 32, so the offset is below 512
    pub fn stray<M: PhysMem64>(&self, mem: &M) -> [u64; 32] {
        let mut out = [0u64; 32];
        for (k, word) in out.iter_mut().enumerate() {
            let at = self.pa + k as u64 * 16;
            let enabled = mem.read(at + 8);
            if mem.read(at) & !enabled == 0 {
                continue;
            }
            *word = mem.fetch_and(at, enabled) & !enabled;
        }
        out
    }
}
