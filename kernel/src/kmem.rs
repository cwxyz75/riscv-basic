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

/// Where physical address 0 is in the direct map: the first slot's address plus the gigabytes `_start` moved it up (HD3: random, from the device tree's seed).
pub fn direct_base() -> u64 {
    abi::layout::DIRECT_BASE.wrapping_add(crate::arch::direct_shift() << 30)
}

use core::ptr::{addr_of, addr_of_mut, read_volatile, write_volatile};
use kcore::aspace::{AddressSpace, Vmos};
use kcore::buddy::{Meta, META_BYTES};
use kcore::frame::{FrameDb, PAGE_SIZE};
use kcore::pagetable::{PageTable, Perms, PhysMem};
use kcore::Mm;

/// Physical memory contents through the direct map.
pub struct DirectMap;

impl PhysMem for DirectMap {
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: crate::kmem::direct_base() + pa: pa is a physical address inside RAM, which the direct map covers
    fn read_u64(&self, pa: u64) -> u64 {
        // SAFETY: callers pass the address of a frame the frame database manages
        // (page tables and VMO pages), which `init` direct-maps read-write; the
        // address is 8-byte aligned by construction.
        unsafe { read_volatile((crate::kmem::direct_base() + pa) as *const u64) }
    }

    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: crate::kmem::direct_base() + pa: pa is a physical address inside RAM, which the direct map covers
    fn write_u64(&mut self, pa: u64, value: u64) {
        // SAFETY: as for `read_u64`.
        unsafe { write_volatile((crate::kmem::direct_base() + pa) as *mut u64, value) }
    }
}

/// A 32-bit word of user memory reached by its physical address through the direct map, with atomic operations (the PI futex's lock word, L23: the
/// user stores to it with its own compare-and-swaps, so the kernel's reads and writes must be atomic too).
pub struct PhysWord(pub u64);

impl kcore::pifutex::Word for PhysWord {
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: crate::kmem::direct_base() + pa: pa is a physical address inside RAM, which the direct map covers
    fn load(&self) -> u32 {
        // SAFETY: `self.0` is the physical address of a resident, 4-aligned user word that the caller keeps from going (the big kernel lock and
        // the address-space lock were held when it was resolved, and the caller holds the big lock still); the direct map covers it.
        unsafe {
            core::sync::atomic::AtomicU32::from_ptr(
                (crate::kmem::direct_base() + self.0) as *mut u32,
            )
        }
        .load(core::sync::atomic::Ordering::SeqCst)
    }
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: as `load`
    fn cas(&self, old: u32, new: u32) -> Result<u32, u32> {
        // SAFETY: as in `load`.
        unsafe {
            core::sync::atomic::AtomicU32::from_ptr(
                (crate::kmem::direct_base() + self.0) as *mut u32,
            )
        }
        .compare_exchange(
            old,
            new,
            core::sync::atomic::Ordering::SeqCst,
            core::sync::atomic::Ordering::SeqCst,
        )
    }
}

/// The kernel's memory state, in three locks of their own (L16): the VMO store (`VMO`), the job ledger (`JOBS`) and the frame database
/// (`FRAME`), nested in that order inside the big kernel lock (which still protects everything else, and the kernel's own address space).
type Cell<T> = klock::Spin<Option<T>, crate::bkl::Machine>;
type Held<'a, T> = klock::Guard<'a, Option<T>, crate::bkl::Machine>;

static VMOS: Cell<Vmos> = klock::Spin::new(klock::Class::Vmo, 0, None);
static JOBS: Cell<kcore::job::Jobs> = klock::Spin::new(klock::Class::Jobs, 0, None);
static FRAMES: Cell<Mm<DirectMap>> = klock::Spin::new(klock::Class::Frame, 0, None);
/// The kernel's own address space object (regions the kernel maps lazily, which only the self-tests use, and its page table): under its own lock (class
/// `KSTACK`, instance 2), between `JOBS` and `FRAME` (`get_as`).
static KASPACE: Cell<AddressSpace> = klock::Spin::new(klock::Class::Kstack, 2, None);

/// The kernel's root page table (a frame number), set once at boot. The top-level entries never change after boot (OD41); the second level and below of the
/// heap window and the stack window are edited under `KPT`.
static KERNEL_ROOT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The lock of the kernel page table's edits (class `KSTACK`, instance 1: the stack slots, instance 0, come first): the heap window and the stack window
/// are mapped under it, before the frame lock (T224).
pub static KPT: klock::Spin<(), crate::bkl::Machine> =
    klock::Spin::new(klock::Class::Kstack, 1, ());

/// A view of the kernel's page table (for the creation of a process, which copies its upper half, and for the edits made under `KPT`). Do not `destroy` it.
pub fn kernel_table() -> PageTable {
    PageTable::from_root(KERNEL_ROOT.load(core::sync::atomic::Ordering::Relaxed))
}

/// Everything the fault handler and the self-tests share: the three locks held (taken in class order, released in reverse), and the
/// kernel's address space (under the big kernel lock). Not `Send`; callers must not keep it across a context switch or across an access
/// that can fault (the handler takes the same locks).
pub struct Kmem {
    pub mm: &'static mut Mm<DirectMap>,
    pub vmos: &'static mut Vmos,
    /// The job ledger (L16): not part of the VMO store.
    pub jobs: &'static mut kcore::job::Jobs,
    // (dropped in this order: FRAME, then JOBS, then VMO)
    _frame: Held<'static, Mm<DirectMap>>,
    _jobs: Held<'static, kcore::job::Jobs>,
    _vmos: Held<'static, Vmos>,
}

/// `Kmem` and the kernel's own address space: takes `VMO`, `JOBS`, the kernel address space and `FRAME`, in that order (for the kernel's page-fault handler and the
/// self-tests, which map and fault in the kernel's lazy regions).
pub struct KmemAs {
    pub mm: &'static mut Mm<DirectMap>,
    pub vmos: &'static mut Vmos,
    pub jobs: &'static mut kcore::job::Jobs,
    pub aspace: &'static mut AddressSpace,
    // (dropped in this order: FRAME, then the address space, then JOBS, then VMO)
    _frame: Held<'static, Mm<DirectMap>>,
    _kas: Held<'static, AddressSpace>,
    _jobs: Held<'static, kcore::job::Jobs>,
    _vmos: Held<'static, Vmos>,
}

/// The shared side of the per-hart frame caches: each call is one take of `FRAME`.
struct SharedFrames;

impl kcore::fcache::Shared for SharedFrames {
    fn refill(&mut self, out: &mut [u64]) -> usize {
        frames().mm.db.refill(out)
    }
    fn drain(&mut self, frames_back: &[u64]) {
        frames().mm.db.drain(frames_back);
    }
}

/// A frame (reference count 1) from this hart's cache, which refills from the database in batches under `FRAME` (L16). Not to be called
/// with a memory lock held (a refill takes `FRAME`).
pub fn frame_alloc() -> Option<u64> {
    crate::bkl::check_use(klock::Class::Frame); // (the order checker sees every call, not only the ones that refill)
    let was = crate::irq::save_and_disable();
    // SAFETY: interrupts are off, and this is the calling hart's own record.
    let r = unsafe {
        crate::percpu::this()
            .frame_cache
            .with(|c| c.alloc(&mut SharedFrames))
    };
    crate::irq::restore(was);
    r
}

/// Give every frame in this hart's cache back to the database (a hart going offline, PM1).
pub fn flush_frame_cache() {
    crate::bkl::check_use(klock::Class::Frame);
    let was = crate::irq::save_and_disable();
    // SAFETY: interrupts are off, and this is the calling hart's own record.
    unsafe {
        crate::percpu::this()
            .frame_cache
            .with(|c| c.flush(&mut SharedFrames))
    };
    crate::irq::restore(was);
}

/// Give a frame back to this hart's cache; the caller holds its only reference (a kernel-owned frame: a slab, the pool, a stack).
pub fn frame_free_sole(pfn: u64) {
    crate::bkl::check_use(klock::Class::Frame);
    let was = crate::irq::save_and_disable();
    // SAFETY: as in `frame_alloc`.
    unsafe {
        crate::percpu::this()
            .frame_cache
            .with(|c| c.free_sole(pfn, &mut SharedFrames))
    };
    crate::irq::restore(was);
}

/// Frames the harts' caches hold (free for the leak oracles).
pub fn cached_frames() -> usize {
    (0..crate::percpu::MAX_HARTS)
        .map(|i| crate::percpu::hart(i).frame_cache.len())
        .sum()
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
    let mut g = JOBS.lock();
    // SAFETY: every reference points into the storage of the lock whose guard is stored in the same struct, and the guards are fields declared after the
    // references, so they drop after them. The spin locks are not recursive (a second take on this hart panics) and are held with interrupts off, so no
    // other `&mut` to the same value exists while this one does. The type does not stop safe code from moving a reference out and past the guard's drop (the
    // fields are public); no caller does (T242: every use is `kmem::get().field`).
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
pub fn get() -> Kmem {
    let mut vmo = VMOS.lock();
    let mut jobs = JOBS.lock();
    let mut frame = FRAMES.lock();
    // SAFETY: every reference points into the storage of the lock whose guard is stored in the same struct, and the guards are fields declared after the
    // references, so they drop after them. The spin locks are not recursive (a second take on this hart panics) and are held with interrupts off, so no
    // other `&mut` to the same value exists while this one does. The type does not stop safe code from moving a reference out and past the guard's drop (the
    // fields are public); no caller does (T242: every use names the field of the temporary, `kmem::get().field`, or binds it for the statement).
    unsafe {
        Kmem {
            mm: inside(&mut frame),
            vmos: inside(&mut vmo),
            jobs: inside(&mut jobs),
            _frame: frame,
            _jobs: jobs,
            _vmos: vmo,
        }
    }
}

#[track_caller]
pub fn get_as() -> KmemAs {
    let mut vmo = VMOS.lock();
    let mut jobs = JOBS.lock();
    let mut kas = KASPACE.lock();
    let mut frame = FRAMES.lock();
    // SAFETY: every reference points into the storage of the lock whose guard is stored in the same struct, and the guards are fields declared after the
    // references, so they drop after them. The spin locks are not recursive (a second take on this hart panics) and are held with interrupts off, so no
    // other `&mut` to the same value exists while this one does. The type does not stop safe code from moving a reference out and past the guard's drop (the
    // fields are public); no caller does (T242: every use is `kmem::get().field`).
    unsafe {
        KmemAs {
            mm: inside(&mut frame),
            vmos: inside(&mut vmo),
            jobs: inside(&mut jobs),
            aspace: inside(&mut kas),
            _frame: frame,
            _kas: kas,
            _jobs: jobs,
            _vmos: vmo,
        }
    }
}

/// The three memory locks and nothing else (L17 part c: the page-fault path without the big kernel lock): the same `VMO`, `JOBS`, `FRAME`
/// as `get`, in the same order, but not the kernel's address space, which only the big lock protects.
pub struct Mem {
    pub mm: &'static mut Mm<DirectMap>,
    pub vmos: &'static mut Vmos,
    pub jobs: &'static mut kcore::job::Jobs,
    // (dropped in this order: FRAME, then JOBS, then VMO)
    _frame: Held<'static, Mm<DirectMap>>,
    _jobs: Held<'static, kcore::job::Jobs>,
    _vmos: Held<'static, Vmos>,
}

/// Take `VMO`, `JOBS` and `FRAME`. No big kernel lock is needed for the locks themselves; the caller must hold the address-space lock
/// of the space it works on (`AS` is before them in the order).
#[track_caller]
pub fn mem() -> Mem {
    let mut vmo = VMOS.lock();
    let mut jobs = JOBS.lock();
    let mut frame = FRAMES.lock();
    // SAFETY: every reference points into the storage of the lock whose guard is stored in the same struct, and the guards are fields declared after the
    // references, so they drop after them. The spin locks are not recursive (a second take on this hart panics) and are held with interrupts off, so no
    // other `&mut` to the same value exists while this one does. The type does not stop safe code from moving a reference out and past the guard's drop (the
    // fields are public); no caller does (T242: every use is `kmem::get().field`).
    unsafe {
        Mem {
            mm: inside(&mut frame),
            vmos: inside(&mut vmo),
            jobs: inside(&mut jobs),
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
    // SAFETY: every reference points into the storage of the lock whose guard is stored in the same struct, and the guards are fields declared after the
    // references, so they drop after them. The spin locks are not recursive (a second take on this hart panics) and are held with interrupts off, so no
    // other `&mut` to the same value exists while this one does. The type does not stop safe code from moving a reference out and past the guard's drop (the
    // fields are public); no caller does (T242: every use is `kmem::get().field`).
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
    static __stack_bottom: u8;
    static __stack_top: u8;
}

/// The boot hart's boot stack, `[low, high)` (the linker's `.stack`; its idle thread runs on it).
pub fn boot_stack() -> (u64, u64) {
    (sym(addr_of!(__stack_bottom)), sym(addr_of!(__stack_top)))
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
        let at = (crate::kmem::direct_base()
            + usable.get(host).expect("the host range").0 * PAGE_SIZE)
            as *mut Meta;
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
    // What the tree says about the devices (parsed once): the IOMMU (IOMMU5) and the interrupt controller (PL5c, with the IOMMU's sources),
    // and what the windows a physical VMO may name (PL5, D1) are made of: every admitted device range minus what the kernel itself drives
    // (the console, the test finisher, the APLIC's pages and the IOMMU's registers, all found in the tree) and minus RAM and everything
    // reserved. The windows themselves are built only once the IOMMU denies every device (`build_windows`, IOMMU-SPEC 2.4).
    {
        let nodes = crate::dtb::dtb_blob()
            .and_then(|b| kcore::fdt::parse(b).ok())
            .unwrap_or_default();
        let ram: alloc::vec::Vec<(u64, u64)> =
            memory.iter().chain(reserved.iter()).copied().collect();
        crate::iommu::discover(&nodes, &ram);
        crate::aia::discover(&nodes, &ram, crate::iommu::kernel_sources());
        let mut driven: alloc::vec::Vec<(u64, u64)> = alloc::vec::Vec::new();
        driven.extend(crate::console::base().map(|b| (b, 0x1000)));
        driven.extend(crate::finisher::base().map(|b| (b, 0x1000)));
        driven.extend(crate::aia::pages().map(|p| (p, 0x1000)));
        driven.extend(crate::iommu::page().map(|p| (p, 0x1000)));
        // a host whose map puts only some of its functions behind the IOMMU gets no window (IOMMU-SPEC 2.6): say so, or the
        // cause of every later PCI failure is invisible
        for host in nodes.iter().filter(|n| {
            n.is_compatible(kcore::iommutree::PCI_HOST)
                && n.prop("iommu-map").is_some()
                && !kcore::iommutree::fully_behind_the_iommu(n, &nodes)
        }) {
            use core::fmt::Write;
            let _ = writeln!(
                crate::console::Console,
                "kernel: iommu: PCI host {} is not wholly behind the IOMMU (its iommu-map): no device window",
                host.path
            );
        }
        // SAFETY: boot code, one hart, before anything reads it.
        unsafe { *addr_of_mut!(WINDOW_INPUTS) = Some((nodes, driven, ram)) };
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
                crate::kmem::direct_base() + pfn * PAGE_SIZE,
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
        pt.map(
            &mut mm,
            crate::kmem::direct_base() + pfn * PAGE_SIZE,
            pfn,
            Perms::R,
        )
        .expect("device tree map");
    }
    // the devices the kernel drives, found in the tree: the console, the test device, the interrupt controller's pages. Each page once (the
    // console and the test device may share one, and a page may not be mapped twice).
    let mut pages: alloc::vec::Vec<u64> = crate::console::base()
        .into_iter()
        .chain(crate::finisher::base().filter(|_| !crate::RELEASE))
        .chain(crate::aia::pages())
        .chain(crate::iommu::page())
        .map(|d| d & !(PAGE_SIZE - 1))
        .collect();
    pages.sort_unstable();
    pages.dedup();
    for page in pages {
        pt.map(
            &mut mm,
            crate::kmem::direct_base() + page,
            page / PAGE_SIZE,
            Perms::RW,
        )
        .expect("device page");
    }
    let root = pt.root();
    *KASPACE.lock() = Some(AddressSpace::new(PageTable::from_root(root)));
    KERNEL_ROOT.store(root, core::sync::atomic::Ordering::Relaxed);
    // (the shared zero frame is made now, so that a count of free frames taken later has it in its baseline)
    let mut vmos = Vmos::new();
    vmos.ensure_zero(&mut mm);
    // (PL8) the DMA backstop is derived from RAM, like the others
    vmos.set_dma_total_cap(
        kcore::limits::Backstops::for_ram(
            (memory_frames as u64).saturating_mul(PAGE_SIZE),
            crate::kstack::MAX_SLOTS as usize,
        )
        .dma_total_pages,
    );
    *VMOS.lock() = Some(vmos);
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

/// What `build_windows` builds the windows from (the parsed tree, the ranges the kernel drives, RAM and reserved memory), kept by
/// `init` until then.
#[allow(clippy::type_complexity)]
static mut WINDOW_INPUTS: Option<(
    alloc::vec::Vec<kcore::fdt::Node>,
    alloc::vec::Vec<(u64, u64)>,
    alloc::vec::Vec<(u64, u64)>,
)> = None;

/// Build the device windows (PL5, D1; IOMMU-SPEC 2.6). Only after the IOMMU denies every device (2.4 step 5, asserted): a PCI
/// window lets the holder of the root resource make a function a bus master, and that is safe only with the directory empty.
/// Boot only, once, on the boot hart.
pub fn build_windows() {
    assert!(
        crate::iommu::is_on(),
        "the device windows were built before the IOMMU denied every device (IOMMU-SPEC 2.4)"
    );
    // SAFETY: boot code, one hart, before any other hart or thread runs and before anything reads `WINDOWS`.
    unsafe {
        if let Some((nodes, driven, ram)) = (*addr_of_mut!(WINDOW_INPUTS)).take() {
            *addr_of_mut!(WINDOWS) = kcore::devwin::device_windows(&nodes, &driven, &ram);
        }
    }
}

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
    let root = kernel_table().root();
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
        // SAFETY: the kernel table maps everything the kernel runs from and touches. No flush (L18): the user entries of the space being
        // left stay cached under its ASID, which nothing runs under until its owner does (a hart flushes before it takes a newer
        // generation's ASID, and teardown's shootdown reaches every hart before the table's frames are reused).
        unsafe { crate::arch::aspace_install_noflush(want) };
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
    let root = kernel_table().root();
    let f = frames();
    let mut out = [0u64; 256];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = kcore::pagetable::PhysMem::read_u64(&f.mm.mem, (root << 12) + (256 + i as u64) * 8);
    }
    out
}

/// Remember the kernel half now (called once, after the S1 memory tests, before any process).
#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
pub fn snapshot_kernel_half() {
    // SAFETY: the one call (`kmain`, after the memory tests) is before any reader: `kernel_half_unchanged` is called only on the paths that load or end a process, and
    // the first process is loaded after it. Interrupts are on and the other harts are running by then, but none of them reads `KERNEL_HALF` yet (T242), and the first reader runs after the spawn of the process's first thread, whose thread-table lock orders this store before the read (KT14).
    unsafe { *addr_of_mut!(KERNEL_HALF) = kernel_half_now() };
}

#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
pub fn kernel_half_unchanged() -> bool {
    // SAFETY: read of a static written once by `snapshot_kernel_half`.
    kernel_half_now() == unsafe { *addr_of!(KERNEL_HALF) }
}

/// Free frames, plus the frames held by kernel stack slots (which keep theirs for ever once made, so
/// a leak check that compares before and after must not count a slot made in between as a leak).
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: a sum of three frame counts, each at most the number of frames
pub fn free_frames_accounted() -> usize {
    // (the four counts are read one after another, and an idle hart's heap service moves frames between them with no common lock: the sum is read until two
    // reads agree)
    let read = || {
        let free = get().mm.db.free_count(); // (the locks are released before the calls below)
        free + cached_frames() + crate::kstack::frames_held() + crate::heap::frames_held()
    };
    let mut last = read();
    for _ in 0..1000 {
        let now = read();
        if now == last {
            return now;
        }
        last = now;
    }
    last
}
