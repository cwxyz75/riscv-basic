//! The advanced interrupt architecture (OD75; QEMU `virt` with `aia=aplic-imsic`): the supervisor
//! domain's APLIC in MSI mode, and each hart's own IMSIC supervisor file, reached through the
//! indirect CSRs (`siselect`/`sireg`/`stopei`, the Ssaia extension).
//!
//! **Interrupt identities** (IOMMU-SPEC 8.1, IO12; IOMMU4). Every message a hart takes from its file carries an identity the kernel
//! allocated per hart (`kcore::msiid`: `riscv,num-ids` of them in each file), and every allocated identity has one route: identity 1 is
//! the inter-hart interrupt on every hart (`send_ipi` writes it to the target's `seteipnum_le`), and the others are wired APLIC sources
//! (behind an interrupt object, or owned by the kernel). No identity is derived from a source number.
//!
//! An identity is **opened** (its `eie` bit set) by its own hart only, when its route is installed, and a route is not used before
//! that: a source is aimed at an identity (the APLIC target word: hart index, identity) only once the identity is open. On the
//! calling hart that is at once; on another hart it is that hart's work, asked for with an inter-hart message (`IPI_IRQ`), and the
//! hart that opens it also aims the source and tells the old hart to retire the identity the route left. **Retire**: the old hart
//! closes the identity, reads and clears its pending bit (a message sent before the source was aimed elsewhere: delivered to the route
//! if it moved, dropped if it ended), and only then frees it. So an identity is never given to another route while a message for its
//! old owner can still arrive (T59's in-flight message at re-creation).
//!
//! A source a process may take (the tree's admitted devices' interrupts, `kcore::aiatree`) is delivered to the hart that created or
//! last acknowledged its object (`interrupt_create`, `interrupt_ack`), or, when that hart goes offline, to the hart with the fewest
//! routes (`evacuate`). A source that fires is **masked** at the APLIC by the trap handler and unmasked by `interrupt_ack`, so a
//! level-triggered line cannot storm. There is no claim to hold open: the message was consumed when `stopei` was read. After
//! unmasking, a source whose line is still high is re-pended with `setipnum`, so a level that outlived the handler fires again.
//!
//! The route table is under the object lock (`OBJ`, the big kernel lock: IOMMU-SPEC 11.1), like the interrupt objects: the claim path
//! takes it after reading `stopei`, and `interrupt_create`, `interrupt_ack`, an object's destruction and a hart going offline write it.
//! Nothing here allocates after boot.

use core::arch::asm;
use core::fmt::Write as _;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use kcore::msiid::{IdSet, MsiIds, Place, Route, Slot};

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

/// The sources the kernel owns (IOMMU-SPEC 8.6: kernel-owned wired sources, never in the set `interrupt_create` admits), each with
/// its handler. A `Kernel` route is installed for each at boot; `deliver` masks the source, calls the handler and unmasks it again.
/// None yet: IOMMU5 adds the IOMMU's fault-queue and command-queue sources.
const KERNEL_SOURCES: &[KernelSource] = &[];

/// A source the kernel owns and its handler (called with the source masked; it is unmasked after).
type KernelSource = (u32, fn(u32));

/// The route table and what it needs, under the object lock.
struct Table {
    ids: MsiIds,
    /// Per source (wired or kernel-owned): the identity the APLIC aims it at (its Live identity), if it has a route.
    at: alloc::vec::Vec<Option<Place>>,
}

static mut TABLE: Option<Table> = None;
/// Per interrupt file: the hart has work in its file (an identity to open or to retire). Set under the lock before the message that
/// says so; the hart takes the lock when it sees it.
static mut WORK: alloc::vec::Vec<AtomicBool> = alloc::vec::Vec::new();
/// Per interrupt file: the index (`percpu`) of the hart that owns it, once that hart brought it up (`usize::MAX` before).
static mut HART_OF_FILE: alloc::vec::Vec<AtomicUsize> = alloc::vec::Vec::new();
/// Per hart (`percpu` index): its interrupt file, once brought up (`usize::MAX` before).
static FILE_OF_HART: [AtomicUsize; crate::percpu::MAX_HARTS] =
    [const { AtomicUsize::new(usize::MAX) }; crate::percpu::MAX_HARTS];

/// Debug builds: the table is open in a `with_table` closure (the object lock makes this one flag for the machine).
#[cfg(debug_assertions)]
static TABLE_OPEN: AtomicBool = AtomicBool::new(false);

/// Run `f` on the route table, which the object lock protects. `None` without a controller.
#[track_caller]
fn with_table<R>(f: impl FnOnce(&mut Table) -> R) -> Option<R> {
    debug_assert!(
        !crate::irq::enabled() && crate::bkl::holds(),
        "the interrupt route table touched without the object lock"
    );
    #[cfg(debug_assertions)]
    assert!(
        !TABLE_OPEN.swap(true, Ordering::Relaxed),
        "with_table opened inside itself: two live `&mut` of the route table"
    );
    // SAFETY: the object lock is held (debug-checked), so no other hart has the table; the closure does not re-enter (debug-checked
    // just above) and the reference does not outlive it. The table is written only by `discover`, at boot, before any other hart runs.
    let r = unsafe { (*core::ptr::addr_of_mut!(TABLE)).as_mut().map(f) };
    #[cfg(debug_assertions)]
    TABLE_OPEN.store(false, Ordering::Relaxed);
    r
}

fn work_flag(file: usize) -> Option<&'static AtomicBool> {
    // SAFETY: sized once by `discover` at boot; only the atomics inside change afterwards.
    unsafe { (&*core::ptr::addr_of!(WORK)).get(file) }
}

fn hart_of_file(file: usize) -> Option<usize> {
    // SAFETY: as `work_flag`.
    unsafe { (&*core::ptr::addr_of!(HART_OF_FILE)).get(file) }
        .map(|a| a.load(Ordering::Acquire))
        .filter(|&h| h != usize::MAX)
}

/// The interrupt file of the calling hart (`None` before `init_hart`, or for a hart the IMSIC does not list).
fn this_file() -> Option<usize> {
    let me = crate::percpu::this().index.load(Ordering::Relaxed);
    FILE_OF_HART
        .get(me)
        .map(|a| a.load(Ordering::Acquire))
        .filter(|&f| f != usize::MAX)
}

/// Read the interrupt controller out of the parsed tree and size the per-source state and the route table (needs the heap). `ram` is
/// every range that is RAM or reserved: a controller whose registers lie in it, or beyond the direct map, is not used (it would alias
/// memory the kernel owns).
#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; boot only, once, on the boot stack (the boot-time high-water budget check bounds the sum)
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
    let files = aia.hart_order.len();
    let Some(ids) = MsiIds::new(files, aia.num_ids) else {
        return;
    };
    // (T183: the per-source state is sized by the highest source in use, not by the controller's source count)
    let top = aia
        .sources
        .iter()
        .copied()
        .chain(KERNEL_SOURCES.iter().map(|k| k.0))
        .filter(|&s| s <= aia.num_sources)
        .max()
        .unwrap_or(0);
    let table = Table {
        ids,
        at: alloc::vec![None; top as usize + 1],
    };
    // SAFETY: boot code, one hart, before anything reads them.
    unsafe {
        *core::ptr::addr_of_mut!(TABLE) = Some(table);
        *core::ptr::addr_of_mut!(WORK) = (0..files).map(|_| AtomicBool::new(false)).collect();
        *core::ptr::addr_of_mut!(HART_OF_FILE) =
            (0..files).map(|_| AtomicUsize::new(usize::MAX)).collect();
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

/// The identity of the inter-hart interrupt on every hart.
pub const IPI_ID: u32 = kcore::msiid::IPI as u32;

/// Does the IMSIC list hart `hart_id` (can it be sent a message)?
pub fn lists_hart(hart_id: usize) -> bool {
    info().is_some_and(|i| i.hart_order.iter().any(|&h| h as usize == hart_id))
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
            (crate::kmem::direct_base() + i.imsic_base + index as u64 * 0x1000) as *mut u32,
            IPI_ID,
        );
    }
}

/// The number of slots an interrupt-object table needs: one per source up to the highest a process may take, plus the unused 0 (zero
/// without a controller).
pub fn slots() -> usize {
    info().map_or(0, |i| i.sources.last().map_or(0, |&s| s as usize + 1))
}

/// May a process take source `irq`? Only the sources the admitted devices name in the tree (virtio-mmio and the RTC on `virt`).
pub fn irq_allowed(irq: u32) -> bool {
    info().is_some_and(|i| i.sources.binary_search(&irq).is_ok())
}

static PRESENT: AtomicBool = AtomicBool::new(false);
/// Device interrupts taken, taken on a hart other than the boot hart, and taken on an identity that is not the one its source is aimed
/// at nor one it is moving away from (must stay 0). Read through `debug_stat` 3, 4 and 5.
static TAKEN: AtomicUsize = AtomicUsize::new(0);
static TAKEN_OFF_BOOT: AtomicUsize = AtomicUsize::new(0);
static MISROUTED: AtomicUsize = AtomicUsize::new(0);
/// Messages taken on an identity with no route (`debug_stat` 140; must stay 0), and whether the console was told about the first.
static NO_ROUTE: AtomicU64 = AtomicU64::new(0);
static NO_ROUTE_SAID: AtomicBool = AtomicBool::new(false);
/// The highest identity a device message was taken on (`debug_stat` 142: the suite walks every `eie` register with it).
static TOP_ID: AtomicU64 = AtomicU64::new(0);
/// Identities retired after their route moved (`debug_stat` 143).
static MOVES: AtomicU64 = AtomicU64::new(0);

/// The delivery counters: `which` 3 taken, 4 taken off the boot hart, 5 misrouted; 140 messages with no route, 141 the free identities of
/// every file together (the object lock is held: `debug_stat` reads under it), 142 the highest identity a device message was taken on,
/// 143 identities retired after a move, 144 the identities of a file (`riscv,num-ids`).
pub fn stat(which: u64) -> u64 {
    match which {
        3 => TAKEN.load(Ordering::Relaxed) as u64,
        4 => TAKEN_OFF_BOOT.load(Ordering::Relaxed) as u64,
        5 => MISROUTED.load(Ordering::Relaxed) as u64,
        140 => NO_ROUTE.load(Ordering::Relaxed),
        141 => crate::bkl::ensure(|| with_table(|t| t.ids.free_total())).unwrap_or(0),
        142 => TOP_ID.load(Ordering::Relaxed),
        143 => MOVES.load(Ordering::Relaxed),
        144 => info().map_or(0, |i| u64::from(i.num_ids)),
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
    (crate::kmem::direct_base() + base + off) as *mut u32
}

pub fn present() -> bool {
    PRESENT.load(Ordering::Relaxed)
}

/// Switch the supervisor APLIC the tree described on in MSI mode with every source in use level-high and masked, bring up this hart's
/// IMSIC and install the kernel's own sources. Does nothing without a message-signalled APLIC (`discover` found none) or on an APLIC
/// that cannot do MSI mode.
pub fn init() {
    let Some(aia) = info() else {
        return;
    };
    for &s in &aia.dropped {
        let _ = writeln!(
            crate::console::Console,
            "aia: source {s}, named by a device, is not one of the APLIC's {} sources: not used",
            aia.num_sources
        );
    }
    let kernel = || {
        KERNEL_SOURCES.iter().filter(|k| {
            let ok = (1..=aia.num_sources).contains(&k.0) && !irq_allowed(k.0);
            if !ok {
                let _ = writeln!(
                    crate::console::Console,
                    "aia: kernel source {} is not a source of the APLIC or is one a process may take: not used",
                    k.0
                );
            }
            ok
        })
    };
    // SAFETY: the APLIC pages are direct-mapped device memory (`kmem::init`); these are its
    // documented registers: per-source configuration (level high), the enable-clear number register
    // (masked until a route owns the source), then the domain config.
    unsafe {
        for s in aia.sources.iter().copied().chain(kernel().map(|k| k.0)) {
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
    if this_file().is_none() {
        return; // (the boot hart is not in the IMSIC's list: no interrupt can reach it)
    }
    PRESENT.store(true, Ordering::SeqCst);
    // the kernel's own sources, on the boot hart (the only one running): moved when it goes offline, like any route
    crate::irq::without(|| {
        for &(s, _) in kernel() {
            if install_here(s, Route::Kernel(s)).is_ok() {
                aim_and_unmask(s);
            } else {
                let _ = writeln!(
                    crate::console::Console,
                    "aia: no identity for kernel source {s}"
                );
            }
        }
    });
}

/// The `siselect` number of the `eip` (pending) and `eie` (enable) register that holds identity `id`: on RV64 the even-numbered ones,
/// 64 identities each.
fn eip_reg(id: u32) -> usize {
    0x80 + 2 * (id as usize / 64)
}
fn eie_reg(id: u32) -> usize {
    0xC0 + 2 * (id as usize / 64)
}

/// Bring up this hart's IMSIC supervisor file: delivery on, every identity passes the threshold, every identity closed and not pending
/// but the inter-hart one (opened here: the only identity that has a route before anything asks), then supervisor external interrupts
/// on. Records which file is this hart's.
pub fn init_hart() {
    let Some(i) = info() else {
        return;
    };
    let hart_id = crate::percpu::this().hart_id.load(Ordering::Relaxed);
    let me = crate::percpu::this().index.load(Ordering::Relaxed);
    let file = i.hart_order.iter().position(|&h| h as usize == hart_id);
    // SAFETY: Ssaia CSRs of this hart, with interrupts off (boot, or a secondary before it enables them): select the IMSIC's
    // `eidelivery` (0x70) and write 1, its `eithreshold` (0x72) and write 0 (everything passes), then every `eip` and `eie` register
    // up to `num-ids` and clear them, set the inter-hart identity's enable bit, and finally sie.SEIE. These touch only this hart's
    // interrupt-file state.
    unsafe {
        asm!(
            "csrw 0x150, {sel_del}",
            "csrw 0x151, {one}",
            "csrw 0x150, {sel_thr}",
            "csrw 0x151, zero",
            sel_del = in(reg) 0x70usize,
            sel_thr = in(reg) 0x72usize,
            one = in(reg) 1usize,
        );
        for r in 0..=(i.num_ids as usize / 64) {
            asm!(
                "csrw 0x150, {eip}",
                "csrw 0x151, zero",
                "csrw 0x150, {eie}",
                "csrw 0x151, zero",
                eip = in(reg) 0x80 + 2 * r,
                eie = in(reg) 0xC0 + 2 * r,
            );
        }
        asm!(
            "csrw 0x150, {sel_eie}",
            "csrs 0x151, {ipi}",
            "csrs sie, {seie}",
            sel_eie = in(reg) eie_reg(IPI_ID),
            ipi = in(reg) 1usize << IPI_ID,
            seie = in(reg) 1usize << 9,
        );
    }
    if let (Some(f), Some(slot)) = (file, FILE_OF_HART.get(me)) {
        // SAFETY: as `work_flag`.
        if let Some(h) = unsafe { (&*core::ptr::addr_of!(HART_OF_FILE)).get(f) } {
            h.store(me, Ordering::Release);
        }
        slot.store(f, Ordering::Release);
    }
}

/// Open identity `id` of this hart's file: clear a stale pending bit (a message that reached it while it had no route: counted), then
/// set its enable bit. Interrupts off.
fn open_here(id: u32) {
    debug_assert!(!crate::irq::enabled());
    let bit = 1usize << (id % 64);
    let old: usize;
    // SAFETY: this hart's own `eip` and `eie` registers through `siselect`/`sireg`, with interrupts off so nothing changes `siselect`
    // in between; `csrrc` clears the pending bit and returns the old register, `csrs` sets the enable bit.
    unsafe {
        asm!(
            "csrw 0x150, {eip}",
            "csrrc {old}, 0x151, {bit}",
            "csrw 0x150, {eie}",
            "csrs 0x151, {bit}",
            eip = in(reg) eip_reg(id),
            eie = in(reg) eie_reg(id),
            bit = in(reg) bit,
            old = out(reg) old,
        );
    }
    if old & bit != 0 {
        no_route(this_file().unwrap_or(usize::MAX), id);
    }
}

/// Close identity `id` of this hart's file and read and clear its pending bit: whether a message was waiting on it. Interrupts off.
fn close_here(id: u32) -> bool {
    debug_assert!(!crate::irq::enabled());
    let bit = 1usize << (id % 64);
    let old: usize;
    // SAFETY: as `open_here`: clear the enable bit first (nothing more is taken on it), then take the pending bit.
    unsafe {
        asm!(
            "csrw 0x150, {eie}",
            "csrc 0x151, {bit}",
            "csrw 0x150, {eip}",
            "csrrc {old}, 0x151, {bit}",
            eip = in(reg) eip_reg(id),
            eie = in(reg) eie_reg(id),
            bit = in(reg) bit,
            old = out(reg) old,
        );
    }
    old & bit != 0
}

fn no_route(file: usize, id: u32) {
    NO_ROUTE.fetch_add(1, Ordering::Relaxed);
    if !NO_ROUTE_SAID.swap(true, Ordering::Relaxed) {
        let _ = writeln!(
            crate::console::Console,
            "aia: a message on identity {id} of interrupt file {file}, which has no route (debug_stat 140 counts them; said once)"
        );
    }
}

/// Ask the hart of interrupt file `file` to do its work there (open or retire identities): the flag, then the message.
fn ask(file: usize) {
    if let Some(f) = work_flag(file) {
        f.store(true, Ordering::SeqCst);
    }
    if let Some(h) = hart_of_file(file) {
        crate::smp::send_ipi(h, crate::percpu::IPI_IRQ);
    }
}

/// Has this hart's file work to do (identities to open or retire)? The trap path takes the lock for it.
pub fn has_work() -> bool {
    this_file()
        .and_then(work_flag)
        .is_some_and(|f| f.load(Ordering::Acquire))
}

/// Aim source `s` at identity `p` (the APLIC target word: hart index in bits 31:18, identity in bits 10:0), and make sure the write
/// reached the APLIC before anything that follows (a message to the old identity's hart telling it to retire it).
fn aim(s: u32, p: Place) {
    // SAFETY: the controller exists and `s` is one of its sources (allowed, or a kernel source, checked against `num-sources` at boot),
    // so the target register is inside its mapped window. Reading it back completes the write at the APLIC; the fence orders it before
    // later device writes.
    unsafe {
        let r = reg(TARGET + 4 * (u64::from(s) - 1));
        write_volatile(r, u32::from(p.hart) << 18 | u32::from(p.id));
        let _ = read_volatile(r);
        asm!("fence iorw, iorw");
    }
}

/// End source `s`'s route, if it has one: its identities are Retiring (the hart of each retires them). Under the lock.
fn end_route(t: &mut Table, s: u32) {
    if let Some(p) = t.at.get_mut(s as usize).and_then(Option::take) {
        if let Ok(cancelled) = t.ids.end(p) {
            ask(usize::from(p.hart));
            if let Some(c) = cancelled {
                ask(usize::from(c.hart));
            }
        }
    }
}

/// Is file `f` one routes may go to: its hart is started and online (not draining, offline or starting, IOMMU-SPEC 8.5)?
fn usable_file(f: u16) -> bool {
    hart_of_file(usize::from(f)).is_some_and(|h| {
        h < crate::smp::started()
            && crate::percpu::hart(h).hart_state.load(Ordering::Acquire)
                == crate::percpu::HART_ONLINE
    })
}

/// Is the hart of file `f` going offline or offline (`hart_set_online`, IOMMU-SPEC 8.5)? No route is placed on such a hart: its routes
/// were moved off before it left the scheduler, and a thread still finishing a call there must not bring one back.
fn leaving(f: u16) -> bool {
    hart_of_file(usize::from(f)).is_some_and(|h| {
        h < crate::smp::started()
            && matches!(
                crate::percpu::hart(h).hart_state.load(Ordering::Acquire),
                crate::percpu::HART_DRAINING | crate::percpu::HART_OFFLINE
            )
    })
}

/// Take an identity on this hart for `route` of source `s` (ending any route `s` still has: a destroyed object's), open it here and
/// record it as `s`'s. The source is not aimed yet. On a hart that is `leaving`, the identity is taken on the online hart with the
/// fewest routes instead: that hart opens it, then aims and unmasks the source (`deliver`'s work), and until then the source stays
/// masked. `Err`: no free identity, or the IMSIC does not list this hart.
fn install_here(s: u32, route: Route) -> Result<(), abi::status::Status> {
    let file = this_file().ok_or(abi::status::Status::NoMemory)?;
    let hart = u16::try_from(file).map_err(|_| abi::status::Status::NoMemory)?;
    with_table(|t| {
        // (the identity first: when there is none, nothing has changed, the old route included)
        let p = if !leaving(hart) {
            let id = t
                .ids
                .alloc(hart, route)
                .ok_or(abi::status::Status::NoMemory)?;
            let p = Place { hart, id };
            open_here(u32::from(id));
            let _ = t.ids.opened(p);
            p
        } else {
            let p = t
                .ids
                .alloc_any(route, usable_file)
                .ok_or(abi::status::Status::NoMemory)?;
            ask(usize::from(p.hart));
            p
        };
        // a route the source still has (a destroyed object whose cleanup has not run yet) ends: masked first, so the source cannot
        // fire into its old identity once that is retired and given to someone else
        if matches!(t.at.get(s as usize), Some(Some(_))) {
            disable(s);
            end_route(t, s);
        }
        if let Some(a) = t.at.get_mut(s as usize) {
            *a = Some(p);
        }
        Ok(())
    })
    .unwrap_or(Err(abi::status::Status::NotSupported))
}

/// `interrupt_create`, first half: an identity on the calling hart for source `irq` (the caller checked it is allowed and that no live
/// object owns it). `NoMemory`: the calling hart's file has no free identity (or the IMSIC does not list it: such a hart is never
/// started, PL6). Then the object is made, and `enable` aims and unmasks the source, or `release` gives the identity back.
pub fn take(irq: u32) -> Result<(), abi::status::Status> {
    install_here(irq, Route::Wired(irq))
}

/// `interrupt_create`, second half: aim source `irq` at its identity on the calling hart and unmask it.
pub fn enable(irq: u32) {
    aim_and_unmask(irq);
}

/// `interrupt_ack`: move the source's route to the acknowledging hart (if it is elsewhere, and this hart has a free identity: else it
/// stays where it is) and unmask it; a level that is still asserted fires again.
pub fn reenable(irq: u32) {
    if let Some(file) = this_file() {
        if let Ok(hart) = u16::try_from(file) {
            with_table(|t| {
                let Some(&Some(at)) = t.at.get(irq as usize) else {
                    return;
                };
                // (the route stays where it is, and a move under way with it, unless this acknowledgement moves it: not on its own
                // hart, not onto a hart that is leaving (the evacuation chose where its routes go), not without a free identity here)
                if at.hart == hart
                    || leaving(hart)
                    || t.ids.free_on(hart) == 0
                    || !matches!(t.ids.slot(at), Some(Slot::Live { .. }))
                {
                    return;
                }
                // a move under way (a hart going offline asked another to open one) gives way to this one
                if let Some(Slot::Live { to: Some(next), .. }) = t.ids.slot(at) {
                    if t.ids.end(next).is_ok() {
                        ask(usize::from(next.hart));
                    }
                }
                let Some(id) = t.ids.move_to(at, hart) else {
                    return; // (refused: the source stays where it is)
                };
                let p = Place { hart, id };
                open_here(u32::from(id));
                let old = t.ids.opened(p).and_then(|o| o.retiring);
                if let Some(a) = t.at.get_mut(irq as usize) {
                    *a = Some(p);
                }
                aim(irq, p);
                if let Some(o) = old {
                    ask(usize::from(o.hart));
                }
            });
        }
    }
    #[cfg(feature = "fault-aia-no-unmask")]
    {
        return; // negative control: an acknowledged source stays masked for good
    }
    #[cfg(not(feature = "fault-aia-no-unmask"))]
    aim_and_unmask(irq);
}

/// Aim source `irq` at its identity and unmask it.
fn aim_and_unmask(irq: u32) {
    // (no route, or one whose identity its hart has not opened yet, which aims and unmasks it when it does: the source stays masked)
    let Some(Some(p)) = with_table(|t| {
        t.at.get(irq as usize)
            .copied()
            .flatten()
            .filter(|&p| matches!(t.ids.slot(p), Some(Slot::Live { .. })))
    }) else {
        return;
    };
    aim(irq, p);
    // SAFETY: the controller exists (the caller found it in the tree) and `irq` is one of its sources (checked by the caller against the
    // allow list in user/trap.rs, or a kernel source), so the two registers are inside its mapped window. The set-pending and enable-set
    // number registers of the supervisor APLIC. Pending goes first: set while masked it is forwarded exactly once when the enable
    // follows (the reverse order could forward twice). Setting pending on a level source whose line is low is ignored by the hardware.
    unsafe {
        #[cfg(not(feature = "fault-aia-no-repend"))]
        write_volatile(reg(SETIPNUM), irq);
        write_volatile(reg(SETIENUM), irq);
    }
}

/// The source `irq`'s object is gone (and no newer one owns it): mask it and end its route; the identity is retired by its hart.
pub fn release(irq: u32) {
    disable(irq);
    // (the mask reaches the APLIC before the identity's hart is told to retire it, as `aim` makes sure of a new target)
    // SAFETY: a fence orders this hart's device writes; it touches no memory.
    unsafe { asm!("fence iorw, iorw") };
    with_table(|t| end_route(t, irq));
}

/// Hart `v` (`percpu` index) goes offline (IOMMU-SPEC 8.5): move every route on its file to the online hart with the fewest routes.
/// All or nothing: `NoMemory`, with nothing changed, when the other harts have too few free identities (or there is no other online
/// hart and `v` has routes). The harts the routes move to open them and aim the sources; `v` retires the old identities (an offline
/// hart still takes its file's messages). The caller holds the object lock with interrupts off (`hart_set_online`, which also checks
/// first that the hart may go).
pub fn evacuate_locked(v: usize) -> Result<(), abi::status::Status> {
    let Some(file) = FILE_OF_HART
        .get(v)
        .map(|a| a.load(Ordering::Acquire))
        .filter(|&f| f != usize::MAX)
    else {
        return Ok(()); // (no file: no routes)
    };
    let Ok(from) = u16::try_from(file) else {
        return Ok(());
    };
    with_table(|t| {
        let Table { ids, at } = t;
        ids.evacuate(from, usable_file, |c| match c {
            kcore::msiid::Evacuated::Cancelled { at: p, .. } => ask(usize::from(p.hart)),
            kcore::msiid::Evacuated::Moved { to, .. } => ask(usize::from(to.hart)),
            // (a new route not opened yet is placed again: the source's record follows it, and both harts have work)
            kcore::msiid::Evacuated::Replaced { route, from, to } => {
                if let Route::Wired(s) | Route::Kernel(s) = route {
                    if let Some(a) = at.get_mut(s as usize).filter(|a| **a == Some(from)) {
                        *a = Some(to);
                    }
                }
                ask(usize::from(from.hart));
                ask(usize::from(to.hart));
            }
        })
    })
    .unwrap_or(Ok(()))
    .map_err(|()| abi::status::Status::NoMemory)
}

/// Mask source `irq`.
pub fn disable(irq: u32) {
    // SAFETY: the controller exists and `irq` was allowed (an Interrupt object or the allow list) or is a kernel source before this is
    // reached; the enable-clear number register of the supervisor APLIC is inside its mapped window and writing a source number only
    // masks that source.
    unsafe { write_volatile(reg(CLRIENUM), irq) };
}

/// Take every message waiting in this hart's IMSIC file (reading `stopei` claims one at a time, lowest identity first: the inter-hart
/// one before any device's) and hand each identity to `took`. No lock: the routes are looked up afterwards, under it (`deliver`).
pub fn claim(mut took: impl FnMut(u32)) {
    loop {
        let top: usize;
        // SAFETY: `csrrw stopei, x0` reads the top pending-and-enabled id and clears its
        // pending bit (the claim); it changes no other state.
        unsafe { asm!("csrrw {}, 0x15c, zero", out(reg) top) };
        let id = ((top >> 16) & 0x7ff) as u32;
        if id == 0 {
            return;
        }
        took(id);
    }
}

/// What a message on an identity of this hart's file is for.
enum Act {
    /// A wired source behind an interrupt object: mask it, then the object fires. The flag: the identity is Live but not the one the
    /// source is aimed at (a misroute: must not happen). A message on an identity the source moved away from is not one: it was on
    /// its way before the move.
    Wired(u32, bool),
    /// A source the kernel owns.
    Kernel(u32),
    /// An identity whose route ended: dropped.
    Ended,
    /// Nobody's.
    NoRoute,
}

fn act(t: &Table, p: Place) -> Act {
    match t.ids.slot(p) {
        Some(Slot::Live {
            route: Route::Wired(s),
            ..
        }) => Act::Wired(s, t.at.get(s as usize).copied().flatten() != Some(p)),
        Some(Slot::Retiring {
            route: Route::Wired(s),
            moved: true,
        }) => Act::Wired(s, false),
        Some(Slot::Live {
            route: Route::Kernel(s),
            ..
        })
        | Some(Slot::Retiring {
            route: Route::Kernel(s),
            moved: true,
        }) => Act::Kernel(s),
        Some(Slot::Retiring { moved: false, .. }) => Act::Ended,
        // (vectors have no identity before IOMMU7; a Free or Opening identity takes no message)
        _ => Act::NoRoute,
    }
}

/// Deliver a message on identity `p` of this hart's file. Returns whether an interrupt object was signalled.
fn deliver_one(p: Place, fired: &mut impl FnMut(u32) -> bool) -> bool {
    let a = with_table(|t| act(t, p)).unwrap_or(Act::NoRoute);
    match a {
        Act::Wired(s, misrouted) => {
            disable(s);
            let this = crate::percpu::this();
            TAKEN.fetch_add(1, Ordering::Relaxed);
            TOP_ID.fetch_max(u64::from(p.id), Ordering::Relaxed);
            if this.index.load(Ordering::Relaxed) != 0 {
                TAKEN_OFF_BOOT.fetch_add(1, Ordering::Relaxed);
            }
            if misrouted {
                MISROUTED.fetch_add(1, Ordering::Relaxed);
            }
            fired(s)
        }
        Act::Kernel(s) => {
            disable(s);
            if let Some(&(_, handler)) = KERNEL_SOURCES.iter().find(|k| k.0 == s) {
                handler(s);
            }
            aim_and_unmask(s);
            false
        }
        Act::Ended => false,
        Act::NoRoute => {
            no_route(usize::from(p.hart), u32::from(p.id));
            false
        }
    }
}

/// Deliver the device messages a `claim` took on this hart, then do this hart's work in its file (open the identities it was asked to
/// open and aim their sources; close, drain and free the ones it was asked to retire). Each device source is masked at the APLIC and
/// handed to `fired` (whether an interrupt object took it). Under the lock. Returns whether any object was signalled.
pub fn deliver(ids: &IdSet, mut fired: impl FnMut(u32) -> bool) -> bool {
    let Some(file) = this_file() else {
        return false;
    };
    let Ok(hart) = u16::try_from(file) else {
        return false;
    };
    let mut any = false;
    for id in ids.iter().filter(|&id| id != IPI_ID) {
        let Ok(id) = u16::try_from(id) else { continue };
        any |= deliver_one(Place { hart, id }, &mut fired);
    }
    if work_flag(file).is_some_and(|f| f.swap(false, Ordering::AcqRel)) {
        let n = with_table(|t| t.ids.num_ids()).unwrap_or(0);
        for id in 2..=n {
            let p = Place { hart, id };
            match with_table(|t| t.ids.slot(p)).flatten() {
                Some(Slot::Opening { .. }) => {
                    open_here(u32::from(id));
                    let fresh = with_table(|t| {
                        let o = t.ids.opened(p)?;
                        if let Some(r) = o.retiring {
                            ask(usize::from(r.hart));
                        }
                        let (Route::Wired(s) | Route::Kernel(s)) = o.route else {
                            return None;
                        };
                        if let Some(a) = t.at.get_mut(s as usize) {
                            *a = Some(p);
                        }
                        aim(s, p);
                        // (a route that moved keeps its source's mask as it was; a new one, placed here by a leaving hart, is unmasked now)
                        o.retiring.is_none().then_some(s)
                    })
                    .flatten();
                    if let Some(s) = fresh {
                        aim_and_unmask(s);
                    }
                }
                Some(Slot::Retiring { moved, .. }) => {
                    if close_here(u32::from(id)) {
                        any |= deliver_one(p, &mut fired);
                    }
                    with_table(|t| t.ids.retired(p));
                    if moved {
                        MOVES.fetch_add(1, Ordering::Relaxed);
                    }
                }
                _ => {}
            }
        }
    }
    any
}
