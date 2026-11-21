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
use crate::job::{JobId, Jobs, Resource};
use crate::pagemap::PageMap;
use crate::pagetable::{MapError, PageTable, PageTableFormat, Perms, PhysMem, Sv39};
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

/// What a fault's first step found (`AddressSpace::fault_lookup`).
#[derive(Debug, PartialEq, Eq)]
pub enum Lookup {
    /// The page is already mapped and allows the access: nothing to do.
    Mapped(MappedPages),
    /// The page to fetch from the VMO and map.
    Plan(FaultPlan),
}

/// The region and page a fault is about, with the region's generation: enough to fetch the page without holding the address space and to
/// notice afterwards that the region went away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaultPlan {
    start: u64,
    page: u64,
    page_va: u64,
    vmo: u64,
    vmo_page: u64,
    perms: Perms,
    pages: u64,
    gen: u64,
}

impl FaultPlan {
    /// The VMO and the page in it the fault needs.
    pub fn vmo_page(&self) -> (u64, u64) {
        (self.vmo, self.vmo_page.saturating_add(self.page))
    }
}

/// Why `AddressSpace::fault_map` did not map.
#[derive(Debug, PartialEq, Eq)]
pub enum FaultMapError {
    /// The region was unmapped (or replaced) since the lookup: look the fault up again.
    Stale,
    Fault(FaultError),
}

/// The largest region (1 GiB), so that unmapping one is bounded work.
pub const MAX_REGION_PAGES: u64 = 1 << 18;

/// What a protect changed (MM7): the caller owes the other harts a shootdown after a downgrade and an instruction-cache sync for new code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProtectStats {
    /// Some page lost a permission it had (a downgrade): every hart must drop its translations of the range.
    pub downgraded: bool,
    /// Pages that became executable (PL7).
    pub exec_pages: u64,
}

/// Why `protect_range` refused (nothing changed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtectError {
    /// Not page aligned, empty, or outside the address space.
    BadRange,
    /// Part of the range is not mapped.
    NotMapped,
    /// A permission asked for is beyond what the mapping may be raised to, or write and execute together.
    Denied,
    /// Splitting at the edges would pass `MAX_REGIONS`.
    TooMany,
}

/// A region as `process_map_info` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionInfo {
    pub start: u64,
    pub pages: u64,
    pub vmo: u64,
    pub vmo_page: u64,
    pub perms: Perms,
    pub max_perms: Perms,
}

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
    /// The address space already has `MAX_REGIONS` regions.
    TooMany,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmoGone;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmoError {
    Gone,
    OutOfRange,
    NoMemory,
    /// Device memory is never read or written through the kernel.
    Device,
    /// The VMO's memory is fixed (DMA frames are committed for good).
    Fixed,
    /// The VMO was not made resizable.
    NotResizable,
}

/// What backs a VMO.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmoKind {
    /// Lazily committed anonymous memory.
    Plain,
    /// Device memory (MMIO): fixed frames the frame database does not manage; never freed, never
    /// read or written through the kernel, never charged as pages.
    Device,
    /// Physically contiguous RAM committed at creation, whose address a DMA-right holder may learn.
    Dma,
}

struct Vmo {
    kind: VmoKind,
    /// The job charged for this VMO's committed pages (it holds one use of it).
    job: JobId,
    pages: u64,
    /// Holders of the VMO: its creator (the object a handle names, or the kernel for its own) and every region that maps it (L15). The VMO is
    /// freed when the last is gone.
    refs: u32,
    /// Committed pages by index: the frame, on which the VMO holds one reference.
    committed: PageMap,
    /// Made with the RESIZABLE flag: `set_size` may change `pages` (MM6).
    resizable: bool,
}

struct VmoSlot {
    gen: u32,
    vmo: Option<Vmo>,
}

/// All memory objects, by id.
#[derive(Default)]
pub struct Vmos {
    /// The VMOs, in slots; an id is `(generation << 32) | (index + 1)`, so a destroyed VMO's id is never valid again. A slab, not a `BTreeMap`,
    /// because creating a VMO must be able to report that it has no memory (KT9): `reserve_one` makes the room, and `create` then cannot fail.
    slots: alloc::vec::Vec<VmoSlot>,
    free: alloc::vec::Vec<u32>,
    live: usize,
    /// Pages held by live DMA VMOs.
    dma_pages: u64,
}

impl Vmos {
    pub fn new() -> Vmos {
        Vmos::default()
    }

    /// Make room for one more VMO, in memory: after `Ok`, `create`, `create_in`, `create_physical` and `create_dma` do not allocate for their
    /// own bookkeeping, and destroying any VMO never does (KT9).
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: free.len() <= slots.len()
    pub fn reserve_one(&mut self) -> Result<(), abi::status::Status> {
        let no_memory = |_| abi::status::Status::NoMemory;
        self.slots.try_reserve(1).map_err(no_memory)?;
        self.free
            .try_reserve(self.slots.len() + 1 - self.free.len())
            .map_err(no_memory)
    }

    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: the index is below 2^32 (slots are bounded by the object quota)
    fn place(&mut self, vmo: Vmo) -> u64 {
        if let Some(i) = self.free.pop() {
            if let Some(slot) = self.slots.get_mut(i as usize) {
                slot.vmo = Some(vmo);
                self.live += 1;
                return (u64::from(slot.gen) << 32) | (u64::from(i) + 1);
            }
            // (not reached: a free index is a slot)
            self.free.push(i);
        }
        self.live += 1;
        self.slots.push(VmoSlot {
            gen: 0,
            vmo: Some(vmo),
        });
        self.slots.len() as u64
    }

    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: the index was bounds-checked by `get`
    fn slot_index(&self, id: u64) -> Option<usize> {
        let index = (id & 0xffff_ffff).checked_sub(1)? as usize;
        let s = self.slots.get(index)?;
        (s.vmo.is_some() && u64::from(s.gen) == id >> 32).then_some(index)
    }

    fn vmo(&self, id: u64) -> Option<&Vmo> {
        self.slots.get(self.slot_index(id)?)?.vmo.as_ref()
    }

    /// The VMO of `id` for modification, on the slot table alone, so that the caller can still use the other fields (the job ledger) while it holds the VMO.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: as `slot_index`
    fn vmo_in(slots: &mut [VmoSlot], id: u64) -> Option<&mut Vmo> {
        let index = (id & 0xffff_ffff).checked_sub(1)? as usize;
        let s = slots.get_mut(index)?;
        if u64::from(s.gen) != id >> 32 {
            return None;
        }
        s.vmo.as_mut()
    }

    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: the index came from `slot_index`; a slot whose generation is exhausted is retired
    fn take(&mut self, id: u64) -> Option<Vmo> {
        let i = self.slot_index(id)?;
        let slot = &mut self.slots[i];
        let v = slot.vmo.take();
        // (CNT1: a slot whose 32-bit generation is exhausted is retired, not wrapped: its ids stay invalid for ever)
        if slot.gen < u32::MAX {
            slot.gen += 1;
            self.free.push(i as u32);
        }
        self.live -= 1;
        v
    }

    /// A new lazily committed VMO of `pages` pages (nothing allocated yet), charged to the
    /// root job (the kernel's own VMOs).
    pub fn create(&mut self, jobs: &mut Jobs, pages: u64) -> u64 {
        self.create_in(jobs, pages, JobId::ROOT)
    }

    /// A new lazily committed VMO whose pages are charged to `job` as they are committed.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: the id counter is a u64 that starts at 1 and gains one per VMO, bounded by the object quota
    pub fn create_in(&mut self, jobs: &mut Jobs, pages: u64, job: JobId) -> u64 {
        jobs.retain(job);
        self.place(Vmo {
            kind: VmoKind::Plain,
            job,
            pages,
            refs: 1,
            committed: PageMap::new(),
            resizable: false,
        })
    }

    /// A VMO over device memory starting at frame `first` (the caller has checked the range is
    /// inside an allowed device window). Holds one use of the root job.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: the id counter as above; `first + i` for i < pages: the caller passed the device-window check (a range inside one window, so no overflow)
    pub fn create_physical(&mut self, jobs: &mut Jobs, first: Pfn, pages: u64) -> u64 {
        jobs.retain(JobId::ROOT);
        self.place(Vmo {
            kind: VmoKind::Device,
            job: JobId::ROOT,
            pages,
            refs: 1,
            committed: PageMap::from_run(pages, first),
            resizable: false,
        })
    }

    /// A VMO of `pages` physically contiguous zeroed frames, charged to `job` now. At most
    /// `MAX_DMA_PAGES` per object and `MAX_DMA_TOTAL_PAGES` in the system.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: pages is checked against MAX_DMA_PAGES and the system total just above, so the sums are small; `first + i` is inside the contiguous run the allocator returned
    pub fn create_dma<M: PhysMem>(
        &mut self,
        mm: &mut Mm<M>,
        jobs: &mut Jobs,
        pages: u64,
        job: JobId,
    ) -> Result<u64, VmoError> {
        use abi::limits::{MAX_DMA_PAGES, MAX_DMA_TOTAL_PAGES};
        if pages == 0 || pages > MAX_DMA_PAGES || self.dma_pages + pages > MAX_DMA_TOTAL_PAGES {
            return Err(VmoError::OutOfRange);
        }
        jobs.charge(job, Resource::Pages, pages)
            .map_err(|_| VmoError::NoMemory)?;
        let Some(first) = mm.db.alloc_contiguous(pages as usize) else {
            jobs.credit(job, Resource::Pages, pages);
            return Err(VmoError::NoMemory);
        };
        for i in 0..pages {
            mm.mem.zero_frame(first + i);
        }
        jobs.retain(job);
        self.dma_pages += pages;
        Ok(self.place(Vmo {
            kind: VmoKind::Dma,
            job,
            pages,
            refs: 1,
            committed: PageMap::from_run(pages, first),
            resizable: false,
        }))
    }

    pub fn kind(&self, id: u64) -> Option<VmoKind> {
        self.vmo(id).map(|v| v.kind)
    }

    /// The physical address of a DMA VMO (its first byte).
    pub fn phys_addr(&self, id: u64) -> Option<u64> {
        let v = self.vmo(id).filter(|v| v.kind == VmoKind::Dma)?;
        v.committed.get(0).map(|pfn| pfn << 12)
    }

    /// How many VMOs exist (a leak counter for tests).
    pub fn count(&self) -> usize {
        self.live
    }

    pub fn exists(&self, id: u64) -> bool {
        self.vmo(id).is_some()
    }

    pub fn pages(&self, id: u64) -> Option<u64> {
        self.vmo(id).map(|v| v.pages)
    }

    /// The frame backing a committed page.
    pub fn frame(&self, id: u64, page: u64) -> Option<Pfn> {
        self.vmo(id)?.committed.get(page)
    }

    /// Take one more reference on a VMO (a region maps it). `Err` if the id is not a live VMO.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: a VMO has one reference per region plus its creator's: bounded by the region and object limits, far below u32::MAX
    pub fn retain(&mut self, id: u64) -> Result<(), VmoGone> {
        let v = Self::vmo_in(&mut self.slots, id).ok_or(VmoGone)?;
        v.refs += 1;
        Ok(())
    }

    /// How many holders a live VMO has (its creator and the regions that map it).
    pub fn refs(&self, id: u64) -> Option<u32> {
        self.vmo(id).map(|v| v.refs)
    }

    /// Drop one reference on a VMO; the last one frees it: its pages go back and its job is credited. The creator's reference is dropped when the
    /// object a handle names is destroyed (or by the kernel for its own VMOs), a mapping's when the region goes (L15: no scan for VMOs that
    /// nothing maps any more). Frames still mapped somewhere hold references of their own.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: refs >= 1 for a live VMO; dma_pages was increased by exactly vmo.pages when the VMO was created, so it is at least vmo.pages
    pub fn release<M: PhysMem>(
        &mut self,
        mm: &mut Mm<M>,
        jobs: &mut Jobs,
        id: u64,
    ) -> Result<(), VmoGone> {
        let v = Self::vmo_in(&mut self.slots, id).ok_or(VmoGone)?;
        v.refs -= 1;
        if v.refs > 0 {
            return Ok(());
        }
        let vmo = self.take(id).ok_or(VmoGone)?;
        if vmo.kind == VmoKind::Device {
            // device frames are not ours: nothing was charged and nothing is freed
            jobs.release(vmo.job);
            return Ok(());
        }
        if vmo.kind == VmoKind::Dma {
            self.dma_pages -= vmo.pages;
        }
        jobs.credit(vmo.job, Resource::Pages, vmo.committed.len() as u64);
        jobs.release(vmo.job);
        let mut committed = vmo.committed;
        committed.drain_pfns(|pfn| {
            let dropped = mm.db.dec(pfn);
            debug_assert!(
                dropped.is_ok(),
                "a committed page holds the VMO's reference"
            );
        });
        Ok(())
    }

    /// Write bytes into a VMO at `offset`, committing pages as needed (loading a program
    /// image, filling a buffer before it is shared). Nothing is written if the range does
    /// not fit inside the VMO.
    #[allow(clippy::indexing_slicing)] // INVARIANT: `range` comes from `access`, which computed it inside `data`
    pub fn write<M: PhysMem>(
        &mut self,
        mm: &mut Mm<M>,
        jobs: &mut Jobs,
        id: u64,
        offset: u64,
        data: &[u8],
    ) -> Result<(), VmoError> {
        self.access(mm, jobs, id, offset, data.len(), |mm, pa, range| {
            mm.mem.write_bytes(pa, &data[range])
        })
    }

    /// Read bytes out of a VMO (uncommitted pages read as zeros and stay uncommitted).
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: offset + len was checked with checked_add against the VMO's size (pages <= MAX_VMO_PAGES, so the product is small); `done < len` bounds every slice
    pub fn read<M: PhysMem>(
        &mut self,
        mm: &mut Mm<M>,
        id: u64,
        offset: u64,
        out: &mut [u8],
    ) -> Result<(), VmoError> {
        let len = out.len();
        let vmo = self.vmo(id).ok_or(VmoError::Gone)?;
        if vmo.kind == VmoKind::Device {
            return Err(VmoError::Device);
        }
        let end = offset.checked_add(len as u64).ok_or(VmoError::OutOfRange)?;
        if end > vmo.pages * PAGE_SIZE {
            return Err(VmoError::OutOfRange);
        }
        let mut done = 0;
        while done < len {
            let at = offset + done as u64;
            let (page, inner) = (at / PAGE_SIZE, (at % PAGE_SIZE) as usize);
            let n = (PAGE_SIZE as usize - inner).min(len - done);
            match vmo.committed.get(page) {
                Some(pfn) => mm
                    .mem
                    .read_bytes((pfn << 12) + inner as u64, &mut out[done..done + n]),
                None => out[done..done + n].fill(0),
            }
            done += n;
        }
        Ok(())
    }

    #[allow(
        clippy::arithmetic_side_effects,
        clippy::expect_used,
        clippy::indexing_slicing
    )] // INVARIANT: as `read`: the range was checked first; the committed lookup is of a page `page_for` committed in the loop above (nothing runs in between)
    fn access<M: PhysMem>(
        &mut self,
        mm: &mut Mm<M>,
        jobs: &mut Jobs,
        id: u64,
        offset: u64,
        len: usize,
        mut each: impl FnMut(&mut Mm<M>, u64, core::ops::Range<usize>),
    ) -> Result<(), VmoError> {
        let vmo = self.vmo(id).ok_or(VmoError::Gone)?;
        if vmo.kind == VmoKind::Device {
            return Err(VmoError::Device);
        }
        let pages = vmo.pages;
        let end = offset.checked_add(len as u64).ok_or(VmoError::OutOfRange)?;
        if end > pages * PAGE_SIZE {
            return Err(VmoError::OutOfRange);
        }
        // Commit everything first so a failure part-way leaves no half-written data.
        let mut at = offset;
        while at < end {
            self.page_for(mm, jobs, None, id, at / PAGE_SIZE)
                .map_err(|_| VmoError::NoMemory)?;
            at = (at / PAGE_SIZE + 1) * PAGE_SIZE;
        }
        let vmo = self.vmo(id).expect("exists");
        let mut done = 0;
        while done < len {
            let at = offset + done as u64;
            let (page, inner) = (at / PAGE_SIZE, at % PAGE_SIZE);
            let n = ((PAGE_SIZE - inner) as usize).min(len - done);
            let pfn = vmo.committed.get(page).expect("committed above");
            each(mm, (pfn << 12) + inner, done..done + n);
            done += n;
        }
        Ok(())
    }

    /// Commit pages `first .. first + pages` of VMO `id` now: charge the VMO's job and take frames, so that
    /// later touches can neither fail nor be charged (slice V: the creator pays, up front). All or
    /// nothing: if the job's limit or the frames run out, everything this call committed is given back
    /// and `NoMemory` is returned, with the VMO and the ledger exactly as before. Pages already
    /// committed are left as they are. Device memory is never committed this way.
    #[allow(clippy::expect_used, clippy::indexing_slicing)] // INVARIANT: the id was looked up at the top and `committed` was checked in the same call; first + pages was checked against the VMO's size
    pub fn commit<M: PhysMem>(
        &mut self,
        mm: &mut Mm<M>,
        jobs: &mut Jobs,
        id: u64,
        first: u64,
        pages: u64,
    ) -> Result<(), VmoError> {
        let vmo = self.vmo(id).ok_or(VmoError::Gone)?;
        if vmo.kind == VmoKind::Device {
            return Err(VmoError::Device);
        }
        let end = first.checked_add(pages).ok_or(VmoError::OutOfRange)?;
        if end > vmo.pages {
            return Err(VmoError::OutOfRange);
        }
        // Refuse at once what cannot fit (no frames taken and given back for nothing): count the pages
        // this call would commit and compare with the job's room and the free frames.
        let needed = (first..end).filter(|p| !vmo.committed.contains(*p)).count() as u64;
        if needed > jobs.room(vmo.job, Resource::Pages) || needed > mm.db.free_count() as u64 {
            return Err(VmoError::NoMemory);
        }
        // (the list of this call's commits, for undoing them, is allocated before the first frame is taken: KT9)
        let mut done: alloc::vec::Vec<u64> = alloc::vec::Vec::new();
        done.try_reserve_exact(needed as usize)
            .map_err(|_| VmoError::NoMemory)?;
        for page in first..end {
            if self.vmo(id).is_some_and(|v| v.committed.contains(page)) {
                continue;
            }
            match self.page_for(mm, jobs, None, id, page) {
                Ok(_) => done.push(page),
                Err(_) => {
                    // undo this call's commits: frames back, charges back
                    let vmo = Self::vmo_in(&mut self.slots, id).expect("checked above");
                    for p in done {
                        if let Some(pfn) = vmo.committed.remove(p) {
                            let _ = mm.db.dec(pfn);
                            jobs.credit(vmo.job, Resource::Pages, 1);
                        }
                    }
                    return Err(VmoError::NoMemory);
                }
            }
        }
        Ok(())
    }

    /// Are all pages `first .. first + pages` of VMO `id` committed? (Device memory counts as
    /// committed: it is fixed.) False for an unknown VMO or a range past its end.
    pub fn is_committed(&self, id: u64, first: u64, pages: u64) -> bool {
        let Some(vmo) = self.vmo(id) else {
            return false;
        };
        let Some(end) = first.checked_add(pages) else {
            return false;
        };
        if end > vmo.pages {
            return false;
        }
        vmo.kind == VmoKind::Device || (first..end).all(|p| vmo.committed.contains(p))
    }

    /// Pages of VMO `id` that have a frame now (committed by a fault, a write or `commit`).
    pub fn committed_pages(&self, id: u64) -> Option<u64> {
        self.vmo(id).map(|v| v.committed.len() as u64)
    }

    /// Mark a new VMO resizable (`vmo_create` with the RESIZABLE flag, MM6): `set_size` may then change its size. Plain VMOs only.
    pub fn set_resizable(&mut self, id: u64) {
        if let Some(v) = Self::vmo_in(&mut self.slots, id) {
            if v.kind == VmoKind::Plain {
                v.resizable = true;
            }
        }
    }

    /// Was the VMO made resizable?
    pub fn is_resizable(&self, id: u64) -> bool {
        self.vmo(id).is_some_and(|v| v.resizable)
    }

    /// Give back the committed pages of `first .. first + pages` (MM6): the VMO drops its reference on each frame and its job is credited.
    /// **The caller has already unmapped the range from every address space** (`AddressSpace::unmap_vmo_pages`) and shot the other harts'
    /// translations down: a frame a mapping still holds would otherwise stay readable and survive. The pages read as zeros afterwards (a
    /// fresh page is committed on the next touch). Returns how many pages were freed. Plain VMOs only (device memory and DMA frames are
    /// fixed).
    pub fn decommit<M: PhysMem>(
        &mut self,
        mm: &mut Mm<M>,
        jobs: &mut Jobs,
        id: u64,
        first: u64,
        pages: u64,
    ) -> Result<u64, VmoError> {
        let vmo = Self::vmo_in(&mut self.slots, id).ok_or(VmoError::Gone)?;
        match vmo.kind {
            VmoKind::Device => return Err(VmoError::Device),
            VmoKind::Dma => return Err(VmoError::Fixed),
            VmoKind::Plain => {}
        }
        let end = first.checked_add(pages).ok_or(VmoError::OutOfRange)?;
        if end > vmo.pages {
            return Err(VmoError::OutOfRange);
        }
        let mut freed = 0u64;
        for page in first..end {
            if let Some(pfn) = vmo.committed.remove(page) {
                let _ = mm.db.dec(pfn);
                freed = freed.saturating_add(1);
            }
        }
        jobs.credit(vmo.job, Resource::Pages, freed);
        Ok(freed)
    }

    /// Change the size of a resizable VMO to `pages` (MM6). Growing changes the size only (nothing is committed). Shrinking gives back the
    /// committed pages past the new end (the caller has unmapped that range, as for `decommit`); a mapping that reaches past the new end
    /// keeps its length, and a touch beyond it is refused at the fault. Not for VMOs with a mapped range the caller did not clear.
    pub fn set_size<M: PhysMem>(
        &mut self,
        mm: &mut Mm<M>,
        jobs: &mut Jobs,
        id: u64,
        pages: u64,
    ) -> Result<(), VmoError> {
        let vmo = self.vmo(id).ok_or(VmoError::Gone)?;
        match vmo.kind {
            VmoKind::Device => return Err(VmoError::Device),
            VmoKind::Dma => return Err(VmoError::Fixed),
            VmoKind::Plain => {}
        }
        if !vmo.resizable {
            return Err(VmoError::NotResizable);
        }
        let old = vmo.pages;
        if pages < old {
            self.decommit(mm, jobs, id, pages, old.saturating_sub(pages))?;
        }
        if let Some(v) = Self::vmo_in(&mut self.slots, id) {
            v.pages = pages;
        }
        Ok(())
    }

    /// The job a VMO's pages are charged to.
    pub fn job_of(&self, id: u64) -> Option<JobId> {
        self.vmo(id).map(|v| v.job)
    }

    /// "Give me a page for this offset": the committed frame, allocating a zeroed
    /// one on first touch. The VMO keeps its own reference.
    fn page_for<M: PhysMem>(
        &mut self,
        mm: &mut Mm<M>,
        jobs: &mut Jobs,
        mut reserve: Option<&mut Reservation>,
        id: u64,
        page: u64,
    ) -> Result<Pfn, FaultError> {
        let vmo = Self::vmo_in(&mut self.slots, id).ok_or(FaultError::VmoGone)?;
        if let Some(pfn) = vmo.committed.get(page) {
            return Ok(pfn);
        }
        // (a VMO that was shrunk under a mapping: the page is past its end, the access is refused like one outside any region)
        if page >= vmo.pages {
            return Err(FaultError::NoRegion);
        }
        // The job's limit first (a full job is "out of memory" for its own programs only), from the address space's prepaid pages when
        // it has them (a fault), else charged now; then the frame. A frame that cannot be had gives the page back.
        let prepaid = match reserve.as_deref_mut() {
            Some(r) => r.take(jobs, vmo.job).map_err(|_| FaultError::NoMemory)?,
            None => {
                jobs.charge(vmo.job, Resource::Pages, 1)
                    .map_err(|_| FaultError::NoMemory)?;
                false
            }
        };
        let vmo_job = vmo.job;
        let give_back = |jobs: &mut Jobs, reserve: &mut Option<&mut Reservation>| match reserve
            .as_deref_mut()
        {
            Some(r) if prepaid => r.put_back(jobs, vmo_job),
            _ => jobs.credit(vmo_job, Resource::Pages, 1),
        };
        let Some(pfn) = mm.db.alloc() else {
            give_back(jobs, &mut reserve);
            return Err(FaultError::NoMemory);
        };
        // (the bookkeeping first: if it cannot be had, the frame and the charge go back and nothing was committed)
        if vmo.committed.insert(page, pfn).is_err() {
            let _ = mm.db.dec(pfn);
            give_back(jobs, &mut reserve);
            return Err(FaultError::NoMemory);
        }
        mm.mem.zero_frame(pfn);
        Ok(pfn)
    }
}

/// Pages a fault mapped (the faulting page and the committed ones around it): flush `first..=last`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MappedPages {
    pub first: u64,
    pub last: u64,
    /// How many of the pages this fault mapped are executable (PL7): the caller must make the instruction caches of every hart agree with
    /// memory before the thread runs them (`fence.i` here and on the other harts) when this is not 0. A page that was already mapped is not
    /// counted: a spurious fault maps nothing.
    pub exec: u64,
}

/// Fault-around works inside a window of this many pages (64 KiB), aligned (KERNEL-V1-SPEC 4.5): at most 15 neighbours.
pub const FAULT_AROUND_WINDOW_PAGES: u64 = 16;

struct Region {
    pages: u64,
    vmo: u64,
    vmo_page: u64,
    perms: Perms,
    /// What the mapping may be raised to (`process_protect`): read, write if the VMO handle had WRITE when it was mapped, execute if it had
    /// EXECUTE (MM7). Never more than the rights the mapper held.
    max_perms: Perms,
    /// Which incarnation of the region this is: a number from the address space's counter, never reused, so a fault that looked the region up
    /// and later finds a region with another number (or none) at that start knows it was unmapped (and maybe replaced) meanwhile.
    gen: u64,
}

/// Pages an address space prepays from a job at a time (KERNEL-V1-SPEC 4.7): the fault path takes a page from the reservation and
/// asks the ledger only to refill it, so the ledger is consulted once per batch, not once per fault.
pub const RESERVATION_PAGES: u64 = 64;
/// A refill takes at most `room / RESERVATION_ROOM_DIVISOR` pages (and at least one).
pub const RESERVATION_ROOM_DIVISOR: u64 = 8;

/// Pages charged to a job (they count as its usage, J1) but not yet committed to any VMO: an address space's prepaid pool. A reservation
/// is for one job at a time; a fault on a VMO of another job gives the remainder back and prepays from that one.
#[derive(Debug, Default)]
pub struct Reservation {
    job: Option<JobId>,
    pages: u64,
}

impl Reservation {
    /// Prepaid pages left.
    pub fn pages(&self) -> u64 {
        self.pages
    }

    /// Spend one page of `job`'s allowance. `Ok(true)`: it came from the prepaid pool (a refill charged the ledger by up to
    /// [`RESERVATION_PAGES`], an eighth of the job's room at most, one page at least, when the pool was empty or belonged to another job);
    /// `Ok(false)`: it was charged to the ledger now (a job with no limit has nothing to prepay against: a prepayment would only make its
    /// usage drift from what its VMOs hold). `Err` when the job has no room for even one page.
    pub fn take(&mut self, jobs: &mut Jobs, job: JobId) -> Result<bool, crate::job::JobError> {
        if self.job == Some(job) && self.pages > 0 {
            self.pages = self.pages.saturating_sub(1);
            if self.pages == 0 {
                // (nothing prepaid is left: stop holding the job)
                self.job = None;
                jobs.release(job);
            }
            return Ok(true);
        }
        let room = jobs.room(job, Resource::Pages);
        if room == 0 {
            self.release(jobs);
            return Err(crate::job::JobError::Exceeded);
        }
        if room == crate::job::UNLIMITED {
            jobs.charge(job, Resource::Pages, 1)?;
            return Ok(false);
        }
        self.release(jobs);
        // (an eighth of the room at most, so that a prepayment near a job's limit cannot starve the job's other users: a 64-page job
        // prepays 8 pages at a time, not all 64)
        let n = (room / RESERVATION_ROOM_DIVISOR).clamp(1, RESERVATION_PAGES);
        jobs.charge(job, Resource::Pages, n)?;
        if n > 1 {
            jobs.retain(job); // (the prepaid pages are charged to the job: it must outlive them)
            self.job = Some(job);
            self.pages = n.saturating_sub(1); // (one is spent now)
        }
        Ok(true)
    }

    /// Take back a page `take` spent from the pool that was not committed after all (the frame could not be had): it stays prepaid if the
    /// pool is still the job's, else it goes back to the ledger.
    pub fn put_back(&mut self, jobs: &mut Jobs, job: JobId) {
        if self.job == Some(job) {
            self.pages = self.pages.saturating_add(1);
        } else {
            jobs.credit(job, Resource::Pages, 1);
        }
    }

    /// Give every prepaid page back to the job (teardown; an idle address space).
    pub fn release(&mut self, jobs: &mut Jobs) {
        if let Some(job) = self.job.take() {
            jobs.credit(job, Resource::Pages, self.pages);
            jobs.release(job);
        }
        self.pages = 0;
    }
}

/// A page table plus the regions that say what its holes mean.
pub struct AddressSpace<F: PageTableFormat = Sv39> {
    table: PageTable<F>,
    regions: BTreeMap<u64, Region>,
    /// The prepaid pages the fault path spends (L16).
    reserve: Reservation,
    /// The generation the next region gets (see `Region::gen`).
    next_gen: u64,
}

impl<F: PageTableFormat> AddressSpace<F> {
    pub fn new(table: PageTable<F>) -> AddressSpace<F> {
        AddressSpace {
            table,
            regions: BTreeMap::new(),
            reserve: Reservation::default(),
            next_gen: 1,
        }
    }

    /// The prepaid pages this address space holds (they count as its job's usage).
    pub fn reserved_pages(&self) -> u64 {
        self.reserve.pages()
    }

    /// Give the prepaid pages back to their job (a quiet address space; teardown does it itself).
    pub fn release_reservation(&mut self, jobs: &mut Jobs) {
        self.reserve.release(jobs);
    }

    pub fn table(&self) -> &PageTable<F> {
        &self.table
    }

    /// Describe `pages` pages at `start` as a window onto `vmo` from `vmo_page`.
    /// Nothing is mapped until a fault asks for it.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: start + len was checked with checked_add and canonical-ness first; pages <= MAX_REGION_PAGES keeps the products small; `end - 1` cannot underflow because len > 0
    pub fn map_region(
        &mut self,
        vmos: &mut Vmos,
        start: u64,
        pages: u64,
        vmo: u64,
        vmo_page: u64,
        perms: Perms,
    ) -> Result<(), RegionError> {
        self.map_region_max(vmos, start, pages, vmo, vmo_page, perms, perms)
    }

    /// `map_region` with the permissions the mapping may later be raised to (`max_perms`, at least `perms`).
    #[allow(clippy::too_many_arguments, clippy::arithmetic_side_effects)] // INVARIANT: as `map_region`
    pub fn map_region_max(
        &mut self,
        vmos: &mut Vmos,
        start: u64,
        pages: u64,
        vmo: u64,
        vmo_page: u64,
        perms: Perms,
        max_perms: Perms,
    ) -> Result<(), RegionError> {
        if !start.is_multiple_of(PAGE_SIZE) {
            return Err(RegionError::Misaligned);
        }
        if !F::is_canonical(start) {
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
        if !F::is_canonical(end - 1) || (start as i64) < 0 && (end - 1) as i64 >= 0 {
            return Err(RegionError::BadRange);
        }
        perms.validate().map_err(RegionError::BadPerms)?;
        let total = vmos.pages(vmo).ok_or(RegionError::NoSuchVmo)?;
        if vmo_page.checked_add(pages).is_none_or(|e| e > total) {
            return Err(RegionError::OutsideVmo);
        }
        if self.regions.len() >= abi::limits::MAX_REGIONS {
            return Err(RegionError::TooMany);
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
        // (the region holds a reference on its VMO until it goes: L15)
        vmos.retain(vmo).map_err(|_| RegionError::NoSuchVmo)?;
        self.regions.insert(
            start,
            Region {
                pages,
                vmo,
                vmo_page,
                perms,
                max_perms,
                gen: self.next_gen,
            },
        );
        self.next_gen = self.next_gen.saturating_add(1); // (a u64 counter: it does not wrap in any lifetime)
        Ok(())
    }

    /// The lowest page-aligned address at or after `from` where `bytes` bytes fit between regions and inside
    /// `[lo, hi)`: the stack arena search (`thread_spawn`). `from` below `lo` starts at `lo`. `None` when no gap at or
    /// after `from` is big enough; callers wrap by asking again from `lo`.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: region addresses and sizes were checked when the regions were inserted (map_region): s + r.pages * PAGE_SIZE is below the canonical limit
    pub fn find_gap(&self, lo: u64, hi: u64, bytes: u64, from: u64) -> Option<u64> {
        if bytes == 0 || !bytes.is_multiple_of(PAGE_SIZE) || hi < lo {
            return None;
        }
        let mut at = from.max(lo).next_multiple_of(PAGE_SIZE);
        // a region that starts before `at` may reach past it
        if let Some((&s, r)) = self.regions.range(..=at).next_back() {
            at = at.max(s + r.pages * PAGE_SIZE);
        }
        if at >= hi {
            return None;
        }
        for (&s, r) in self.regions.range(at..hi) {
            if s >= at.checked_add(bytes)? {
                break;
            }
            at = at.max(s + r.pages * PAGE_SIZE);
        }
        (at.checked_add(bytes)? <= hi).then_some(at)
    }

    /// The permissions of the region covering `va`, if there is one (mapped yet or not).
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: a region's start plus its size was checked when it was inserted
    pub fn region_perms_at(&self, va: u64) -> Option<Perms> {
        let (&start, r) = self.regions.range(..=va).next_back()?;
        (va < start + r.pages * PAGE_SIZE).then_some(r.perms)
    }

    /// The region that contains `va`, or the first one above it (`process_map_info`'s walk), with its start.
    pub fn region_at_or_after(&self, va: u64) -> Option<RegionInfo> {
        let below = self.regions.range(..=va).next_back();
        if let Some((&s, r)) = below {
            if va < s.saturating_add(r.pages.saturating_mul(PAGE_SIZE)) {
                return Some(Self::info(s, r));
            }
        }
        self.regions
            .range(va..)
            .next()
            .map(|(&s, r)| Self::info(s, r))
    }

    fn info(start: u64, r: &Region) -> RegionInfo {
        RegionInfo {
            start,
            pages: r.pages,
            vmo: r.vmo,
            vmo_page: r.vmo_page,
            perms: r.perms,
            max_perms: r.max_perms,
        }
    }

    /// Is there a region that starts exactly at `va`?
    pub fn has_region_at(&self, va: u64) -> bool {
        self.regions.contains_key(&va)
    }

    /// How many regions the address space has.
    pub fn region_count(&self) -> usize {
        self.regions.len()
    }

    /// Every region, in address order (tests and audits).
    pub fn regions_info(&self) -> alloc::vec::Vec<RegionInfo> {
        self.regions
            .iter()
            .map(|(&s, r)| Self::info(s, r))
            .collect()
    }

    /// Split the region that contains `va` in two at `va` (page aligned): both halves hold a reference on the VMO, keep the permissions and
    /// take fresh generations (a fault looked up before the split must look again). Nothing happens when `va` is at a region's start or
    /// in no region. `Err(TooMany)` when the table of regions is full.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: va lies strictly inside the region, so the halves' page counts are positive and their sum is the old count
    pub fn split_at(&mut self, vmos: &mut Vmos, va: u64) -> Result<(), RegionError> {
        let Some((&start, r)) = self.regions.range(..=va).next_back() else {
            return Ok(());
        };
        if va == start || va >= start + r.pages * PAGE_SIZE {
            return Ok(());
        }
        if self.regions.len() >= abi::limits::MAX_REGIONS {
            return Err(RegionError::TooMany);
        }
        let lower_pages = (va - start) / PAGE_SIZE;
        let (vmo, vmo_page, perms, max_perms, pages) =
            (r.vmo, r.vmo_page, r.perms, r.max_perms, r.pages);
        vmos.retain(vmo).map_err(|_| RegionError::NoSuchVmo)?;
        let (lower_gen, upper_gen) = (self.next_gen, self.next_gen.saturating_add(1));
        self.next_gen = self.next_gen.saturating_add(2);
        if let Some(low) = self.regions.get_mut(&start) {
            low.pages = lower_pages;
            low.gen = lower_gen;
        }
        self.regions.insert(
            va,
            Region {
                pages: pages - lower_pages,
                vmo,
                vmo_page: vmo_page + lower_pages,
                perms,
                max_perms,
                gen: upper_gen,
            },
        );
        Ok(())
    }

    /// Unmap `[start, start + len)` (page aligned; holes are fine): regions are split at the edges and the ones inside go, with their
    /// mapped pages (MM7). Returns how many regions were removed. **The caller must flush every hart's TLB before a freed frame can be
    /// reused.** Nothing changes on `Err`.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: start + len was checked with checked_add; the walk is over keys inside the range
    pub fn unmap_range<M: PhysMem>(
        &mut self,
        mm: &mut Mm<M>,
        vmos: &mut Vmos,
        jobs: &mut Jobs,
        start: u64,
        len: u64,
    ) -> Result<u64, RegionError> {
        let end = Self::range_end(start, len)?;
        if self.regions.len().saturating_add(2) > abi::limits::MAX_REGIONS {
            // (room for the two splits: a range inside one region needs both)
            let inside_one = self
                .regions
                .range(..=start)
                .next_back()
                .is_some_and(|(&s, r)| s + r.pages * PAGE_SIZE >= end);
            if inside_one {
                return Err(RegionError::TooMany);
            }
        }
        self.split_at(vmos, start)?;
        self.split_at(vmos, end)?;
        let starts: alloc::vec::Vec<u64> =
            self.regions.range(start..end).map(|(&s, _)| s).collect();
        let n = starts.len() as u64;
        for s in starts {
            let _ = self.unmap_region(mm, vmos, jobs, s);
        }
        Ok(n)
    }

    /// `start + len` for a page-aligned, non-empty range inside the address space.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: `end - 1` cannot underflow: `len` is not 0 and `end = start + len` was checked
    fn range_end(start: u64, len: u64) -> Result<u64, RegionError> {
        if !start.is_multiple_of(PAGE_SIZE) || !len.is_multiple_of(PAGE_SIZE) || len == 0 {
            return Err(RegionError::Misaligned);
        }
        let end = start.checked_add(len).ok_or(RegionError::BadRange)?;
        if !F::is_canonical(start)
            || !F::is_canonical(end - 1)
            || (start as i64) < 0 && (end - 1) as i64 >= 0
        {
            return Err(RegionError::BadRange);
        }
        Ok(end)
    }

    /// Change the permissions of `[start, start + len)` to `perms` (MM7): the whole range must be mapped (regions that touch each other
    /// count as one), `perms` must be within each region's `max_perms` and never write with execute; regions are split at the edges, the
    /// page-table entries change in place, and neighbours that now agree merge. Nothing changes on `Err`. The caller owes the harts a
    /// shootdown if `downgraded`, and an instruction-cache sync if `exec_pages` is not 0.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: start + len was checked; region page counts bound every sum
    pub fn protect_range<M: PhysMem>(
        &mut self,
        mm: &mut Mm<M>,
        vmos: &mut Vmos,
        jobs: &mut Jobs,
        start: u64,
        len: u64,
        perms: Perms,
    ) -> Result<ProtectStats, ProtectError> {
        let end = Self::range_end(start, len).map_err(|_| ProtectError::BadRange)?;
        if perms.write && perms.exec || perms.validate().is_err() {
            return Err(ProtectError::Denied);
        }
        // coverage first (INVALID_ARGS), then the ceiling (ACCESS_DENIED), both before anything changes
        let mut at = start;
        let mut denied = false;
        while at < end {
            let (&s, r) = self
                .regions
                .range(..=at)
                .next_back()
                .filter(|(&s, r)| at < s + r.pages * PAGE_SIZE)
                .ok_or(ProtectError::NotMapped)?;
            denied |= perms.write && !r.max_perms.write || perms.exec && !r.max_perms.exec;
            at = s + r.pages * PAGE_SIZE;
        }
        if denied {
            return Err(ProtectError::Denied);
        }
        if self.regions.len().saturating_add(2) > abi::limits::MAX_REGIONS {
            return Err(ProtectError::TooMany);
        }
        self.split_at(vmos, start)
            .map_err(|_| ProtectError::TooMany)?;
        self.split_at(vmos, end)
            .map_err(|_| ProtectError::TooMany)?;
        let mut stats = ProtectStats::default();
        let starts: alloc::vec::Vec<u64> =
            self.regions.range(start..end).map(|(&s, _)| s).collect();
        for s in starts {
            let gen = self.next_gen;
            self.next_gen = self.next_gen.saturating_add(1);
            let Some(r) = self.regions.get_mut(&s) else {
                continue;
            };
            r.perms = Perms {
                user: r.perms.user,
                ..perms
            };
            r.gen = gen;
            let (pages, new) = (r.pages, r.perms);
            for p in 0..pages {
                if let Some(old) = self.table.protect(mm, s + p * PAGE_SIZE, new) {
                    if old.read && !new.read || old.write && !new.write || old.exec && !new.exec {
                        stats.downgraded = true;
                    }
                    if !old.exec && new.exec {
                        stats.exec_pages += 1;
                    }
                }
            }
        }
        self.merge_near(mm, vmos, jobs, start, end);
        Ok(stats)
    }

    /// Merge regions that touch, map the same VMO at contiguous offsets and agree on permissions, from the one before `lo` to the one at
    /// `hi` (the keys of the regions an operation changed). Each merge releases the VMO reference of the region that goes.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: region page counts bound the sums
    pub fn merge_near<M: PhysMem>(
        &mut self,
        mm: &mut Mm<M>,
        vmos: &mut Vmos,
        jobs: &mut Jobs,
        lo: u64,
        hi: u64,
    ) {
        let first = self
            .regions
            .range(..lo)
            .next_back()
            .map(|(&s, _)| s)
            .unwrap_or(lo);
        let keys: alloc::vec::Vec<u64> = self.regions.range(first..=hi).map(|(&s, _)| s).collect();
        let Some((&first_key, rest)) = keys.split_first() else {
            return;
        };
        let mut cur = first_key;
        for &next in rest {
            let mergeable = match (self.regions.get(&cur), self.regions.get(&next)) {
                (Some(x), Some(y)) => {
                    cur + x.pages * PAGE_SIZE == next
                        && x.vmo == y.vmo
                        && x.vmo_page + x.pages == y.vmo_page
                        && x.perms == y.perms
                        && x.max_perms == y.max_perms
                }
                _ => false,
            };
            if !mergeable {
                cur = next;
                continue;
            }
            if let Some(y) = self.regions.remove(&next) {
                let gen = self.next_gen;
                self.next_gen = self.next_gen.saturating_add(1);
                if let Some(x) = self.regions.get_mut(&cur) {
                    x.pages += y.pages;
                    x.gen = gen;
                }
                let _ = vmos.release(mm, jobs, y.vmo);
            }
        }
    }

    /// Does a region of this address space map any of the pages `first .. first + pages` of VMO `vmo`?
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: a region's page range was checked when it was inserted
    pub fn maps_vmo_range(&self, vmo: u64, first: u64, pages: u64) -> bool {
        let end = first.saturating_add(pages);
        self.regions
            .values()
            .any(|r| r.vmo == vmo && first < r.vmo_page + r.pages && r.vmo_page < end)
    }

    /// Unmap, in this address space, every mapped page of VMO `vmo` among its pages `first .. first + pages` (MM6: before the VMO gives the
    /// pages back). Drops each mapping's reference on its frame; regions stay. Returns how many pages were unmapped. **The caller must flush
    /// the TLBs (every hart) before the VMO's own references on those frames are dropped.**
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: a region's start plus its size was checked when it was inserted; the page numbers are bounded by the region's length
    pub fn unmap_vmo_pages<M: PhysMem>(
        &mut self,
        mm: &mut Mm<M>,
        vmo: u64,
        first: u64,
        pages: u64,
    ) -> u64 {
        let end = first.saturating_add(pages);
        let mut cleared = 0u64;
        for (&start, r) in self.regions.iter() {
            if r.vmo != vmo {
                continue;
            }
            // the region maps the VMO's pages `r.vmo_page .. r.vmo_page + r.pages`
            let lo = first.max(r.vmo_page);
            let hi = end.min(r.vmo_page + r.pages);
            let mut p = lo;
            while p < hi {
                let va = start + (p - r.vmo_page) * PAGE_SIZE;
                if let Some((pfn, _)) = self.table.unmap(mm, va) {
                    if mm.db.is_managed(pfn) {
                        let _ = mm.db.dec(pfn);
                    }
                    cleared += 1;
                }
                p += 1;
            }
        }
        cleared
    }

    /// Remove the region starting at `start`, unmapping its pages and dropping the
    /// mappings' references. **The caller must flush the TLB before
    /// any frame freed here can be reused**: this crate has no notion of TLBs.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: p < r.pages and the region's range was checked when it was inserted
    pub fn unmap_region<M: PhysMem>(
        &mut self,
        mm: &mut Mm<M>,
        vmos: &mut Vmos,
        jobs: &mut Jobs,
        start: u64,
    ) -> Result<(), RegionError> {
        let region = self.regions.remove(&start).ok_or(RegionError::NoRegion)?;
        for p in 0..region.pages {
            if let Some((pfn, _)) = self.table.unmap(mm, start + p * PAGE_SIZE) {
                if !mm.db.is_managed(pfn) {
                    continue; // device memory: the mapping holds no reference
                }
                let dropped = mm.db.dec(pfn);
                debug_assert!(
                    dropped.is_ok(),
                    "a mapped frame holds the mapping's reference"
                );
            }
        }
        // the region's reference on its VMO goes last (the VMO may be freed by it: the frames the mapping held are already dropped)
        let _ = vmos.release(mm, jobs, region.vmo);
        Ok(())
    }

    /// Handle a page fault at `va` for `access`: map the page if the region allows
    /// the access, and the committed pages around it (fault-around, MM1). A fault on a page
    /// that is already mapped (and allowed) is spurious and succeeds.
    pub fn handle_fault<M: PhysMem>(
        &mut self,
        mm: &mut Mm<M>,
        vmos: &mut Vmos,
        jobs: &mut Jobs,
        va: u64,
        access: Access,
    ) -> Result<(), FaultError> {
        self.handle_fault_mapped(mm, vmos, jobs, va, access)
            .map(|_| ())
    }

    /// `handle_fault`, saying which pages it mapped: the faulting page and, in the same 64 KiB aligned window and the same region,
    /// every page the VMO has **already committed** and that was not mapped yet (never one it would have to commit). The
    /// caller flushes `first..=last` (page by page) before returning to the faulting instruction: a mapping that was absent may
    /// still be cached as absent unless the implementation has Svvptc, and a stale entry is harmless but a fault. A neighbour that
    /// cannot be mapped (no memory for a table) is simply left for its own fault. Device memory is not mapped around.
    pub fn handle_fault_mapped<M: PhysMem>(
        &mut self,
        mm: &mut Mm<M>,
        vmos: &mut Vmos,
        jobs: &mut Jobs,
        va: u64,
        access: Access,
    ) -> Result<MappedPages, FaultError> {
        // (the three steps run back to back here, under one hold of the address space; a caller that releases it between the steps, as
        // a pager wait must (MM9, KERNEL-V1-SPEC 4.5), asks `fault_map` whether the region is still the one it looked up)
        loop {
            let plan = match self.fault_lookup(mm, va, access)? {
                Lookup::Mapped(m) => return Ok(m),
                Lookup::Plan(plan) => plan,
            };
            let (fetch_vmo, fetch_page) = plan.vmo_page();
            let pfn = vmos.page_for(mm, jobs, Some(&mut self.reserve), fetch_vmo, fetch_page)?;
            match self.fault_map(mm, vmos, &plan, pfn) {
                Ok(m) => return Ok(m),
                Err(FaultMapError::Stale) => continue,
                Err(FaultMapError::Fault(e)) => return Err(e),
            }
        }
    }

    /// First step of a fault: find the region and check the access. `Mapped` when the page is already mapped and allows it (a spurious
    /// fault); otherwise the plan for the page to fetch.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: the region was found with start <= va, so va - start does not underflow; page < region.pages bounds the sums
    pub fn fault_lookup<M: PhysMem>(
        &self,
        mm: &Mm<M>,
        va: u64,
        access: Access,
    ) -> Result<Lookup, FaultError> {
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
            return if ok {
                Ok(Lookup::Mapped(MappedPages {
                    first: page_va,
                    last: page_va,
                    exec: 0,
                }))
            } else {
                Err(FaultError::Denied)
            };
        }
        Ok(Lookup::Plan(FaultPlan {
            start,
            page,
            page_va,
            vmo: region.vmo,
            vmo_page: region.vmo_page,
            perms: region.perms,
            pages: region.pages,
            gen: region.gen,
        }))
    }

    /// Last step of a fault: map the page `pfn` the VMO gave for `plan` (and the committed neighbours, fault-around). `Stale` when the
    /// region the plan came from is gone or was replaced since the lookup (an unmap in between): nothing is mapped, and the caller looks
    /// the fault up again.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: the window arithmetic is on page-aligned addresses inside the region
    pub fn fault_map<M: PhysMem>(
        &mut self,
        mm: &mut Mm<M>,
        vmos: &mut Vmos,
        plan: &FaultPlan,
        pfn: Pfn,
    ) -> Result<MappedPages, FaultMapError> {
        match self.regions.get(&plan.start) {
            Some(r) if r.gen == plan.gen => {}
            _ => return Err(FaultMapError::Stale),
        }
        let (start, page_va, vmo, perms, vmo_page0, region_pages) = (
            plan.start,
            plan.page_va,
            plan.vmo,
            plan.perms,
            plan.vmo_page,
            plan.pages,
        );
        let managed = mm.db.is_managed(pfn);
        if managed {
            mm.db
                .inc(pfn)
                .map_err(|_| FaultMapError::Fault(FaultError::NoMemory))?;
        }
        if let Err(e) = self.table.map(mm, page_va, pfn, perms) {
            if managed {
                let dropped = mm.db.dec(pfn);
                debug_assert!(
                    dropped.is_ok(),
                    "the reference just taken must be droppable"
                );
            }
            return Err(FaultMapError::Fault(match e {
                MapError::NoMemory => FaultError::NoMemory,
                other => FaultError::MapFailed(other),
            }));
        }
        let mut out = MappedPages {
            first: page_va,
            last: page_va,
            exec: u64::from(perms.exec),
        };
        if vmos.kind(vmo) == Some(VmoKind::Device) {
            return Ok(out);
        }
        // fault-around: the 64 KiB aligned window, clipped to the region
        let window = FAULT_AROUND_WINDOW_PAGES * PAGE_SIZE;
        let lo = (page_va & !(window - 1)).max(start);
        let hi = ((page_va & !(window - 1)) + window).min(start + region_pages * PAGE_SIZE);
        let mut q = lo;
        while q < hi {
            if q != page_va {
                let qpage = (q - start) / PAGE_SIZE;
                if let Some(qpfn) = vmos.frame(vmo, vmo_page0 + qpage) {
                    if self.table.translate(mm, q).is_none() {
                        let qmanaged = mm.db.is_managed(qpfn);
                        if qmanaged && mm.db.inc(qpfn).is_err() {
                            break;
                        }
                        if self.table.map(mm, q, qpfn, perms).is_err() {
                            if qmanaged {
                                let _ = mm.db.dec(qpfn);
                            }
                            break;
                        }
                        out.first = out.first.min(q);
                        out.last = out.last.max(q);
                        out.exec = out.exec.saturating_add(u64::from(perms.exec));
                    }
                }
            }
            q += PAGE_SIZE;
        }
        Ok(out)
    }

    /// Unmap everything and free the tables of a process whose table shares the kernel's
    /// upper half (`PageTable::new_with_kernel_half`): the shared half is left alone.
    pub fn destroy_user<M: PhysMem>(mut self, mm: &mut Mm<M>, vmos: &mut Vmos, jobs: &mut Jobs) {
        let starts: alloc::vec::Vec<u64> = self.regions.keys().copied().collect();
        for start in starts {
            let _ = self.unmap_region(mm, vmos, jobs, start);
        }
        self.reserve.release(jobs);
        self.table.destroy_user(mm);
    }

    /// Unmap everything and free the tables.
    pub fn destroy<M: PhysMem>(mut self, mm: &mut Mm<M>, vmos: &mut Vmos, jobs: &mut Jobs) {
        let starts: alloc::vec::Vec<u64> = self.regions.keys().copied().collect();
        for start in starts {
            let _ = self.unmap_region(mm, vmos, jobs, start);
        }
        self.reserve.release(jobs);
        self.table.destroy(mm);
    }
}
