//! Sv39 page tables, as pure logic over a [`PhysMem`] (the contents of physical
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
    /// Fill a frame with zeros.
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

const V: u64 = 1 << 0;
const R: u64 = 1 << 1;
const W: u64 = 1 << 2;
const X: u64 = 1 << 3;
const U: u64 = 1 << 4;
const A: u64 = 1 << 6;
const D: u64 = 1 << 7;
const FLAGS: u64 = 0x3ff;

fn is_leaf(pte: u64) -> bool {
    pte & (R | W | X) != 0
}

fn ppn(pte: u64) -> Pfn {
    (pte >> 10) & ((1 << 44) - 1)
}

fn perms_of(pte: u64) -> Perms {
    Perms {
        read: pte & R != 0,
        write: pte & W != 0,
        exec: pte & X != 0,
        user: pte & U != 0,
    }
}

fn index(va: u64, level: u32) -> u64 {
    (va >> (12 + 9 * level)) & 0x1ff
}

/// Sv39 virtual addresses are 39 bits, sign-extended.
pub fn is_canonical(va: u64) -> bool {
    let top = (va as i64) >> 38;
    top == 0 || top == -1
}

/// A page-table root. Holds no references: pass the same `Mm` each time.
#[derive(Debug)]
pub struct PageTable {
    root: Pfn,
}

impl PageTable {
    /// A fresh, empty table (one frame for the root).
    pub fn new<M: PhysMem>(mm: &mut Mm<M>) -> Option<PageTable> {
        let root = mm.db.alloc()?;
        mm.mem.zero_frame(root);
        Some(PageTable { root })
    }

    /// Wrap an existing root (the kernel's boot table). Do not `destroy` it.
    pub fn from_root(root: Pfn) -> PageTable {
        PageTable { root }
    }

    pub fn root(&self) -> Pfn {
        self.root
    }

    /// Map the page at `va` to frame `pfn`. Allocates intermediate tables as needed;
    /// if it cannot, nothing changes.
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
        if !is_canonical(va) {
            return Err(MapError::NonCanonical);
        }
        perms.validate()?;
        // Walk down, remembering where the path stops.
        let mut table = self.root;
        let mut level = 2;
        loop {
            let slot = (table << 12) + index(va, level) * 8;
            let pte = mm.mem.read_u64(slot);
            if pte & V == 0 {
                break;
            }
            if is_leaf(pte) {
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
            table = ppn(pte);
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
            mm.mem.write_u64(slot, (fresh << 10) | V);
            table = fresh;
            level -= 1;
        }
        let slot = (table << 12) + index(va, 0) * 8;
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
        mm.mem.write_u64(slot, pte);
        Ok(())
    }

    /// Unmap the page at `va`, returning what it mapped. Tables left empty are freed.
    /// **The caller must flush the TLB (`sfence.vma`) before any frame freed here is
    /// reused**: this crate has no notion of TLBs.
    pub fn unmap<M: PhysMem>(&mut self, mm: &mut Mm<M>, va: u64) -> Option<(Pfn, Perms)> {
        if !va.is_multiple_of(PAGE_SIZE) || !is_canonical(va) {
            return None;
        }
        // path[i] = (table frame, slot address) for level 2 - i
        let mut path: [(Pfn, u64); 3] = [(0, 0); 3];
        let mut table = self.root;
        for level in (0..=2u32).rev() {
            let slot = (table << 12) + index(va, level) * 8;
            path[(2 - level) as usize] = (table, slot);
            let pte = mm.mem.read_u64(slot);
            if pte & V == 0 {
                return None;
            }
            if is_leaf(pte) {
                if level != 0 {
                    return None; // a huge page is not ours to unmap one 4 KiB page at a time
                }
                mm.mem.write_u64(slot, 0);
                let result = (ppn(pte), perms_of(pte));
                // Free tables that became empty, bottom up, never the root.
                for up in (1..=2usize).rev() {
                    let (frame, _) = path[up];
                    let empty = (0..512).all(|i| mm.mem.read_u64((frame << 12) + i * 8) & V == 0);
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
            table = ppn(pte);
        }
        None
    }

    /// Translate `va` (any byte of a page). Huge leaves translate too.
    pub fn translate<M: PhysMem>(&self, mm: &Mm<M>, va: u64) -> Option<Translation> {
        if !is_canonical(va) {
            return None;
        }
        let mut table = self.root;
        for level in (0..=2u32).rev() {
            let pte = mm.mem.read_u64((table << 12) + index(va, level) * 8);
            if pte & V == 0 {
                return None;
            }
            if is_leaf(pte) {
                // For a huge leaf the low frame-number bits come from the address.
                let span = 1u64 << (9 * level);
                let offset_pages = (va >> 12) & (span - 1);
                return Some(Translation {
                    pfn: ppn(pte) + offset_pages,
                    perms: perms_of(pte),
                });
            }
            if level == 0 {
                return None;
            }
            table = ppn(pte);
        }
        None
    }

    /// Call `f(va, translation, level)` for every leaf mapping, in address order.
    /// `level` is 0 for a 4 KiB page, 1 for 2 MiB, 2 for 1 GiB. Used to audit a
    /// table (for example: no leaf is both writable and executable).
    pub fn for_each_leaf<M: PhysMem>(&self, mm: &Mm<M>, f: &mut dyn FnMut(u64, Translation, u32)) {
        fn walk<M: PhysMem>(
            mm: &Mm<M>,
            table: Pfn,
            level: u32,
            prefix: u64,
            f: &mut dyn FnMut(u64, Translation, u32),
        ) {
            for i in 0..512u64 {
                let pte = mm.mem.read_u64((table << 12) + i * 8);
                if pte & V == 0 {
                    continue;
                }
                let va = prefix | (i << (12 + 9 * level));
                if is_leaf(pte) {
                    // Sv39 addresses are sign-extended from bit 38.
                    let full = if va & (1 << 38) != 0 {
                        va | 0xffff_ff80_0000_0000
                    } else {
                        va
                    };
                    f(
                        full,
                        Translation {
                            pfn: ppn(pte),
                            perms: perms_of(pte),
                        },
                        level,
                    );
                } else if level > 0 {
                    walk(mm, ppn(pte), level - 1, va, f);
                }
            }
        }
        walk(mm, self.root, 2, 0, f);
    }

    /// Free every table frame (the root too). The leaves must be unmapped (or are
    /// not ours to free): their frames are not touched.
    pub fn destroy<M: PhysMem>(self, mm: &mut Mm<M>) {
        fn walk<M: PhysMem>(mm: &mut Mm<M>, table: Pfn, level: u32) {
            if level > 0 {
                for i in 0..512 {
                    let pte = mm.mem.read_u64((table << 12) + i * 8);
                    if pte & V != 0 && !is_leaf(pte) {
                        walk(mm, ppn(pte), level - 1);
                    }
                }
            }
            let _ = mm.db.dec(table);
        }
        walk(mm, self.root, 2);
    }
}

#[allow(dead_code)]
const _: u64 = FLAGS;
