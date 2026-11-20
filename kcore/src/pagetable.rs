//! Page tables, generic over a [`PageTableFormat`] (Sv39 is the one implementation in the kernel), as pure logic over a [`PhysMem`] (the contents of physical
//! frames) and the frame database. The kernel's `PhysMem` is the direct map; the
//! tests' is a fake. Tables are made of ordinary frames from [`FrameDb`].
//!
//! The table never touches the reference counts of the *leaf* frames it maps:
//! that is the caller's business (see `aspace`). It does own its table frames and
//! frees a table as soon as it is empty, so map/unmap sequences return every frame.

use crate::frame::{Pfn, PAGE_SIZE};
use crate::Mm;

/// Access to the contents of physical memory, by physical address.
pub trait PhysMem {
    fn read_u64(&self, pa: u64) -> u64;
    fn write_u64(&mut self, pa: u64, value: u64);
    /// Copy bytes out of physical memory (any alignment). Whole aligned words go at once.
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: pa is a frame address the caller got from the frame database; i < out.len() and the 8-byte arm checks out.len() - i >= 8 first
    fn read_bytes(&self, pa: u64, out: &mut [u8]) {
        let mut i = 0;
        while i < out.len() {
            let a = pa + i as u64;
            if a & 7 == 0 && out.len() - i >= 8 {
                out[i..i + 8].copy_from_slice(&self.read_u64(a).to_le_bytes());
                i += 8;
            } else {
                out[i] = (self.read_u64(a & !7) >> ((a & 7) * 8)) as u8;
                i += 1;
            }
        }
    }
    /// Copy bytes into physical memory (any alignment). Whole aligned words go at once.
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: as `read_bytes`: i < data.len() and the 8-byte arm checks the room first
    fn write_bytes(&mut self, pa: u64, data: &[u8]) {
        let mut i = 0;
        while i < data.len() {
            let a = pa + i as u64;
            if a & 7 == 0 && data.len() - i >= 8 {
                let mut w = [0u8; 8];
                w.copy_from_slice(&data[i..i + 8]);
                self.write_u64(a, u64::from_le_bytes(w));
                i += 8;
            } else {
                let shift = (a & 7) * 8;
                let word = self.read_u64(a & !7);
                self.write_u64(
                    a & !7,
                    (word & !(0xff << shift)) | (u64::from(data[i]) << shift),
                );
                i += 1;
            }
        }
    }
    /// Fill a frame with zeros.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: word < 512 and pfn is a frame of the table
    fn zero_frame(&mut self, pfn: Pfn) {
        for word in 0..PAGE_SIZE / 8 {
            self.write_u64((pfn << 12) + word * 8, 0);
        }
    }
}

/// Permissions of a mapping. `user` is the U bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Perms {
    pub read: bool,
    pub write: bool,
    pub exec: bool,
    pub user: bool,
}

impl Perms {
    pub const R: Perms = Perms {
        read: true,
        write: false,
        exec: false,
        user: false,
    };
    pub const RW: Perms = Perms {
        read: true,
        write: true,
        exec: false,
        user: false,
    };
    pub const RX: Perms = Perms {
        read: true,
        write: false,
        exec: true,
        user: false,
    };
    pub const X: Perms = Perms {
        read: false,
        write: false,
        exec: true,
        user: false,
    };

    pub const fn user(self) -> Perms {
        Perms { user: true, ..self }
    }

    /// W^X (INVARIANTS.md M2) and the hardware's reserved combinations.
    pub fn validate(self) -> Result<(), MapError> {
        if self.write && self.exec {
            Err(MapError::WriteExec)
        } else if !(self.read || self.exec) {
            // No access at all, or write without read (reserved in the PTE).
            Err(MapError::BadPerms)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapError {
    Misaligned,
    NonCanonical,
    WriteExec,
    BadPerms,
    AlreadyMapped,
    /// A huge page is in the way (the early kernel mappings are 1 GiB leaves).
    HugeConflict,
    NoMemory,
}

/// What a virtual address translates to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Translation {
    pub pfn: Pfn,
    pub perms: Perms,
}

/// What differs between page-table formats (AR1c, KERNEL-V1-SPEC 2.6): the entry encoding, the number of levels, which entries
/// are leaves, and which addresses are valid. Tables are 4 KiB frames of 512 eight-byte entries, 9 index bits per level, in every
/// format here; the top half of the root table (entries 256 and up) is the kernel's.
pub trait PageTableFormat {
    /// Levels of the walk (3 for Sv39: levels 2, 1, 0; leaves above level 0 are huge pages).
    const LEVELS: u32;
    /// Is this entry present?
    fn is_valid(pte: u64) -> bool;
    /// Is a valid entry at `level` a leaf (a mapping) and not a pointer to the next table?
    fn is_leaf(pte: u64, level: u32) -> bool;
    /// The frame a valid entry names (a table or a leaf's first frame).
    fn pfn_of(pte: u64) -> Pfn;
    /// An entry pointing at the next-level table in frame `pfn`.
    fn table_entry(pfn: Pfn) -> u64;
    /// A level-0 leaf mapping frame `pfn` with `perms` (already validated).
    fn leaf_entry(pfn: Pfn, perms: Perms) -> u64;
    /// The permissions a leaf entry grants.
    fn perms_of(pte: u64) -> Perms;
    /// Is `va` an address this format can map (sign-extended from its top bit)?
    fn is_canonical(va: u64) -> bool;
    /// The full virtual address of a walked address whose bits above the format's range are not yet filled in.
    fn extend(va: u64) -> u64;
}

/// The Sv39 format (RISC-V): three levels, 39-bit sign-extended addresses.
#[derive(Debug, Clone, Copy)]
pub struct Sv39;

const V: u64 = 1 << 0;
const R: u64 = 1 << 1;
const W: u64 = 1 << 2;
const X: u64 = 1 << 3;
const U: u64 = 1 << 4;
const A: u64 = 1 << 6;
const D: u64 = 1 << 7;

impl PageTableFormat for Sv39 {
    const LEVELS: u32 = 3;
    fn is_valid(pte: u64) -> bool {
        pte & V != 0
    }
    fn is_leaf(pte: u64, _level: u32) -> bool {
        pte & (R | W | X) != 0
    }
    fn pfn_of(pte: u64) -> Pfn {
        (pte >> 10) & ((1 << 44) - 1)
    }
    fn table_entry(pfn: Pfn) -> u64 {
        (pfn << 10) | V
    }
    fn leaf_entry(pfn: Pfn, perms: Perms) -> u64 {
        let mut pte = (pfn << 10) | V | A | D;
        if perms.read {
            pte |= R;
        }
        if perms.write {
            pte |= W;
        }
        if perms.exec {
            pte |= X;
        }
        if perms.user {
            pte |= U;
        }
        pte
    }
    fn perms_of(pte: u64) -> Perms {
        Perms {
            read: pte & R != 0,
            write: pte & W != 0,
            exec: pte & X != 0,
            user: pte & U != 0,
        }
    }
    /// Sv39 virtual addresses are 39 bits, sign-extended.
    fn is_canonical(va: u64) -> bool {
        let top = (va as i64) >> 38;
        top == 0 || top == -1
    }
    fn extend(va: u64) -> u64 {
        if va & (1 << 38) != 0 {
            va | 0xffff_ff80_0000_0000
        } else {
            va
        }
    }
}

#[allow(clippy::arithmetic_side_effects)] // INVARIANT: the shift is below 64 (level <= 3) and the mask keeps the index below 512
fn index(va: u64, level: u32) -> u64 {
    (va >> (12 + 9 * level)) & 0x1ff
}

/// The deepest walk any format here has.
const MAX_LEVELS: usize = 4;

/// A page-table root. Holds no references: pass the same `Mm` each time.
#[derive(Debug)]
pub struct PageTable<F: PageTableFormat = Sv39> {
    root: Pfn,
    _format: core::marker::PhantomData<F>,
}

impl PageTable<Sv39> {
    /// A fresh, empty Sv39 table (one frame for the root).
    pub fn new<M: PhysMem>(mm: &mut Mm<M>) -> Option<PageTable> {
        PageTable::create(mm)
    }

    /// A fresh Sv39 table for a user process, sharing the kernel's upper half (see `create_with_kernel_half`).
    pub fn new_with_kernel_half<M: PhysMem>(
        mm: &mut Mm<M>,
        kernel: &PageTable,
    ) -> Option<PageTable> {
        PageTable::create_with_kernel_half(mm, kernel)
    }

    /// Wrap an existing Sv39 root (the kernel's boot table). Do not `destroy` it.
    pub fn from_root(root: Pfn) -> PageTable {
        PageTable::wrap_root(root)
    }
}

impl<F: PageTableFormat> PageTable<F> {
    /// A fresh, empty table (one frame for the root).
    pub fn create<M: PhysMem>(mm: &mut Mm<M>) -> Option<PageTable<F>> {
        let root = mm.db.alloc()?;
        mm.mem.zero_frame(root);
        Some(PageTable {
            root,
            _format: core::marker::PhantomData,
        })
    }

    /// A fresh table for a user process: empty in the lower half, and sharing the
    /// kernel's upper half (top-level entries 256..512 are copied, so every process sees
    /// the same kernel mappings; OD41). Free it with `destroy_user`, never `destroy`.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: i < 512 table slots, and the roots are frames of the table
    pub fn create_with_kernel_half<M: PhysMem>(
        mm: &mut Mm<M>,
        kernel: &PageTable<F>,
    ) -> Option<PageTable<F>> {
        let table = PageTable::create(mm)?;
        for i in 256..512u64 {
            let pte = mm.mem.read_u64((kernel.root << 12) + i * 8);
            mm.mem.write_u64((table.root << 12) + i * 8, pte);
        }
        Some(table)
    }

    /// Wrap an existing root (the kernel's boot table). Do not `destroy` it.
    pub fn wrap_root(root: Pfn) -> PageTable<F> {
        PageTable {
            root,
            _format: core::marker::PhantomData,
        }
    }

    /// Free the root and every table of the lower (user) half. The shared kernel half is
    /// not touched. The user leaves must already be unmapped.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: i < 256 slots of the root table, and the root is a frame of the table
    pub fn destroy_user<M: PhysMem>(self, mm: &mut Mm<M>) {
        #[allow(clippy::arithmetic_side_effects)] // INVARIANT: indices are below 512; level decreases only while it is above 0
        fn walk<F: PageTableFormat, M: PhysMem>(mm: &mut Mm<M>, table: Pfn, level: u32) {
            if level > 0 {
                for i in 0..512 {
                    let pte = mm.mem.read_u64((table << 12) + i * 8);
                    if F::is_valid(pte) && !F::is_leaf(pte, level) {
                        walk::<F, M>(mm, F::pfn_of(pte), level - 1);
                    }
                }
            }
            let _ = mm.db.dec(table);
        }
        for i in 0..256u64 {
            let top = F::LEVELS - 1;
            let pte = mm.mem.read_u64((self.root << 12) + i * 8);
            if F::is_valid(pte) && !F::is_leaf(pte, top) {
                walk::<F, M>(mm, F::pfn_of(pte), top - 1);
            }
        }
        let _ = mm.db.dec(self.root);
    }

    pub fn root(&self) -> Pfn {
        self.root
    }

    /// Map the page at `va` to frame `pfn`. Allocates intermediate tables as needed;
    /// if it cannot, nothing changes.
    #[allow(clippy::arithmetic_side_effects, clippy::expect_used)] // INVARIANT: the slot is a table frame plus index * 8 with index < 512; level decreases only while above 0; the fresh frame was reserved by the free-count check at the top
    pub fn map<M: PhysMem>(
        &mut self,
        mm: &mut Mm<M>,
        va: u64,
        pfn: Pfn,
        perms: Perms,
    ) -> Result<(), MapError> {
        if !va.is_multiple_of(PAGE_SIZE) {
            return Err(MapError::Misaligned);
        }
        if !F::is_canonical(va) {
            return Err(MapError::NonCanonical);
        }
        perms.validate()?;
        // Walk down, remembering where the path stops.
        let mut table = self.root;
        let mut level = F::LEVELS - 1;
        loop {
            let slot = (table << 12) + index(va, level) * 8;
            let pte = mm.mem.read_u64(slot);
            if !F::is_valid(pte) {
                break;
            }
            if F::is_leaf(pte, level) {
                return Err(if level == 0 {
                    MapError::AlreadyMapped
                } else {
                    MapError::HugeConflict
                });
            }
            if level == 0 {
                // A non-leaf entry at level 0 is malformed; treat the slot as taken.
                return Err(MapError::AlreadyMapped);
            }
            table = F::pfn_of(pte);
            level -= 1;
        }
        // `level` tables are missing below `table`: levels `level` down to 1 need new frames.
        let missing = level as usize;
        if mm.db.free_count() < missing {
            return Err(MapError::NoMemory);
        }
        while level > 0 {
            let fresh = mm.db.alloc().expect("free count was checked");
            mm.mem.zero_frame(fresh);
            let slot = (table << 12) + index(va, level) * 8;
            mm.mem.write_u64(slot, F::table_entry(fresh));
            table = fresh;
            level -= 1;
        }
        let slot = (table << 12) + index(va, 0) * 8;
        mm.mem.write_u64(slot, F::leaf_entry(pfn, perms));
        Ok(())
    }

    /// Unmap the page at `va`, returning what it mapped. Tables left empty are freed.
    /// **The caller must flush the TLB before any frame freed here is
    /// reused**: this crate has no notion of TLBs.
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: level is below LEVELS (at most 4) and the path array has MAX_LEVELS entries; indices are below 512
    pub fn unmap<M: PhysMem>(&mut self, mm: &mut Mm<M>, va: u64) -> Option<(Pfn, Perms)> {
        if !va.is_multiple_of(PAGE_SIZE) || !F::is_canonical(va) {
            return None;
        }
        let top = F::LEVELS - 1;
        // path[i] = (table frame, slot address) for level top - i
        let mut path: [(Pfn, u64); MAX_LEVELS] = [(0, 0); MAX_LEVELS];
        let mut table = self.root;
        for level in (0..=top).rev() {
            let slot = (table << 12) + index(va, level) * 8;
            path[(top - level) as usize] = (table, slot);
            let pte = mm.mem.read_u64(slot);
            if !F::is_valid(pte) {
                return None;
            }
            if F::is_leaf(pte, level) {
                if level != 0 {
                    return None; // a huge page is not ours to unmap one 4 KiB page at a time
                }
                mm.mem.write_u64(slot, 0);
                let result = (F::pfn_of(pte), F::perms_of(pte));
                // Free tables that became empty, bottom up, never the root.
                for up in (1..=top as usize).rev() {
                    let (frame, _) = path[up];
                    let empty =
                        (0..512).all(|i| !F::is_valid(mm.mem.read_u64((frame << 12) + i * 8)));
                    if !empty {
                        break;
                    }
                    let (_, parent_slot) = path[up - 1];
                    mm.mem.write_u64(parent_slot, 0);
                    let _ = mm.db.dec(frame);
                }
                return Some(result);
            }
            if level == 0 {
                return None;
            }
            table = F::pfn_of(pte);
        }
        None
    }

    /// Translate `va` (any byte of a page). Huge leaves translate too.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: indices are below 512; the span is 1 << (9 * level) with level < 4; a leaf's pfn plus an in-span page offset stays a frame number
    pub fn translate<M: PhysMem>(&self, mm: &Mm<M>, va: u64) -> Option<Translation> {
        if !F::is_canonical(va) {
            return None;
        }
        let mut table = self.root;
        for level in (0..F::LEVELS).rev() {
            let pte = mm.mem.read_u64((table << 12) + index(va, level) * 8);
            if !F::is_valid(pte) {
                return None;
            }
            if F::is_leaf(pte, level) {
                // For a huge leaf the low frame-number bits come from the address.
                let span = 1u64 << (9 * level);
                let offset_pages = (va >> 12) & (span - 1);
                return Some(Translation {
                    pfn: F::pfn_of(pte) + offset_pages,
                    perms: F::perms_of(pte),
                });
            }
            if level == 0 {
                return None;
            }
            table = F::pfn_of(pte);
        }
        None
    }

    /// Call `f(va, translation, level)` for every leaf mapping, in address order.
    /// `level` is 0 for a 4 KiB page, 1 for 2 MiB, 2 for 1 GiB. Used to audit a
    /// table (for example: no leaf is both writable and executable).
    pub fn for_each_leaf<M: PhysMem>(&self, mm: &Mm<M>, f: &mut dyn FnMut(u64, Translation, u32)) {
        #[allow(clippy::arithmetic_side_effects)] // INVARIANT: indices are below 512; level decreases only while it is above 0
        fn walk<F: PageTableFormat, M: PhysMem>(
            mm: &Mm<M>,
            table: Pfn,
            level: u32,
            prefix: u64,
            f: &mut dyn FnMut(u64, Translation, u32),
        ) {
            for i in 0..512u64 {
                let pte = mm.mem.read_u64((table << 12) + i * 8);
                if !F::is_valid(pte) {
                    continue;
                }
                let va = prefix | (i << (12 + 9 * level));
                if F::is_leaf(pte, level) {
                    f(
                        F::extend(va),
                        Translation {
                            pfn: F::pfn_of(pte),
                            perms: F::perms_of(pte),
                        },
                        level,
                    );
                } else if level > 0 {
                    walk::<F, M>(mm, F::pfn_of(pte), level - 1, va, f);
                }
            }
        }
        walk::<F, M>(mm, self.root, F::LEVELS.saturating_sub(1), 0, f);
    }

    /// Free every table frame (the root too). The leaves must be unmapped (or are
    /// not ours to free): their frames are not touched.
    pub fn destroy<M: PhysMem>(self, mm: &mut Mm<M>) {
        #[allow(clippy::arithmetic_side_effects)] // INVARIANT: indices are below 512; level decreases only while it is above 0
        fn walk<F: PageTableFormat, M: PhysMem>(mm: &mut Mm<M>, table: Pfn, level: u32) {
            if level > 0 {
                for i in 0..512 {
                    let pte = mm.mem.read_u64((table << 12) + i * 8);
                    if F::is_valid(pte) && !F::is_leaf(pte, level) {
                        walk::<F, M>(mm, F::pfn_of(pte), level - 1);
                    }
                }
            }
            let _ = mm.db.dec(table);
        }
        walk::<F, M>(mm, self.root, F::LEVELS.saturating_sub(1));
    }
}
