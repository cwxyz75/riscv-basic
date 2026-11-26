//! Device domains (IOMMU3; IOMMU-SPEC sections 3 to 7, OD148 step b): the store the `IOMMU` lock protects, as pure code over the
//! formats of `iommucore`. A domain is one device's I/O address space: its second-stage page table, its I/O address allocator, its
//! mapping records, its fault ring, its MSI page table (empty here: IOMMU7 grants vectors) and its job. The device directory and the
//! command queue are singletons in the store.
//!
//! **The phases.** Every operation is split where the kernel changes locks (IOMMU-SPEC 11.2), and the boundaries are what the
//! concurrency tests explore. Nothing here takes a lock or knows about one: each method is one critical section under `IOMMU`, and the
//! steps under `VMO` and `JOBS` are the caller's (`Vmos::retain_for_io`, `Vmos::release`, and the charge helpers at the end of this
//! file). A fence is never waited for here: an operation that needs one queues it and returns a [`Ticket`]; the caller polls
//! ([`IoDomains::poll`]) with no lock held and then completes ([`IoDomains::complete`], [`IoDomains::destroy_complete`]) or, when the
//! bound passes or the queue fails, abandons ([`IoDomains::abandon`], [`IoDomains::destroy_abandon`]), which leaks what the fence
//! guarded, counted (IO8).
//!
//! * `create`: the caller charges the job first (`charge_create`); the store writes a `translate` context with an empty MSI page table,
//!   queues the directory invalidation and a fence.
//! * `map`: the caller takes the VMO's hold and frame list (`retain_for_io`, under `VMO`), charges the job (`charge_map`, under `JOBS`)
//!   and allocates the mapping's [`Record`] with no lock held; [`IoDomains::map`] reserves a range, links the record and writes the
//!   leaves (the table frames were prepaid), and queues the invalidation and a fence. A failure after a leaf was written is rolled back
//!   as an unmap.
//! * `unmap`: [`IoDomains::unmap`] clears the leaves, makes the range pending and queues the invalidation and a fence; after the fence,
//!   [`IoDomains::complete`] frees the range and hands the record back ([`Released`]), and only then does the caller drop the hold
//!   (`Vmos::release`) and settle the record (table frames freed, job credited). Removal uses only what the record owns: it allocates
//!   nothing (6.5).
//! * `destroy`: quarantine context, directory invalidation, the domain's whole soft context invalidated, fence; after it, every table
//!   frame, every hold and the soft-context id come back ([`Detached`]). Between the fence and the freeing is where IOMMU7 retires the
//!   domain's interrupt identities.
//! * `fault` and `fault_read`: the fault ring of 16 records and a dropped count per domain, allocated with it (7.4).
//!
//! **Records and lists.** A mapping is a record of its own ([`Record`], allocated by the caller before the store is entered and charged
//! as [`MAPPING_BYTES`]). The records are linked into the domain intrusively: by I/O address in a treap whose links are the records'
//! own boxes, and through their `next` link into the pending and leaked lists, so linking, unlinking and walking allocate nothing.
//!
//! **Table frames.** A table frame that an unmap or a destruction empties is not freed while the IOMMU may still walk it: it is
//! threaded onto a chain through its own first word (a page-aligned link, whose valid bit is clear, so a stale walk through it faults)
//! and freed only by the caller after the fence ([`Released::settle`], [`Detached::finish`]).

use crate::fbox::FBox;
use crate::frame::Pfn;
use crate::job::{IoResource, JobError, JobId, Jobs, Resource};
use alloc::vec::Vec;
use iommucore::cmdq::{CmdQueue, Command};
use iommucore::ddt::{device_id_bits, Ddt, DeviceContext, TableRoot};
use iommucore::faultq::FaultRecord;
use iommucore::iopt::{Access, Iopt, PtMode};
use iommucore::iova::Iova;
use iommucore::msi::{MsiTable, MsiWindow};
use iommucore::regs::{self, Cqcsr};
use iommucore::{PhysMem64, Regs, TableFrames, PAGE_SHIFT, PAGE_SIZE};

/// The first I/O address of every domain's window (IOMMU-SPEC 3.3): 2^40, above every physical address of the machine. (Constants of
/// the ABI when IOMMU6 adds the calls.)
pub const IO_BASE: u64 = 1 << 40;
/// The pages of the window: 1 GiB.
pub const IO_WINDOW_PAGES: u64 = 1 << 18;
/// The interrupt window at the bottom of the window: page `k` is vector `k` (IOMMU-SPEC 3.3, 8.2). Never a DMA mapping.
pub const IO_VECTOR_PAGES: u64 = 256;
/// The DMA part of the window.
pub const IO_DMA_PAGES: u64 = IO_WINDOW_PAGES - IO_VECTOR_PAGES;
/// Fault records a domain's ring keeps (IOMMU-SPEC 7.4).
pub const FAULT_RING: usize = 16;
/// Console lines for faults, per second, for the whole system (IOMMU-SPEC 7.2).
pub const FAULT_LINES_PER_SECOND: u32 = 16;
/// A second-stage root: four frames, 16 KiB, aligned to its size.
pub const ROOT_FRAMES: u64 = 4;
/// Ranges of at most this many pages are invalidated page by page; a longer one by its domain's whole soft context (one command, so a
/// removal always fits the queue).
const INVAL_PAGES: usize = 4;
/// The ticket of commands that could not be queued (the queue had no room and the IOMMU did not drain it): never done.
const NO_TICKET: u64 = u64::MAX;
/// The soft-context id of the shared empty table of quarantined devices.
const QUARANTINE_GSCID: u16 = 0;

/// The frames of a domain's tables, from the kernel's allocator (`FRAME`, inside `IOMMU`). `TableFrames::alloc` gives one zeroed frame
/// (its charge was prepaid by the caller, `charge_map`); a second-stage root is four contiguous zeroed frames aligned to 16 KiB.
pub trait IoFrames: TableFrames {
    fn alloc_root(&mut self) -> Option<u64>;
    fn free_root(&mut self, pa: u64);
}

/// A domain: (generation << 32) | (slot + 1). A destroyed domain's id never names a later one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DomainId(u64);

/// The command-queue position of a fence: done once the IOMMU's head has passed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ticket(u64);

impl Ticket {
    pub fn seq(self) -> u64 {
        self.0
    }
}

/// What a poll of a fence found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fence {
    Done,
    /// Not yet: poll again (the kernel, until `IOMMU_FENCE_BOUND`).
    Waiting,
    /// The command queue reported an error, or the commands could not be queued: the fence will never complete (6.4).
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoError {
    /// A device id the directory does not cover; an unmap that does not name exactly a live mapping.
    InvalidArgs,
    /// A live domain exists for the device (create); the domain was destroyed (map, unmap, fault read).
    BadState,
    /// Memory: the store's room, table frames, an I/O range, a soft-context id.
    NoMemory,
    /// The fence has not completed: the step after it may not run yet.
    NotDone,
}

/// The state of a device id in the directory (IOMMU-SPEC 3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceState {
    Unclaimed,
    Attached,
    Quarantined,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Attached,
    /// `destroy` has run: the device is quarantined and the fence is outstanding; maps and unmaps are refused.
    Detaching,
}

// ---------------------------------------------------------------------------------------------------------------------------
// table-frame chains

/// Table frames waiting for a fence, threaded through their own first word.
#[derive(Debug, Default)]
struct Chain {
    head: u64,
    n: u64,
}

impl Chain {
    fn push<M: PhysMem64>(&mut self, mem: &M, pa: u64) {
        // (pa is page-aligned, so the link has the valid bit clear: a walk that still reaches this frame faults)
        mem.write(pa, self.head);
        self.head = pa;
        self.n = self.n.saturating_add(1);
    }

    /// Free every frame of the chain; how many.
    fn free_all<M: PhysMem64, F: TableFrames>(&mut self, mem: &M, frames: &mut F) -> u64 {
        let n = self.n;
        while self.head != 0 {
            let next = mem.read(self.head);
            frames.free(self.head);
            self.head = next;
        }
        self.n = 0;
        n
    }
}

/// Table frames for a removal: frees go onto a chain, and nothing is ever allocated (6.5).
struct NoAlloc<'a, M: PhysMem64> {
    mem: &'a M,
    chain: &'a mut Chain,
}

impl<M: PhysMem64> TableFrames for NoAlloc<'_, M> {
    fn alloc(&mut self) -> Option<u64> {
        None
    }
    fn free(&mut self, pa: u64) {
        self.chain.push(self.mem, pa);
    }
}

/// Table frames for a map: allocations from the caller's frames (counted, prepaid), frees (of tables the map itself emptied on a
/// roll-back) onto the record's chain.
struct Deferring<'a, M: PhysMem64, F: TableFrames> {
    mem: &'a M,
    frames: &'a mut F,
    chain: &'a mut Chain,
    allocs: u64,
}

impl<M: PhysMem64, F: TableFrames> TableFrames for Deferring<'_, M, F> {
    fn alloc(&mut self) -> Option<u64> {
        let pa = self.frames.alloc()?;
        self.allocs = self.allocs.saturating_add(1);
        Some(pa)
    }
    fn free(&mut self, pa: u64) {
        self.chain.push(self.mem, pa);
    }
}

// ---------------------------------------------------------------------------------------------------------------------------
// mapping records

type Link = Option<FBox<Mapping>>;

/// One mapping (IOMMU-SPEC 4.3): its I/O range, its VMO window and access, its hold (the VMO id: the hold is a reference the record
/// owns), its fence ticket, the table frames its removal detached, and its links.
#[derive(Debug)]
pub struct Mapping {
    page: u64,
    pages: u64,
    vmo: u64,
    vmo_page: u64,
    access: Access,
    ticket: u64,
    chain: Chain,
    left: Link,
    right: Link,
    next: Link,
}

/// The kernel bytes one mapping record is charged (IOMMU-SPEC 5.3).
pub const MAPPING_BYTES: u64 = core::mem::size_of::<Mapping>() as u64;

/// A mapping record not yet in a domain (made by the caller with no lock held, IOMMU-SPEC 11.2).
#[derive(Debug)]
pub struct Record(FBox<Mapping>);

impl Record {
    /// A record for `pages` pages of `vmo` from its page `vmo_page`, with `access`; `None` when the allocation fails (the map's
    /// NO_MEMORY, with nothing else changed).
    pub fn new(vmo: u64, vmo_page: u64, pages: u64, access: Access) -> Option<Record> {
        FBox::try_new(Mapping {
            page: 0,
            pages,
            vmo,
            vmo_page,
            access,
            ticket: NO_TICKET,
            chain: Chain::default(),
            left: None,
            right: None,
            next: None,
        })
        .ok()
        .map(Record)
    }
    pub fn vmo(&self) -> u64 {
        self.0.vmo
    }
    pub fn pages(&self) -> u64 {
        self.0.pages
    }
}

/// The treap's priority of a mapping at `page`: a mix of the address, so the shape is balanced in expectation whatever the order of
/// the addresses (the allocator hands them out lowest first).
fn prio(page: u64) -> u64 {
    let mut z = page.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Split `t` into the nodes below `page` and the rest.
fn split(t: Link, page: u64) -> (Link, Link) {
    match t {
        None => (None, None),
        Some(mut n) => {
            if n.page < page {
                let (a, b) = split(n.right.take(), page);
                n.right = a;
                (Some(n), b)
            } else {
                let (a, b) = split(n.left.take(), page);
                n.left = b;
                (a, Some(n))
            }
        }
    }
}

/// Join two treaps, every node of `a` below every node of `b`.
fn merge(a: Link, b: Link) -> Link {
    match (a, b) {
        (None, x) | (x, None) => x,
        (Some(mut a), Some(mut b)) => {
            if prio(a.page) > prio(b.page) {
                a.right = merge(a.right.take(), Some(b));
                Some(a)
            } else {
                b.left = merge(Some(a), b.left.take());
                Some(b)
            }
        }
    }
}

fn insert(t: &mut Link, mut n: FBox<Mapping>) {
    match t {
        Some(cur) if prio(cur.page) >= prio(n.page) => {
            if n.page < cur.page {
                insert(&mut cur.left, n);
            } else {
                insert(&mut cur.right, n);
            }
        }
        _ => {
            let (a, b) = split(t.take(), n.page);
            n.left = a;
            n.right = b;
            *t = Some(n);
        }
    }
}

fn find(t: &Link, page: u64) -> Option<&Mapping> {
    let mut cur = t.as_deref();
    while let Some(n) = cur {
        if page == n.page {
            return Some(n);
        }
        cur = if page < n.page {
            n.left.as_deref()
        } else {
            n.right.as_deref()
        };
    }
    None
}

fn remove(t: &mut Link, page: u64) -> Link {
    let mut cur = t;
    while cur.as_ref().is_some_and(|n| n.page != page) {
        let n = cur.as_mut()?;
        cur = if page < n.page {
            &mut n.left
        } else {
            &mut n.right
        };
    }
    let mut n = cur.take()?;
    *cur = merge(n.left.take(), n.right.take());
    Some(n)
}

/// Move every node of the treap `t` onto the list `out` (through `next`), allocating nothing and without recursion: a node with a left
/// child is rotated right; one without is moved.
fn flatten(mut t: Link, out: &mut Link) -> u64 {
    let mut count = 0u64;
    while let Some(mut n) = t.take() {
        if let Some(mut l) = n.left.take() {
            n.left = l.right.take();
            l.right = Some(n);
            t = Some(l);
        } else {
            t = n.right.take();
            n.next = out.take();
            *out = Some(n);
            count = count.saturating_add(1);
        }
    }
    count
}

fn push(list: &mut Link, mut n: FBox<Mapping>) {
    n.next = list.take();
    *list = Some(n);
}

fn pop(list: &mut Link) -> Link {
    let mut n = list.take()?;
    *list = n.next.take();
    Some(n)
}

/// Unlink the first record of `list` that `pred` accepts.
fn unlink(list: &mut Link, pred: impl Fn(&Mapping) -> bool) -> Link {
    let mut cur = list;
    while cur.as_ref().is_some_and(|n| !pred(n)) {
        cur = &mut cur.as_mut()?.next;
    }
    let mut n = cur.take()?;
    *cur = n.next.take();
    Some(n)
}

/// Drop a list one record at a time (a long list must not be dropped by recursion).
fn drop_list(list: &mut Link) {
    while let Some(n) = pop(list) {
        drop(n);
    }
}

// ---------------------------------------------------------------------------------------------------------------------------
// domains

const NO_FAULT: FaultRecord = FaultRecord::decode([0; 4]);

struct Domain {
    device: u32,
    job: JobId,
    state: State,
    gscid: u16,
    /// Taken by the destruction.
    iopt: Option<Iopt>,
    msi: MsiTable,
    iova: Iova,
    /// Live mappings, by I/O address.
    live: Link,
    mappings: u64,
    /// Mappings whose leaves are cleared and whose fence is outstanding.
    pending: Link,
    /// Mappings whose fence was abandoned: never freed (IO8).
    leaked: Link,
    /// Table frames in the page table (the root not counted).
    tables: u64,
    /// Pages of the live and pending mappings (charged to the job as I/O-mapped pages).
    io_pages: u64,
    /// A fence of this domain was abandoned: its job keeps the use the domain held, for the charges that stay.
    leaks: bool,
    ring: [FaultRecord; FAULT_RING],
    ring_head: usize,
    ring_len: usize,
    dropped: u32,
    destroy_ticket: u64,
    /// The store's list of leaked domains.
    next: Option<FBox<Domain>>,
}

/// The kernel bytes a domain is charged at creation (IOMMU-SPEC 5.3): its body with the fault ring, the root table, the MSI page table
/// and the I/O address allocator's two bitmaps.
pub const DOMAIN_BYTES: u64 = core::mem::size_of::<Domain>() as u64
    + ROOT_FRAMES * PAGE_SIZE
    + PAGE_SIZE
    + 2 * IO_WINDOW_PAGES.div_ceil(64) * 8;

impl Domain {
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: ring_head < FAULT_RING and ring_len <= FAULT_RING, so the index is reduced modulo FAULT_RING
    fn store_fault(&mut self, rec: FaultRecord) {
        if self.ring_len == FAULT_RING {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        self.ring[(self.ring_head + self.ring_len) % FAULT_RING] = rec;
        self.ring_len += 1;
    }

    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: as store_fault; ring_len >= 1 in the arm that subtracts
    fn read_fault(&mut self) -> Option<FaultRead> {
        if self.ring_len == 0 {
            return None;
        }
        let record = self.ring[self.ring_head];
        self.ring_head = (self.ring_head + 1) % FAULT_RING;
        self.ring_len -= 1;
        let dropped = core::mem::take(&mut self.dropped);
        Some(FaultRead {
            record,
            dropped,
            more: self.ring_len > 0,
        })
    }
}

struct Slot {
    gen: u32,
    dom: Option<FBox<Domain>>,
}

/// What the store has counted (IOMMU-SPEC 7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IoStats {
    /// Faults handed to `fault`.
    pub faults: u64,
    /// Faults past the console's rate, not printed.
    pub faults_unprinted: u64,
    /// The last fault: device id, address, cause.
    pub last_fault: Option<(u32, u64, u16)>,
    /// Commands queued.
    pub commands: u64,
    /// Fences abandoned (a timeout or a failed queue).
    pub fence_timeouts: u64,
    /// Times the command queue had no room and the commands were not queued (their fence fails).
    pub queue_stuck: u64,
    /// Mapping records leaked with their holds, the pages they mapped, and table frames leaked (roots and MSI tables included).
    pub holds_leaked: u64,
    pub pages_leaked: u64,
    pub tables_leaked: u64,
    /// Live domains (attached or detaching).
    pub domains: u64,
    /// Pages of live and pending mappings, and of leaked ones.
    pub io_pages: u64,
}

/// The domain store (the state the `IOMMU` lock protects).
pub struct IoDomains {
    ddt: Ddt,
    cmdq: CmdQueue,
    quarantine: TableRoot,
    slots: Vec<Slot>,
    free: Vec<u32>,
    gscids: Vec<u64>,
    /// Records leaked by abandoned fences of domains that are gone.
    leaked: Link,
    leaked_domains: Option<FBox<Domain>>,
    stats: IoStats,
    window_start: u64,
    window_lines: u32,
}

/// A fault handed to the store: which domain stored it, and whether the console line is due.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaultOutcome {
    /// The domain whose ring took the record (raise DEVICE_FAULT on it), if the device is attached.
    pub domain: Option<DomainId>,
    /// Print the line (within the rate).
    pub print: bool,
}

/// The oldest unread fault of a domain, the faults dropped since the previous read, and whether more are waiting (DEVICE_FAULT stays).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaultRead {
    pub record: FaultRecord,
    pub dropped: u32,
    pub more: bool,
}

/// A successful map: the I/O address, the fence to wait for, and the table frames it took (to settle the prepayment).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mapped {
    pub iova: u64,
    pub ticket: Ticket,
    pub tables: u64,
}

/// A map that failed.
#[derive(Debug)]
pub enum MapFailed {
    /// Nothing was written: the record comes back, the caller drops the hold and refunds the charge.
    Refused(IoError, Record),
    /// Leaves were written and are cleared again: the roll-back is an unmap (4.3), to be waited for and completed like one; `tables`
    /// settles the prepayment.
    RolledBack { pending: Pending, tables: u64 },
}

/// An unmap (or a map's roll-back) waiting for its fence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pending {
    domain: DomainId,
    page: u64,
    ticket: Ticket,
}

impl Pending {
    pub fn ticket(&self) -> Ticket {
        self.ticket
    }
    /// The domain and the I/O address of the range.
    pub fn range(&self) -> (DomainId, u64) {
        (self.domain, self.page << PAGE_SHIFT)
    }
}

/// A destruction waiting for its fence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Destroying {
    domain: DomainId,
    ticket: Ticket,
}

impl Destroying {
    pub fn ticket(&self) -> Ticket {
        self.ticket
    }
    pub fn domain(&self) -> DomainId {
        self.domain
    }
}

/// A mapping whose fence has completed, out of the store: the caller drops the hold (`Vmos::release` of [`Released::vmo`], under `VMO`)
/// and settles the record ([`Released::settle`]).
#[derive(Debug)]
pub struct Released {
    rec: FBox<Mapping>,
    job: JobId,
}

impl Released {
    /// The VMO whose hold the record owns.
    pub fn vmo(&self) -> u64 {
        self.rec.vmo
    }
    pub fn pages(&self) -> u64 {
        self.rec.pages
    }
    /// Free the table frames the removal detached, credit the job (the I/O-mapped pages, the record's kernel bytes and the tables'), and
    /// free the record. Allocates nothing.
    pub fn settle<M: PhysMem64, F: TableFrames>(
        mut self,
        mem: &M,
        frames: &mut F,
        jobs: &mut Jobs,
    ) {
        let tables = self.rec.chain.free_all(mem, frames);
        jobs.credit_io(self.job, IoResource::MappedPages, self.rec.pages);
        jobs.credit(
            self.job,
            Resource::KernelBytes,
            MAPPING_BYTES.saturating_add(tables.saturating_mul(PAGE_SIZE)),
        );
    }
}

/// A destroyed domain, out of the store after its fence: its mappings ([`Detached::next_record`], each settled as a [`Released`]), then
/// its tables, root, MSI page table and charge ([`Detached::finish`]).
pub struct Detached {
    dom: FBox<Domain>,
    records: Link,
    chain: Chain,
    root: u64,
}

impl Detached {
    /// The next of the domain's mappings (live or pending at the destruction), whose hold the caller drops and which it settles.
    pub fn next_record(&mut self) -> Option<Released> {
        let rec = pop(&mut self.records)?;
        Some(Released {
            rec,
            job: self.dom.job,
        })
    }

    pub fn job(&self) -> JobId {
        self.dom.job
    }

    /// Free the page-table frames, the root and the MSI page table, credit the job the domain's kernel bytes and its tables', and drop
    /// the domain's use of its job, unless a fence of the domain was abandoned (the leaked charges stay, and so does the use that keeps
    /// the job record: J2). Call after every record was taken. Allocates nothing.
    pub fn finish<M: PhysMem64, F: IoFrames>(mut self, mem: &M, frames: &mut F, jobs: &mut Jobs) {
        debug_assert!(
            self.records.is_none(),
            "Detached::finish before every record was taken (their holds would be lost)"
        );
        let tables = self.chain.free_all(mem, frames);
        frames.free_root(self.root);
        frames.free(self.dom.msi.root());
        let job = self.dom.job;
        jobs.credit(
            job,
            Resource::KernelBytes,
            DOMAIN_BYTES.saturating_add(tables.saturating_mul(PAGE_SIZE)),
        );
        if !self.dom.leaks {
            jobs.release(job);
        }
    }
}

/// The commands that invalidate `pages` pages from `page` in soft context `gscid`: page by page for a short range, the whole soft
/// context for a long one.
#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)] // INVARIANT: i < pages <= INVAL_PAGES in the loop; page + i is inside the window
fn inval_range(gscid: u16, page: u64, pages: u64) -> ([Command; INVAL_PAGES], usize) {
    let mut cmds = [Command::iotinval_gvma(gscid, None); INVAL_PAGES];
    if pages > INVAL_PAGES as u64 {
        return (cmds, 1);
    }
    for i in 0..pages {
        cmds[i as usize] = Command::iotinval_gvma(gscid, Some((page + i) << PAGE_SHIFT));
    }
    (cmds, pages as usize)
}

impl IoDomains {
    /// The store over the directory `ddt` (already written to `ddtp`) and the command queue `cmdq` (already enabled), with `quarantine`
    /// the kernel's empty second-stage table that quarantined devices translate through (soft-context id 0). `None` without memory.
    pub fn new(ddt: Ddt, cmdq: CmdQueue, quarantine: &Iopt) -> Option<IoDomains> {
        let quarantine = quarantine.context_root(u32::from(QUARANTINE_GSCID))?;
        let mut gscids = Vec::new();
        gscids.try_reserve_exact(1024).ok()?;
        gscids.resize(1024, 0u64);
        if let Some(w) = gscids.first_mut() {
            *w = 1 << QUARANTINE_GSCID;
        }
        Some(IoDomains {
            ddt,
            cmdq,
            quarantine,
            slots: Vec::new(),
            free: Vec::new(),
            gscids,
            leaked: None,
            leaked_domains: None,
            stats: IoStats::default(),
            window_start: 0,
            window_lines: 0,
        })
    }

    pub fn stats(&self) -> IoStats {
        self.stats
    }

    // ---- slots ----------------------------------------------------------------------------------------------------------

    fn index_of(&self, d: DomainId) -> Option<usize> {
        let i = usize::try_from((d.0 & 0xffff_ffff).checked_sub(1)?).ok()?;
        let s = self.slots.get(i)?;
        (s.dom.is_some() && u64::from(s.gen) == d.0 >> 32).then_some(i)
    }

    fn dom(&self, d: DomainId) -> Option<&Domain> {
        self.slots.get(self.index_of(d)?)?.dom.as_deref()
    }

    fn dom_mut(&mut self, d: DomainId) -> Option<&mut Domain> {
        let i = self.index_of(d)?;
        self.slots.get_mut(i)?.dom.as_deref_mut()
    }

    /// Room for one more domain: after `Ok`, `place` and `take` allocate nothing.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: free.len() <= slots.len() (every free index is a slot)
    fn reserve_slot(&mut self) -> Result<(), IoError> {
        self.slots.try_reserve(1).map_err(|_| IoError::NoMemory)?;
        let want = self.slots.len() + 1 - self.free.len();
        self.free.try_reserve(want).map_err(|_| IoError::NoMemory)
    }

    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: slot indices are below 2^32 (the free list holds u32; domains are bounded by device ids)
    fn place(&mut self, dom: FBox<Domain>) -> DomainId {
        if let Some(i) = self.free.pop() {
            if let Some(slot) = self.slots.get_mut(i as usize) {
                slot.dom = Some(dom);
                return DomainId((u64::from(slot.gen) << 32) | (u64::from(i) + 1));
            }
        }
        self.slots.push(Slot {
            gen: 0,
            dom: Some(dom),
        });
        DomainId(self.slots.len() as u64)
    }

    /// Take a domain out of its slot; the slot's next generation never matches the old id (a slot whose generations are spent is retired).
    fn take(&mut self, i: usize) -> Option<FBox<Domain>> {
        let slot = self.slots.get_mut(i)?;
        let dom = slot.dom.take()?;
        if slot.gen < u32::MAX {
            slot.gen = slot.gen.wrapping_add(1);
            if let Ok(i) = u32::try_from(i) {
                // (the capacity was reserved when the slot was made: no allocation)
                self.free.push(i);
            }
        }
        self.stats.domains = self.stats.domains.saturating_sub(1);
        Some(dom)
    }

    fn by_device(&self, device_id: u32) -> Option<usize> {
        self.slots
            .iter()
            .position(|s| s.dom.as_ref().is_some_and(|d| d.device == device_id))
    }

    /// The live domain of a device (attached or detaching).
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: i is a slot index, below 2^32
    pub fn domain_of(&self, device_id: u32) -> Option<DomainId> {
        let i = self.by_device(device_id)?;
        let s = self.slots.get(i)?;
        Some(DomainId((u64::from(s.gen) << 32) | (i as u64 + 1)))
    }

    #[allow(clippy::arithmetic_side_effects, clippy::cast_possible_truncation)] // INVARIANT: w < 1024 and b < 64, so w * 64 + b < 65536
    fn alloc_gscid(&mut self) -> Option<u16> {
        for (w, word) in self.gscids.iter_mut().enumerate() {
            if *word != u64::MAX {
                let b = (!*word).trailing_zeros();
                *word |= 1 << b;
                return u16::try_from(w * 64 + b as usize).ok();
            }
        }
        None
    }

    fn free_gscid(&mut self, id: u16) {
        if let Some(w) = self.gscids.get_mut(usize::from(id / 64)) {
            *w &= !(1u64 << (id % 64));
        }
    }

    // ---- the command queue ----------------------------------------------------------------------------------------------

    /// Read the IOMMU's head; false if it is out of range (an IOMMU that misbehaves).
    fn refresh<R: Regs>(&mut self, regs: &mut R) -> bool {
        let head = regs.read32(regs::CQH);
        self.cmdq.completed(head).is_ok()
    }

    fn done(&self, t: Ticket) -> bool {
        t.0 != NO_TICKET && self.cmdq.is_done(t.0)
    }

    /// Queue `cmds` and a fence (with PR and PW) and tell the IOMMU; the fence's ticket. A queue without room for all of them (after
    /// reading the head again) takes none, and the ticket is one that never completes: the caller's wait fails and abandons (6.4).
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: outstanding < entries (one slot is always free), so the room is not negative; cmds has at most INVAL_PAGES + 2 commands
    fn submit<M: PhysMem64, R: Regs>(&mut self, mem: &M, regs: &mut R, cmds: &[Command]) -> Ticket {
        let Some(fence) = Command::iofence_c(None) else {
            return Ticket(NO_TICKET);
        };
        self.refresh(regs);
        let room = u64::from(self.cmdq.entries()) - 1 - self.cmdq.outstanding();
        if room < cmds.len() as u64 + 1 {
            self.stats.queue_stuck = self.stats.queue_stuck.saturating_add(1);
            return Ticket(NO_TICKET);
        }
        let mut tail = self.cmdq.tail();
        for c in cmds.iter().chain(core::iter::once(&fence)) {
            if let Ok(t) = self.cmdq.push(mem, *c) {
                tail = t;
            }
        }
        regs.write32(regs::CQT, tail);
        self.stats.commands = self.stats.commands.saturating_add(cmds.len() as u64 + 1);
        Ticket(self.cmdq.ticket())
    }

    /// Poll a fence (no lock is held between polls; the kernel stops at `IOMMU_FENCE_BOUND`).
    pub fn poll<R: Regs>(&mut self, regs: &mut R, t: Ticket) -> Fence {
        if t.0 == NO_TICKET {
            return Fence::Failed;
        }
        if !self.refresh(regs) {
            return Fence::Failed;
        }
        if self.cmdq.is_done(t.0) {
            return Fence::Done;
        }
        if Cqcsr(regs.read32(regs::CQCSR)).failed() {
            Fence::Failed
        } else {
            Fence::Waiting
        }
    }

    // ---- create ---------------------------------------------------------------------------------------------------------

    /// The state of `device_id` in the directory.
    pub fn device_state<M: PhysMem64>(&self, mem: &M, device_id: u32) -> DeviceState {
        match self.ddt.get(mem, device_id) {
            Some(w) if w[0] & 1 != 0 && w[0] & (1 << 4) != 0 => DeviceState::Quarantined,
            Some(w) if w[0] & 1 != 0 => DeviceState::Attached,
            _ => DeviceState::Unclaimed,
        }
    }

    /// A domain for `device_id`, paid for by `job` (the caller has charged `DOMAIN_BYTES` and holds a use for it: `charge_create`):
    /// an empty page table, an empty MSI page table, the interrupt window reserved in the allocator, a soft-context id; the device's
    /// context is written `translate` (from Unclaimed or Quarantined: one side of every change is a deny state) and the directory
    /// invalidation and a fence are queued. On any error nothing is left behind and the device's state is unchanged.
    pub fn create<M: PhysMem64, R: Regs, F: IoFrames>(
        &mut self,
        mem: &M,
        regs: &mut R,
        frames: &mut F,
        device_id: u32,
        job: JobId,
    ) -> Result<(DomainId, Ticket), IoError> {
        if device_id
            .checked_shr(device_id_bits(self.ddt.levels(), self.ddt.format()))
            .unwrap_or(0)
            != 0
        {
            return Err(IoError::InvalidArgs);
        }
        if self.by_device(device_id).is_some() {
            return Err(IoError::BadState);
        }
        self.reserve_slot()?;
        let mut iova =
            Iova::new(IO_BASE >> PAGE_SHIFT, IO_WINDOW_PAGES).map_err(|_| IoError::NoMemory)?;
        if iova.alloc(IO_VECTOR_PAGES) != Some(IO_BASE >> PAGE_SHIFT) {
            return Err(IoError::NoMemory); // (not reached: a fresh allocator's lowest fit is the window's start)
        }
        let window = MsiWindow::new(IO_BASE, 8).map_err(|_| IoError::NoMemory)?;
        let Some(root) = frames.alloc_root() else {
            return Err(IoError::NoMemory);
        };
        let Some(msi_pa) = frames.alloc() else {
            frames.free_root(root);
            return Err(IoError::NoMemory);
        };
        let undo_frames = |frames: &mut F| {
            frames.free(msi_pa);
            frames.free_root(root);
        };
        let (Ok(iopt), Ok(msi)) = (
            Iopt::new(mem, root, PtMode::Sv39x4),
            MsiTable::new(mem, msi_pa, window),
        ) else {
            undo_frames(frames);
            return Err(IoError::NoMemory);
        };
        let Some(gscid) = self.alloc_gscid() else {
            undo_frames(frames);
            return Err(IoError::NoMemory);
        };
        let Some(ctx_root) = iopt.context_root(u32::from(gscid)) else {
            self.free_gscid(gscid);
            undo_frames(frames);
            return Err(IoError::NoMemory);
        };
        let dom = match FBox::try_new(Domain {
            device: device_id,
            job,
            state: State::Attached,
            gscid,
            iopt: Some(iopt),
            msi,
            iova,
            live: None,
            mappings: 0,
            pending: None,
            leaked: None,
            tables: 0,
            io_pages: 0,
            leaks: false,
            ring: [NO_FAULT; FAULT_RING],
            ring_head: 0,
            ring_len: 0,
            dropped: 0,
            destroy_ticket: NO_TICKET,
            next: None,
        }) {
            Ok(d) => d,
            Err(_) => {
                self.free_gscid(gscid);
                undo_frames(frames);
                return Err(IoError::NoMemory);
            }
        };
        let was_quarantined = self.device_state(mem, device_id) == DeviceState::Quarantined;
        if self
            .ddt
            .set(
                mem,
                frames,
                device_id,
                &DeviceContext::translate(ctx_root, Some(&msi)),
            )
            .is_err()
        {
            drop(dom);
            self.free_gscid(gscid);
            undo_frames(frames);
            return Err(IoError::NoMemory);
        }
        // the context left behind: the directory entry, and (from quarantine) the shared empty table's soft context
        let cmds = [
            Command::iodir_inval_ddt(Some(device_id)),
            Command::iotinval_gvma(QUARANTINE_GSCID, None),
        ];
        let n = if was_quarantined { 2 } else { 1 };
        let ticket = self.submit(mem, regs, cmds.get(..n).unwrap_or(&cmds));
        self.stats.domains = self.stats.domains.saturating_add(1);
        Ok((self.place(dom), ticket))
    }

    pub fn job_of(&self, d: DomainId) -> Option<JobId> {
        Some(self.dom(d)?.job)
    }

    pub fn device_of(&self, d: DomainId) -> Option<u32> {
        Some(self.dom(d)?.device)
    }

    /// The domain's soft-context id (for tests).
    pub fn gscid_of(&self, d: DomainId) -> Option<u16> {
        Some(self.dom(d)?.gscid)
    }

    /// Live mappings, pending ones and leaked ones of a domain.
    pub fn counts(&self, d: DomainId) -> Option<(u64, u64, u64)> {
        let dom = self.dom(d)?;
        let len = |mut l: &Link| {
            let mut n = 0u64;
            while let Some(r) = l {
                n = n.saturating_add(1);
                l = &r.next;
            }
            n
        };
        Some((dom.mappings, len(&dom.pending), len(&dom.leaked)))
    }

    /// The domain's table frames (the root not counted, nor the frames detached onto records), and the pages of its live, pending and
    /// leaked mappings (a leaked range stays taken and charged).
    pub fn usage(&self, d: DomainId) -> Option<(u64, u64)> {
        let dom = self.dom(d)?;
        Some((dom.tables, dom.io_pages))
    }

    /// The live mapping at `io_address`: (pages, VMO, VMO page, access).
    pub fn mapping_at(&self, d: DomainId, io_address: u64) -> Option<(u64, u64, u64, Access)> {
        let m = find(&self.dom(d)?.live, io_address >> PAGE_SHIFT)?;
        Some((m.pages, m.vmo, m.vmo_page, m.access))
    }

    // ---- map ------------------------------------------------------------------------------------------------------------

    /// Map the record's VMO pages, whose frames are `pfns` (from `retain_for_io`, whose hold the record now owns), at the lowest free
    /// range of the domain's window, with the record's access. Table frames come from `frames` (prepaid by `charge_map`). Queues the
    /// invalidation of the range and a fence: the caller waits, and on a failed fence unmaps again (4.3).
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: page + i stays inside the window the allocator handed out; i < pfns.len()
    pub fn map<M: PhysMem64, R: Regs, F: TableFrames>(
        &mut self,
        mem: &M,
        regs: &mut R,
        frames: &mut F,
        d: DomainId,
        rec: Record,
        pfns: &[Pfn],
    ) -> Result<Mapped, MapFailed> {
        let mut rec = rec.0;
        let n = rec.pages;
        if n == 0 || pfns.len() as u64 != n {
            return Err(MapFailed::Refused(IoError::InvalidArgs, Record(rec)));
        }
        let Some(dom) = self.dom_mut(d).filter(|x| x.state == State::Attached) else {
            return Err(MapFailed::Refused(IoError::BadState, Record(rec)));
        };
        let Some(page) = dom.iova.alloc(n) else {
            return Err(MapFailed::Refused(IoError::NoMemory, Record(rec)));
        };
        rec.page = page;
        let Some(iopt) = dom.iopt.as_mut() else {
            return Err(MapFailed::Refused(IoError::BadState, Record(rec)));
        };
        let access = rec.access;
        let before = rec.chain.n;
        let mut df = Deferring {
            mem,
            frames,
            chain: &mut rec.chain,
            allocs: 0,
        };
        let mut written = 0u64;
        let mut failed = false;
        for (i, pfn) in pfns.iter().enumerate() {
            let at = (page + i as u64) << PAGE_SHIFT;
            if iopt
                .map(mem, &mut df, at, pfn << PAGE_SHIFT, access)
                .is_err()
            {
                failed = true;
                break;
            }
            written += 1;
        }
        if failed {
            for i in 0..written {
                iopt.unmap(mem, &mut df, (page + i) << PAGE_SHIFT);
            }
        }
        let allocs = df.allocs;
        let chained = rec.chain.n - before;
        dom.tables = dom.tables + allocs - chained;
        dom.io_pages += n;
        let gscid = dom.gscid;
        if failed {
            if dom.iova.release(page, n).is_err() {
                debug_assert!(false, "a range just allocated is live");
            }
            let (cmds, k) = inval_range(gscid, page, n);
            let ticket = self.submit(mem, regs, cmds.get(..k).unwrap_or(&cmds));
            rec.ticket = ticket.0;
            self.stats.io_pages += n;
            if let Some(dom) = self.dom_mut(d) {
                push(&mut dom.pending, rec);
            }
            return Err(MapFailed::RolledBack {
                pending: Pending {
                    domain: d,
                    page,
                    ticket,
                },
                tables: allocs,
            });
        }
        dom.mappings += 1;
        insert(&mut dom.live, rec);
        self.stats.io_pages += n;
        let (cmds, k) = inval_range(gscid, page, n);
        let ticket = self.submit(mem, regs, cmds.get(..k).unwrap_or(&cmds));
        Ok(Mapped {
            iova: page << PAGE_SHIFT,
            ticket,
            tables: allocs,
        })
    }

    // ---- unmap ----------------------------------------------------------------------------------------------------------

    /// 6.1 step 1: clear the leaves of exactly one live mapping, make its range pending (never handed out until `complete`), queue the
    /// invalidation of the range and a fence. Allocates nothing; the table frames the unmap emptied go onto the record's chain.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: page + i is inside the mapping's range, inside the window
    pub fn unmap<M: PhysMem64, R: Regs>(
        &mut self,
        mem: &M,
        regs: &mut R,
        d: DomainId,
        io_address: u64,
        pages: u64,
    ) -> Result<Pending, IoError> {
        let dom = self
            .dom_mut(d)
            .filter(|x| x.state == State::Attached)
            .ok_or(IoError::BadState)?;
        if io_address & (PAGE_SIZE - 1) != 0 {
            return Err(IoError::InvalidArgs);
        }
        let page = io_address >> PAGE_SHIFT;
        if find(&dom.live, page).is_none_or(|m| m.pages != pages) {
            return Err(IoError::InvalidArgs);
        }
        let Some(mut rec) = remove(&mut dom.live, page) else {
            return Err(IoError::InvalidArgs);
        };
        dom.mappings -= 1;
        let before = rec.chain.n;
        if let Some(iopt) = dom.iopt.as_mut() {
            let mut na = NoAlloc {
                mem,
                chain: &mut rec.chain,
            };
            for i in 0..pages {
                iopt.unmap(mem, &mut na, (page + i) << PAGE_SHIFT);
            }
        }
        dom.tables -= rec.chain.n - before;
        if dom.iova.release(page, pages).is_err() {
            debug_assert!(false, "a live mapping's range is live in the allocator");
        }
        let gscid = dom.gscid;
        let (cmds, k) = inval_range(gscid, page, pages);
        let ticket = self.submit(mem, regs, cmds.get(..k).unwrap_or(&cmds));
        rec.ticket = ticket.0;
        if let Some(dom) = self.dom_mut(d) {
            push(&mut dom.pending, rec);
        }
        Ok(Pending {
            domain: d,
            page,
            ticket,
        })
    }

    /// 6.1 step 3, once the fence has completed: the range becomes free and the record leaves the domain, to be settled by the caller
    /// with no lock held (step 4). `Ok(None)` when the domain's destruction already took the record (its own fence covered it).
    /// `NotDone` before the fence.
    pub fn complete(&mut self, p: &Pending) -> Result<Option<Released>, IoError> {
        if !self.done(p.ticket) {
            return Err(IoError::NotDone);
        }
        let Some(dom) = self.dom_mut(p.domain) else {
            return Ok(None);
        };
        let Some(rec) = unlink(&mut dom.pending, |m| {
            m.page == p.page && m.ticket == p.ticket.0
        }) else {
            return Ok(None);
        };
        if dom.iova.complete(rec.page, rec.pages).is_err() {
            debug_assert!(false, "a pending record's range is pending");
        }
        dom.io_pages = dom.io_pages.saturating_sub(rec.pages);
        let job = dom.job;
        self.stats.io_pages = self.stats.io_pages.saturating_sub(rec.pages);
        Ok(Some(Released { rec, job }))
    }

    /// The fence of `p` will not complete (6.4): the record, with its hold and its detached tables, goes to the leaked list and is never
    /// released; its range stays pending for the life of the domain; its charges stay. Counted. False if there was nothing to abandon.
    pub fn abandon(&mut self, p: &Pending) -> bool {
        let Some(dom) = self.dom_mut(p.domain) else {
            return false;
        };
        let Some(rec) = unlink(&mut dom.pending, |m| {
            m.page == p.page && m.ticket == p.ticket.0
        }) else {
            return false;
        };
        dom.leaks = true;
        let (pages, tables) = (rec.pages, rec.chain.n);
        push(&mut dom.leaked, rec);
        let s = &mut self.stats;
        s.fence_timeouts = s.fence_timeouts.saturating_add(1);
        s.holds_leaked = s.holds_leaked.saturating_add(1);
        s.pages_leaked = s.pages_leaked.saturating_add(pages);
        s.tables_leaked = s.tables_leaked.saturating_add(tables);
        true
    }

    // ---- destroy --------------------------------------------------------------------------------------------------------

    /// 6.3 step 1: refuse new maps, write the device's context to the quarantine state, queue the directory invalidation, the
    /// invalidation of the domain's whole soft context (its translations and MSI entries) and a fence. `None` if the domain is already
    /// being destroyed or gone (destroying a destroyed domain does nothing). Allocates nothing.
    pub fn destroy<M: PhysMem64, R: Regs>(
        &mut self,
        mem: &M,
        regs: &mut R,
        d: DomainId,
    ) -> Option<Destroying> {
        let quarantine = self.quarantine;
        let dom = self.dom_mut(d).filter(|x| x.state == State::Attached)?;
        dom.state = State::Detaching;
        let (device, gscid) = (dom.device, dom.gscid);
        let mut chain = Chain::default();
        let mut na = NoAlloc {
            mem,
            chain: &mut chain,
        };
        // (the device's directory pages exist: writing a valid context over a valid one allocates and frees nothing)
        if self
            .ddt
            .set(mem, &mut na, device, &DeviceContext::deny(quarantine))
            .is_err()
        {
            self.ddt.clear(mem, &mut na, device);
        }
        debug_assert_eq!(chain.n, 0, "a quarantine write freed no directory page");
        let cmds = [
            Command::iodir_inval_ddt(Some(device)),
            Command::iotinval_gvma(gscid, None),
        ];
        let ticket = self.submit(mem, regs, &cmds);
        if let Some(dom) = self.dom_mut(d) {
            dom.destroy_ticket = ticket.0;
        }
        Some(Destroying { domain: d, ticket })
    }

    /// 6.3 steps 2 to 4, once the fence has completed (IOMMU7 retires the domain's interrupt identities before this): the domain leaves
    /// the store with every table frame, its mappings (live, and pending ones whose waiters have not completed them: this fence covers
    /// them) and its soft-context id, which is free again. Mappings whose own fence was abandoned stay leaked. `NotDone` before the fence;
    /// `Ok(None)` if there is nothing (another caller completed or abandoned it). Allocates nothing.
    pub fn destroy_complete<M: PhysMem64>(
        &mut self,
        mem: &M,
        x: &Destroying,
    ) -> Result<Option<Detached>, IoError> {
        if !self.done(x.ticket) {
            return Err(IoError::NotDone);
        }
        let Some(i) = self.index_of(x.domain) else {
            return Ok(None);
        };
        if self
            .slots
            .get(i)
            .and_then(|s| s.dom.as_ref())
            .is_none_or(|dom| dom.state != State::Detaching || dom.destroy_ticket != x.ticket.0)
        {
            return Ok(None);
        }
        let Some(mut dom) = self.take(i) else {
            return Ok(None);
        };
        let mut chain = Chain::default();
        let mut root = 0;
        if let Some(iopt) = dom.iopt.take() {
            root = iopt.destroy(
                mem,
                &mut NoAlloc {
                    mem,
                    chain: &mut chain,
                },
            );
        }
        debug_assert_eq!(chain.n, dom.tables, "every table frame is on the chain");
        let mut records = None;
        flatten(dom.live.take(), &mut records);
        while let Some(r) = pop(&mut dom.pending) {
            push(&mut records, r);
        }
        while let Some(r) = pop(&mut dom.leaked) {
            push(&mut self.leaked, r);
        }
        let mut live_pages = 0u64;
        let mut l = &records;
        while let Some(r) = l {
            live_pages = live_pages.saturating_add(r.pages);
            l = &r.next;
        }
        self.stats.io_pages = self.stats.io_pages.saturating_sub(live_pages);
        dom.io_pages = dom.io_pages.saturating_sub(live_pages);
        dom.mappings = 0;
        let gscid = dom.gscid;
        self.free_gscid(gscid);
        Ok(Some(Detached {
            dom,
            records,
            chain,
            root,
        }))
    }

    /// The destruction's fence will not complete (6.4): everything it would have released is leaked and counted: the domain's tables,
    /// root and MSI page table, its mappings with their holds, its soft-context id (never reused) and its charges. The device id is free
    /// for a new domain (whose own context write and fence follow). False if there was nothing to abandon.
    pub fn destroy_abandon(&mut self, x: &Destroying) -> bool {
        let Some(i) = self.index_of(x.domain) else {
            return false;
        };
        if self
            .slots
            .get(i)
            .and_then(|s| s.dom.as_ref())
            .is_none_or(|dom| dom.state != State::Detaching || dom.destroy_ticket != x.ticket.0)
        {
            return false;
        }
        let Some(mut dom) = self.take(i) else {
            return false;
        };
        dom.leaks = true;
        let mut holds = 0u64;
        let mut pages = 0u64;
        let mut tables = dom.tables.saturating_add(ROOT_FRAMES + 1);
        count_tree(&dom.live, &mut holds, &mut pages, &mut tables);
        let mut l = &dom.pending;
        while let Some(r) = l {
            holds = holds.saturating_add(1);
            pages = pages.saturating_add(r.pages);
            tables = tables.saturating_add(r.chain.n);
            l = &r.next;
        }
        let s = &mut self.stats;
        s.fence_timeouts = s.fence_timeouts.saturating_add(1);
        s.holds_leaked = s.holds_leaked.saturating_add(holds);
        s.pages_leaked = s.pages_leaked.saturating_add(pages);
        s.tables_leaked = s.tables_leaked.saturating_add(tables);
        dom.next = self.leaked_domains.take();
        self.leaked_domains = Some(dom);
        true
    }

    // ---- faults ---------------------------------------------------------------------------------------------------------

    /// A record from the fault queue (7.1): counted; stored in the ring of the device's domain if it has an attached one (a quarantined
    /// or detaching device's faults reach no ring); and whether the console line is due, at most `FAULT_LINES_PER_SECOND` per second of
    /// the `time` counter (`now`, `freq` ticks a second). Allocates nothing.
    pub fn fault(&mut self, rec: FaultRecord, now: u64, freq: u64) -> FaultOutcome {
        let s = &mut self.stats;
        s.faults = s.faults.saturating_add(1);
        s.last_fault = Some((rec.device_id, rec.iotval, rec.cause));
        let mut domain = None;
        if let Some(i) = self.by_device(rec.device_id) {
            if let Some(slot) = self.slots.get_mut(i) {
                let gen = slot.gen;
                if let Some(dom) = slot
                    .dom
                    .as_deref_mut()
                    .filter(|x| x.state == State::Attached)
                {
                    dom.store_fault(rec);
                    domain = u64::try_from(i)
                        .ok()
                        .map(|i| DomainId((u64::from(gen) << 32) | i.saturating_add(1)));
                }
            }
        }
        if now.saturating_sub(self.window_start) >= freq || now < self.window_start {
            self.window_start = now;
            self.window_lines = 0;
        }
        let print = self.window_lines < FAULT_LINES_PER_SECOND;
        if print {
            self.window_lines = self.window_lines.saturating_add(1);
        } else {
            self.stats.faults_unprinted = self.stats.faults_unprinted.saturating_add(1);
        }
        FaultOutcome { domain, print }
    }

    /// `iodomain_fault_read` (7.4): the oldest unread record of the domain's ring, with the faults dropped since the previous successful
    /// read (reset by this one). `Ok(None)` when the ring is empty (SHOULD_WAIT; the count is kept). `BadState` once the domain is gone.
    pub fn fault_read(&mut self, d: DomainId) -> Result<Option<FaultRead>, IoError> {
        Ok(self.dom_mut(d).ok_or(IoError::BadState)?.read_fault())
    }
}

/// Count a treap's records, pages and detached tables (recursion is bounded by the treap's depth, logarithmic in expectation).
fn count_tree(t: &Link, holds: &mut u64, pages: &mut u64, tables: &mut u64) {
    if let Some(n) = t {
        *holds = holds.saturating_add(1);
        *pages = pages.saturating_add(n.pages);
        *tables = tables.saturating_add(n.chain.n);
        count_tree(&n.left, holds, pages, tables);
        count_tree(&n.right, holds, pages, tables);
    }
}

impl Drop for IoDomains {
    fn drop(&mut self) {
        // (only the host tests drop a store; long lists are dropped one record at a time, never by recursion)
        drop_list(&mut self.leaked);
        while let Some(mut d) = self.leaked_domains.take() {
            self.leaked_domains = d.next.take();
            drop_list(&mut d.pending);
            drop_list(&mut d.leaked);
            let mut l = None;
            flatten(d.live.take(), &mut l);
            drop_list(&mut l);
        }
        for s in &mut self.slots {
            if let Some(d) = s.dom.as_mut() {
                drop_list(&mut d.pending);
                drop_list(&mut d.leaked);
                let mut l = None;
                flatten(d.live.take(), &mut l);
                drop_list(&mut l);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------------
// charging (the caller's steps under `JOBS`: IOMMU-SPEC 5.3, J1, J2)

/// Charge `job` a domain's kernel bytes and take the use the domain holds of it (from creation until its record is freed: J2). Undo with
/// `uncharge_create` if the creation fails.
pub fn charge_create(jobs: &mut Jobs, job: JobId) -> Result<(), JobError> {
    jobs.charge(job, Resource::KernelBytes, DOMAIN_BYTES)?;
    jobs.retain(job);
    Ok(())
}

pub fn uncharge_create(jobs: &mut Jobs, job: JobId) {
    jobs.credit(job, Resource::KernelBytes, DOMAIN_BYTES);
    jobs.release(job);
}

/// The table frames a map of `pages` pages may need at most, wherever its range falls: the leaf tables it spans (one more than a full
/// count, for a range that straddles), and the tables above them (likewise). The root is the domain's.
pub const fn table_prepay(pages: u64) -> u64 {
    pages
        .div_ceil(512)
        .saturating_add(1)
        .saturating_add(pages.div_ceil(512 * 512).saturating_add(1))
}

/// What a map charged its domain's job before the store was entered: the I/O-mapped pages, the record's kernel bytes and the table
/// frames it may need (prepaid, because `JOBS` is never taken under `IOMMU`).
#[derive(Debug)]
#[must_use = "a map's charge is settled or refunded"]
pub struct MapCharge {
    job: JobId,
    pages: u64,
    prepaid: u64,
}

/// Charge a map of `pages` pages to `job`, all or nothing (J1).
pub fn charge_map(jobs: &mut Jobs, job: JobId, pages: u64) -> Result<MapCharge, JobError> {
    let prepaid = table_prepay(pages);
    jobs.charge_io(job, IoResource::MappedPages, pages)?;
    let bytes = MAPPING_BYTES.saturating_add(prepaid.saturating_mul(PAGE_SIZE));
    if let Err(e) = jobs.charge(job, Resource::KernelBytes, bytes) {
        jobs.credit_io(job, IoResource::MappedPages, pages);
        return Err(e);
    }
    Ok(MapCharge {
        job,
        pages,
        prepaid,
    })
}

impl MapCharge {
    /// The store took `tables` table frames (`Mapped::tables`, `MapFailed::RolledBack`): give back the rest of the prepayment. What
    /// stays is credited when the mapping's record is settled and when its tables are freed.
    pub fn settle(self, jobs: &mut Jobs, tables: u64) {
        debug_assert!(
            tables <= self.prepaid,
            "the prepayment covers every table frame"
        );
        let rest = self.prepaid.saturating_sub(tables);
        if rest > 0 {
            jobs.credit(
                self.job,
                Resource::KernelBytes,
                rest.saturating_mul(PAGE_SIZE),
            );
        }
    }

    /// The map was refused before anything was written: give everything back.
    pub fn refund(self, jobs: &mut Jobs) {
        jobs.credit_io(self.job, IoResource::MappedPages, self.pages);
        jobs.credit(
            self.job,
            Resource::KernelBytes,
            MAPPING_BYTES.saturating_add(self.prepaid.saturating_mul(PAGE_SIZE)),
        );
    }
}
