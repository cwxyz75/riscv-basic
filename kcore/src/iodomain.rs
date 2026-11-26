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
/// Every hold of `IOMMU` does at most this many pages of work (IOMMU-SPEC amendment 2026-11-26 part 1, OD168): a map writes, an unmap
/// clears and a destroyed domain's completion frees this many leaves or tables at a time, the lock released between steps.
pub const RANGE_STEP: u64 = crate::aspace::RANGE_STEP as u64;
/// The ticket of an unmap whose invalidation and fence the negative-control kernel skips (`fault-iommu-skip-inval`): done at once.
#[cfg(feature = "fault-iommu-skip-inval")]
const SKIPPED: u64 = u64::MAX - 1;
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

impl DomainId {
    /// The id as a word (what the kernel's domain object stores).
    pub const fn raw(self) -> u64 {
        self.0
    }
    /// An id from a word `raw` gave (any other word names no domain: every lookup checks the slot and its generation).
    pub const fn from_raw(raw: u64) -> DomainId {
        DomainId(raw)
    }
}

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
    /// The commands found the queue full and were not queued: queue them again with the operation's `requeue_*` (which allocates
    /// nothing) and keep waiting, within the same bound. A full queue is not a failure (6.4): only the bound or a queue error is.
    Unqueued,
    /// The command queue reported an error: the fence will never complete (6.4).
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
    /// Leaves an unmap (or a roll-back) has cleared so far: a record in the pending list with fewer than `pages` cleared is being
    /// unmapped in steps (OD168, OD170), and has no fence yet.
    cleared: u64,
    vmo: u64,
    vmo_page: u64,
    access: Access,
    ticket: u64,
    prio: u64,
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
            cleared: 0,
            vmo,
            vmo_page,
            access,
            ticket: NO_TICKET,
            prio: 0,
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

/// The next priority for the treap (splitmix64 over the store's secret state): random and unknown to callers, so the shape is
/// balanced in expectation whatever addresses are kept (the recursion of `split`, `merge` and `insert` is bounded by the depth).
fn next_prio(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
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
            if a.prio > b.prio {
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
        Some(cur) if cur.prio >= n.prio => {
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

/// Is there a record in `list` that `pred` accepts?
fn find_list(list: &Link, pred: impl Fn(&Mapping) -> bool) -> bool {
    let mut cur = list;
    while let Some(n) = cur {
        if pred(n) {
            return true;
        }
        cur = &n.next;
    }
    false
}

/// Find the first record of `list` that `pred` accepts.
fn find_list_mut(list: &mut Link, pred: impl Fn(&Mapping) -> bool) -> Option<&mut Mapping> {
    // (found by position first: a borrow returned from inside the walk would outlive the loop's own)
    let mut at = 0usize;
    let mut cur = &*list;
    loop {
        let n = cur.as_deref()?;
        if pred(n) {
            break;
        }
        at = at.saturating_add(1);
        cur = &n.next;
    }
    let mut n = list.as_deref_mut()?;
    for _ in 0..at {
        n = n.next.as_deref_mut()?;
    }
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
    /// Mappings whose leaves are being cleared or are cleared and whose fence is outstanding, how many, and the table frames their
    /// removals detached (so an abandoned destruction counts its leak without a walk).
    pending: Link,
    pending_n: u64,
    pending_tables: u64,
    /// Mappings whose fence was abandoned: never freed (IO8), and their pages (counted in `io_pages` too: a leaked range stays taken).
    leaked: Link,
    leaked_pages: u64,
    /// The caller's name for the domain (the kernel's object), handed back with a fault (`FaultOutcome::tag`).
    tag: [u64; 2],
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
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: the counts are of the list's records and their chained frames
    fn push_pending(&mut self, rec: FBox<Mapping>) {
        self.pending_n += 1;
        self.pending_tables += rec.chain.n;
        push(&mut self.pending, rec);
    }

    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: as push_pending: the record was counted when it joined
    fn unlink_pending(&mut self, pred: impl Fn(&Mapping) -> bool) -> Link {
        let rec = unlink(&mut self.pending, pred)?;
        self.pending_n -= 1;
        self.pending_tables -= rec.chain.n;
        Some(rec)
    }

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
    /// Times the command queue had no room and the commands were not queued (the waiter queues them again).
    pub queue_stuck: u64,
    /// Directory pages a failed `create` unlinked, waiting for their fence before they are freed.
    pub dir_pages_waiting: u64,
    /// Mapping records leaked with their holds, the pages they mapped, and table frames leaked (roots and MSI tables included).
    pub holds_leaked: u64,
    pub pages_leaked: u64,
    pub tables_leaked: u64,
    /// Live domains (attached or detaching).
    pub domains: u64,
    /// Domain slots made (a destroyed domain's slot is reused by a later domain, under a new generation).
    pub slots: u64,
    /// Pages of live and pending mappings (leaked ones are counted apart, in `pages_leaked`).
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
    /// Records leaked by abandoned fences of domains that are gone: a list of lists, the heads linked through `left`.
    leaked: Link,
    leaked_domains: Option<FBox<Domain>>,
    /// Destructions abandoned (a map in steps whose domain went meanwhile leaks its record if this moved: `MapFailed::Leaked`).
    abandons: u64,
    stats: IoStats,
    window_start: u64,
    window_lines: u32,
    /// The state of the generator of the treap's priorities (secret, so no caller can choose addresses that unbalance it).
    prio_state: u64,
    /// Directory pages a failed `create` unlinked, freed once `dir_dead_ticket`'s fence (after a whole-directory invalidation) is done.
    dir_dead: Chain,
    dir_dead_ticket: Ticket,
}

/// A fault handed to the store: which domain stored it, and whether the console line is due.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaultOutcome {
    /// The domain whose ring took the record (raise DEVICE_FAULT on it), if the device is attached.
    pub domain: Option<DomainId>,
    /// Print the line (within the rate).
    pub print: bool,
    /// The domain's tag (`IoDomains::set_tag`: the kernel's object), if a ring took the record.
    pub tag: Option<[u64; 2]>,
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
    /// settles the prepayment; `error` is the map's (NO_MEMORY for a table frame, BAD_STATE for a destruction that began meanwhile).
    RolledBack {
        pending: Pending,
        tables: u64,
        error: IoError,
    },
    /// The domain's destruction completed while the map was in steps (OD168 point c): its fence covered every leaf the map wrote and
    /// its tables went with the domain. The record comes back at once, to be settled (its hold dropped) like a completed unmap's; the
    /// map is BAD_STATE.
    Dropped { released: Released, tables: u64 },
    /// As `Dropped`, but a destruction's fence was abandoned since the map began: the record was leaked with its hold (IO8). The map
    /// is BAD_STATE; the caller settles `tables` and keeps the record's charges.
    Leaked { tables: u64 },
}

/// A map in steps (OD168): its range is reserved and its record is the map's alone until the last step publishes it.
#[derive(Debug)]
#[must_use = "a map in progress owns its record and hold: step it to the end"]
pub struct MapCursor {
    domain: DomainId,
    job: JobId,
    rec: FBox<Mapping>,
    /// Leaves written so far (the record's `cleared` counts the leaves a roll-back has cleared again).
    written: u64,
    /// Table frames the map allocated.
    allocs: u64,
    /// The map is failing with this error, and its steps clear what it wrote.
    back: Option<IoError>,
    /// The store's count of abandoned destructions when the map began.
    abandons: u64,
}

impl MapCursor {
    /// The VMO pages whose frames the next step writes: (first VMO page, count, at most `RANGE_STEP`); `None` when the next step
    /// writes nothing (the last step published the record, or the map is rolling back).
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: written <= pages; vmo_page + pages was checked by the VMO's hold
    pub fn next_frames(&self) -> Option<(u64, u64)> {
        if self.back.is_some() || self.written >= self.rec.pages {
            return None;
        }
        Some((
            self.rec.vmo_page + self.written,
            (self.rec.pages - self.written).min(RANGE_STEP),
        ))
    }
    pub fn domain(&self) -> DomainId {
        self.domain
    }
    pub fn vmo(&self) -> u64 {
        self.rec.vmo
    }
    pub fn pages(&self) -> u64 {
        self.rec.pages
    }
    /// The I/O address the map will have.
    pub fn io_address(&self) -> u64 {
        self.rec.page << PAGE_SHIFT
    }
}

/// One step of a map.
#[derive(Debug)]
pub enum MapStep {
    /// Take `IOMMU` again (after reading the next frames: `MapCursor::next_frames`).
    More(MapCursor),
    /// The map is published; wait for its fence.
    Done(Mapped),
    Failed(MapFailed),
}

/// An unmap in steps (OD168): its record is in the domain's pending list, its range pending, and its leaves are being cleared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "an unmap in progress is stepped to the end"]
pub struct Unmapping {
    domain: DomainId,
    page: u64,
}

/// One step of an unmap.
#[derive(Debug)]
pub enum UnmapStep {
    /// Take `IOMMU` again.
    More(Unmapping),
    /// Every leaf is clear and the invalidation and fence are queued: wait, then `complete`.
    Queued(Pending),
    /// The domain's destruction took the record (OD170): the unmap is OK, and the destruction releases the record after its own fence.
    Taken,
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
#[must_use = "a released mapping owns a hold on its VMO and charges: drop the hold and settle it"]
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
    /// The first VMO page the record maps.
    pub fn vmo_page(&self) -> u64 {
        self.rec.vmo_page
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

/// A destroyed domain, out of the store after its fence (nothing can walk its tables any more, so it is worked on with no lock): its
/// mappings ([`Detached::next_record`], each settled as a [`Released`]), its tables a step at a time ([`Detached::step`]), then its
/// root, MSI page table and charge ([`Detached::finish`]). Every call does a bounded amount of work (OD168).
#[must_use = "a detached domain owns its mappings' holds, its frames and its charge: take every record, then finish it"]
pub struct Detached {
    dom: FBox<Domain>,
    cursor: u64,
    freed: u64,
    tables_done: bool,
}

impl Detached {
    /// The next of the domain's mappings (live, pending or being unmapped at the destruction), whose hold the caller drops and which it
    /// settles. Amortized constant work: the index is taken apart by rotations, never by recursion.
    pub fn next_record(&mut self) -> Option<Released> {
        let job = self.dom.job;
        if let Some(rec) = pop(&mut self.dom.pending) {
            return Some(Released { rec, job });
        }
        let mut root = self.dom.live.take()?;
        // rotate right until the root has no left child, then the root is the smallest record: pop it
        while let Some(mut l) = root.left.take() {
            root.left = l.right.take();
            l.right = Some(root);
            root = l;
        }
        self.dom.live = root.right.take();
        Some(Released { rec: root, job })
    }

    pub fn job(&self) -> JobId {
        self.dom.job
    }

    /// Free at most `RANGE_STEP` of the domain's table frames; true when they are all freed. Allocates nothing.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: freed counts frames of one table
    pub fn step<M: PhysMem64, F: TableFrames>(&mut self, mem: &M, frames: &mut F) -> bool {
        if self.tables_done {
            return true;
        }
        let Some(iopt) = self.dom.iopt.as_ref() else {
            self.tables_done = true;
            return true;
        };
        let (k, done) = iopt.destroy_step(mem, frames, &mut self.cursor, RANGE_STEP);
        self.freed += k;
        self.tables_done = done;
        done
    }

    /// Free what is left of the page table (call `step` first to keep each call bounded), the root and the MSI page table, credit the
    /// job the domain's kernel bytes and its tables', and drop the domain's use of its job, unless a fence of the domain was abandoned
    /// (the leaked charges stay, and so does the use that keeps the job record: J2). Call after every record was taken. Allocates
    /// nothing.
    pub fn finish<M: PhysMem64, F: IoFrames>(mut self, mem: &M, frames: &mut F, jobs: &mut Jobs) {
        debug_assert!(
            self.dom.pending.is_none() && self.dom.live.is_none(),
            "Detached::finish before every record was taken (their holds would be lost)"
        );
        while !self.step(mem, frames) {}
        debug_assert_eq!(
            self.freed, self.dom.tables,
            "every table frame of the domain was freed"
        );
        if let Some(iopt) = self.dom.iopt.take() {
            frames.free_root(iopt.root());
        }
        frames.free(self.dom.msi.root());
        let job = self.dom.job;
        jobs.credit(
            job,
            Resource::KernelBytes,
            DOMAIN_BYTES.saturating_add(self.freed.saturating_mul(PAGE_SIZE)),
        );
        if !self.dom.leaks {
            jobs.release(job);
        }
    }
}

/// The commands that invalidate what an unmap (or a map's roll-back) of `rec` changed: page by page for a short range that emptied no
/// table; otherwise the whole soft context. An address-specific `IOTINVAL.GVMA` need not drop the IOMMU's cached non-leaf entries
/// (page-walk caches), so a table frame the unmap detached is covered only by the whole-context form, which drops them all.
fn inval_for(gscid: u16, rec: &Mapping) -> ([Command; INVAL_PAGES], usize) {
    if rec.chain.n > 0 {
        return ([Command::iotinval_gvma(gscid, None); INVAL_PAGES], 1);
    }
    inval_range(gscid, rec.page, rec.pages)
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
    ///
    /// `seed` seeds the priorities of the mapping index (a treap): the kernel passes entropy, so the index's depth stays logarithmic in
    /// expectation whatever addresses a driver keeps.
    pub fn new(ddt: Ddt, cmdq: CmdQueue, quarantine: &Iopt, seed: u64) -> Option<IoDomains> {
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
            abandons: 0,
            stats: IoStats::default(),
            window_start: 0,
            window_lines: 0,
            prio_state: seed,
            dir_dead: Chain::default(),
            dir_dead_ticket: Ticket(NO_TICKET),
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
        self.stats.slots = self.slots.len() as u64;
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
        #[cfg(feature = "fault-iommu-skip-inval")]
        if t.0 == SKIPPED {
            return true;
        }
        t.0 != NO_TICKET && self.cmdq.is_done(t.0)
    }

    /// Queue `cmds` and a fence (with PR and PW) and tell the IOMMU; the fence's ticket. A queue without room for all of them (after
    /// reading the head again) takes none, and the ticket says so (`poll` gives `Unqueued`): the waiter queues them again.
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
            return Fence::Unqueued;
        }
        #[cfg(feature = "fault-iommu-skip-inval")]
        if t.0 == SKIPPED {
            return Fence::Done;
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

    /// Queue the invalidation of the whole directory and a fence for the dead directory pages (a later fence covers earlier pages
    /// too: fences complete in order). Allocates nothing; a full queue leaves the ticket unqueued for the next create to retry.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: the counter is the chain's length, a count of frames
    fn queue_dir_dead<M: PhysMem64, R: Regs>(&mut self, mem: &M, regs: &mut R) {
        self.dir_dead_ticket = self.submit(mem, regs, &[Command::iodir_inval_ddt(None)]);
        self.stats.dir_pages_waiting = self.dir_dead.n;
    }

    /// Free the dead directory pages once their fence is done (or queue it, if it found the queue full).
    fn free_dir_dead<M: PhysMem64, R: Regs, F: TableFrames>(
        &mut self,
        mem: &M,
        regs: &mut R,
        frames: &mut F,
    ) {
        if self.dir_dead.n == 0 {
            return;
        }
        if self.dir_dead_ticket.0 == NO_TICKET {
            self.queue_dir_dead(mem, regs);
            return;
        }
        self.refresh(regs);
        if self.done(self.dir_dead_ticket) {
            self.dir_dead.free_all(mem, frames);
            self.stats.dir_pages_waiting = 0;
        }
    }

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
        self.free_dir_dead(mem, regs, frames);
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
            pending_n: 0,
            pending_tables: 0,
            leaked: None,
            leaked_pages: 0,
            tag: [0; 2],
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
        // (a directory page the write links and then unlinks again on a failure may already be in the IOMMU's directory cache: it is
        // freed only after a whole-directory invalidation and its fence, by a later create)
        let before = self.dir_dead.n;
        let mut df = Deferring {
            mem,
            frames,
            chain: &mut self.dir_dead,
            allocs: 0,
        };
        let set = self.ddt.set(
            mem,
            &mut df,
            device_id,
            &DeviceContext::translate(ctx_root, Some(&msi)),
        );
        if set.is_err() {
            drop(dom);
            self.free_gscid(gscid);
            undo_frames(frames);
            if self.dir_dead.n > before {
                self.queue_dir_dead(mem, regs);
            }
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

    /// The domain's job, while it is in the store. A `JobId` is an index that is reused once its job is freed, so the kernel does not
    /// carry this across a lock release to charge later: it charges the job its domain *object* names, whose own use keeps the job
    /// alive until the last handle and vector object are gone (IOMMU-SPEC 4.5; the use the store holds, `charge_create`'s, ends at
    /// `Detached::finish`).
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
    /// range of the domain's window, with the record's access: every step of [`IoDomains::map_begin`] and [`IoDomains::map_step`] in
    /// one call (the host tests' single critical section; the kernel takes `IOMMU` once per step). Table frames come from `frames`
    /// (prepaid by `charge_map`). Queues the invalidation of the range and a fence: the caller waits, and on a failed fence unmaps
    /// again (4.3).
    pub fn map<M: PhysMem64, R: Regs, F: TableFrames>(
        &mut self,
        mem: &M,
        regs: &mut R,
        frames: &mut F,
        d: DomainId,
        rec: Record,
        pfns: &[Pfn],
    ) -> Result<Mapped, MapFailed> {
        if rec.0.pages == 0 || pfns.len() as u64 != rec.0.pages {
            return Err(MapFailed::Refused(IoError::InvalidArgs, rec));
        }
        let mut cur = self.map_begin(d, rec)?;
        loop {
            let chunk = match cur.next_frames() {
                Some((first, n)) => {
                    let at = usize::try_from(first.saturating_sub(cur.rec.vmo_page))
                        .unwrap_or(usize::MAX);
                    pfns.get(at..at.saturating_add(usize::try_from(n).unwrap_or(usize::MAX)))
                        .unwrap_or(&[])
                }
                None => &[],
            };
            match self.map_step(mem, regs, frames, cur, chunk) {
                MapStep::More(c) => cur = c,
                MapStep::Done(m) => return Ok(m),
                MapStep::Failed(f) => return Err(f),
            }
        }
    }

    /// The first hold of a map (OD168 point a): reserve the lowest free range of the record's length in the domain's window. The range
    /// stays reserved (never handed out) through the steps; the record is not published until the last one. Allocates nothing.
    pub fn map_begin(&mut self, d: DomainId, rec: Record) -> Result<MapCursor, MapFailed> {
        let mut rec = rec.0;
        let n = rec.pages;
        if n == 0 {
            return Err(MapFailed::Refused(IoError::InvalidArgs, Record(rec)));
        }
        let prio = next_prio(&mut self.prio_state);
        let Some(dom) = self
            .dom_mut(d)
            .filter(|x| x.state == State::Attached && x.iopt.is_some())
        else {
            return Err(MapFailed::Refused(IoError::BadState, Record(rec)));
        };
        let job = dom.job;
        let Some(page) = dom.iova.alloc(n) else {
            return Err(MapFailed::Refused(IoError::NoMemory, Record(rec)));
        };
        rec.page = page;
        rec.cleared = 0;
        rec.prio = prio;
        Ok(MapCursor {
            domain: d,
            job,
            rec,
            written: 0,
            allocs: 0,
            back: None,
            abandons: self.abandons,
        })
    }

    /// One more step of a map, one hold of `IOMMU` (OD168 points a and c): write the leaves of the next `RANGE_STEP` pages, whose
    /// frames are `pfns` (`MapCursor::next_frames` says which: the caller reads them under `VMO` between steps), or, once the map is
    /// failing, clear `RANGE_STEP` of the leaves it wrote. The last write publishes the record and queues the invalidation and the
    /// fence (`Done`). A domain marked destroyed since the last step makes the map roll back its own leaves and fail with BAD_STATE;
    /// a table frame that cannot be allocated, with NO_MEMORY. A finished roll-back is an unmap (`MapFailed::RolledBack`: its
    /// fence is waited for and completed like one); a domain whose destruction has completed meanwhile took the leaves with its
    /// tables after its own fence, and the record comes back at once (`MapFailed::Dropped`). A destruction never waits for a map.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: page + i stays inside the reserved range; written, undone <= pages; tables counts frames in the table
    pub fn map_step<M: PhysMem64, R: Regs, F: TableFrames>(
        &mut self,
        mem: &M,
        regs: &mut R,
        frames: &mut F,
        mut cur: MapCursor,
        pfns: &[Pfn],
    ) -> MapStep {
        let Some(dom) = self.dom_mut(cur.domain) else {
            let tables = cur.allocs;
            if self.abandons != cur.abandons {
                // a destruction was abandoned since the map began, perhaps this domain's: its tables may still be walked, so the
                // record keeps its hold for good (counted as leaked, IO8). Only an IOMMU that has stopped completing fences gets here.
                let rec = cur.rec;
                let s = &mut self.stats;
                s.holds_leaked = s.holds_leaked.saturating_add(1);
                s.pages_leaked = s.pages_leaked.saturating_add(rec.pages);
                s.tables_leaked = s.tables_leaked.saturating_add(rec.chain.n);
                self.leak_list(rec);
                return MapStep::Failed(MapFailed::Leaked { tables });
            }
            // the destruction completed: its fence covered every leaf this map wrote, and its tables went with it
            return MapStep::Failed(MapFailed::Dropped {
                released: Released {
                    rec: cur.rec,
                    job: cur.job,
                },
                tables,
            });
        };
        if dom.state != State::Attached && cur.back.is_none() {
            cur.back = Some(IoError::BadState);
            cur.rec.cleared = 0;
        }
        let Some(iopt) = dom.iopt.as_mut() else {
            // (not reached: a domain in the store keeps its table until its destruction completes)
            cur.back.get_or_insert(IoError::BadState);
            return MapStep::More(cur);
        };
        let n = cur.rec.pages;
        let page = cur.rec.page;
        let access = cur.rec.access;
        if cur.back.is_none() {
            let want = (n - cur.written).min(RANGE_STEP);
            if pfns.len() as u64 != want {
                debug_assert!(false, "a map step is given the frames next_frames names");
                cur.back = Some(IoError::InvalidArgs);
                return MapStep::More(cur);
            }
            let before = cur.rec.chain.n;
            let mut df = Deferring {
                mem,
                frames,
                chain: &mut cur.rec.chain,
                allocs: 0,
            };
            for pfn in pfns {
                let at = (page + cur.written) << PAGE_SHIFT;
                if iopt
                    .map(mem, &mut df, at, pfn << PAGE_SHIFT, access)
                    .is_err()
                {
                    cur.back = Some(IoError::NoMemory);
                    break;
                }
                cur.written += 1;
            }
            let allocs = df.allocs;
            cur.allocs += allocs;
            dom.tables = dom.tables + allocs - (cur.rec.chain.n - before);
            if cur.back.is_none() {
                if cur.written < n {
                    return MapStep::More(cur);
                }
                // published: from here an unmap or the destruction can name it
                dom.mappings += 1;
                dom.io_pages += n;
                let gscid = dom.gscid;
                let tables = cur.allocs;
                insert(&mut dom.live, cur.rec);
                self.stats.io_pages += n;
                let (cmds, k) = inval_range(gscid, page, n);
                let ticket = self.submit(mem, regs, cmds.get(..k).unwrap_or(&cmds));
                return MapStep::Done(Mapped {
                    iova: page << PAGE_SHIFT,
                    ticket,
                    tables,
                });
            }
            return MapStep::More(cur); // (the roll-back starts at the next hold)
        }
        // rolling back: clear what was written, a step at a time (`cleared` counts the leaves undone)
        let mut cleared = cur.rec.cleared;
        let undo = (cur.written - cleared).min(RANGE_STEP);
        let before = cur.rec.chain.n;
        let mut na = NoAlloc {
            mem,
            chain: &mut cur.rec.chain,
        };
        for _ in 0..undo {
            iopt.unmap(mem, &mut na, (page + cleared) << PAGE_SHIFT);
            cleared += 1;
        }
        cur.rec.cleared = cleared;
        dom.tables -= cur.rec.chain.n - before;
        if cur.rec.cleared < cur.written {
            return MapStep::More(cur);
        }
        // every leaf is cleared: the roll-back is an unmap (4.3, 6.1): the range pending, the record waiting for a fence
        if dom.iova.release(page, n).is_err() {
            debug_assert!(false, "a range a map reserved is live");
        }
        dom.io_pages += n;
        let gscid = dom.gscid;
        let error = cur.back.unwrap_or(IoError::NoMemory);
        let tables = cur.allocs;
        let mut rec = cur.rec;
        rec.cleared = rec.pages;
        let (cmds, k) = inval_for(gscid, &rec);
        let ticket = self.submit(mem, regs, cmds.get(..k).unwrap_or(&cmds));
        rec.ticket = ticket.0;
        self.stats.io_pages += n;
        if let Some(dom) = self.dom_mut(cur.domain) {
            dom.push_pending(rec);
        }
        MapStep::Failed(MapFailed::RolledBack {
            pending: Pending {
                domain: cur.domain,
                page,
                ticket,
            },
            tables,
            error,
        })
    }

    // ---- queueing again (a fence that found the queue full: `Fence::Unqueued`) ----------------------------------------------

    /// Queue `cmds` again for a waiter whose ticket is unqueued: the new ticket, or `None` if the queue is still full.
    fn requeue(
        &mut self,
        mem: &impl PhysMem64,
        regs: &mut impl Regs,
        cmds: &[Command],
    ) -> Option<Ticket> {
        let t = self.submit(mem, regs, cmds);
        (t.0 != NO_TICKET).then_some(t)
    }

    /// Queue a create's commands again (`t` is its ticket, updated). `Done` if the domain is gone (nothing is left to wait for).
    pub fn requeue_create<M: PhysMem64, R: Regs>(
        &mut self,
        mem: &M,
        regs: &mut R,
        d: DomainId,
        t: &mut Ticket,
    ) -> Fence {
        if t.0 != NO_TICKET {
            return Fence::Waiting;
        }
        let Some(device) = self.dom(d).map(|x| x.device) else {
            return Fence::Done;
        };
        let cmds = [
            Command::iodir_inval_ddt(Some(device)),
            Command::iotinval_gvma(QUARANTINE_GSCID, None),
        ];
        match self.requeue(mem, regs, &cmds) {
            Some(n) => {
                *t = n;
                Fence::Waiting
            }
            None => Fence::Unqueued,
        }
    }

    /// Queue a map's commands again (`m.ticket` updated). `Done` if the mapping is no longer live (an unmap or the domain's destruction
    /// has its own fence).
    pub fn requeue_map<M: PhysMem64, R: Regs>(
        &mut self,
        mem: &M,
        regs: &mut R,
        d: DomainId,
        m: &mut Mapped,
    ) -> Fence {
        if m.ticket.0 != NO_TICKET {
            return Fence::Waiting;
        }
        let Some((gscid, page, pages)) = self
            .dom(d)
            .filter(|x| x.state == State::Attached)
            .and_then(|x| Some((x.gscid, find(&x.live, m.iova >> PAGE_SHIFT)?)))
            .map(|(g, r)| (g, r.page, r.pages))
        else {
            return Fence::Done;
        };
        let (cmds, k) = inval_range(gscid, page, pages);
        match self.requeue(mem, regs, cmds.get(..k).unwrap_or(&cmds)) {
            Some(n) => {
                m.ticket = n;
                Fence::Waiting
            }
            None => Fence::Unqueued,
        }
    }

    /// Queue an unmap's (or a roll-back's) commands again (`p`'s ticket and its record's updated). `Done` if the record is no longer
    /// pending (the domain's destruction took it: `complete` then gives `Ok(None)`).
    pub fn requeue_unmap<M: PhysMem64, R: Regs>(
        &mut self,
        mem: &M,
        regs: &mut R,
        p: &mut Pending,
    ) -> Fence {
        if p.ticket.0 != NO_TICKET {
            return Fence::Waiting;
        }
        let Some(dom) = self.dom_mut(p.domain) else {
            return Fence::Done;
        };
        let gscid = dom.gscid;
        let (page, ticket) = (p.page, p.ticket.0);
        let Some(rec) = find_list_mut(&mut dom.pending, |m| {
            m.page == page && m.ticket == ticket && m.cleared == m.pages
        }) else {
            return Fence::Done;
        };
        let (cmds, k) = inval_for(gscid, rec);
        let Some(n) = self.requeue(mem, regs, cmds.get(..k).unwrap_or(&cmds)) else {
            return Fence::Unqueued;
        };
        if let Some(rec) = self.dom_mut(p.domain).and_then(|dom| {
            find_list_mut(&mut dom.pending, |m| {
                m.page == page && m.ticket == ticket && m.cleared == m.pages
            })
        }) {
            rec.ticket = n.0;
        }
        p.ticket = n;
        Fence::Waiting
    }

    /// Queue a destruction's commands again (`x`'s ticket and the domain's updated). `Done` if there is no such destruction any more.
    pub fn requeue_destroy<M: PhysMem64, R: Regs>(
        &mut self,
        mem: &M,
        regs: &mut R,
        x: &mut Destroying,
    ) -> Fence {
        if x.ticket.0 != NO_TICKET {
            return Fence::Waiting;
        }
        let Some((device, gscid)) = self
            .dom(x.domain)
            .filter(|dom| dom.state == State::Detaching && dom.destroy_ticket == NO_TICKET)
            .map(|dom| (dom.device, dom.gscid))
        else {
            return Fence::Done;
        };
        let cmds = [
            Command::iodir_inval_ddt(Some(device)),
            Command::iotinval_gvma(gscid, None),
        ];
        let Some(n) = self.requeue(mem, regs, &cmds) else {
            return Fence::Unqueued;
        };
        if let Some(dom) = self.dom_mut(x.domain) {
            dom.destroy_ticket = n.0;
        }
        x.ticket = n;
        Fence::Waiting
    }

    // ---- unmap ----------------------------------------------------------------------------------------------------------

    /// 6.1 step 1 in one call: every step of [`IoDomains::unmap_begin`] and [`IoDomains::unmap_step`] (the host tests' single critical
    /// section; the kernel takes `IOMMU` once per step). Allocates nothing; the table frames the unmap emptied go onto the record's
    /// chain.
    pub fn unmap<M: PhysMem64, R: Regs>(
        &mut self,
        mem: &M,
        regs: &mut R,
        d: DomainId,
        io_address: u64,
        pages: u64,
    ) -> Result<Pending, IoError> {
        let mut u = self.unmap_begin(d, io_address, pages)?;
        loop {
            match self.unmap_step(mem, regs, u) {
                UnmapStep::More(n) => u = n,
                UnmapStep::Queued(p) => return Ok(p),
                // (not reached: nothing can destroy the domain inside one call)
                UnmapStep::Taken => return Err(IoError::BadState),
            }
        }
    }

    /// The first hold of an unmap (6.1 step 1, OD168): exactly one live mapping leaves the index (no later call can name it), its range
    /// becomes pending (never handed out until `complete`) and its record joins the pending list, its leaves still to clear. Allocates
    /// nothing.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: mappings counts the index's records, and one is removed
    pub fn unmap_begin(
        &mut self,
        d: DomainId,
        io_address: u64,
        pages: u64,
    ) -> Result<Unmapping, IoError> {
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
        if dom.iova.release(page, pages).is_err() {
            debug_assert!(false, "a live mapping's range is live in the allocator");
        }
        rec.cleared = 0;
        rec.ticket = NO_TICKET;
        dom.push_pending(rec);
        Ok(Unmapping { domain: d, page })
    }

    /// One more hold of an unmap: clear the next `RANGE_STEP` leaves; after the last, queue the invalidation (page-wise, or of the whole
    /// soft context if a table frame was freed: OD169) and a fence (`Queued`). A domain whose destruction has begun takes the record
    /// as it is (`Taken`, OD170: the unmap is OK, the destruction's fence covers its leaves and its completion releases the record).
    /// Allocates nothing.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: cleared <= pages; page + cleared is inside the mapping's range; the table counts are frames moved from the table to the chain
    pub fn unmap_step<M: PhysMem64, R: Regs>(
        &mut self,
        mem: &M,
        regs: &mut R,
        u: Unmapping,
    ) -> UnmapStep {
        let Some(dom) = self
            .dom_mut(u.domain)
            .filter(|x| x.state == State::Attached)
        else {
            return UnmapStep::Taken;
        };
        let Domain {
            pending,
            iopt,
            tables,
            pending_tables,
            gscid,
            ..
        } = dom;
        let Some(rec) = find_list_mut(pending, |m| {
            m.page == u.page && m.ticket == NO_TICKET && m.cleared < m.pages
        }) else {
            return UnmapStep::Taken; // (not reached: only the destruction takes a pending record before its fence)
        };
        let mut cleared = rec.cleared;
        let k = (rec.pages - cleared).min(RANGE_STEP);
        let before = rec.chain.n;
        if let Some(iopt) = iopt.as_mut() {
            let mut na = NoAlloc {
                mem,
                chain: &mut rec.chain,
            };
            for _ in 0..k {
                iopt.unmap(mem, &mut na, (u.page + cleared) << PAGE_SHIFT);
                cleared += 1;
            }
        }
        rec.cleared = cleared;
        let chained = rec.chain.n - before;
        *tables -= chained;
        *pending_tables += chained;
        if cleared < rec.pages {
            return UnmapStep::More(u);
        }
        let (cmds, n) = inval_for(*gscid, rec);
        #[cfg(not(feature = "fault-iommu-skip-inval"))]
        let ticket = self.submit(mem, regs, cmds.get(..n).unwrap_or(&cmds));
        // (the negative control: the leaves are clear but nothing is invalidated and no fence is waited for)
        #[cfg(feature = "fault-iommu-skip-inval")]
        let ticket = {
            let _ = (regs, cmds, n);
            Ticket(SKIPPED)
        };
        if let Some(rec) = self.dom_mut(u.domain).and_then(|dom| {
            find_list_mut(&mut dom.pending, |m| {
                m.page == u.page && m.ticket == NO_TICKET
            })
        }) {
            rec.ticket = ticket.0;
        }
        UnmapStep::Queued(Pending {
            domain: u.domain,
            page: u.page,
            ticket,
        })
    }

    /// The pending record of `p`, once its leaves are all clear.
    fn is_pending(p: &Pending) -> impl Fn(&Mapping) -> bool + '_ {
        move |m: &Mapping| m.page == p.page && m.ticket == p.ticket.0 && m.cleared == m.pages
    }

    /// 6.1 step 3, once the fence has completed: the range becomes free and the record leaves the domain, to be settled by the caller
    /// with no lock held (step 4). `Ok(None)` when the domain's destruction already took the record (its own fence covered it).
    /// `NotDone` before the fence.
    pub fn complete(&mut self, p: &Pending) -> Result<Option<Released>, IoError> {
        let done = self.done(p.ticket);
        let Some(dom) = self.dom_mut(p.domain) else {
            return Ok(None);
        };
        if !find_list(&dom.pending, Self::is_pending(p)) {
            return Ok(None);
        }
        if !done {
            return Err(IoError::NotDone);
        }
        let Some(rec) = dom.unlink_pending(Self::is_pending(p)) else {
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
        let Some(rec) = dom.unlink_pending(Self::is_pending(p)) else {
            return false;
        };
        dom.leaks = true;
        let (pages, tables) = (rec.pages, rec.chain.n);
        dom.leaked_pages = dom.leaked_pages.saturating_add(pages);
        push(&mut dom.leaked, rec);
        let s = &mut self.stats;
        s.io_pages = s.io_pages.saturating_sub(pages);
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
    /// the store with every table frame and its mappings (live, pending ones whose waiters have not completed them, and ones being
    /// unmapped in steps: this fence covers them all), and its soft-context id is free again. Mappings whose own fence was abandoned
    /// stay leaked. Constant work: the tables and the records are released by the caller from the [`Detached`] with no lock held, a
    /// step at a time (OD168). `NotDone` before the fence; `Ok(None)` if there is nothing (another caller completed or abandoned it).
    /// Allocates nothing.
    pub fn destroy_complete<M: PhysMem64>(
        &mut self,
        _mem: &M,
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
        if let Some(head) = dom.leaked.take() {
            self.leak_list(head);
        }
        let held = dom.io_pages.saturating_sub(dom.leaked_pages);
        self.stats.io_pages = self.stats.io_pages.saturating_sub(held);
        dom.io_pages = held;
        dom.mappings = 0;
        let gscid = dom.gscid;
        self.free_gscid(gscid);
        Ok(Some(Detached {
            dom,
            cursor: 0,
            freed: 0,
            tables_done: false,
        }))
    }

    /// Keep a list of leaked records for good (a list of lists, linked through the heads' `left`: constant work).
    fn leak_list(&mut self, mut head: FBox<Mapping>) {
        head.left = self.leaked.take();
        self.leaked = Some(head);
    }

    /// The destruction's fence will not complete (6.4): everything it would have released is leaked and counted: the domain's tables,
    /// root and MSI page table, its mappings with their holds, its soft-context id (never reused) and its charges. The device id is free
    /// for a new domain (whose own context write and fence follow). A map in steps on the domain leaks its record too
    /// (`MapFailed::Leaked`). False if there was nothing to abandon. Constant work.
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
        self.abandons = self.abandons.wrapping_add(1);
        dom.leaks = true;
        let holds = dom.mappings.saturating_add(dom.pending_n);
        let pages = dom.io_pages.saturating_sub(dom.leaked_pages);
        let tables = dom
            .tables
            .saturating_add(ROOT_FRAMES + 1)
            .saturating_add(dom.pending_tables);
        let s = &mut self.stats;
        s.io_pages = s.io_pages.saturating_sub(pages);
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
        let mut tag = None;
        if let Some(i) = self.by_device(rec.device_id) {
            if let Some(slot) = self.slots.get_mut(i) {
                let gen = slot.gen;
                if let Some(dom) = slot
                    .dom
                    .as_deref_mut()
                    .filter(|x| x.state == State::Attached)
                {
                    dom.store_fault(rec);
                    tag = Some(dom.tag);
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
        FaultOutcome { domain, print, tag }
    }

    /// A domain whose destruction has begun and not completed (the kernel drains these with no lock held: a destruction begun under
    /// another lock, a handle close or a teardown, completes before the call returns to user mode: 6.3). Linear in the slots.
    pub fn next_detaching(&self) -> Option<Destroying> {
        self.slots.iter().enumerate().find_map(|(i, s)| {
            let dom = s.dom.as_deref().filter(|d| d.state == State::Detaching)?;
            let i = u64::try_from(i).ok()?.checked_add(1)?;
            Some(Destroying {
                domain: DomainId((u64::from(s.gen) << 32) | i),
                ticket: Ticket(dom.destroy_ticket),
            })
        })
    }

    /// Name the domain for its faults (`FaultOutcome::tag`): the kernel stores its object's id. False if the domain is gone.
    pub fn set_tag(&mut self, d: DomainId, tag: [u64; 2]) -> bool {
        self.dom_mut(d).map(|dom| dom.tag = tag).is_some()
    }

    /// Are there unread faults in the domain's ring (its DEVICE_FAULT signal)?
    pub fn has_faults(&self, d: DomainId) -> bool {
        self.dom(d).is_some_and(|dom| dom.ring_len > 0)
    }

    /// `iodomain_fault_read` (7.4): the oldest unread record of the domain's ring, with the faults dropped since the previous successful
    /// read (reset by this one). `Ok(None)` when the ring is empty (SHOULD_WAIT; the count is kept). `BadState` once the domain is gone.
    pub fn fault_read(&mut self, d: DomainId) -> Result<Option<FaultRead>, IoError> {
        Ok(self.dom_mut(d).ok_or(IoError::BadState)?.read_fault())
    }
}

impl Drop for IoDomains {
    fn drop(&mut self) {
        // (only the host tests drop a store; long lists are dropped one record at a time, never by recursion)
        while let Some(mut head) = self.leaked.take() {
            self.leaked = head.left.take();
            let mut l = Some(head);
            drop_list(&mut l);
        }
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

/// Charge `job` a domain's kernel bytes and take the use the store's domain record holds of it (from creation until `Detached::finish`,
/// or for good if a fence of the domain was abandoned: J2). Undo with `uncharge_create` if the creation fails. This is the record's use
/// only: the kernel's domain object takes its own, held to its last handle and vector object (IOMMU-SPEC 4.5), so a map that read the job
/// from the object cannot charge a job that a finished destruction let go.
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
