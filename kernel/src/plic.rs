//! The platform-level interrupt controller (QEMU `virt`, S6a-2): just enough for interrupt
//! objects. Sources are enabled only while an interrupt object owns them, and only for the boot
//! hart's S-mode context; when one fires the trap handler claims it, **masks it** (clears its
//! enable bit) and completes the claim; `interrupt_ack` unmasks. A level-triggered device
//! therefore cannot storm. Machines without a PLIC (AIA: APLIC + IMSIC) report
//! `present() == false` and interrupt objects there are NOT_SUPPORTED (OD64; drivers poll).

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

pub const BASE: u64 = 0x0c00_0000;
/// Contexts mapped: two per hart for the most harts the kernel supports. The boot hart is
/// whichever one OpenSBI picks, so its claim page cannot be known at build time.
const CONTEXTS: u64 = 2 * crate::percpu::MAX_HARTS as u64;

/// The pages of the PLIC the kernel maps: priorities, the enable words, every context's claim page.
pub fn pages() -> impl Iterator<Item = u64> {
    [BASE, BASE + 0x2000]
        .into_iter()
        .chain((0..CONTEXTS).map(|c| BASE + 0x20_0000 + c * 0x1000))
}

/// How many pages `pages` yields.
pub const PAGE_COUNT: usize = 2 + CONTEXTS as usize;

static PRESENT: AtomicBool = AtomicBool::new(false);
/// The S-mode context of the boot hart (`2 * hart_id + 1`).
static CONTEXT: AtomicUsize = AtomicUsize::new(1);
/// The device tree's physical address (OpenSBI hands it over in `a1`).
pub static DTB: AtomicUsize = AtomicUsize::new(0);

fn reg(off: u64) -> *mut u32 {
    (abi::layout::DIRECT_BASE + BASE + off) as *mut u32
}

fn enable_word(ctx: usize) -> *mut u32 {
    // enable bits for sources 0..31 of a context: BASE + 0x2000 + ctx * 0x80 (the page mapped
    // covers contexts 0..31)
    reg(0x2000 + ctx as u64 * 0x80)
}

fn claim_reg(ctx: usize) -> *mut u32 {
    reg(0x20_0000 + ctx as u64 * 0x1000 + 4)
}

pub fn present() -> bool {
    PRESENT.load(Ordering::Relaxed)
}

/// Look for a PLIC in the device tree and, if there is one, open the boot hart's context
/// (threshold 0, no source enabled) and allow supervisor external interrupts on this hart.
pub fn init(boot_hart: usize) {
    let dtb = DTB.load(Ordering::Relaxed) as u64;
    // The top 2 MiB of RAM, where QEMU puts the tree, is direct-mapped read-only by `kmem`.
    let ram_top = abi::layout::RAM_BASE + abi::layout::RAM_SIZE;
    if dtb < ram_top - 2 * 1024 * 1024 || dtb >= ram_top - 64 {
        return;
    }
    let head = (abi::layout::DIRECT_BASE + dtb) as *const u8;
    // SAFETY: `dtb` lies inside the direct-mapped top 2 MiB (checked above); the header's own
    // total size is clamped to what is left of that region before the slice is formed.
    let blob = unsafe {
        let total =
            u32::from_be_bytes([*head.add(4), *head.add(5), *head.add(6), *head.add(7)]) as u64;
        let len = total.min(ram_top - dtb) as usize;
        core::slice::from_raw_parts(head, len)
    };
    if !kcore::fdt::has_compatible(blob, "riscv,plic0")
        && !kcore::fdt::has_compatible(blob, "sifive,plic-1.0.0")
    {
        return;
    }
    let ctx = 2 * boot_hart + 1;
    CONTEXT.store(ctx, Ordering::Relaxed);
    // SAFETY: the PLIC pages are direct-mapped device memory (`kmem::init`); these are its
    // documented registers: the enable bits of our context (all clear) and its threshold.
    unsafe {
        for w in 0..3u64 {
            // every enable word of the context (sources 0..95): firmware may have left some set
            write_volatile(enable_word(ctx).byte_add(4 * w as usize), 0);
        }
        write_volatile(reg(0x20_0000 + ctx as u64 * 0x1000), 0);
        core::arch::asm!("csrs sie, {}", in(reg) 1usize << 9); // SEIE
    }
    PRESENT.store(true, Ordering::SeqCst);
}

/// Enable source `irq` (1..=31) at priority 1.
pub fn enable(irq: u32) {
    let ctx = CONTEXT.load(Ordering::Relaxed);
    // SAFETY: as in `init`: the source's priority word and our context's enable word.
    unsafe {
        write_volatile(reg(4 * irq as u64), 1);
        let w = read_volatile(enable_word(ctx));
        write_volatile(enable_word(ctx), w | 1 << irq);
    }
}

/// Mask source `irq`.
pub fn disable(irq: u32) {
    let ctx = CONTEXT.load(Ordering::Relaxed);
    // SAFETY: as in `init`.
    unsafe {
        let w = read_volatile(enable_word(ctx));
        write_volatile(enable_word(ctx), w & !(1 << irq));
    }
}

/// Complete the claim of `irq` (the PLIC ignores one that is not claimed).
pub fn complete(irq: u32) {
    let ctx = CONTEXT.load(Ordering::Relaxed);
    // SAFETY: the claim register of our context; writing an id completes that claim.
    unsafe { write_volatile(claim_reg(ctx), irq) };
}

/// Handle a supervisor external interrupt: claim every pending source, mask it and tell `fired`
/// (which returns whether an interrupt object took it). A taken source's claim stays **open**
/// until `interrupt_ack` completes it (a level-triggered line that is still high at completion
/// would otherwise be re-latched as a second interrupt); a source nobody owns, or outside the
/// range, is completed at once and stays masked. Returns whether any object was signalled.
pub fn handle(mut fired: impl FnMut(u32) -> bool) -> bool {
    let ctx = CONTEXT.load(Ordering::Relaxed);
    let mut any = false;
    loop {
        // SAFETY: the claim register of our context: reading claims, writing the same id completes.
        let irq = unsafe { read_volatile(claim_reg(ctx)) };
        if irq == 0 {
            return any;
        }
        if irq > abi::layout::MAX_IRQ {
            complete(irq); // not ours to mask: no shift on a bogus id, just close it
            continue;
        }
        disable(irq);
        if fired(irq) {
            any = true;
        } else {
            complete(irq);
        }
    }
}
