//! The device directory table (IOMMU-SPEC 3.1; the IOMMU specification's "Device-Directory-Table"): one, two or three levels,
//! base (32-byte) or extended (64-byte) device contexts, leaf pages allocated when a context is first written and returned when
//! every context in them is invalid again.
//!
//! A [`DeviceContext`] has three constructors, the three shapes the kernel writes (IOMMU-SPEC 3.1): **invalid** (Unclaimed),
//! **deny** (Quarantined: valid, translating through an empty table, fault reporting off, no MSI page table) and **translate**
//! (Attached: one stage through the domain's table, with its MSI page table). There is no way to build a valid context with both
//! stages Bare (IO2).

use crate::iopt::PtMode;
use crate::msi::MsiTable;
use crate::{ppn_addr, ppn_field, PhysMem64, TableFrames, PAGE_SHIFT, PAGE_SIZE, PPN_MASK};

/// The number of levels of the directory (`ddtp.iommu_mode` 1LVL, 2LVL, 3LVL).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Levels {
    One,
    Two,
    Three,
}

impl Levels {
    const fn count(self) -> u32 {
        match self {
            Levels::One => 1,
            Levels::Two => 2,
            Levels::Three => 3,
        }
    }
}

/// The device-context format: base (32 bytes, no MSI fields) or extended (64 bytes), chosen by the `MSI_FLAT` capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Base,
    Extended,
}

impl Format {
    pub const fn context_bytes(self) -> u64 {
        match self {
            Format::Base => 32,
            Format::Extended => 64,
        }
    }
    /// The device-id bits that index a leaf page: 7 for base contexts (128 per page), 6 for extended (64 per page).
    pub const fn leaf_bits(self) -> u32 {
        match self {
            Format::Base => 7,
            Format::Extended => 6,
        }
    }
    const fn words(self) -> usize {
        match self {
            Format::Base => 4,
            Format::Extended => 8,
        }
    }
}

/// The widest device id a directory covers: base 7, 16, 24 bits; extended 6, 15, 24 bits (the IOMMU specification's software
/// guidelines; QEMU `riscv_iommu_ctx_fetch`).
pub const fn device_id_bits(levels: Levels, format: Format) -> u32 {
    match (levels, format) {
        (Levels::One, Format::Base) => 7,
        (Levels::One, Format::Extended) => 6,
        (Levels::Two, Format::Base) => 16,
        (Levels::Two, Format::Extended) => 15,
        (Levels::Three, _) => 24,
    }
}

const TC_V: u64 = 1 << 0;
const TC_DTF: u64 = 1 << 4;
const MSIPTP_FLAT: u64 = 1 << 60;
const ATP_MODE_SHIFT: u32 = 60;
const GSCID_SHIFT: u32 = 44;
const PSCID_SHIFT: u32 = 12;
const DDTE_V: u64 = 1 << 0;

/// A page-table root as a device context names it: its address, format and soft-context id (`GSCID` in `iohgatp` for the second
/// stage, `PSCID` in `ta` for the first).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableRoot {
    pa: u64,
    mode: PtMode,
    soft_id: u32,
}

impl TableRoot {
    /// `None` if the root is not aligned to its size, does not fit the 44-bit page number, or the id does not fit its field.
    pub const fn new(pa: u64, mode: PtMode, soft_id: u32) -> Option<TableRoot> {
        let id_bits = if mode.second_stage() { 16 } else { 20 };
        if pa & mode.root_bytes().wrapping_sub(1) != 0
            || (pa >> PAGE_SHIFT) > PPN_MASK
            || (soft_id >> id_bits) != 0
        {
            return None;
        }
        Some(TableRoot { pa, mode, soft_id })
    }
    pub const fn pa(&self) -> u64 {
        self.pa
    }
    pub const fn mode(&self) -> PtMode {
        self.mode
    }
    pub const fn soft_id(&self) -> u32 {
        self.soft_id
    }

    /// (`iohgatp`, `ta`, `fsc`) for this root: exactly one stage is not Bare.
    const fn stage_words(&self) -> (u64, u64, u64) {
        let atp = (self.mode.atp_mode() << ATP_MODE_SHIFT) | (self.pa >> PAGE_SHIFT);
        if self.mode.second_stage() {
            (atp | ((self.soft_id as u64) << GSCID_SHIFT), 0, 0)
        } else {
            (0, (self.soft_id as u64) << PSCID_SHIFT, atp)
        }
    }
}

/// A device context, in its 64-byte extended layout: `tc`, `iohgatp`, `ta`, `fsc`, `msiptp`, `msi_addr_mask`,
/// `msi_addr_pattern`, reserved. The base format is the first four words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceContext {
    words: [u64; 8],
}

impl DeviceContext {
    /// Not valid: every transaction of the device is refused and reported.
    pub const fn invalid() -> DeviceContext {
        DeviceContext { words: [0; 8] }
    }

    /// Valid, translating through `empty` (a table the kernel keeps empty, shared by every quarantined device), with fault
    /// reporting off (`tc.DTF`) and no MSI page table: memory and messages are refused and not reported (IOMMU-SPEC 3.1).
    pub const fn deny(empty: TableRoot) -> DeviceContext {
        let (iohgatp, ta, fsc) = empty.stage_words();
        DeviceContext {
            words: [TC_V | TC_DTF, iohgatp, ta, fsc, 0, 0, 0, 0],
        }
    }

    /// Valid, translating through `root`, with hardware A/D updates off and fault reporting on; with `msi`, the device's writes to
    /// the MSI window go through that flat MSI page table (extended format only).
    pub const fn translate(root: TableRoot, msi: Option<&MsiTable>) -> DeviceContext {
        let (iohgatp, ta, fsc) = root.stage_words();
        let (msiptp, mask, pattern) = match msi {
            Some(t) => (
                MSIPTP_FLAT | (t.root() >> PAGE_SHIFT),
                t.window().mask(),
                t.window().pattern(),
            ),
            None => (0, 0, 0),
        };
        DeviceContext {
            words: [TC_V, iohgatp, ta, fsc, msiptp, mask, pattern, 0],
        }
    }

    pub const fn words(&self) -> [u64; 8] {
        self.words
    }
    pub const fn is_valid(&self) -> bool {
        self.words[0] & TC_V != 0
    }
    /// The context names an MSI table (`translate` writes `msiptp` only with one, and it is never 0 then: its mode is Flat).
    const fn has_msi(&self) -> bool {
        self.words[4] != 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DdtError {
    /// The device id is wider than the directory covers.
    DeviceIdTooWide,
    /// The context carries an MSI page table, which the base format cannot hold.
    MsiNeedsExtended,
    /// The root is not page-aligned or does not fit `ddtp`.
    Misaligned,
    /// [`TableFrames`] gave no frame (everything this call allocated was given back).
    NoFrame,
}

/// The device directory: its root page, number of levels and context format.
#[derive(Debug, PartialEq, Eq)]
pub struct Ddt {
    root: u64,
    levels: Levels,
    format: Format,
}

impl Ddt {
    /// A directory whose root is the page at `root_pa`, which is cleared (every device id invalid).
    pub fn new<M: PhysMem64>(
        mem: &M,
        root_pa: u64,
        levels: Levels,
        format: Format,
    ) -> Result<Ddt, DdtError> {
        if root_pa & (PAGE_SIZE - 1) != 0 || ppn_addr(ppn_field(root_pa)) != root_pa {
            return Err(DdtError::Misaligned);
        }
        clear_page(mem, root_pa);
        Ok(Ddt {
            root: root_pa,
            levels,
            format,
        })
    }

    pub const fn root(&self) -> u64 {
        self.root
    }
    pub const fn levels(&self) -> Levels {
        self.levels
    }
    pub const fn format(&self) -> Format {
        self.format
    }
    /// How many pages the directory holds: the root and every non-leaf and leaf page below it. Reads only (IOMMU6: the kernel's
    /// leak check counts them, since a quarantined device keeps its context, and so its directory pages, for good).
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: at most 1 + 512 + 512 * 512 pages
    pub fn pages<M: PhysMem64>(&self, mem: &M) -> u64 {
        fn below<M: PhysMem64>(mem: &M, page: u64, depth: u32) -> u64 {
            if depth == 0 {
                return 0;
            }
            (0..512u64)
                .map(|i| mem.read(page + i * 8))
                .filter(|de| de & DDTE_V != 0)
                .map(|de| 1 + below(mem, ppn_addr(de), depth - 1))
                .sum()
        }
        1 + below(mem, self.root, self.levels.count() - 1)
    }

    /// The value to write to `ddtp` for this directory.
    pub const fn ddtp(&self) -> crate::regs::Ddtp {
        crate::regs::Ddtp::directory(self.levels, self.root)
    }

    fn check_id(&self, device_id: u32) -> Result<(), DdtError> {
        if device_id
            .checked_shr(device_id_bits(self.levels, self.format))
            .unwrap_or(0)
            != 0
        {
            Err(DdtError::DeviceIdTooWide)
        } else {
            Ok(())
        }
    }

    /// The index of `device_id` in a non-leaf directory page `depth` levels above the leaf pages (1 or 2).
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: depth is 1 or 2, so the shift is at most 25
    fn dir_index(&self, device_id: u32, depth: u32) -> u64 {
        u64::from(device_id >> (self.format.leaf_bits() + 9 * (depth - 1))) & 0x1ff
    }

    /// The address of `device_id`'s context in its leaf page at `leaf`.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: the offset is below 4 KiB (64 contexts of 64 bytes or 128 of 32) and `leaf` is a page
    fn context_addr(&self, leaf: u64, device_id: u32) -> u64 {
        let index = u64::from(device_id) & ((1 << self.format.leaf_bits()) - 1);
        leaf + index * self.format.context_bytes()
    }

    /// The directory pages on the way to `device_id`'s leaf page, from the root down, as far as they exist: `pages[i]` is
    /// `levels - 1 - i` levels above the leaf pages. Returns how many exist.
    #[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)] // INVARIANT: n <= levels <= 3; depth >= 1 inside the loop
    fn path<M: PhysMem64>(&self, mem: &M, device_id: u32) -> ([u64; 3], usize) {
        let mut pages = [0u64; 3];
        pages[0] = self.root;
        let mut n = 1;
        let mut depth = self.levels.count() - 1;
        while depth > 0 {
            let de = mem.read(pages[n - 1] + self.dir_index(device_id, depth) * 8);
            if de & DDTE_V == 0 {
                break;
            }
            pages[n] = ppn_addr(de);
            n += 1;
            depth -= 1;
        }
        (pages, n)
    }

    /// Write `ctx` as `device_id`'s context, allocating the directory pages on the way if needed. The word that holds the valid
    /// bit is written last when the new context is valid and first when it is not, so the IOMMU never sees a valid `tc` with
    /// fields of a context that was never written (IOMMU-SPEC 3.1).
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: depth counts down from levels - 1 to 0; w < words <= 8
    pub fn set<M: PhysMem64, F: TableFrames>(
        &mut self,
        mem: &M,
        frames: &mut F,
        device_id: u32,
        ctx: &DeviceContext,
    ) -> Result<(), DdtError> {
        self.check_id(device_id)?;
        if self.format == Format::Base && ctx.has_msi() {
            return Err(DdtError::MsiNeedsExtended);
        }
        if !ctx.is_valid() {
            self.clear(mem, frames, device_id);
            return Ok(());
        }
        let mut page = self.root;
        let mut depth = self.levels.count() - 1;
        while depth > 0 {
            let slot = page + self.dir_index(device_id, depth) * 8;
            let de = mem.read(slot);
            if de & DDTE_V == 0 {
                let Some(new) = frames.alloc() else {
                    self.prune(mem, frames, device_id);
                    return Err(DdtError::NoFrame);
                };
                mem.write(slot, ppn_field(new) | DDTE_V);
                page = new;
            } else {
                page = ppn_addr(de);
            }
            depth -= 1;
        }
        write_context(
            mem,
            self.context_addr(page, device_id),
            &ctx.words[..self.format.words()],
        );
        Ok(())
    }

    /// Make `device_id`'s context invalid and return the directory pages that became empty. Allocates nothing.
    #[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)] // INVARIANT: n == levels means pages[n - 1] is the leaf page
    pub fn clear<M: PhysMem64, F: TableFrames>(&mut self, mem: &M, frames: &mut F, device_id: u32) {
        if self.check_id(device_id).is_err() {
            return;
        }
        let (pages, n) = self.path(mem, device_id);
        if n as u32 != self.levels.count() {
            return;
        }
        let words = [0u64; 8];
        write_context(
            mem,
            self.context_addr(pages[n - 1], device_id),
            &words[..self.format.words()],
        );
        self.prune(mem, frames, device_id);
    }

    /// Free the empty directory pages on the way to `device_id`, bottom up (never the root).
    #[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)] // INVARIANT: 1 <= i < n <= 3; the page at path index i is levels - 1 - i above the leaves
    fn prune<M: PhysMem64, F: TableFrames>(&self, mem: &M, frames: &mut F, device_id: u32) {
        let (pages, n) = self.path(mem, device_id);
        let mut i = n - 1;
        while i > 0 {
            if !page_empty(mem, pages[i]) {
                return;
            }
            let parent_depth = self.levels.count() - i as u32;
            mem.write(
                pages[i - 1] + self.dir_index(device_id, parent_depth) * 8,
                0,
            );
            frames.free(pages[i]);
            i -= 1;
        }
    }

    /// `device_id`'s context as the IOMMU would read it (the base format's last four words read as zero), or `None` if the id is
    /// too wide or its leaf page does not exist.
    #[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)] // INVARIANT: n == levels; w < words <= 8
    pub fn get<M: PhysMem64>(&self, mem: &M, device_id: u32) -> Option<[u64; 8]> {
        self.check_id(device_id).ok()?;
        let (pages, n) = self.path(mem, device_id);
        if n as u32 != self.levels.count() {
            return None;
        }
        let at = self.context_addr(pages[n - 1], device_id);
        let mut words = [0u64; 8];
        for (w, word) in words.iter_mut().enumerate().take(self.format.words()) {
            *word = mem.read(at + w as u64 * 8);
        }
        Some(words)
    }
}

/// Write a context's words: `tc` last if the new `tc` is valid, first if it is not.
#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: words has 4 or 8 entries; w < 8
fn write_context<M: PhysMem64>(mem: &M, at: u64, words: &[u64]) {
    let valid = words[0] & TC_V != 0;
    if !valid {
        mem.write(at, words[0]);
    }
    for (w, word) in words.iter().enumerate().skip(1) {
        mem.write(at + w as u64 * 8, *word);
    }
    if valid {
        mem.write(at, words[0]);
    }
}

#[allow(clippy::arithmetic_side_effects)] // INVARIANT: i < 512
fn clear_page<M: PhysMem64>(mem: &M, page: u64) {
    for i in 0..PAGE_SIZE / 8 {
        mem.write(page + i * 8, 0);
    }
}

#[allow(clippy::arithmetic_side_effects)] // INVARIANT: i < 512
fn page_empty<M: PhysMem64>(mem: &M, page: u64) -> bool {
    (0..PAGE_SIZE / 8).all(|i| mem.read(page + i * 8) == 0)
}
