//! The advanced interrupt architecture (OD75; QEMU `virt` with `aia=aplic-imsic`): the supervisor
//! domain's APLIC in MSI mode, and each hart's own IMSIC supervisor file, reached through the
//! indirect CSRs (`siselect`/`sireg`/`stopei`, the Ssaia extension).
//!
//! Message id 1 of every hart's file is the inter-hart interrupt: `send_ipi` writes it to the target's `seteipnum_le` and the handler claims it
//! with the rest. A device source `n` is message id `n + 1`.
//!
//! A source a process may take (the tree's admitted devices' interrupts, `kcore::aiatree`) is delivered as message id `n` to the hart that asked for it
//! (`enable` points the source's target at the calling hart). Every hart enables all of the ids
//! in its own IMSIC at start, so retargeting is one APLIC write, never a cross-hart call. A source that fires is **masked** at the APLIC by the trap handler and unmasked by
//! `interrupt_ack`, so a level-triggered line cannot storm. There is no claim to hold open: the
//! message was consumed when `stopei` was read. After unmasking, a source whose line is still
//! high is re-pended with `setipnum`, so a level that outlived the handler fires again.

use core::arch::asm;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// What the device tree says about the interrupt controller (`discover`, called by `kmem::init` once the tree is parsed); `None` on a
/// machine without a message-signalled APLIC (PL5c).
static mut INFO: Option<kcore::aiatree::Aia> = None;

/// The APLIC and its IMSIC as the tree describes them.
pub fn info() -> Option<&'static kcore::aiatree::Aia> {
    // SAFETY: written once by `discover` at boot, before any other hart or thread runs; read-only afterwards.
    unsafe { (*core::ptr::addr_of!(INFO)).as_ref() }
}

/// Pages of the APLIC the kernel maps: domaincfg, sourcecfg, the set/clear banks and the targets.
pub const PAGE_COUNT: usize = 4;

/// Read the interrupt controller out of the parsed tree and size the per-source state (needs the heap). `ram` is every range that is RAM or
/// reserved: a controller whose registers lie in it, or beyond the direct map, is not used (it would alias memory the kernel owns).
pub fn discover(nodes: &[kcore::fdt::Node], ram: &[(u64, u64)]) {
    let Some(aia) = kcore::aiatree::discover(nodes) else {
        return;
    };
    let imsic_bytes = aia.hart_order.len() as u64 * 0x1000;
    for span in [
        (aia.aplic_base, PAGE_COUNT as u64 * 0x1000),
        (aia.imsic_base, imsic_bytes),
    ] {
        let end = span.0.saturating_add(span.1);
        if end > abi::layout::DIRECT_MAP_PHYS_LIMIT
            || ram
                .iter()
                .any(|&(a, s)| span.0 < a.saturating_add(s) && a < end)
        {
            return;
        }
    }
    let aimed: alloc::vec::Vec<AtomicUsize> = (0..=aia.num_sources)
        .map(|_| AtomicUsize::new(usize::MAX))
        .collect();
    // SAFETY: boot code, one hart, before anything reads them.
    unsafe {
        *core::ptr::addr_of_mut!(AIMED) = aimed;
        *core::ptr::addr_of_mut!(INFO) = Some(aia);
    }
}

/// The physical pages of the interrupt controller the kernel maps: the APLIC's and every hart's IMSIC supervisor file (none without them).
pub fn pages() -> impl Iterator<Item = u64> {
    let aplic = info().map(|i| i.aplic_base);
    let imsic = info().map(|i| (i.imsic_base, i.hart_order.len() as u64));
    (0..PAGE_COUNT as u64)
        .filter_map(move |i| aplic.map(|b| b + i * 0x1000))
        .chain((0..imsic.map_or(0, |m| m.1)).filter_map(move |i| imsic.map(|m| m.0 + i * 0x1000)))
}

/// The message id of the inter-hart interrupt.
pub const IPI_ID: u32 = 1;
/// Its bit in the mask `claim` returns.
pub const IPI_BIT: u64 = 1 << IPI_ID;

/// Message id of device source `source`.
const fn eiid(source: u32) -> u32 {
    source + 1
}

/// Interrupt `hart_id` (the firmware's id): write the inter-hart id to its IMSIC file's `seteipnum_le`. The caller has recorded the reason.
pub fn send_ipi(hart_id: usize) {
    let Some(i) = info() else {
        return;
    };
    let Some(index) = i.hart_order.iter().position(|&h| h as usize == hart_id) else {
        return;
    };
    // SAFETY: the target's IMSIC supervisor-file page is direct-mapped device memory (`pages`, `kmem::init`); `seteipnum_le` is its first
    // register and writing an id to it only sets that id pending.
    unsafe {
        write_volatile(
            (abi::layout::DIRECT_BASE + i.imsic_base + index as u64 * 0x1000) as *mut u32,
            IPI_ID,
        );
    }
}

/// The number of slots an interrupt-object table needs: one per source plus the unused 0 (zero without a controller).
pub fn slots() -> usize {
    info().map_or(0, |i| i.num_sources as usize + 1)
}

/// May a process take source `irq`? Only the sources the admitted devices name in the tree (virtio-mmio and the RTC on `virt`).
pub fn irq_allowed(irq: u32) -> bool {
    info().is_some_and(|i| i.sources.binary_search(&irq).is_ok())
}

static PRESENT: AtomicBool = AtomicBool::new(false);
/// The hart each source was last aimed at (`enable`), to check delivery went where it was asked; one slot per source of the controller.
static mut AIMED: alloc::vec::Vec<AtomicUsize> = alloc::vec::Vec::new();

fn aimed(irq: u32) -> Option<&'static AtomicUsize> {
    // SAFETY: sized once by `discover` at boot; the atomics inside are the only thing that changes afterwards.
    unsafe { (&*core::ptr::addr_of!(AIMED)).get(irq as usize) }
}
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
    let base = info().map_or(0, |i| i.aplic_base);
    (abi::layout::DIRECT_BASE + base + off) as *mut u32
}

pub fn present() -> bool {
    PRESENT.load(Ordering::Relaxed)
}

/// Switch the supervisor APLIC the tree described on in MSI mode with every source a process may take level-high and masked, and bring up
/// this hart's IMSIC. Does nothing without a message-signalled APLIC (`discover` found none) or on an APLIC that cannot do MSI mode.
pub fn init() {
    let Some(aia) = info() else {
        return;
    };
    // SAFETY: the APLIC pages are direct-mapped device memory (`kmem::init`); these are its
    // documented registers: per-source configuration (level high), the target, the enable-clear
    // number register (masked until an interrupt object owns the source), then the domain config.
    unsafe {
        for &s in &aia.sources {
            write_volatile(reg(SOURCECFG + 4 * (u64::from(s) - 1)), SM_LEVEL_HIGH);
            write_volatile(reg(CLRIENUM), s);
        }
        write_volatile(reg(DOMAINCFG), DOMAINCFG_IE_MSI);
        // DM is read-only 0 on an APLIC that cannot do MSI mode: then there is nothing to use.
        if read_volatile(reg(DOMAINCFG)) & (1 << 2) == 0 {
            return;
        }
    }
    init_hart();
    PRESENT.store(true, Ordering::SeqCst);
}

/// Open this hart's IMSIC supervisor file to the ids of the sources a process may take (all below 64: one `eie` register) and allow
/// supervisor external interrupts on this hart.
pub fn init_hart() {
    let ids: usize = info().map_or(0, |i| {
        i.sources
            .iter()
            .fold(1usize << IPI_ID, |m, &s| m | (1usize << eiid(s)))
    });
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

/// Deliver source `irq` to the calling hart and unmask it (`interrupt_create`).
pub fn enable(irq: u32) {
    aim_and_unmask(irq);
}

/// `interrupt_ack`: aim the source at the acknowledging hart and unmask it; a level that is still
/// asserted fires again.
pub fn reenable(irq: u32) {
    #[cfg(feature = "fault-aia-no-unmask")]
    {
        let _ = irq;
        return; // negative control: an acknowledged source stays masked for good
    }
    #[cfg(not(feature = "fault-aia-no-unmask"))]
    aim_and_unmask(irq);
}

fn aim_and_unmask(irq: u32) {
    let hart_id = crate::percpu::this().hart_id.load(Ordering::Relaxed);
    // the APLIC target wants the hart's index in the IMSIC's interrupt-file order, which the tree gives (the id is the firmware's)
    let Some(index) = info().and_then(|i| i.hart_order.iter().position(|&h| h as usize == hart_id))
    else {
        return; // a hart the IMSIC does not list cannot take messages: the source stays masked
    };
    let hart = index as u32;
    if let Some(a) = aimed(irq) {
        a.store(hart_id, Ordering::Relaxed);
    }
    // SAFETY: the source's target word (hart index in bits 31:18, message id in bits 10:0), then
    // the set-pending and enable-set number registers of the supervisor APLIC. Pending goes first:
    // set while masked it is forwarded exactly once when the enable follows (the reverse order could
    // forward twice). Setting pending on a level source whose line is low is ignored by the hardware.
    unsafe {
        write_volatile(
            reg(TARGET + 4 * (u64::from(irq) - 1)),
            hart << 18 | eiid(irq),
        );
        #[cfg(not(feature = "fault-aia-no-repend"))]
        write_volatile(reg(SETIPNUM), irq);
        write_volatile(reg(SETIENUM), irq);
    }
}

/// Mask source `irq`.
pub fn disable(irq: u32) {
    // SAFETY: the enable-clear number register of the supervisor APLIC.
    unsafe { write_volatile(reg(CLRIENUM), irq) };
}

/// Take every message waiting in this hart's IMSIC (reading `stopei` claims one at a time); the result has bit `n` set for each message id `n`
/// that was pending (ids below 64: bit 1 is the inter-hart interrupt, the others are device sources + 1).
pub fn claim() -> u64 {
    let mut mask = 0u64;
    loop {
        let top: usize;
        // SAFETY: `csrrw stopei, x0` reads the top pending-and-enabled id and clears its
        // pending bit (the claim); it changes no other state.
        unsafe { asm!("csrrw {}, 0x15c, zero", out(reg) top) };
        let id = (top >> 16) as u32;
        if id == 0 {
            return mask;
        }
        if id < 64 {
            mask |= 1 << id;
        }
    }
}

/// Deliver the device messages of a `claim` mask: mask each source at the APLIC and tell `fired` (whether an interrupt object took it).
/// Returns whether any object was signalled.
pub fn deliver(mask: u64, mut fired: impl FnMut(u32) -> bool) -> bool {
    let mut any = false;
    for id in 2..64u32 {
        if mask & (1 << id) == 0 {
            continue;
        }
        let source = id - 1;
        if !irq_allowed(source) {
            continue;
        }
        disable(source);
        let this = crate::percpu::this();
        TAKEN.fetch_add(1, Ordering::Relaxed);
        if this.index.load(Ordering::Relaxed) != 0 {
            TAKEN_OFF_BOOT.fetch_add(1, Ordering::Relaxed);
        }
        if aimed(source).map(|a| a.load(Ordering::Relaxed))
            != Some(this.hart_id.load(Ordering::Relaxed))
        {
            MISROUTED.fetch_add(1, Ordering::Relaxed);
        }
        if fired(source) {
            any = true;
        }
    }
    any
}
