//! Kernel memory: the frame database, the real kernel page table (built with the
//! same pure `kcore` code the host tests exercise), and the kernel's own address
//! space, whose lazily committed regions the self-tests use.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::panic,
    clippy::unreachable
)] // KT3 (KERNEL-V1-SPEC 7.4): no panic a program can cause; remaining sites carry `#[allow] // INVARIANT:` (scripts/kernel_allows.py)

use abi::layout::DIRECT_BASE;
use core::ptr::{addr_of, addr_of_mut, read_volatile, write_volatile};
use kcore::aspace::{AddressSpace, Vmos};
use kcore::buddy::{Meta, META_BYTES};
use kcore::frame::{FrameDb, PAGE_SIZE};
use kcore::pagetable::{PageTable, Perms, PhysMem};
use kcore::Mm;

/// Physical memory contents through the direct map.
pub struct DirectMap;

impl PhysMem for DirectMap {
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: DIRECT_BASE + pa: pa is a physical address inside RAM, which the direct map covers
    fn read_u64(&self, pa: u64) -> u64 {
        // SAFETY: callers pass the address of a frame the frame database manages
        // (page tables and VMO pages), which `init` direct-maps read-write; the
        // address is 8-byte aligned by construction.
        unsafe { read_volatile((DIRECT_BASE + pa) as *const u64) }
    }

    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: DIRECT_BASE + pa: pa is a physical address inside RAM, which the direct map covers
    fn write_u64(&mut self, pa: u64, value: u64) {
        // SAFETY: as for `read_u64`.
        unsafe { write_volatile((DIRECT_BASE + pa) as *mut u64, value) }
    }
}

/// The kernel's memory state, in three locks of their own (L16): the VMO store (`VMO`), the job ledger (`JOBS`) and the frame database
/// (`FRAME`), nested in that order inside the big kernel lock (which still protects everything else, and the kernel's own address space).
type Cell<T> = klock::Spin<Option<T>, crate::bkl::Machine>;
type Held<'a, T> = klock::Guard<'a, Option<T>, crate::bkl::Machine>;

static VMOS: Cell<Vmos> = klock::Spin::new(klock::Class::Vmo, 0, None);
static JOBS: Cell<kcore::job::Jobs> = klock::Spin::new(klock::Class::Jobs, 0, None);
static FRAMES: Cell<Mm<DirectMap>> = klock::Spin::new(klock::Class::Frame, 0, None);
static mut KASPACE: Option<AddressSpace> = None;

/// Everything the fault handler and the self-tests share: the three locks held (taken in class order, released in reverse), and the
/// kernel's address space (under the big kernel lock). Not `Send`; callers must not keep it across a context switch or across an access
/// that can fault (the handler takes the same locks).
pub struct Kmem {
    pub mm: &'static mut Mm<DirectMap>,
    pub vmos: &'static mut Vmos,
    /// The job ledger (L16): not part of the VMO store.
    pub jobs: &'static mut kcore::job::Jobs,
    pub aspace: &'static mut AddressSpace,
    // (dropped in this order: FRAME, then JOBS, then VMO)
    _frame: Held<'static, Mm<DirectMap>>,
    _jobs: Held<'static, kcore::job::Jobs>,
    _vmos: Held<'static, Vmos>,
}

/// The frame database alone (`FRAME`), for the code that needs only frames: the heap's pool (the kernel stacks also need the address space, so they use `get`).
pub struct Frames {
    pub mm: &'static mut Mm<DirectMap>,
    _frame: Held<'static, Mm<DirectMap>>,
}

/// The job ledger alone (`JOBS`), for the code that only charges or refunds objects.
pub struct JobsRef {
    pub jobs: &'static mut kcore::job::Jobs,
    _jobs: Held<'static, kcore::job::Jobs>,
}

/// Take `JOBS` alone (not inside `get`'s three locks: taking it again would be a recursive take).
#[track_caller]
pub fn jobs() -> JobsRef {
    debug_assert!(
        crate::bkl::held(),
        "the job ledger touched without the big kernel lock"
    );
    let mut g = JOBS.lock();
    // SAFETY: as in `get`.
    unsafe {
        JobsRef {
            jobs: inside(&mut g),
            _jobs: g,
        }
    }
}

/// The value inside a held lock, as a reference that lives as long as the guard does (the owner keeps the guard next to it).
///
/// SAFETY (callers): the reference must not outlive the guard it came from; `Kmem` and `Frames` store both and drop the guard last.
#[allow(clippy::expect_used)] // INVARIANT: `init` fills every cell before anything takes one (boot order)
unsafe fn inside<T>(g: &mut Held<'static, T>) -> &'static mut T {
    let p: *mut T = g.as_mut().expect("kmem::init was not called");
    // SAFETY: the pointer is to the lock's own storage (a static), valid for 'static; the guard held by the caller keeps others out.
    unsafe { &mut *p }
}

/// The kernel memory state: takes `VMO`, `JOBS` and `FRAME` in that order (the order checker sees them). Needs the big kernel lock
/// (asserted once it is enforced). Callers must not keep it across an access that can fault or across a context switch.
#[track_caller]
#[allow(clippy::expect_used)] // INVARIANT: KASPACE is set by `init` before anything else calls this (boot order); a panic is the right answer to a kernel bug
pub fn get() -> Kmem {
    debug_assert!(
        crate::bkl::held(),
        "memory state touched without the big kernel lock"
    );
    let mut vmo = VMOS.lock();
    let mut jobs = JOBS.lock();
    let mut frame = FRAMES.lock();
    // SAFETY: each reference comes from the guard stored next to it; the guards drop after the references (field order). The big
    // kernel lock is held (checked above), so nobody else reaches the address space.
    unsafe {
        Kmem {
            mm: inside(&mut frame),
            vmos: inside(&mut vmo),
            jobs: inside(&mut jobs),
            aspace: (*addr_of_mut!(KASPACE))
                .as_mut()
                .expect("kmem::init was not called"),
            _frame: frame,
            _jobs: jobs,
            _vmos: vmo,
        }
    }
}

/// The frame database alone: takes only `FRAME`. No big kernel lock is needed for the lock itself.
#[track_caller]
pub fn frames() -> Frames {
    let mut frame = FRAMES.lock();
    // SAFETY: as in `get`.
    unsafe {
        Frames {
            mm: inside(&mut frame),
            _frame: frame,
        }
    }
}

extern "C" {
    static __text_start: u8;
    static __text_end: u8;
    static __rodata_start: u8;
    static __rodata_end: u8;
    static __data_start: u8;
    static __kernel_end: u8;
}

fn sym(s: *const u8) -> u64 {
    s as u64
}

pub struct Sections {
    pub text: (u64, u64),
    pub rodata: (u64, u64),
    pub data: (u64, u64),
}

/// The kernel image's sections (virtual addresses).
pub fn sections() -> Sections {
    // Taking the address of a linker-defined symbol reads nothing.
    Sections {
        text: (sym(addr_of!(__text_start)), sym(addr_of!(__text_end))),
        rodata: (sym(addr_of!(__rodata_start)), sym(addr_of!(__rodata_end))),
        data: (sym(addr_of!(__data_start)), sym(addr_of!(__kernel_end))),
    }
}

#[allow(clippy::arithmetic_side_effects, clippy::expect_used)] // INVARIANT: boot code: the linker's section bounds, mapped once; a failure to map is a kernel that cannot run
fn map_image(pt: &mut PageTable, mm: &mut Mm<DirectMap>, (start, end): (u64, u64), perms: Perms) {
    let mut va = start;
    while va < end {
        pt.map(
            mm,
            va,
            (va - crate::arch::kernel_delta()) / PAGE_SIZE,
            perms,
        )
        .expect("mapping the kernel image");
        va += PAGE_SIZE;
    }
}

/// Build the kernel page table and switch to it. Needs the heap and the early
/// boot table (which maps the direct map, so new tables can be written).
#[allow(clippy::large_stack_frames)]
// KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
#[allow(clippy::arithmetic_side_effects, clippy::expect_used)] // INVARIANT: boot code: addresses come from the device tree and the linker script; a kernel that cannot build its own page table cannot run
pub fn init() {
    let s = sections();
    // What memory exists and which of it the allocator may have (PL4, KERNEL-V1-SPEC 2.3): the tree's `memory` ranges, minus its reserved
    // ranges (firmware), the loader's reservation block, the tree's own frames and the kernel image. Nothing here is a constant.
    let delta = crate::arch::kernel_delta();
    let image_start = s.text.0 - delta;
    let image_end = s.data.1 - delta;
    let (memory, mut reserved) =
        crate::dtb::memory_map().expect("a device tree below 64 GiB that describes the machine's memory (T180: RAM above 64 GiB is not supported yet)");
    reserved.push((image_start, image_end - image_start));
    // Firmware keeps its footprint just below the load address. The tree normally says so (OpenSBI adds `/reserved-memory`), but a tree
    // that does not must not make the firmware's own memory the first thing handed out (and the frame metadata's home): the 2 MiB below the
    // image (where OpenSBI's own region ends at the default load address) stays reserved when it lies in the same RAM range. Nothing more:
    // the cost is a fixed 2 MiB wherever the image is.
    if let Some(&(base, _)) = memory
        .iter()
        .find(|&&(b, s)| b <= image_start && image_start < b.saturating_add(s))
    {
        reserved.push((
            base.max(image_start.saturating_sub(2 << 20)),
            image_start - base.max(image_start.saturating_sub(2 << 20)),
        ));
    }
    // Only the physical space the direct map reaches is used (`DIRECT_MAP_PHYS_LIMIT`): RAM above it is left alone, loudly.
    let limit = abi::layout::DIRECT_MAP_PHYS_LIMIT;
    if memory.iter().any(|&(b, s)| b.saturating_add(s) > limit) {
        use core::fmt::Write;
        let _ = writeln!(
            crate::console::Console,
            "kernel: RAM above {} GiB is not used (the direct map ends there)",
            limit >> 30
        );
    }
    let memory: alloc::vec::Vec<(u64, u64)> = memory
        .iter()
        .filter(|&&(b, _)| b < limit)
        .map(|&(b, s)| (b, s.min(limit - b)))
        .collect();
    let usable = kcore::memmap::usable_frames(&memory, &reserved);
    let usable_total: usize = usable.iter().map(|r| r.1).sum();
    // The frame metadata (MM2): one `Meta` per managed frame, carved from the start of the first usable range that can hold it, before any
    // allocator exists and never from the kernel heap. Its frames are not managed (the database starts after them) but are direct-mapped
    // read-write with the rest of the usable memory, by the loop below.
    let meta_frames = (usable_total * META_BYTES).div_ceil(PAGE_SIZE as usize) as u64;
    let host = usable
        .iter()
        .position(|r| r.1 as u64 > meta_frames)
        .expect("a range of memory big enough for the frame metadata");
    let managed_ranges: alloc::vec::Vec<(u64, usize)> = usable
        .iter()
        .enumerate()
        .map(|(i, &(f, c))| {
            if i == host {
                (f + meta_frames, c - meta_frames as usize)
            } else {
                (f, c)
            }
        })
        .collect();
    let managed: usize = managed_ranges.iter().map(|r| r.1).sum();
    // SAFETY: boot code, one hart, before anything reads it.
    unsafe { *addr_of_mut!(USABLE) = usable.clone() };
    let storage: &'static mut [Meta] = {
        let at =
            (DIRECT_BASE + usable.get(host).expect("the host range").0 * PAGE_SIZE) as *mut Meta;
        // SAFETY: the first `meta_frames` frames of a usable range are RAM that nothing else uses and the database will not hand out;
        // the early boot table direct-maps all physical space below 64 GiB; `managed` entries fit in `meta_frames` frames (sized from
        // the larger `usable_total`); an all-zero `Meta` is a valid value and the slice is cleared before it is formed; it is the only
        // reference to that memory.
        unsafe {
            core::ptr::write_bytes(at, 0, managed);
            core::slice::from_raw_parts_mut(at, managed)
        }
    };
    let memory_frames: usize = kcore::memmap::usable_frames(&memory, &[])
        .iter()
        .map(|r| r.1)
        .sum();
    // SAFETY: boot code, one hart, before anything reads it.
    unsafe {
        *addr_of_mut!(STATS) = MemStats {
            memory_frames: memory_frames as u64,
            usable_frames: usable_total as u64,
            managed_frames: managed as u64,
            meta_frames,
            ram_first: memory.iter().map(|r| r.0).min().unwrap_or(0) / PAGE_SIZE,
            ram_end: memory
                .iter()
                .map(|r| r.0.saturating_add(r.1))
                .max()
                .unwrap_or(0)
                / PAGE_SIZE,
            image_first: image_start / PAGE_SIZE,
            image_end: image_end.div_ceil(PAGE_SIZE),
        };
    }
    // What the tree says about the devices (parsed once): the interrupt controller (PL5c) and the windows a physical VMO may name (PL5, D1):
    // every admitted device range minus what the kernel itself drives (the console, the test finisher and the APLIC's pages, all found in the
    // tree) and minus RAM and everything reserved.
    {
        let nodes = crate::dtb::dtb_blob()
            .and_then(|b| kcore::fdt::parse(b).ok())
            .unwrap_or_default();
        let ram: alloc::vec::Vec<(u64, u64)> =
            memory.iter().chain(reserved.iter()).copied().collect();
        crate::aia::discover(&nodes, &ram);
        let mut driven: alloc::vec::Vec<(u64, u64)> = alloc::vec::Vec::new();
        driven.extend(crate::console::base().map(|b| (b, 0x1000)));
        driven.extend(crate::finisher::base().map(|b| (b, 0x1000)));
        driven.extend(crate::aia::pages().map(|p| (p, 0x1000)));
        let windows = kcore::devwin::device_windows(&nodes, &driven, &ram);
        // SAFETY: boot code, one hart, before anything reads it.
        unsafe { *addr_of_mut!(WINDOWS) = windows };
    }
    let mut mm = Mm {
        db: FrameDb::with_ranges(&managed_ranges, storage).expect("room for the frame metadata"),
        mem: DirectMap,
    };
    let mut pt = PageTable::new(&mut mm).expect("a frame for the kernel root");
    // The image, section by section, with the permissions each deserves (W^X).
    map_image(&mut pt, &mut mm, s.text, Perms::RX);
    map_image(&mut pt, &mut mm, s.rodata, Perms::R);
    map_image(&mut pt, &mut mm, s.data, Perms::RW);
    // The direct map: every usable frame (the managed ones and the metadata's), never the image or the firmware's reserved ranges, so
    // the code cannot be written through an alias. MM3: the largest aligned pages that fit, 1 GiB then 2 MiB, 4 KiB only at the
    // unaligned edges; user mappings stay 4 KiB.
    for &(first, count) in &usable {
        for (pfn, level) in kcore::pagetable::plan_range(first, count as u64, 2) {
            pt.map_large(
                &mut mm,
                DIRECT_BASE + pfn * PAGE_SIZE,
                pfn,
                level,
                Perms::RW,
            )
            .expect("direct map");
        }
    }
    // The device tree's own frames (not usable memory: reserved above) are mapped read-only (the interrupt controller probe reads them).
    // Only frames inside the RAM the tree describes are mapped, and a tree that overlaps the image is refused (the image is never
    // reachable through the direct map, readable or not).
    let tree = crate::dtb::dtb_blob().map_or(0..0, |b| {
        let pa = crate::dtb::DTB.load(core::sync::atomic::Ordering::Relaxed) as u64;
        pa / PAGE_SIZE..(pa + b.len() as u64).div_ceil(PAGE_SIZE)
    });
    assert!(
        tree.is_empty()
            || tree.end <= image_start / PAGE_SIZE
            || tree.start >= image_end.div_ceil(PAGE_SIZE),
        "the device tree overlaps the kernel image"
    );
    let in_ram = |pfn: u64| {
        memory
            .iter()
            .any(|&(b, s)| pfn * PAGE_SIZE >= b && (pfn + 1) * PAGE_SIZE <= b.saturating_add(s))
    };
    for pfn in tree.filter(|&p| in_ram(p)) {
        pt.map(&mut mm, DIRECT_BASE + pfn * PAGE_SIZE, pfn, Perms::R)
            .expect("device tree map");
    }
    // the devices the kernel drives, found in the tree: the console, the test device, the interrupt controller's pages. Each page once (the
    // console and the test device may share one, and a page may not be mapped twice).
    let mut pages: alloc::vec::Vec<u64> = crate::console::base()
        .into_iter()
        .chain(crate::finisher::base().filter(|_| !crate::RELEASE))
        .chain(crate::aia::pages())
        .map(|d| d & !(PAGE_SIZE - 1))
        .collect();
    pages.sort_unstable();
    pages.dedup();
    for page in pages {
        pt.map(&mut mm, DIRECT_BASE + page, page / PAGE_SIZE, Perms::RW)
            .expect("device page");
    }
    let root = pt.root();
    let aspace = AddressSpace::new(PageTable::from_root(root));
    // SAFETY: called once at boot with interrupts off, before anything calls `get`.
    unsafe { *addr_of_mut!(KASPACE) = Some(aspace) };
    *VMOS.lock() = Some(Vmos::new());
    *JOBS.lock() = Some(kcore::job::Jobs::new());
    *FRAMES.lock() = Some(mm);
    let token = crate::arch::aspace_token(root);
    KERNEL_ASPACE.store(token, core::sync::atomic::Ordering::SeqCst);
    // SAFETY: the new table maps everything the kernel is executing from and
    // touching: text, rodata, data/bss/stack, the direct map and the devices;
    // installing it drops the translations of the early table.
    unsafe { crate::arch::aspace_install(token) };
}

/// What `init` found, for the self-tests and `debug_stat`.
#[derive(Clone, Copy, Default)]
pub struct MemStats {
    /// Frames of RAM the device tree describes (whole frames).
    pub memory_frames: u64,
    /// Of those, the frames not reserved, not the image and not the tree: what the kernel may use.
    pub usable_frames: u64,
    /// Of those, the frames the allocator manages (usable minus the metadata's own).
    pub managed_frames: u64,
    /// Frames the frame metadata occupies.
    pub meta_frames: u64,
    /// The first frame of the lowest RAM range and the frame after the last of the highest.
    pub ram_first: u64,
    pub ram_end: u64,
    /// The kernel image's frames, first to one past the last.
    pub image_first: u64,
    pub image_end: u64,
}

/// The device windows (page-aligned `(start, end)` byte ranges) built at boot from the tree; read-only afterwards.
static mut WINDOWS: alloc::vec::Vec<(u64, u64)> = alloc::vec::Vec::new();

/// The device windows a physical VMO may name.
pub fn device_windows() -> &'static [(u64, u64)] {
    // SAFETY: written once by `init` at boot, before any other hart or thread runs; read-only afterwards.
    unsafe { &*addr_of!(WINDOWS) }
}

/// The usable ranges `init` gave the direct map, `(first frame, count)` (read-only after boot).
static mut USABLE: alloc::vec::Vec<(u64, usize)> = alloc::vec::Vec::new();

/// The usable ranges (read-only after boot).
pub fn usable_ranges() -> &'static [(u64, usize)] {
    // SAFETY: written once by `init` at boot, before any other hart or thread runs; read-only afterwards.
    unsafe { &*addr_of!(USABLE) }
}

static mut STATS: MemStats = MemStats {
    memory_frames: 0,
    usable_frames: 0,
    managed_frames: 0,
    meta_frames: 0,
    ram_first: 0,
    ram_end: 0,
    image_first: 0,
    image_end: 0,
};

/// What `init` found (read-only after boot).
pub fn stats() -> MemStats {
    // SAFETY: written once by `init` at boot, before any other hart or thread runs; read-only afterwards.
    unsafe { *addr_of!(STATS) }
}

/// The kernel's own address-space token, recorded once by `init` (the secondary harts load it).
pub static KERNEL_ASPACE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Switch back to the kernel's own page table (before a process's root is freed).
pub fn use_kernel_table() {
    let root = crate::irq::without(|| get().aspace.table().root());
    // SAFETY: the kernel's table maps everything the kernel executes from and touches;
    // installing it drops the translations of the table being left.
    unsafe { crate::arch::aspace_install(crate::arch::aspace_token(root)) };
}

/// Load the kernel's own table if the hart has something else in it (a user process's root).
pub fn use_kernel_table_if_needed() {
    if cfg!(feature = "fault-skip-kernel-satp") {
        return; // negative control: a hart keeps a dead process's page table
    }
    let want = KERNEL_ASPACE.load(core::sync::atomic::Ordering::Relaxed);
    if crate::arch::aspace_current() != want {
        // SAFETY: the kernel table maps everything the kernel runs from and touches; installing it
        // drops the translations of the table being left.
        unsafe { crate::arch::aspace_install(want) };
    }
}

/// Flush the whole TLB.
pub fn flush_tlb() {
    crate::arch::tlb_flush_all();
}

/// Flush the translations of the pages `first..=last` (virtual addresses, 4 KiB apart) of address space `asid`, one `sfence.vma`
/// each (MM1): a resolved page fault drops only what it changed, not every other page this hart has cached.
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: va runs over page-aligned addresses of pages a fault mapped inside the user half or the kernel's own table
pub fn flush_pages(first: u64, last: u64, asid: u16) {
    let mut va = first & !0xfff;
    while va <= last {
        crate::arch::tlb_flush_page(va, asid);
        va += 4096;
    }
}

/// The kernel's top-level (upper half) page-table entries as `init` left them. Process page
/// tables copy these at creation, so the kernel must never create or free a top-level slot
/// afterwards (OD41); `kernel_half_unchanged` checks.
static mut KERNEL_HALF: [u64; 256] = [0; 256];

#[allow(clippy::arithmetic_side_effects)] // INVARIANT: i < 256 slots of the root table plus 256, and the root is a frame of the direct map
#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
fn kernel_half_now() -> [u64; 256] {
    crate::irq::without(|| {
        let root = get().aspace.table().root();
        let mut out = [0u64; 256];
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = kcore::pagetable::PhysMem::read_u64(
                &get().mm.mem,
                (root << 12) + (256 + i as u64) * 8,
            );
        }
        out
    })
}

/// Remember the kernel half now (called once, after the S1 memory tests, before any process).
#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
pub fn snapshot_kernel_half() {
    // SAFETY: called once at boot, interrupts off or a single thread, before `kernel_half_unchanged`.
    unsafe { *addr_of_mut!(KERNEL_HALF) = kernel_half_now() };
}

#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
pub fn kernel_half_unchanged() -> bool {
    // SAFETY: read of a static written once by `snapshot_kernel_half`.
    kernel_half_now() == unsafe { *addr_of!(KERNEL_HALF) }
}

/// Free frames, plus the frames held by kernel stack slots (which keep theirs for ever once made, so
/// a leak check that compares before and after must not count a slot made in between as a leak).
/// Must be called with the big kernel lock held, like `get`.
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: a sum of three frame counts, each at most the number of frames
pub fn free_frames_accounted() -> usize {
    let free = get().mm.db.free_count(); // (the locks are released before the two calls below)
    free + crate::kstack::frames_held() + crate::heap::frames_held()
}
