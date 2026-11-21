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
            self.page_for(mm, jobs, id, at / PAGE_SIZE)
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
            match self.page_for(mm, jobs, id, page) {
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
        id: u64,
        page: u64,
    ) -> Result<Pfn, FaultError> {
        let vmo = Self::vmo_in(&mut self.slots, id).ok_or(FaultError::VmoGone)?;
        if let Some(pfn) = vmo.committed.get(page) {
            return Ok(pfn);
        }
        // The job's limit first (a full job is "out of memory" for its own programs only),
        // then the frame; a frame that cannot be had gives the charge back.
        jobs.charge(vmo.job, Resource::Pages, 1)
            .map_err(|_| FaultError::NoMemory)?;
        let Some(pfn) = mm.db.alloc() else {
            jobs.credit(vmo.job, Resource::Pages, 1);
            return Err(FaultError::NoMemory);
        };
        // (the bookkeeping first: if it cannot be had, the frame and the charge go back and nothing was committed)
        if vmo.committed.insert(page, pfn).is_err() {
            let _ = mm.db.dec(pfn);
            jobs.credit(vmo.job, Resource::Pages, 1);
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
}

/// Fault-around works inside a window of this many pages (64 KiB), aligned (KERNEL-V1-SPEC 4.5): at most 15 neighbours.
pub const FAULT_AROUND_WINDOW_PAGES: u64 = 16;

struct Region {
    pages: u64,
    vmo: u64,
    vmo_page: u64,
    perms: Perms,
}

/// A page table plus the regions that say what its holes mean.
pub struct AddressSpace<F: PageTableFormat = Sv39> {
    table: PageTable<F>,
    regions: BTreeMap<u64, Region>,
}

impl<F: PageTableFormat> AddressSpace<F> {
    pub fn new(table: PageTable<F>) -> AddressSpace<F> {
        AddressSpace {
            table,
            regions: BTreeMap::new(),
        }
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
            },
        );
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
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: the region was found with start <= va, so va - start does not underflow; page < region.pages bounds the sums; the window arithmetic is on page-aligned addresses inside the region
    pub fn handle_fault_mapped<M: PhysMem>(
        &mut self,
        mm: &mut Mm<M>,
        vmos: &mut Vmos,
        jobs: &mut Jobs,
        va: u64,
        access: Access,
    ) -> Result<MappedPages, FaultError> {
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
                Ok(MappedPages {
                    first: page_va,
                    last: page_va,
                })
            } else {
                Err(FaultError::Denied)
            };
        }
        let (vmo, perms, region_pages, vmo_page0) =
            (region.vmo, region.perms, region.pages, region.vmo_page);
        let pfn = vmos.page_for(mm, jobs, vmo, vmo_page0 + page)?;
        let managed = mm.db.is_managed(pfn);
        if managed {
            mm.db.inc(pfn).map_err(|_| FaultError::NoMemory)?;
        }
        if let Err(e) = self.table.map(mm, page_va, pfn, perms) {
            if managed {
                let dropped = mm.db.dec(pfn);
                debug_assert!(
                    dropped.is_ok(),
                    "the reference just taken must be droppable"
                );
            }
            return Err(match e {
                MapError::NoMemory => FaultError::NoMemory,
                other => FaultError::MapFailed(other),
            });
        }
        let mut out = MappedPages {
            first: page_va,
            last: page_va,
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
        self.table.destroy_user(mm);
    }

    /// Unmap everything and free the tables.
    pub fn destroy<M: PhysMem>(mut self, mm: &mut Mm<M>, vmos: &mut Vmos, jobs: &mut Jobs) {
        let starts: alloc::vec::Vec<u64> = self.regions.keys().copied().collect();
        for start in starts {
            let _ = self.unmap_region(mm, vmos, jobs, start);
        }
        self.table.destroy(mm);
    }
}
