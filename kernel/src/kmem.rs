//! Kernel memory: the frame database, the real kernel page table (built with the
//! same pure `kcore` code the host tests exercise), and the kernel's own address
//! space, whose lazily committed regions the self-tests use.

use abi::layout::{DIRECT_BASE, KERNEL_OFFSET, RAM_BASE, RAM_SIZE};
use core::ptr::{addr_of, addr_of_mut, read_volatile, write_volatile};
use kcore::aspace::{AddressSpace, Vmos};
use kcore::frame::{FrameDb, PAGE_SIZE};
use kcore::pagetable::{PageTable, Perms, PhysMem};
use kcore::Mm;

/// Physical memory contents through the direct map.
pub struct DirectMap;

impl PhysMem for DirectMap {
    fn read_u64(&self, pa: u64) -> u64 {
        // SAFETY: callers pass the address of a frame the frame database manages
        // (page tables and VMO pages), which `init` direct-maps read-write; the
        // address is 8-byte aligned by construction.
        unsafe { read_volatile((DIRECT_BASE + pa) as *const u64) }
    }

    fn write_u64(&mut self, pa: u64, value: u64) {
        // SAFETY: as for `read_u64`.
        unsafe { write_volatile((DIRECT_BASE + pa) as *mut u64, value) }
    }
}

/// Everything the fault handler and the self-tests share.
pub struct Kmem {
    pub mm: Mm<DirectMap>,
    pub vmos: Vmos,
    pub aspace: AddressSpace,
}

static mut KMEM: Option<Kmem> = None;

/// The kernel memory state, protected by the big kernel lock (`get` asserts it is held once the
/// lock is enforced). Callers must not keep this reference across an access that can fault (the
/// self-tests drop it before touching lazily committed memory) or across a context switch.
///
/// A safe function only because the aliasing rule is enforced by the lock and by convention.
#[track_caller]
pub fn get() -> &'static mut Kmem {
    debug_assert!(
        crate::bkl::held(),
        "memory state touched without the big kernel lock"
    );
    // SAFETY: the big kernel lock is held (checked above once it is enforced); `init` filled the option before any
    // user of `get` runs, and callers follow the rule above, so no two live
    // references alias.
    unsafe {
        (*addr_of_mut!(KMEM))
            .as_mut()
            .expect("kmem::init was not called")
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

fn map_image(pt: &mut PageTable, mm: &mut Mm<DirectMap>, (start, end): (u64, u64), perms: Perms) {
    let mut va = start;
    while va < end {
        pt.map(mm, va, (va - KERNEL_OFFSET) / PAGE_SIZE, perms)
            .expect("mapping the kernel image");
        va += PAGE_SIZE;
    }
}

/// Build the kernel page table and switch to it. Needs the heap and the early
/// boot table (which maps the direct map, so new tables can be written).
pub fn init() {
    let s = sections();
    let kernel_end_phys = s.data.1 - KERNEL_OFFSET;
    let first = kernel_end_phys.div_ceil(PAGE_SIZE);
    // QEMU puts the device tree near the top of RAM; keep the top 2 MiB out of the
    // frame database so it survives until the device manager parses it (S6).
    let end = (RAM_BASE + RAM_SIZE - 2 * 1024 * 1024) / PAGE_SIZE;
    let mut mm = Mm {
        db: FrameDb::new(first, (end - first) as usize),
        mem: DirectMap,
    };
    let mut pt = PageTable::new(&mut mm).expect("a frame for the kernel root");
    // The image, section by section, with the permissions each deserves (W^X).
    map_image(&mut pt, &mut mm, s.text, Perms::RX);
    map_image(&mut pt, &mut mm, s.rodata, Perms::R);
    map_image(&mut pt, &mut mm, s.data, Perms::RW);
    // The direct map: every frame the frame database manages (never the image, so
    // the code cannot be written through an alias), and the two device pages we use.
    for pfn in first..end {
        pt.map(&mut mm, DIRECT_BASE + pfn * PAGE_SIZE, pfn, Perms::RW)
            .expect("direct map");
    }
    for device in [abi::finisher::BASE as u64, 0x1000_0000] {
        pt.map(&mut mm, DIRECT_BASE + device, device / PAGE_SIZE, Perms::RW)
            .expect("device page");
    }
    let root = pt.root();
    let aspace = AddressSpace::new(PageTable::from_root(root));
    // SAFETY: called once at boot with interrupts off, before anything calls `get`.
    unsafe {
        *addr_of_mut!(KMEM) = Some(Kmem {
            mm,
            vmos: Vmos::new(),
            aspace,
        })
    };
    let satp = (8u64 << 60) | root;
    KERNEL_SATP.store(satp, core::sync::atomic::Ordering::SeqCst);
    // SAFETY: the new table maps everything the kernel is executing from and
    // touching: text, rodata, data/bss/stack, the direct map and the two devices.
    // `sfence.vma` then drops the translations of the early table.
    unsafe {
        core::arch::asm!("csrw satp, {}", "sfence.vma zero, zero", in(reg) satp);
    }
}

/// The first frame the frame database manages (everything below it is the kernel
/// image, the firmware or unused).
pub fn first_managed_frame() -> u64 {
    sections()
        .data
        .1
        .wrapping_sub(KERNEL_OFFSET)
        .div_ceil(PAGE_SIZE)
}

/// The current `satp` value.
pub fn satp() -> u64 {
    let v: u64;
    // SAFETY: reads a CSR.
    unsafe { core::arch::asm!("csrr {}, satp", out(reg) v) };
    v
}

/// The kernel's own `satp` value, recorded once by `init` (the secondary harts load it).
pub static KERNEL_SATP: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Switch back to the kernel's own page table (before a process's root is freed).
pub fn use_kernel_table() {
    let root = crate::irq::without(|| get().aspace.table().root());
    let satp = (8u64 << 60) | root;
    // SAFETY: the kernel's table maps everything the kernel executes from and touches;
    // the flush drops the translations of the table being left.
    unsafe {
        core::arch::asm!("csrw satp, {}", "sfence.vma zero, zero", in(reg) satp);
    }
}

/// Load the kernel's own `satp` if the hart has something else in it (a user process's root).
pub fn use_kernel_table_if_needed() {
    if cfg!(feature = "fault-skip-kernel-satp") {
        return; // negative control: a hart keeps a dead process's page table
    }
    let want = KERNEL_SATP.load(core::sync::atomic::Ordering::Relaxed);
    if satp() != want {
        // SAFETY: the kernel table maps everything the kernel runs from and touches; the flush
        // drops the translations of the table being left.
        unsafe { core::arch::asm!("csrw satp, {}", "sfence.vma zero, zero", in(reg) want) };
    }
}

/// Flush the whole TLB.
pub fn flush_tlb() {
    // SAFETY: `sfence.vma` with no operands flushes all translations; no other effect.
    unsafe { core::arch::asm!("sfence.vma zero, zero") };
}

/// The kernel's top-level (upper half) page-table entries as `init` left them. Process page
/// tables copy these at creation, so the kernel must never create or free a top-level slot
/// afterwards (OD41); `kernel_half_unchanged` checks.
static mut KERNEL_HALF: [u64; 256] = [0; 256];

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
pub fn snapshot_kernel_half() {
    // SAFETY: called once at boot, interrupts off or a single thread, before `kernel_half_unchanged`.
    unsafe { *addr_of_mut!(KERNEL_HALF) = kernel_half_now() };
}

pub fn kernel_half_unchanged() -> bool {
    // SAFETY: read of a static written once by `snapshot_kernel_half`.
    kernel_half_now() == unsafe { *addr_of!(KERNEL_HALF) }
}
