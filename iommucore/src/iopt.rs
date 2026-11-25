//! I/O page tables (IOMMU-SPEC 3.2): Sv39, Sv48 and Sv57 for the first stage and Sv39x4, Sv48x4 and Sv57x4 for the second
//! (whose root is four pages, 16 KiB, and indexes two more address bits), 4 KiB leaves only.
//!
//! Every leaf is written with A and D set (the device does not set them: IOMMU0 item 3, and the context leaves hardware updates
//! off) and with U set: a transaction without a process id is a user-mode access for the first stage, and the second stage treats
//! every access as not requesting supervisor privilege, so a leaf without U faults in either (IOMMU0 item 3; QEMU
//! `riscv_iommu_spa_fetch`). Tables are freed as soon as they are empty, so a sequence of maps and unmaps returns every frame;
//! `unmap` and `destroy` never ask [`TableFrames`] for a frame.
//!
//! The shape follows `kcore::pagetable` (which is Sv39 only and generic over the CPU's formats); the code is not shared, because
//! the leaf bits and the root size differ and this crate does not depend on `kcore`.

use crate::ddt::TableRoot;
use crate::{ppn_addr, ppn_field, PhysMem64, TableFrames, PAGE_SHIFT, PAGE_SIZE};

/// A page-table format: the stage and the number of levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtMode {
    Sv39,
    Sv48,
    Sv57,
    Sv39x4,
    Sv48x4,
    Sv57x4,
}

impl PtMode {
    /// A second-stage (`iohgatp`) format.
    pub const fn second_stage(self) -> bool {
        matches!(self, PtMode::Sv39x4 | PtMode::Sv48x4 | PtMode::Sv57x4)
    }
    pub const fn levels(self) -> u32 {
        match self {
            PtMode::Sv39 | PtMode::Sv39x4 => 3,
            PtMode::Sv48 | PtMode::Sv48x4 => 4,
            PtMode::Sv57 | PtMode::Sv57x4 => 5,
        }
    }
    /// The width of an I/O address: the second stage's root indexes two more bits.
    pub const fn iova_bits(self) -> u32 {
        match self {
            PtMode::Sv39 => 39,
            PtMode::Sv48 => 48,
            PtMode::Sv57 => 57,
            PtMode::Sv39x4 => 41,
            PtMode::Sv48x4 => 50,
            PtMode::Sv57x4 => 59,
        }
    }
    /// The size of the root table: 16 KiB for the second stage, one page for the first.
    pub const fn root_bytes(self) -> u64 {
        if self.second_stage() {
            4 * PAGE_SIZE
        } else {
            PAGE_SIZE
        }
    }
    /// The `MODE` field of `iohgatp` or `iosatp` (bits 63:60).
    pub(crate) const fn atp_mode(self) -> u64 {
        match self {
            PtMode::Sv39 | PtMode::Sv39x4 => 8,
            PtMode::Sv48 | PtMode::Sv48x4 => 9,
            PtMode::Sv57 | PtMode::Sv57x4 => 10,
        }
    }
}

/// What a mapping lets the device do. There is no execute and no write without read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    ReadWrite,
}

/// The kind of a device's access, for [`Iopt::translate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Read,
    Write,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapError {
    /// The I/O address or the physical address is not page-aligned, or the root is not aligned to its size.
    Misaligned,
    /// The I/O address is outside the format's range, or the physical address does not fit a page-table entry.
    OutOfRange,
    /// Something is mapped there already.
    AlreadyMapped,
    /// [`TableFrames`] gave no frame (everything this call allocated was given back).
    NoFrame,
}

/// The access would fault in the IOMMU (any page-fault cause).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageFault;

// Page-table entry bits (the privileged architecture's, which the IOMMU specification uses for both stages).
pub const PTE_V: u64 = 1 << 0;
pub const PTE_R: u64 = 1 << 1;
pub const PTE_W: u64 = 1 << 2;
pub const PTE_X: u64 = 1 << 3;
pub const PTE_U: u64 = 1 << 4;
pub const PTE_G: u64 = 1 << 5;
pub const PTE_A: u64 = 1 << 6;
pub const PTE_D: u64 = 1 << 7;
/// Bits 63:54 (reserved, PBMT and N): this crate never writes them, and an entry with any of them set is treated as corrupt.
pub const PTE_HIGH: u64 = 0xffc0_0000_0000_0000;

const ENTRIES_PER_PAGE: u64 = 512;
const MAX_LEVELS: usize = 5;

/// The leaf this crate writes for `pa` with `access`.
pub const fn leaf(pa: u64, access: Access) -> u64 {
    let w = match access {
        Access::Read => 0,
        Access::ReadWrite => PTE_W,
    };
    ppn_field(pa) | PTE_V | PTE_R | w | PTE_U | PTE_A | PTE_D
}

/// One I/O page table: its root and format. The root's frames belong to the caller (four contiguous pages for the second stage);
/// every other table comes from [`TableFrames`].
#[derive(Debug, PartialEq, Eq)]
pub struct Iopt {
    root: u64,
    mode: PtMode,
}

impl Iopt {
    /// A table rooted at `root_pa`, which must be aligned to [`PtMode::root_bytes`]; the root is cleared.
    pub fn new<M: PhysMem64>(mem: &M, root_pa: u64, mode: PtMode) -> Result<Iopt, MapError> {
        if root_pa & mode.root_bytes().wrapping_sub(1) != 0 {
            return Err(MapError::Misaligned);
        }
        if ppn_addr(ppn_field(root_pa)) != root_pa {
            return Err(MapError::OutOfRange);
        }
        let mut off = 0;
        while off < mode.root_bytes() {
            mem.write(root_pa | off, 0);
            off = off.wrapping_add(8);
        }
        Ok(Iopt {
            root: root_pa,
            mode,
        })
    }

    pub const fn root(&self) -> u64 {
        self.root
    }
    pub const fn mode(&self) -> PtMode {
        self.mode
    }

    /// The root as a device context names it, with the soft-context id of the domain (`GSCID` for the second stage, `PSCID` for
    /// the first). `None` if the id does not fit its field (16 or 20 bits).
    pub const fn context_root(&self, soft_id: u32) -> Option<TableRoot> {
        TableRoot::new(self.root, self.mode, soft_id)
    }

    /// The index bits of `iova` at `level` (0 is the leaf level).
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: level < levels <= 5, so the shift is at most 48 and the width at most 11
    fn index(&self, iova: u64, level: u32) -> u64 {
        let top = level + 1 == self.mode.levels();
        let bits = if top && self.mode.second_stage() {
            11
        } else {
            9
        };
        (iova >> (PAGE_SHIFT + 9 * level)) & ((1 << bits) - 1)
    }

    /// The address is in the format's range: below `2^bits` for the second stage, sign-extended from the top bit for the first.
    fn in_range(&self, iova: u64) -> bool {
        let bits = self.mode.iova_bits();
        if self.mode.second_stage() {
            iova.checked_shr(bits).unwrap_or(0) == 0
        } else {
            let top = (iova as i64).checked_shr(bits.wrapping_sub(1)).unwrap_or(0);
            top == 0 || top == -1
        }
    }

    /// The tables on the way to `iova`'s leaf, from the root down, as far as they exist: `tables[i]` is the table at level
    /// `levels - 1 - i`. Returns how many exist (at least the root).
    #[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)] // INVARIANT: n <= levels <= MAX_LEVELS; idx < 2048 so idx * 8 < 16 KiB
    fn path<M: PhysMem64>(&self, mem: &M, iova: u64) -> ([u64; MAX_LEVELS], usize) {
        let mut tables = [0u64; MAX_LEVELS];
        tables[0] = self.root;
        let mut n = 1;
        let mut level = self.mode.levels() - 1;
        while level > 0 {
            let pte = mem.read(tables[n - 1] + self.index(iova, level) * 8);
            if pte & PTE_V == 0 || pte & (PTE_R | PTE_W | PTE_X) != 0 {
                break;
            }
            tables[n] = ppn_addr(pte);
            n += 1;
            level -= 1;
        }
        (tables, n)
    }

    /// Map the page at `iova` to the frame at `pa`.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: level counts down from levels - 1 >= 2 and stops at 0; idx * 8 < 16 KiB
    pub fn map<M: PhysMem64, F: TableFrames>(
        &mut self,
        mem: &M,
        frames: &mut F,
        iova: u64,
        pa: u64,
        access: Access,
    ) -> Result<(), MapError> {
        if iova & (PAGE_SIZE - 1) != 0 || pa & (PAGE_SIZE - 1) != 0 {
            return Err(MapError::Misaligned);
        }
        if !self.in_range(iova) || ppn_addr(ppn_field(pa)) != pa {
            return Err(MapError::OutOfRange);
        }
        let mut table = self.root;
        let mut level = self.mode.levels() - 1;
        while level > 0 {
            let slot = table + self.index(iova, level) * 8;
            let pte = mem.read(slot);
            if pte & PTE_V == 0 {
                let Some(new) = frames.alloc() else {
                    self.prune(mem, frames, iova);
                    return Err(MapError::NoFrame);
                };
                mem.write(slot, ppn_field(new) | PTE_V);
                table = new;
            } else if pte & (PTE_R | PTE_W | PTE_X) != 0 {
                return Err(MapError::AlreadyMapped);
            } else {
                table = ppn_addr(pte);
            }
            level -= 1;
        }
        let slot = table + self.index(iova, 0) * 8;
        if mem.read(slot) & PTE_V != 0 {
            return Err(MapError::AlreadyMapped);
        }
        mem.write(slot, leaf(pa, access));
        Ok(())
    }

    /// Remove the leaf at `iova` and return the frame it mapped; free the tables that became empty. Allocates nothing.
    #[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)] // INVARIANT: n == levels exactly when the leaf table was reached, so tables[n - 1] is it
    pub fn unmap<M: PhysMem64, F: TableFrames>(
        &mut self,
        mem: &M,
        frames: &mut F,
        iova: u64,
    ) -> Option<u64> {
        if iova & (PAGE_SIZE - 1) != 0 || !self.in_range(iova) {
            return None;
        }
        let (tables, n) = self.path(mem, iova);
        if n as u32 != self.mode.levels() {
            return None;
        }
        let slot = tables[n - 1] + self.index(iova, 0) * 8;
        let pte = mem.read(slot);
        if pte & PTE_V == 0 {
            return None;
        }
        mem.write(slot, 0);
        self.prune(mem, frames, iova);
        Some(ppn_addr(pte))
    }

    /// Free the empty tables on the way to `iova`, from the bottom up, stopping at the first that is not empty (never the root).
    #[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)] // INVARIANT: 1 <= i < n <= MAX_LEVELS; the table at path index i is at level levels - 1 - i
    fn prune<M: PhysMem64, F: TableFrames>(&self, mem: &M, frames: &mut F, iova: u64) {
        let (tables, n) = self.path(mem, iova);
        let mut i = n - 1;
        while i > 0 {
            if !table_empty(mem, tables[i]) {
                return;
            }
            let parent_level = self.mode.levels() - i as u32;
            mem.write(tables[i - 1] + self.index(iova, parent_level) * 8, 0);
            frames.free(tables[i]);
            i -= 1;
        }
    }

    /// Where the device's `op` at `iova` lands, read from the tables the way the mapper wrote them: the leaf must be valid, with
    /// no high bit, readable, user, accessed, and for a write writable and dirty; a leaf above the last level is a superpage and
    /// must be aligned to it.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: level counts down from levels - 1 and returns at 0; shift = 12 + 9 * level <= 48
    pub fn translate<M: PhysMem64>(&self, mem: &M, iova: u64, op: Op) -> Result<u64, PageFault> {
        if !self.in_range(iova) {
            return Err(PageFault);
        }
        let mut table = self.root;
        let mut level = self.mode.levels() - 1;
        loop {
            let pte = mem.read(table + self.index(iova, level) * 8);
            if pte & PTE_V == 0 || pte & PTE_HIGH != 0 {
                return Err(PageFault);
            }
            if pte & (PTE_R | PTE_W | PTE_X) == 0 {
                if level == 0 {
                    return Err(PageFault);
                }
                table = ppn_addr(pte);
                level -= 1;
                continue;
            }
            let shift = PAGE_SHIFT + 9 * level;
            let span = (1u64 << shift) - 1;
            let ok = pte & PTE_R != 0
                && pte & PTE_U != 0
                && pte & PTE_A != 0
                && ppn_addr(pte) & span == 0
                && match op {
                    Op::Read => true,
                    Op::Write => pte & PTE_W != 0 && pte & PTE_D != 0,
                };
            return if ok {
                Ok(ppn_addr(pte) | (iova & span))
            } else {
                Err(PageFault)
            };
        }
    }

    /// Call `f(iova, pa, access)` for every valid 4 KiB leaf, in address order. (This crate writes no other leaves; a leaf above
    /// the last level is reported with its first page.)
    pub fn for_each_leaf<M: PhysMem64>(&self, mem: &M, mut f: impl FnMut(u64, u64, Access)) {
        self.walk_leaves(
            mem,
            self.root,
            self.mode.levels().wrapping_sub(1),
            0,
            &mut f,
        );
    }

    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: i < entries <= 2048; level >= 1 before the recursion; the address is built from in-range indices
    fn walk_leaves<M: PhysMem64>(
        &self,
        mem: &M,
        table: u64,
        level: u32,
        base: u64,
        f: &mut impl FnMut(u64, u64, Access),
    ) {
        let entries = self.entries(level);
        for i in 0..entries {
            let pte = mem.read(table + i * 8);
            if pte & PTE_V == 0 {
                continue;
            }
            let iova = self.extend(base | (i << (PAGE_SHIFT + 9 * level)));
            if pte & (PTE_R | PTE_W | PTE_X) != 0 {
                let access = if pte & PTE_W != 0 {
                    Access::ReadWrite
                } else {
                    Access::Read
                };
                f(iova, ppn_addr(pte), access);
            } else if level > 0 {
                self.walk_leaves(mem, ppn_addr(pte), level - 1, iova, f);
            }
        }
    }

    /// The entries of a table at `level`.
    fn entries(&self, level: u32) -> u64 {
        if level.wrapping_add(1) == self.mode.levels() && self.mode.second_stage() {
            4 * ENTRIES_PER_PAGE
        } else {
            ENTRIES_PER_PAGE
        }
    }

    /// Sign-extend a first-stage address from its top bit.
    fn extend(&self, iova: u64) -> u64 {
        if self.mode.second_stage() {
            return iova;
        }
        let unused = 64u32.wrapping_sub(self.mode.iova_bits());
        ((iova.wrapping_shl(unused)) as i64).wrapping_shr(unused) as u64
    }

    /// Free every table but the root and clear the root; return the root's address (the caller's frames). Allocates nothing.
    pub fn destroy<M: PhysMem64, F: TableFrames>(self, mem: &M, frames: &mut F) -> u64 {
        self.free_below(mem, frames, self.root, self.mode.levels().wrapping_sub(1));
        self.root
    }

    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: i < entries <= 2048; level >= 1 before the recursion
    fn free_below<M: PhysMem64, F: TableFrames>(
        &self,
        mem: &M,
        frames: &mut F,
        table: u64,
        level: u32,
    ) {
        for i in 0..self.entries(level) {
            let pte = mem.read(table + i * 8);
            if pte & PTE_V != 0 && pte & (PTE_R | PTE_W | PTE_X) == 0 && level > 0 {
                let child = ppn_addr(pte);
                self.free_below(mem, frames, child, level - 1);
                frames.free(child);
            }
            if pte != 0 {
                mem.write(table + i * 8, 0);
            }
        }
    }
}

/// Every entry of the page at `table` is zero.
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: i < 512, so i * 8 < 4096
fn table_empty<M: PhysMem64>(mem: &M, table: u64) -> bool {
    (0..ENTRIES_PER_PAGE).all(|i| mem.read(table + i * 8) == 0)
}
