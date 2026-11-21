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
    /// `map_large` was asked for a level the format does not have.
    BadLevel,
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
    /// A leaf above level 0 (a large page: `level` 1 is 2 MiB, 2 is 1 GiB) mapping the frames from `pfn` on, which must be aligned to
    /// the level's span. In Sv39 it is the same entry as a level-0 leaf; other formats mark blocks differently.
    fn block_entry(pfn: Pfn, perms: Perms, level: u32) -> u64;
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
    fn block_entry(pfn: Pfn, perms: Perms, _level: u32) -> u64 {
        Self::leaf_entry(pfn, perms)
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

    /// Map a large page: `level` 1 (2 MiB) or 2 (1 GiB) in Sv39, `va` and `pfn` aligned to its span. Kernel use only (user mappings stay 4 KiB:
    /// `AddressSpace` never calls it). Allocates the tables above it as needed; if it cannot, nothing changes. A table or a leaf already in
    /// the slot is refused (`HugeConflict` for a table with smaller pages under it, `AlreadyMapped` for a leaf). Level 0 is `map`.
    #[allow(clippy::arithmetic_side_effects, clippy::expect_used)] // INVARIANT: the slot is a table frame plus index * 8 with index < 512; level stays between the leaf level and the top level; the fresh frames were reserved by the free-count check
    pub fn map_large<M: PhysMem>(
        &mut self,
        mm: &mut Mm<M>,
        va: u64,
        pfn: Pfn,
        level: u32,
        perms: Perms,
    ) -> Result<(), MapError> {
        if level == 0 {
            return self.map(mm, va, pfn, perms);
        }
        if level >= F::LEVELS {
            return Err(MapError::BadLevel);
        }
        let span_pages = 1u64 << (9 * level);
        if !va.is_multiple_of(PAGE_SIZE * span_pages) || !pfn.is_multiple_of(span_pages) {
            return Err(MapError::Misaligned);
        }
        if !F::is_canonical(va) {
            return Err(MapError::NonCanonical);
        }
        perms.validate()?;
        if perms.user {
            return Err(MapError::BadPerms); // large pages are the kernel's (KERNEL-V1-SPEC section 1: none for user mappings)
        }
        let mut table = self.root;
        let mut l = F::LEVELS - 1;
        loop {
            let pte = mm.mem.read_u64((table << 12) + index(va, l) * 8);
            if l == level {
                if F::is_valid(pte) {
                    return Err(if F::is_leaf(pte, l) {
                        MapError::AlreadyMapped
                    } else {
                        MapError::HugeConflict
                    });
                }
                break;
            }
            if !F::is_valid(pte) {
                break;
            }
            if F::is_leaf(pte, l) {
                return Err(MapError::HugeConflict);
            }
            table = F::pfn_of(pte);
            l -= 1;
        }
        // tables for levels `l` down to `level + 1` are missing below `table`
        if mm.db.free_count() < (l - level) as usize {
            return Err(MapError::NoMemory);
        }
        while l > level {
            let fresh = mm.db.alloc().expect("free count was checked");
            mm.mem.zero_frame(fresh);
            mm.mem
                .write_u64((table << 12) + index(va, l) * 8, F::table_entry(fresh));
            table = fresh;
            l -= 1;
        }
        mm.mem.write_u64(
            (table << 12) + index(va, level) * 8,
            F::block_entry(pfn, perms, level),
        );
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

    /// Change the permissions of the 4 KiB leaf at `va` in place (the frame stays): returns the old permissions, or `None` if nothing is
    /// mapped there (or a huge page is: not ours to change one page at a time). The caller flushes the translations that a
    /// permission it took away may still be cached under, on every hart.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: the slot is a table frame plus index * 8 with index < 512, as in `translate`
    pub fn protect<M: PhysMem>(&mut self, mm: &mut Mm<M>, va: u64, perms: Perms) -> Option<Perms> {
        if !va.is_multiple_of(PAGE_SIZE) || !F::is_canonical(va) {
            return None;
        }
        let mut table = self.root;
        for level in (0..F::LEVELS).rev() {
            let slot = (table << 12) + index(va, level) * 8;
            let pte = mm.mem.read_u64(slot);
            if !F::is_valid(pte) {
                return None;
            }
            if F::is_leaf(pte, level) {
                if level != 0 {
                    return None;
                }
                mm.mem.write_u64(slot, F::leaf_entry(F::pfn_of(pte), perms));
                return Some(F::perms_of(pte));
            }
            if level == 0 {
                return None;
            }
            table = F::pfn_of(pte);
        }
        None
    }

    /// The first address at or after `va`, below `end`, that has a mapping (a page or a huge leaf), skipping every span whose table is absent
    /// (a 4 KiB page, 2 MiB or 1 GiB at a time, as far up as the first invalid entry): a walk over a mostly empty range costs what is
    /// mapped, not the length of the range. `None` when nothing is mapped in `[va, end)`.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: indices are below 512; the spans are 1 << (12 + 9 * level) with level < 4; the next start is computed with checked arithmetic
    pub fn next_mapped<M: PhysMem>(&self, mm: &Mm<M>, mut va: u64, end: u64) -> Option<u64> {
        while va < end {
            if !F::is_canonical(va) {
                return None;
            }
            let mut table = self.root;
            let mut skip = None;
            for level in (0..F::LEVELS).rev() {
                let pte = mm.mem.read_u64((table << 12) + index(va, level) * 8);
                if !F::is_valid(pte) {
                    skip = Some(PAGE_SIZE << (9 * level));
                    break;
                }
                if F::is_leaf(pte, level) {
                    return Some(va);
                }
                if level == 0 {
                    skip = Some(PAGE_SIZE);
                    break;
                }
                table = F::pfn_of(pte);
            }
            let span = skip?;
            va = (va / span).checked_add(1)?.checked_mul(span)?;
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

/// Cut the frames `first .. first + count` into the largest aligned pages up to `max_level` (a page of level `l` is `1 << (9 * l)` frames,
/// aligned to that many frames): `(frame, level)` in address order, small pages at unaligned edges. Every frame is covered exactly once.
/// `max_level` is clamped to 3 (a 512 GiB page is the most any format here has); a range reaching the top of the frame-number space is
/// cut short at it.
pub fn plan_range(first: Pfn, count: u64, max_level: u32) -> PlanRange {
    PlanRange {
        next: first,
        end: first.saturating_add(count),
        max_level: max_level.min(3),
    }
}

/// The iterator of [`plan_range`].
pub struct PlanRange {
    next: Pfn,
    end: Pfn,
    max_level: u32,
}

impl Iterator for PlanRange {
    type Item = (Pfn, u32);

    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: the span is 1 << (9 * level) with level at most max_level (below 7), and next + span is at most end
    fn next(&mut self) -> Option<(Pfn, u32)> {
        if self.next >= self.end {
            return None;
        }
        let mut level = self.max_level;
        loop {
            let span = 1u64 << (9 * level);
            if level == 0 || (self.next.is_multiple_of(span) && self.end - self.next >= span) {
                let out = (self.next, level);
                self.next += span;
                return Some(out);
            }
            level -= 1;
        }
    }
}
