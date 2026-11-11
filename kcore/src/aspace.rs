//! VMOs (memory objects), address spaces (regions of VMOs mapped at virtual
//! addresses) and the page-fault path that ties them together.
//!
//! Rule (OS-ARCHITECTURE.md, section 6): every memory operation goes through the
//! VMO interface. A fault asks one question, "VMO, give me a page for this
//! offset", and lazy commit is the first answer; copy-on-write and pagers plug in
//! behind the same question later.
//!
//! Reference counts (see `frame`): a VMO holds one reference to each page it has
//! committed, and each mapping of a page holds one more. So a mapping keeps its
//! frame alive after the VMO is destroyed, and a frame returns to the free list
//! exactly when the last holder lets go.

use crate::frame::{Pfn, PAGE_SIZE};
use crate::pagetable::{is_canonical, MapError, PageTable, Perms, PhysMem};
use crate::Mm;
use alloc::collections::BTreeMap;

/// The kind of access that faulted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
    Exec,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultError {
    /// The address is in no region.
    NoRegion,
    /// The region does not allow this kind of access.
    Denied,
    /// The region's VMO has been destroyed and the page was never mapped.
    VmoGone,
    NoMemory,
    /// The page table refused the mapping for a reason other than memory (for
    /// example something is already mapped there with other permissions).
    MapFailed(MapError),
}

/// The largest region (1 GiB), so that unmapping one is bounded work.
pub const MAX_REGION_PAGES: u64 = 1 << 18;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionError {
    Misaligned,
    NonCanonical,
    /// Zero pages, or a range that wraps or leaves the address space.
    BadRange,
    BadPerms(MapError),
    Overlap,
    NoSuchVmo,
    /// The pages asked for are not all inside the VMO.
    OutsideVmo,
    NoRegion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmoGone;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmoError {
    Gone,
    OutOfRange,
    NoMemory,
}

struct Vmo {
    pages: u64,
    /// Committed pages by index: the frame, on which the VMO holds one reference.
    committed: BTreeMap<u64, Pfn>,
}

/// All memory objects, by id.
#[derive(Default)]
pub struct Vmos {
    map: BTreeMap<u64, Vmo>,
    next: u64,
}

impl Vmos {
    pub fn new() -> Vmos {
        Vmos::default()
    }

    /// A new lazily committed VMO of `pages` pages (nothing allocated yet).
    pub fn create(&mut self, pages: u64) -> u64 {
        self.next += 1;
        self.map.insert(
            self.next,
            Vmo {
                pages,
                committed: BTreeMap::new(),
            },
        );
        self.next
    }

    /// How many VMOs exist (a leak counter for tests).
    pub fn count(&self) -> usize {
        self.map.len()
    }

    pub fn exists(&self, id: u64) -> bool {
        self.map.contains_key(&id)
    }

    pub fn pages(&self, id: u64) -> Option<u64> {
        self.map.get(&id).map(|v| v.pages)
    }

    /// The frame backing a committed page.
    pub fn frame(&self, id: u64, page: u64) -> Option<Pfn> {
        self.map.get(&id)?.committed.get(&page).copied()
    }

    /// Destroy a VMO, dropping its references. Pages still mapped stay alive.
    pub fn destroy<M: PhysMem>(&mut self, mm: &mut Mm<M>, id: u64) -> Result<(), VmoGone> {
        let vmo = self.map.remove(&id).ok_or(VmoGone)?;
        for (_, pfn) in vmo.committed {
            let dropped = mm.db.dec(pfn);
            debug_assert!(
                dropped.is_ok(),
                "a committed page holds the VMO's reference"
            );
        }
        Ok(())
    }

    /// Write bytes into a VMO at `offset`, committing pages as needed (loading a program
    /// image, filling a buffer before it is shared). Nothing is written if the range does
    /// not fit inside the VMO.
    pub fn write<M: PhysMem>(
        &mut self,
        mm: &mut Mm<M>,
        id: u64,
        offset: u64,
        data: &[u8],
    ) -> Result<(), VmoError> {
        self.access(mm, id, offset, data.len(), |mm, pa, range| {
            mm.mem.write_bytes(pa, &data[range])
        })
    }

    /// Read bytes out of a VMO (uncommitted pages read as zeros and stay uncommitted).
    pub fn read<M: PhysMem>(
        &mut self,
        mm: &mut Mm<M>,
        id: u64,
        offset: u64,
        out: &mut [u8],
    ) -> Result<(), VmoError> {
        let len = out.len();
        let vmo = self.map.get(&id).ok_or(VmoError::Gone)?;
        let end = offset.checked_add(len as u64).ok_or(VmoError::OutOfRange)?;
        if end > vmo.pages * PAGE_SIZE {
            return Err(VmoError::OutOfRange);
        }
        let mut done = 0;
        while done < len {
            let at = offset + done as u64;
            let (page, inner) = (at / PAGE_SIZE, (at % PAGE_SIZE) as usize);
            let n = (PAGE_SIZE as usize - inner).min(len - done);
            match vmo.committed.get(&page) {
                Some(&pfn) => mm
                    .mem
                    .read_bytes((pfn << 12) + inner as u64, &mut out[done..done + n]),
                None => out[done..done + n].fill(0),
            }
            done += n;
        }
        Ok(())
    }

    fn access<M: PhysMem>(
        &mut self,
        mm: &mut Mm<M>,
        id: u64,
        offset: u64,
        len: usize,
        mut each: impl FnMut(&mut Mm<M>, u64, core::ops::Range<usize>),
    ) -> Result<(), VmoError> {
        let pages = self.map.get(&id).ok_or(VmoError::Gone)?.pages;
        let end = offset.checked_add(len as u64).ok_or(VmoError::OutOfRange)?;
        if end > pages * PAGE_SIZE {
            return Err(VmoError::OutOfRange);
        }
        // Commit everything first so a failure part-way leaves no half-written data.
        let mut at = offset;
        while at < end {
            self.page_for(mm, id, at / PAGE_SIZE)
                .map_err(|_| VmoError::NoMemory)?;
            at = (at / PAGE_SIZE + 1) * PAGE_SIZE;
        }
        let vmo = self.map.get(&id).expect("exists");
        let mut done = 0;
        while done < len {
            let at = offset + done as u64;
            let (page, inner) = (at / PAGE_SIZE, at % PAGE_SIZE);
            let n = ((PAGE_SIZE - inner) as usize).min(len - done);
            let pfn = vmo.committed[&page];
            each(mm, (pfn << 12) + inner, done..done + n);
            done += n;
        }
        Ok(())
    }

    /// "Give me a page for this offset": the committed frame, allocating a zeroed
    /// one on first touch. The VMO keeps its own reference.
    fn page_for<M: PhysMem>(
        &mut self,
        mm: &mut Mm<M>,
        id: u64,
        page: u64,
    ) -> Result<Pfn, FaultError> {
        let vmo = self.map.get_mut(&id).ok_or(FaultError::VmoGone)?;
        if let Some(&pfn) = vmo.committed.get(&page) {
            return Ok(pfn);
        }
        let pfn = mm.db.alloc().ok_or(FaultError::NoMemory)?;
        mm.mem.zero_frame(pfn);
        vmo.committed.insert(page, pfn);
        Ok(pfn)
    }
}

struct Region {
    pages: u64,
    vmo: u64,
    vmo_page: u64,
    perms: Perms,
}

/// A page table plus the regions that say what its holes mean.
pub struct AddressSpace {
    table: PageTable,
    regions: BTreeMap<u64, Region>,
}

impl AddressSpace {
    pub fn new(table: PageTable) -> AddressSpace {
        AddressSpace {
            table,
            regions: BTreeMap::new(),
        }
    }

    pub fn table(&self) -> &PageTable {
        &self.table
    }

    /// Describe `pages` pages at `start` as a window onto `vmo` from `vmo_page`.
    /// Nothing is mapped until a fault asks for it.
    pub fn map_region(
        &mut self,
        vmos: &Vmos,
        start: u64,
        pages: u64,
        vmo: u64,
        vmo_page: u64,
        perms: Perms,
    ) -> Result<(), RegionError> {
        if !start.is_multiple_of(PAGE_SIZE) {
            return Err(RegionError::Misaligned);
        }
        if !is_canonical(start) {
            return Err(RegionError::NonCanonical);
        }
        if pages > MAX_REGION_PAGES {
            return Err(RegionError::BadRange);
        }
        let len = pages
            .checked_mul(PAGE_SIZE)
            .filter(|&l| l > 0)
            .ok_or(RegionError::BadRange)?;
        let end = start.checked_add(len).ok_or(RegionError::BadRange)?;
        if !is_canonical(end - 1) || (start as i64) < 0 && (end - 1) as i64 >= 0 {
            return Err(RegionError::BadRange);
        }
        perms.validate().map_err(RegionError::BadPerms)?;
        let total = vmos.pages(vmo).ok_or(RegionError::NoSuchVmo)?;
        if vmo_page.checked_add(pages).is_none_or(|e| e > total) {
            return Err(RegionError::OutsideVmo);
        }
        // Overlap with the previous region or any region starting inside this one.
        if let Some((&s, r)) = self.regions.range(..=start).next_back() {
            if s + r.pages * PAGE_SIZE > start {
                return Err(RegionError::Overlap);
            }
        }
        if self.regions.range(start..end).next().is_some() {
            return Err(RegionError::Overlap);
        }
        self.regions.insert(
            start,
            Region {
                pages,
                vmo,
                vmo_page,
                perms,
            },
        );
        Ok(())
    }

    /// Does any region of this address space map VMO `vmo`?
    pub fn maps_vmo(&self, vmo: u64) -> bool {
        self.regions.values().any(|r| r.vmo == vmo)
    }

    /// The permissions of the region covering `va`, if there is one (mapped yet or not).
    pub fn region_perms_at(&self, va: u64) -> Option<Perms> {
        let (&start, r) = self.regions.range(..=va).next_back()?;
        (va < start + r.pages * PAGE_SIZE).then_some(r.perms)
    }

    /// Remove the region starting at `start`, unmapping its pages and dropping the
    /// mappings' references. **The caller must flush the TLB (`sfence.vma`) before
    /// any frame freed here can be reused**: this crate has no notion of TLBs.
    pub fn unmap_region<M: PhysMem>(
        &mut self,
        mm: &mut Mm<M>,
        start: u64,
    ) -> Result<(), RegionError> {
        let region = self.regions.remove(&start).ok_or(RegionError::NoRegion)?;
        for p in 0..region.pages {
            if let Some((pfn, _)) = self.table.unmap(mm, start + p * PAGE_SIZE) {
                let dropped = mm.db.dec(pfn);
                debug_assert!(
                    dropped.is_ok(),
                    "a mapped frame holds the mapping's reference"
                );
            }
        }
        Ok(())
    }

    /// Handle a page fault at `va` for `access`: map the page if the region allows
    /// the access. A fault on a page that is already mapped (and allowed) is
    /// spurious and succeeds.
    pub fn handle_fault<M: PhysMem>(
        &mut self,
        mm: &mut Mm<M>,
        vmos: &mut Vmos,
        va: u64,
        access: Access,
    ) -> Result<(), FaultError> {
        let (&start, region) = self
            .regions
            .range(..=va)
            .next_back()
            .ok_or(FaultError::NoRegion)?;
        let page = (va - start) / PAGE_SIZE;
        if page >= region.pages {
            return Err(FaultError::NoRegion);
        }
        let allowed = match access {
            Access::Read => region.perms.read,
            Access::Write => region.perms.write,
            Access::Exec => region.perms.exec,
        };
        if !allowed {
            return Err(FaultError::Denied);
        }
        let page_va = start + page * PAGE_SIZE;
        if let Some(existing) = self.table.translate(mm, page_va) {
            // Already mapped: a spurious fault if the mapping allows the access
            // (for example another hart mapped it first). If it does not, retrying
            // would fault forever, so refuse.
            let ok = match access {
                Access::Read => existing.perms.read,
                Access::Write => existing.perms.write,
                Access::Exec => existing.perms.exec,
            };
            return if ok { Ok(()) } else { Err(FaultError::Denied) };
        }
        let (vmo, perms) = (region.vmo, region.perms);
        let pfn = vmos.page_for(mm, vmo, region.vmo_page + page)?;
        mm.db.inc(pfn).map_err(|_| FaultError::NoMemory)?;
        if let Err(e) = self.table.map(mm, page_va, pfn, perms) {
            let dropped = mm.db.dec(pfn);
            debug_assert!(
                dropped.is_ok(),
                "the reference just taken must be droppable"
            );
            return Err(match e {
                MapError::NoMemory => FaultError::NoMemory,
                other => FaultError::MapFailed(other),
            });
        }
        Ok(())
    }

    /// Unmap everything and free the tables of a process whose table shares the kernel's
    /// upper half (`PageTable::new_with_kernel_half`): the shared half is left alone.
    pub fn destroy_user<M: PhysMem>(mut self, mm: &mut Mm<M>) {
        let starts: alloc::vec::Vec<u64> = self.regions.keys().copied().collect();
        for start in starts {
            let _ = self.unmap_region(mm, start);
        }
        self.table.destroy_user(mm);
    }

    /// Unmap everything and free the tables.
    pub fn destroy<M: PhysMem>(mut self, mm: &mut Mm<M>) {
        let starts: alloc::vec::Vec<u64> = self.regions.keys().copied().collect();
        for start in starts {
            let _ = self.unmap_region(mm, start);
        }
        self.table.destroy(mm);
    }
}
