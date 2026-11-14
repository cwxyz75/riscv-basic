//! The advanced interrupt architecture (OD75; QEMU `virt` with `aia=aplic-imsic`): the supervisor
//! domain's APLIC in MSI mode, and each hart's own IMSIC supervisor file, reached through the
//! indirect CSRs (`siselect`/`sireg`/`stopei`, the Ssaia extension).
//!
//! Source `n` (1..=`MAX_IRQ`) is delivered as message id `n` to the hart that asked for it
//! (`enable` points the source's target at the calling hart). Every hart enables all of the ids
//! in its own IMSIC at start, so retargeting is one APLIC write, never a cross-hart call. A source that fires is **masked** at the APLIC by the trap handler and unmasked by
//! `interrupt_ack`, so a level-triggered line cannot storm. There is no claim to hold open: the
//! message was consumed when `stopei` was read. After unmasking, a source whose line is still
//! high is re-pended with `setipnum`, so a level that outlived the handler fires again.

use core::arch::asm;
use core::ptr::write_volatile;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// The supervisor-domain APLIC of the `virt` board (checked against the device tree at start).
pub const APLIC_BASE: u64 = 0x0d00_0000;
/// Pages of it the kernel maps: domaincfg, sourcecfg, the set/clear banks and the targets.
pub const PAGE_COUNT: usize = 4;

pub fn pages() -> impl Iterator<Item = u64> {
    (0..PAGE_COUNT as u64).map(|i| APLIC_BASE + i * 0x1000)
}

static PRESENT: AtomicBool = AtomicBool::new(false);
/// The hart each source was last aimed at (`enable`), to check delivery went where it was asked.
static AIMED: [AtomicUsize; abi::layout::MAX_IRQ as usize + 1] =
    [const { AtomicUsize::new(usize::MAX) }; abi::layout::MAX_IRQ as usize + 1];
/// Device interrupts taken, taken on a hart other than the boot hart, and taken on a hart other than
/// the one the source was aimed at (must stay 0). Read through `debug_stat` 3, 4 and 5.
static TAKEN: AtomicUsize = AtomicUsize::new(0);
static TAKEN_OFF_BOOT: AtomicUsize = AtomicUsize::new(0);
static MISROUTED: AtomicUsize = AtomicUsize::new(0);

/// The delivery counters: `which` 3 taken, 4 taken off the boot hart, 5 misrouted.
pub fn stat(which: u64) -> u64 {
    match which {
        3 => TAKEN.load(Ordering::Relaxed) as u64,
        4 => TAKEN_OFF_BOOT.load(Ordering::Relaxed) as u64,
        5 => MISROUTED.load(Ordering::Relaxed) as u64,
        _ => 0,
    }
}
/// How many sources the APLIC has (from the device tree); sources above are never enabled.
static SOURCES: AtomicUsize = AtomicUsize::new(0);

const DOMAINCFG: u64 = 0x0000;
const SOURCECFG: u64 = 0x0004; // + 4 * (source - 1)
const SETIPNUM: u64 = 0x1cdc;
const SETIENUM: u64 = 0x1edc;
const CLRIENUM: u64 = 0x1fdc;
const TARGET: u64 = 0x3004; // + 4 * (source - 1)

/// domaincfg: interrupt enable (bit 8) and MSI delivery mode (bit 2).
const DOMAINCFG_IE_MSI: u32 = (1 << 8) | (1 << 2);
/// sourcecfg: level-sensitive, active high (what virtio-mmio asserts).
const SM_LEVEL_HIGH: u32 = 6;

fn reg(off: u64) -> *mut u32 {
    (abi::layout::DIRECT_BASE + APLIC_BASE + off) as *mut u32
}

pub fn present() -> bool {
    PRESENT.load(Ordering::Relaxed)
}

/// Look for the supervisor APLIC in the device tree; if it is where the board has it, switch it on
/// in MSI mode with every source level-high and masked, and bring up this hart's IMSIC.
pub fn init() {
    let Some(blob) = crate::dtb::dtb_blob() else {
        return;
    };
    let Ok(nodes) = kcore::fdt::parse(blob) else {
        return;
    };
    // The supervisor domain is the APLIC with no children (the machine-level one, if the firmware
    // left it in the tree, delegates to it).
    let Some(node) = nodes
        .iter()
        .find(|n| n.is_compatible("riscv,aplic") && n.prop("riscv,children").is_none())
    else {
        return;
    };
    if node.reg().map(|r| r.0) != Some(APLIC_BASE) {
        return;
    }
    let sources = node.u32_prop("riscv,num-sources").unwrap_or(0) as usize;
    if sources <= abi::layout::MAX_IRQ as usize {
        return;
    }
    SOURCES.store(sources, Ordering::Relaxed);
    // SAFETY: the APLIC pages are direct-mapped device memory (`kmem::init`); these are its
    // documented registers: per-source configuration (level high), the target, the enable-clear
    // number register (masked until an interrupt object owns the source), then the domain config.
    unsafe {
        for s in 1..=abi::layout::MAX_IRQ as u64 {
            write_volatile(reg(SOURCECFG + 4 * (s - 1)), SM_LEVEL_HIGH);
            write_volatile(reg(CLRIENUM), s as u32);
        }
        write_volatile(reg(DOMAINCFG), DOMAINCFG_IE_MSI);
    }
    init_hart();
    PRESENT.store(true, Ordering::SeqCst);
}

/// Open this hart's IMSIC supervisor file to ids `1..=MAX_IRQ` and allow supervisor external
/// interrupts on this hart.
pub fn init_hart() {
    let ids: usize = ((1usize << (abi::layout::MAX_IRQ + 1)) - 1) & !1;
    // SAFETY: Ssaia CSRs of this hart: select the IMSIC's `eidelivery` (0x70) and write 1, its
    // `eithreshold` (0x72) and write 0 (everything passes), then `eie0` (0xC0, ids 0..63) and set
    // the ids used; finally sie.SEIE. These touch only this hart's interrupt-file state.
    unsafe {
        asm!(
            "csrw 0x150, {sel_del}",
            "csrw 0x151, {one}",
            "csrw 0x150, {sel_thr}",
            "csrw 0x151, zero",
            "csrw 0x150, {sel_eie}",
            "csrs 0x151, {ids}",
            "csrs sie, {seie}",
            sel_del = in(reg) 0x70usize,
            sel_thr = in(reg) 0x72usize,
            sel_eie = in(reg) 0xC0usize,
            one = in(reg) 1usize,
            ids = in(reg) ids,
            seie = in(reg) 1usize << 9,
        );
    }
}

/// Deliver source `irq` to the calling hart and unmask it; a still-asserted level re-fires.
pub fn enable(irq: u32) {
    let hart = crate::percpu::this().hart_id.load(Ordering::Relaxed) as u32;
    // SAFETY: the source's target word (hart index in bits 31:18, message id in bits 10:0), then
    // the enable-set and set-pending number registers of the supervisor APLIC. Setting pending on
    // a level source whose line is low is ignored by the hardware.
    AIMED[irq as usize].store(hart as usize, Ordering::Relaxed);
    #[cfg(feature = "fault-aia-no-unmask")]
    if TAKEN.load(Ordering::Relaxed) > 0 {
        return; // negative control: after the first interrupt a source stays masked for good
    }
    unsafe {
        write_volatile(reg(TARGET + 4 * (u64::from(irq) - 1)), hart << 18 | irq);
        write_volatile(reg(SETIENUM), irq);
        write_volatile(reg(SETIPNUM), irq);
    }
}

/// Mask source `irq`.
pub fn disable(irq: u32) {
    // SAFETY: the enable-clear number register of the supervisor APLIC.
    unsafe { write_volatile(reg(CLRIENUM), irq) };
}

/// Take every message waiting in this hart's IMSIC (reading `stopei` claims it), mask the source
/// and tell `fired` (whether an interrupt object took it). Returns whether any object was signalled.
pub fn handle(mut fired: impl FnMut(u32) -> bool) -> bool {
    let mut any = false;
    loop {
        let top: usize;
        // SAFETY: `csrrw stopei, x0` reads the top pending-and-enabled id and clears its
        // pending bit (the claim); it changes no other state.
        unsafe { asm!("csrrw {}, 0x15c, zero", out(reg) top) };
        let id = (top >> 16) as u32;
        if id == 0 {
            return any;
        }
        if id > abi::layout::MAX_IRQ {
            continue;
        }
        disable(id);
        let this = crate::percpu::this();
        TAKEN.fetch_add(1, Ordering::Relaxed);
        if this.index.load(Ordering::Relaxed) != 0 {
            TAKEN_OFF_BOOT.fetch_add(1, Ordering::Relaxed);
        }
        if AIMED[id as usize].load(Ordering::Relaxed) != this.hart_id.load(Ordering::Relaxed) {
            MISROUTED.fetch_add(1, Ordering::Relaxed);
        }
        if fired(id) {
            any = true;
        }
    }
}
