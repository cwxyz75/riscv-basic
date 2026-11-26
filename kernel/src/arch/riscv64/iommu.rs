//! The platform IOMMU (IOMMU5; IOMMU-SPEC 2, 6.4, 7, 8.6, 11): required at boot, brought up with an **empty** device directory so
//! that every device id is denied for memory and messages alike (IO1), with no pass-through state ever written (IO2), and its faults
//! drained, counted and reported. The register sequences are `iommucore::driver`'s (host-tested against the simulated IOMMU in every
//! hand-over state); this file owns the register page, the frames, the lock, the clock and the console.
//!
//! * **Discovery** (`discover`, from `kmem::init` once the tree is parsed): `kcore::iommutree` finds the one `riscv,iommu` node, its
//!   sources and the PCI host behind it. A refusal is kept and raised by `init`, the first moment a panic can name it (2.5).
//! * **Bring-up** (`init`, after the interrupt controller and before any device window exists): the directory root, the command
//!   queue and the fault queue from boot-time frames that are never freed; `driver::bring_up` (hand-over, capabilities, `fctl`,
//!   `icvec`, the queues, `ddtp` once); then every cached context and translation is invalidated and fenced; then the boot line, and
//!   only then the device windows (`kmem::build_windows`, which asserts it).
//! * **Interrupts** (8.6): the fault-queue and command-queue sources are kernel-owned wired sources (`aia::KernelSource`), edge-
//!   triggered as the tree says. The fault handler runs under the object lock with interrupts off (`aia::deliver`), takes `IOMMU`,
//!   drains at most 64 records, and prints at most 16 lines a second **after** releasing it (7.1, 7.2). The command queue is polled
//!   (6.4); its interrupt only acknowledges.
//! * **Lock** `IOMMU` (class 11, between `KSTACK` and `FRAME`): the device-domain store (`kcore::iodomain`, which owns the directory
//!   and the command queue), the fault queue and the rate limit. Never held across a wait: a fence is polled with the lock taken only
//!   for each look.
//! * **Device domains** (IOMMU6; IOMMU-SPEC 4, 6, 7.4): the calls are `ksys::iodomain`'s, run over this machine (`Kenv`): the caller's
//!   handles under the object lock, the store under `IOMMU`, the frames under `FRAME`, the waits with no lock held. A destruction begun
//!   by a handle close or a teardown (`closed`, under the object lock) is completed by `drain` with no lock held, at the tail of the
//!   call or fault that began it and at the end of a teardown.

use alloc::vec::Vec;
use core::fmt::Write as _;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use iommucore::cmdq::{CmdQueue, Command};
use iommucore::ddt::{Ddt, Format, Levels};
use iommucore::driver::{self, FaultLine, FenceTimeoutLine, Plan, RateLimit};
use iommucore::faultq::{FaultQueue, FaultRecord};
use iommucore::iopt::{Iopt, PtMode};
use iommucore::regs::{self, Caps, Required};
use iommucore::{PhysMem64, Regs};
use kcore::iodomain::{Fence, IoDomains};
use kcore::iommutree::{Absent, Cause, Iommu};

/// The bound of every wait on the IOMMU (`IOMMU_FENCE_BOUND`, IOMMU-SPEC 6.4): 100 ms of the `time` counter.
const FENCE_BOUND_NS: u64 = 100_000_000;

/// What the tree said: the IOMMU, or why there is none usable (raised by `init`). Written once by `discover` at boot.
static mut TREE: Option<Result<Iommu, Refusal>> = None;

/// Why the boot stops (IOMMU-SPEC 2.5): the tree's refusals; the device's are `driver::Refusal`.
#[derive(Debug, Clone, Copy)]
enum Refusal {
    Absent(Absent),
    TooHigh,
    NotMappable,
}

impl core::fmt::Display for Refusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Refusal::Absent(a) => core::fmt::Display::fmt(a, f),
            Refusal::TooHigh => f.write_str("IOMMU: address map too high (the device tree names a physical address at or above IO_BASE)"),
            Refusal::NotMappable => f.write_str("no IOMMU: its register page overlaps RAM or lies beyond the direct map"),
        }
    }
}

fn tree() -> Option<&'static Iommu> {
    // SAFETY: written once by `discover` at boot, before any other hart or thread runs; read-only afterwards.
    unsafe { (*core::ptr::addr_of!(TREE)).as_ref() }.and_then(|r| r.as_ref().ok())
}

/// Read the IOMMU out of the parsed tree (`kmem::init`, before `aia::discover`, which is handed the IOMMU's sources). `ram` is every
/// range that is RAM or reserved.
pub fn discover(nodes: &[kcore::fdt::Node], ram: &[(u64, u64)]) {
    let r = match kcore::iommutree::discover(nodes) {
        Err(a) => Err(Refusal::Absent(a)),
        Ok(_)
            if kcore::iommutree::highest_address(nodes)
                .is_none_or(|top| top > kcore::iodomain::IO_BASE) =>
        {
            Err(Refusal::TooHigh)
        }
        Ok(i) => {
            let end = i.base.saturating_add(i.size);
            if end > abi::layout::DIRECT_MAP_PHYS_LIMIT
                || ram
                    .iter()
                    .any(|&(a, s)| i.base < a.saturating_add(s) && a < end)
            {
                Err(Refusal::NotMappable)
            } else {
                Ok(i)
            }
        }
    };
    // SAFETY: boot code, one hart, before anything reads it.
    unsafe { *core::ptr::addr_of_mut!(TREE) = Some(r) };
}

/// The IOMMU's sources the kernel owns (IOMMU-SPEC 8.6): the fault queue's and the command queue's. The performance-monitor and
/// page-request sources stay masked (no route is ever made for them).
pub fn kernel_sources() -> Vec<crate::aia::KernelSource> {
    let Some(t) = tree() else {
        return Vec::new();
    };
    let mut v = Vec::new();
    for (cause, handler) in [
        (Cause::FaultQueue, fault_interrupt as fn(u32) -> bool),
        (Cause::CommandQueue, command_interrupt as fn(u32) -> bool),
    ] {
        if let Some(s) = t.source(cause) {
            v.push(crate::aia::KernelSource {
                source: s.number,
                edge: s.trigger == 1 || s.trigger == 2,
                handler,
            });
        }
    }
    v
}

/// The register page, which the kernel maps and cuts from every device window (none without an IOMMU).
pub fn page() -> Option<u64> {
    tree().map(|t| t.base)
}

/// The register page through the direct map.
struct Mmio(u64);

impl Mmio {
    fn new() -> Option<Mmio> {
        tree().map(|t| Mmio(crate::kmem::direct_base() + t.base))
    }
}

impl Regs for Mmio {
    fn read32(&mut self, offset: usize) -> u32 {
        // SAFETY: the register page is direct-mapped device memory (`kmem::init` maps `page()`); `offset` is one of the IOMMU's
        // registers (`iommucore::regs`), inside the page and aligned to its width.
        unsafe { read_volatile((self.0 + offset as u64) as *const u32) }
    }
    fn write32(&mut self, offset: usize, value: u32) {
        // SAFETY: as `read32`.
        unsafe { write_volatile((self.0 + offset as u64) as *mut u32, value) }
    }
    fn read64(&mut self, offset: usize) -> u64 {
        // SAFETY: as `read32`, a 64-bit register.
        unsafe { read_volatile((self.0 + offset as u64) as *const u64) }
    }
    fn write64(&mut self, offset: usize, value: u64) {
        // SAFETY: as `read64`.
        unsafe { write_volatile((self.0 + offset as u64) as *mut u64, value) }
    }
}

/// Physical memory through the direct map, one aligned doubleword at a time (the IOMMU reads the directory and the tables while
/// the kernel writes them: single-copy-atomic accesses).
struct Direct;

impl Direct {
    fn word(pa: u64) -> &'static AtomicU64 {
        // SAFETY: every address the IOMMU structures name is a frame the kernel allocated for them (the directory, the queues, the
        // tables): RAM in the direct map, 8-byte aligned (the formats are doublewords); the frame outlives every access (it is freed
        // only after the fence that ends the IOMMU's use of it).
        unsafe { AtomicU64::from_ptr((crate::kmem::direct_base() + pa) as *mut u64) }
    }
}

impl PhysMem64 for Direct {
    fn read(&self, pa: u64) -> u64 {
        Direct::word(pa).load(Ordering::Acquire)
    }
    fn write(&self, pa: u64, value: u64) {
        Direct::word(pa).store(value, Ordering::Release);
    }
    fn fetch_or(&self, pa: u64, bits: u64) -> u64 {
        Direct::word(pa).fetch_or(bits, Ordering::AcqRel)
    }
    fn fetch_and(&self, pa: u64, bits: u64) -> u64 {
        Direct::word(pa).fetch_and(bits, Ordering::AcqRel)
    }
}

/// What `IOMMU` protects.
struct State {
    /// The domains, the directory and the command queue (every command is queued through it).
    store: IoDomains,
    fq: FaultQueue,
    rate: RateLimit,
    /// The fault records the rate limit admitted and nobody has printed yet: printed one at a time with the lock released
    /// (`fault_interrupt`), kept here rather than on the interrupt stack.
    lines: [Option<FaultRecord>; driver::LINES_PER_SECOND as usize],
}

static STATE: klock::Spin<Option<State>, crate::bkl::Machine> =
    klock::Spin::new(klock::Class::Iommu, 0, None);

/// The directory is written and every device is denied (step 5 done): the device windows may be built.
static ON: AtomicBool = AtomicBool::new(false);
/// A destruction is begun and not ended (`IoDomains::unfinished`), as last seen under the IOMMU lock: written only while it is
/// held, so a `drain` that reads it after its own `closed` sees true until every destruction has ended.
static UNFINISHED: AtomicBool = AtomicBool::new(false);
/// The counters of IOMMU-SPEC 7.3 this slice has (`debug_stat` 150 to 157, SYSCALLS.md).
static FAULTS: AtomicU64 = AtomicU64::new(0);
static DROPPED: AtomicU64 = AtomicU64::new(0);
static OVERFLOWS: AtomicU64 = AtomicU64::new(0);
static LAST_DID: AtomicU64 = AtomicU64::new(0);
static LAST_IOVA: AtomicU64 = AtomicU64::new(0);
static LAST_CAUSE: AtomicU64 = AtomicU64::new(0);
static FENCE_TIMEOUTS: AtomicU64 = AtomicU64::new(0);

/// Is the IOMMU on, every device denied (IOMMU-SPEC 2.4 step 5)?
pub fn is_on() -> bool {
    ON.load(Ordering::Acquire)
}

/// A zeroed frame for an IOMMU structure, by physical address: boot-time ones are never freed.
fn zeroed_frame() -> Option<u64> {
    let pa = crate::kmem::frame_alloc()? * 4096;
    // SAFETY: a frame the database just gave us (reference count 1, nobody else's), direct-mapped RAM, one page.
    unsafe { core::ptr::write_bytes((crate::kmem::direct_base() + pa) as *mut u8, 0, 4096) };
    Some(pa)
}

/// The IOMMU's bring-up (IOMMU-SPEC 2.4). Panics with the fixed lines of 2.5 when there is no usable IOMMU. Boot only, on the boot
/// hart, after `aia::init` (the sources are routed) and before any device window exists.
#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB (the directory, queue and plan values, the boot line's formatting) in boot code called once from `kmain` on the boot stack; the boot-time stack budget check bounds it
pub fn init() {
    // SAFETY: as `tree`.
    let found = unsafe { (*core::ptr::addr_of!(TREE)).as_ref() };
    let t = match found {
        Some(Ok(t)) => t,
        Some(Err(r)) => panic!("{r}"),
        None => panic!("no IOMMU: the device tree could not be read"),
    };
    if t.device_ids > 1 << kcore_device_id_bits() {
        panic!("no IOMMU: the tree names device ids the directory cannot cover");
    }
    // step 4's registration (IOMMU-SPEC 2.4, 8.6): `aia::init` installed both kernel sources, or fault reporting would be dead
    for cause in [Cause::FaultQueue, Cause::CommandQueue] {
        let s = t.source(cause).map(|s| s.number);
        // (the route table is under the object lock with interrupts off, as `aia::init` reaches it at the same point of boot)
        if crate::irq::without(|| s.and_then(crate::aia::route_of)).is_none() {
            panic!("no IOMMU: its {cause:?} interrupt (source {s:?}) could not be registered with the interrupt controller");
        }
    }
    let (Some(root), Some(cqf), Some(fqf)) = (zeroed_frame(), zeroed_frame(), zeroed_frame())
    else {
        panic!("no IOMMU: no frames for its directory and queues");
    };
    // the quarantine table (Q-IOMMU-MODEL 6): one empty second-stage table, 16 KiB aligned, that every device a destroyed domain
    // leaves behind translates through; never freed
    let Some(qpa) = crate::kmem::frames()
        .mm
        .db
        .alloc_contiguous(kcore::iodomain::ROOT_FRAMES as usize)
        .map(|p| p * 4096)
    else {
        panic!("no IOMMU: no frames for its quarantine table");
    };
    // SAFETY: an order-2 block the database just gave us (nobody else's), direct-mapped RAM, ROOT_FRAMES pages.
    unsafe {
        core::ptr::write_bytes(
            (crate::kmem::direct_base() + qpa) as *mut u8,
            0,
            (kcore::iodomain::ROOT_FRAMES * 4096) as usize,
        )
    };
    let mem = Direct;
    let ddt = Ddt::new(&mem, root, Levels::Three, Format::Extended)
        .unwrap_or_else(|_| panic!("no IOMMU: the directory root cannot be encoded"));
    let cq = CmdQueue::new(cqf, driver::CQ_ENTRIES)
        .unwrap_or_else(|_| panic!("no IOMMU: the command queue cannot be encoded"));
    let fq = FaultQueue::new(fqf, driver::FQ_RECORDS)
        .unwrap_or_else(|_| panic!("no IOMMU: the fault queue cannot be encoded"));
    let quarantine = Iopt::new(&mem, qpa, PtMode::Sv39x4)
        .unwrap_or_else(|_| panic!("no IOMMU: the quarantine table cannot be encoded"));
    let top = crate::kmem::usable_ranges()
        .iter()
        .map(|&(first, count)| (first + count as u64) * 4096)
        .max()
        .unwrap_or(0);
    let cycles = crate::timer::cycles_from_ns(FENCE_BOUND_NS);
    let mut r = Mmio::new().unwrap_or_else(|| panic!("no IOMMU"));
    let plan = Plan {
        ddt: &ddt,
        cq: &cq,
        fq: &fq,
        required: Required::covering(top),
        // the wired line of each cause is its position in the tree's `interrupts` (`kcore::iommutree::Cause`)
        icvec: regs::icvec(
            Cause::CommandQueue as u8,
            Cause::FaultQueue as u8,
            Cause::PerfMonitor as u8,
            Cause::PageRequest as u8,
        ),
    };
    let up = match driver::bring_up(&mut r, &plan, &mut crate::timer::now, cycles) {
        Ok(up) => up,
        Err(e) => panic!("{e}"),
    };
    #[cfg(feature = "fault-iommu-bare")]
    driver::write_bare(&mut r); // the negative control: pass-through, the state IO2 forbids
                                // (the mapping index's priorities are seeded from the clock and the entropy pool: its depth stays logarithmic whatever a driver keeps)
    let mut pool = [0u8; 8];
    let _ = crate::entropy::fill(&mut pool); // (not yet seeded: the clock alone)
    let seed = crate::timer::now() ^ u64::from_le_bytes(pool);
    let Some(store) = IoDomains::new(ddt, cq, &quarantine, seed) else {
        panic!("no IOMMU: no memory for the domain store");
    };
    *STATE.lock() = Some(State {
        store,
        fq,
        rate: RateLimit::new(),
        lines: [None; driver::LINES_PER_SECOND as usize],
    });
    // whatever a previous owner left cached is dropped before anything relies on the empty directory
    if fence(&driver::invalidate_everything(), None).is_err() {
        panic!("IOMMU: refused cqcsr (the boot invalidation did not complete)");
    }
    let read_back = regs::decode_ddtp(r.read64(regs::DDTP)).mode;
    let caps: Caps = up.caps;
    let (major, minor) = caps.version();
    let mut line = alloc::format!(
        "kernel: iommu version={major}.{minor} mode={} caps={:#x}",
        driver::mode_name(read_back),
        caps.0
    );
    if up.handover != regs::DdtpMode::Off {
        let _ = write!(line, " handover={}", driver::mode_name(up.handover));
    }
    let _ = writeln!(crate::console::Console, "{line}");
    ON.store(true, Ordering::Release);
}

/// The widest device id the directory covers (3 levels, extended contexts: 24 bits).
fn kcore_device_id_bits() -> u32 {
    iommucore::ddt::device_id_bits(Levels::Three, Format::Extended)
}

/// Queue the kernel's own `cmds` (and a fence) through the store and wait for them with the lock released between looks, bounded by
/// `IOMMU_FENCE_BOUND` (IOMMU-SPEC 6.4); a full queue is queued again within the same bound. A fence that does not complete is counted
/// and reported (`device` names the device it was for); the caller leaks what it guarded.
fn fence(cmds: &[Command], device: Option<u32>) -> Result<(), ()> {
    let queue = || {
        let mut g = STATE.lock();
        match (g.as_mut(), Mmio::new()) {
            (Some(s), Some(mut r)) => Some(s.store.queue(&Direct, &mut r, cmds)),
            _ => None,
        }
    };
    let Some(mut ticket) = queue() else {
        return Err(());
    };
    let t0 = crate::timer::now();
    let bound = crate::timer::cycles_from_ns(FENCE_BOUND_NS);
    loop {
        // (the clock is read before the look, so the last look is taken after the bound passed: a thread held off between a look
        // and the clock does not time out a fence that completed meanwhile)
        let expired = crate::timer::now().wrapping_sub(t0) > bound;
        let state = {
            let mut g = STATE.lock();
            let (Some(s), Some(mut r)) = (g.as_mut(), Mmio::new()) else {
                return Err(());
            };
            s.store.poll(&mut r, ticket)
        };
        match state {
            Fence::Done => return Ok(()),
            Fence::Unqueued if !expired => {
                if let Some(t) = queue() {
                    ticket = t;
                }
            }
            Fence::Waiting if !expired => core::hint::spin_loop(),
            _ => {
                timed_out(device);
                return Err(());
            }
        }
    }
}

/// A fence that did not complete within the bound, or a queue that failed (6.4): counted, and one line.
fn timed_out(device: Option<u32>) {
    FENCE_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
    let _ = writeln!(crate::console::Console, "{}", FenceTimeoutLine(device));
}

/// The fault-queue interrupt (IOMMU-SPEC 7.1, 7.2), under the object lock with interrupts off: drain at most 64 records under
/// `IOMMU`, count them, and print the lines the rate limit admits once the lock is released. Returns whether records were left.
fn fault_interrupt(_source: u32) -> bool {
    let mut raised = false;
    let more = {
        let mut g = STATE.lock();
        let (Some(s), Some(mut r)) = (g.as_mut(), Mmio::new()) else {
            return false;
        };
        let second = crate::timer::cycles_from_ns(1_000_000_000);
        let now = crate::timer::now();
        let State {
            store,
            fq,
            rate,
            lines,
        } = s;
        let d = driver::drain_faults(&mut r, &Direct, fq, driver::DRAIN_PER_INTERRUPT, |rec| {
            FAULTS.fetch_add(1, Ordering::Relaxed);
            // 7.4: the record goes to the ring of the device's domain, if it has a live one, which raises DEVICE_FAULT on its object
            // (the object lock is this handler's: the signal and the ring change in one section, as `iodomain_fault_read` reads them)
            let out = store.fault(rec, now, second);
            if let Some(obj) = out.tag.and_then(kcore::object::ObjId::from_words) {
                raised |=
                    crate::user::with_objs(|objs| kcore::ipc::iodomain_set_fault(objs, obj, true));
            }
            LAST_DID.store(u64::from(rec.device_id), Ordering::Relaxed);
            LAST_IOVA.store(rec.iotval, Ordering::Relaxed);
            LAST_CAUSE.store(
                u64::from(rec.cause) | (u64::from(rec.ttype.code()) << 32),
                Ordering::Relaxed,
            );
            // (a line admitted while the buffer is still full of another hart's unprinted ones is counted as dropped)
            match lines.iter_mut().find(|l| l.is_none()) {
                Some(slot) if rate.admit(now, second) => *slot = Some(rec),
                _ => {
                    DROPPED.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
        if d.overflow {
            OVERFLOWS.fetch_add(1, Ordering::Relaxed);
        }
        d.more
    };
    // (IOMMU-SPEC 11.1: the console line is printed with `IOMMU` released, from a copy taken under it, one line at a time)
    loop {
        let rec = STATE
            .lock()
            .as_mut()
            .and_then(|s| s.lines.iter_mut().find_map(Option::take));
        let Some(rec) = rec else { break };
        let _ = writeln!(crate::console::Console, "{}", FaultLine(&rec));
    }
    if raised {
        crate::user::wake_signal_waiters(); // (a waiter for DEVICE_FAULT: the lock is this handler's, as for a wired source's object)
    }
    more
}

/// The command-queue interrupt: the queue is polled (IOMMU-SPEC 6.4), so this only acknowledges `ipsr.cip` (an error stays in
/// `cqcsr` for the fence that is waiting to see).
fn command_interrupt(_source: u32) -> bool {
    let g = STATE.lock();
    if g.is_some() {
        if let Some(mut r) = Mmio::new() {
            r.write32(regs::IPSR, regs::IPSR_CIP);
        }
    }
    false
}

/// The frames the device directory holds (its root and the pages below). They are the kernel's for good once a device id has had a
/// domain: a destroyed domain's device keeps its quarantine context (IOMMU-SPEC 6.3), so the leak checks count them as accounted.
pub fn directory_frames() -> usize {
    let mut st = STATE.lock();
    let Some(s) = st.as_mut() else { return 0 };
    s.store.directory().pages(&Direct) as usize
}

/// The counters (`debug_stat`, SYSCALLS.md): 150 faults taken, 151 fault lines dropped by the rate limit, 152 fault-queue overflows,
/// 153 the last fault's device id, 154 its address, 155 its cause (bits 11:0) and transaction type (bits 37:32), 156 commands
/// issued, 157 fence timeouts; IOMMU6: 158 frames leaked by fence timeouts (mapped pages and table frames), 159 live domains, 160
/// I/O-mapped pages, 161 mapping records leaked with their holds.
pub fn stat(which: u64) -> u64 {
    let c = match which {
        150 => &FAULTS,
        151 => &DROPPED,
        152 => &OVERFLOWS,
        153 => &LAST_DID,
        154 => &LAST_IOVA,
        155 => &LAST_CAUSE,
        157 => &FENCE_TIMEOUTS,
        // (156: every command the store queued, the calls' and the kernel's own, with their fences: IOMMU6)
        156 | 158..=161 | 147 => {
            let st = STATE.lock().as_ref().map(|s| s.store.stats());
            let Some(st) = st else { return 0 };
            return match which {
                156 => st.commands,
                // (IOMMU7: the stray bits the store took at revoke and release, with the ones `aia` took at acknowledgement)
                147 => st.strays.saturating_add(crate::aia::stat(147)),
                158 => st.pages_leaked.saturating_add(st.tables_leaked),
                159 => st.domains,
                160 => st.io_pages,
                _ => st.holds_leaked,
            };
        }
        _ => return 0,
    };
    c.load(Ordering::Relaxed)
}

// ---- device domains (IOMMU6) -----------------------------------------------------------------------------------------------

/// This machine under the device-domain calls (`ksys::iodomain::IoEnv`): each lock a closure, the clock, the console.
struct Kenv;

impl Kenv {
    /// `IOMMU` and the register page. Present after `init` (no call reaches here before user mode, which starts after it).
    #[allow(clippy::expect_used)] // INVARIANT: `init` filled the state and the tree names the register page before any process runs
    fn locked<T>(f: impl FnOnce(&mut IoDomains, &mut Mmio) -> T) -> T {
        let mut g = STATE.lock();
        let s = g.as_mut().expect("the IOMMU is up before user mode");
        let mut r = Mmio::new().expect("the IOMMU's register page");
        f(&mut s.store, &mut r)
    }
}

impl ksys::iodomain::IoEnv for Kenv {
    type M = crate::kmem::DirectMap;
    type P = Direct;
    type R = Mmio;
    fn obj<T>(&mut self, f: impl FnOnce(ksys::iodomain::IoObj<'_, Self::M>) -> T) -> Option<T> {
        crate::user::with_caller(|objs, handles, job| {
            let k = crate::kmem::get();
            f(ksys::iodomain::IoObj {
                objs,
                handles,
                job,
                mm: k.mm,
                vmos: k.vmos,
                jobs: k.jobs,
            })
        })
    }
    fn obj_iommu<T>(
        &mut self,
        f: impl FnOnce(
            &mut kcore::ipc::Kobjs,
            &kcore::handle::HandleTable,
            &mut IoDomains,
            &Direct,
            &mut Mmio,
        ) -> T,
    ) -> Option<T> {
        crate::user::with_caller(|objs, handles, _| {
            Kenv::locked(|s, r| f(objs, handles, s, &Direct, r))
        })
    }
    fn mem<T>(
        &mut self,
        f: impl FnOnce(
            &mut kcore::Mm<Self::M>,
            &mut kcore::aspace::Vmos,
            &mut kcore::job::Jobs,
            &Direct,
        ) -> T,
    ) -> T {
        let k = crate::kmem::mem();
        f(k.mm, k.vmos, k.jobs, &Direct)
    }
    fn frame<T>(
        &mut self,
        f: impl FnOnce(&mut kcore::frame::FrameDb, &mut Self::M, &Direct) -> T,
    ) -> T {
        let k = crate::kmem::frames();
        let kcore::Mm { db, mem } = k.mm;
        f(db, mem, &Direct)
    }
    fn iommu<T>(
        &mut self,
        f: impl FnOnce(&mut IoDomains, &Direct, &mut Mmio, &mut kcore::frame::FrameDb) -> T,
    ) -> T {
        Kenv::locked(|s, r| {
            // (`FRAME` inside `IOMMU`: 11.2)
            let k = crate::kmem::frames();
            f(s, &Direct, r, &mut k.mm.db)
        })
    }
    fn device_ok(&self, device_id: u64) -> bool {
        tree().is_some_and(|t| device_id < u64::from(t.device_ids))
    }
    fn now(&self) -> u64 {
        crate::timer::now()
    }
    fn fence_bound(&self) -> u64 {
        crate::timer::cycles_from_ns(FENCE_BOUND_NS)
    }
    fn relax(&mut self) {
        // (no lock is held: interrupts are taken and a switch may happen, as between the slices of any long call)
        if crate::bkl::holds() {
            core::hint::spin_loop();
        } else {
            crate::irq::window();
        }
    }
    fn fence_timeout(&mut self, device: Option<u32>) {
        timed_out(device);
    }
    fn irq<T>(&mut self, f: impl FnOnce(ksys::iodomain::IrqCtx<'_>, &Direct) -> T) -> Option<T> {
        crate::user::with_caller_opt(|objs, caller| {
            // (`JOBS` inside `OBJ`, as object quotas are: IOMMU-SPEC 11.2)
            let j = crate::kmem::jobs();
            crate::aia::with_irq(|ids, vecs, harts| {
                f(
                    ksys::iodomain::IrqCtx {
                        objs,
                        caller,
                        jobs: j.jobs,
                        ids,
                        vecs,
                        harts,
                    },
                    &Direct,
                )
            })
        })
    }
    fn irq_iommu<T>(
        &mut self,
        f: impl FnOnce(
            ksys::iodomain::IrqCtx<'_>,
            &mut IoDomains,
            &Direct,
            &mut Mmio,
            &mut kcore::frame::FrameDb,
        ) -> T,
    ) -> Option<T> {
        crate::user::with_caller_opt(|objs, caller| {
            let j = crate::kmem::jobs();
            crate::aia::with_irq(|ids, vecs, harts| {
                // (`OBJ`, `JOBS`, then `IOMMU` and `FRAME` inside it: 11.1)
                Kenv::locked(|s, r| {
                    let k = crate::kmem::frames();
                    f(
                        ksys::iodomain::IrqCtx {
                            objs,
                            caller,
                            jobs: j.jobs,
                            ids,
                            vecs,
                            harts,
                        },
                        s,
                        &Direct,
                        r,
                        &mut k.mm.db,
                    )
                })
            })
        })
    }
}

/// The device-domain calls (IOMMU-SPEC 4, 7.4): `iodomain_create`, `iodomain_map`, `iodomain_unmap`, `iodomain_destroy`,
/// `iodomain_fault_read` and `vmo_create_dma_in`, from the syscall path with no lock held. `None` for any other call.
pub fn call(n: usize, a: [u64; 6]) -> Option<ksys::Reply> {
    use abi::syscall as nr;
    use ksys::iodomain as io;
    let e = &mut Kenv;
    let r = match n {
        nr::IODOMAIN_CREATE => io::iodomain_create(e, a),
        nr::IODOMAIN_MAP => io::iodomain_map(e, a),
        nr::IODOMAIN_UNMAP => io::iodomain_unmap(e, a),
        nr::IODOMAIN_DESTROY => io::iodomain_destroy(e, a),
        nr::IODOMAIN_FAULT_READ => io::iodomain_fault_read(e, a),
        nr::VMO_CREATE_DMA_IN => io::vmo_create_dma_in(e, a),
        nr::IODOMAIN_GRANT_VECTOR => io::iodomain_grant_vector(e, a),
        // (IOMMU7: a vector's acknowledgement and placement; a wired source's are the kernel's own path, `None` here)
        nr::INTERRUPT_ACK => io::interrupt_ack_vector(e, a)?,
        nr::INTERRUPT_DELIVER_HERE => io::interrupt_deliver_here_vector(e, a)?,
        _ => return None,
    };
    Some(r)
}

/// A message on vector identity `p` of this hart's file (the interrupt path, under the object lock): the notice of IOMMU-SPEC 8.5 on
/// the vector's file, with atomic operations and no other lock. Whether its object fired.
pub fn vector_notice(p: kcore::msiid::Place) -> bool {
    crate::user::with_objs(|objs| {
        crate::aia::with_irq(|ids, vecs, _| vecs.notice(objs, ids, &Direct, p))
            .flatten()
            .unwrap_or(false)
    })
}

/// The last handle of a vector object went (its cleanup, under the object lock): revoke steps 1 to 3 under `IOMMU` (IOMMU-SPEC 8.5);
/// `drain` completes it with no lock held.
pub fn vector_closed(v: kcore::ipc::VectorRef) {
    crate::aia::with_irq(|_, vecs, _| {
        let mut g = STATE.lock();
        if let (Some(s), Some(mut r)) = (g.as_mut(), Mmio::new()) {
            ksys::iodomain::vector_closed(vecs, &mut s.store, &Direct, &mut r, v);
            UNFINISHED.store(s.store.unfinished(), Ordering::Release);
        }
    });
    crate::aia::VECTORS_DUE.store(true, Ordering::Release);
}

/// The first `object_wait` on an interrupt object (IOMMU-SPEC 8.5, placement): a vector is moved to the calling hart once. No lock
/// held; one look when no vector waits for its placement.
pub fn first_wait(h: u64) {
    if crate::bkl::holds() {
        return;
    }
    ksys::iodomain::first_wait_vector(&mut Kenv, abi::Handle(h));
}

/// Finish the vector moves a hart's evacuation began (`hart_set_online`, with no lock held): the entries rewritten and fenced, the old
/// identities retired (IOMMU-SPEC 8.5, hart offline).
pub fn settle_moves() {
    if crate::bkl::holds() {
        return;
    }
    ksys::iodomain::settle_moves(&mut Kenv);
}

/// Where the vector of interrupt handle `h` (the caller's) sends its notices: (interrupt file << 16) | identity of the fenced entry's
/// target, `None` for anything else (`debug_stat` 148). Under the object lock.
pub fn vector_route(
    objs: &kcore::ipc::Kobjs,
    handles: &kcore::handle::HandleTable,
    h: u64,
) -> Option<u64> {
    let (_, v) =
        kcore::ipc::interrupt_kind_of(objs, handles, abi::Handle(h), abi::rights::Rights::WAIT)
            .ok()?;
    crate::aia::vector_route_of(v?)
}

/// The last handle of a device domain went (the object's cleanup, under the object lock): 6.3 step 1, which allocates nothing and does
/// not wait; `drain` completes it.
pub fn closed(domain: u64) {
    let mut g = STATE.lock();
    if let (Some(s), Some(mut r)) = (g.as_mut(), Mmio::new()) {
        ksys::iodomain::iodomain_closed(&mut s.store, &Direct, &mut r, domain);
        UNFINISHED.store(s.store.unfinished(), Ordering::Release);
    }
}

/// Complete every destruction begun under another lock (6.3 steps 2 to 4), with no lock held, and wait for those other harts are
/// completing: the tail of every call and fault, and the end of a teardown, so a close returns only when its domain is gone (6.3).
/// One load when nothing is pending; a caller that holds the object lock leaves it to the next.
pub fn drain() {
    if crate::bkl::holds() {
        return;
    }
    let vectors = crate::aia::VECTORS_DUE.swap(false, Ordering::AcqRel);
    if !UNFINISHED.load(Ordering::Acquire) && !vectors {
        return;
    }
    ksys::iodomain::settle_destructions(&mut Kenv);
    // (IOMMU7: the revokes a close began, the moves a hart's evacuation began, and the charges of vectors whose identities are free)
    ksys::iodomain::settle_moves(&mut Kenv);
    ksys::iodomain::settle_vectors(&mut Kenv);
    let g = STATE.lock();
    if let Some(s) = g.as_ref() {
        UNFINISHED.store(s.store.unfinished(), Ordering::Release);
    }
}

/// Directory and table frames for the self-test: zeroed on the way in; on the way out kept off the free lists until the fence
/// that ends the IOMMU's use of them (`iommucore::TableFrames`), then given back.
#[cfg(not(feature = "release"))]
struct TestFrames {
    deferred: Vec<u64>,
}

#[cfg(not(feature = "release"))]
impl iommucore::TableFrames for TestFrames {
    fn alloc(&mut self) -> Option<u64> {
        zeroed_frame()
    }
    fn free(&mut self, pa: u64) {
        self.deferred.push(pa);
    }
}

/// The device id the self-test borrows: beyond every id the PCI host's map gives (0 to 0xffff), so no device can issue it, and
/// inside the directory's 24 bits. (Its fault lines print as `dev=00:00.0`: the line's bus:device.function shows 16 bits.)
#[cfg(not(feature = "release"))]
const SELFTEST_DID: u32 = 0x10_0000;

/// `iommu_translation_matches_the_tables` (IOMMU5; A11): a scratch second-stage table and a context for a device id no device has,
/// then QEMU's own walker, through the debug translation interface, must agree with `iommucore::iopt` for mapped, read-only and
/// unmapped addresses, read and write. Absent interface: a counted skip, said on the console (the boot script refuses it).
#[cfg(not(feature = "release"))]
#[allow(clippy::large_stack_frames)] // KT13: audited, a test-only boot self-test (the tables, the probes and their formatted errors), run once from the kernel test list on the boot stack; the boot-time stack budget check bounds it
pub fn selftest() -> Result<(), alloc::string::String> {
    use alloc::format;
    use iommucore::ddt::DeviceContext;
    use iommucore::iopt::{Access, Iopt, Op, PtMode};
    let mut r = Mmio::new().ok_or("no IOMMU")?;
    if !Caps(r.read64(regs::CAPABILITIES)).dbg() {
        let _ = writeln!(
            crate::console::Console,
            "kernel: iommu self-test skipped: the debug translation interface is absent"
        );
        return Ok(());
    }
    let mem = Direct;
    let mut frames = TestFrames {
        deferred: Vec::new(),
    };
    // two data frames and the table's root (16 KiB, aligned); on a failure here, what was taken goes back (nothing has seen it)
    let rw = zeroed_frame().ok_or("data frames")?;
    let Some(ro) = zeroed_frame() else {
        crate::kmem::frame_free_sole(rw / 4096);
        return Err("data frames".into());
    };
    let give_back_root = |root: u64| {
        let k = crate::kmem::frames();
        for i in 0..4 {
            let _ = k.mm.db.dec(root / 4096 + i);
        }
    };
    let Some(root_pfn) = crate::kmem::frames().mm.db.alloc_contiguous(4) else {
        crate::kmem::frame_free_sole(rw / 4096);
        crate::kmem::frame_free_sole(ro / 4096);
        return Err("frames for the table root".into());
    };
    let root = root_pfn * 4096;
    let mut iopt = match Iopt::new(&mem, root, PtMode::Sv39x4) {
        Ok(t) => t,
        Err(e) => {
            give_back_root(root);
            crate::kmem::frame_free_sole(rw / 4096);
            crate::kmem::frame_free_sole(ro / 4096);
            return Err(format!("Iopt::new: {e:?}"));
        }
    };
    let base = kcore::iodomain::IO_BASE + (1 << 30);
    let soft_id = 0xfffe; // (no domain exists yet; this id is the self-test's, and every cached entry of it is dropped at the end)
    let mut result = (|| -> Result<(), alloc::string::String> {
        iopt.map(&mem, &mut frames, base, rw, Access::ReadWrite)
            .map_err(|e| format!("map: {e:?}"))?;
        iopt.map(&mem, &mut frames, base + 0x20_0000, ro, Access::Read)
            .map_err(|e| format!("map: {e:?}"))?;
        let ctx = DeviceContext::translate(iopt.context_root(soft_id).ok_or("context root")?, None);
        {
            let mut g = STATE.lock();
            let s = g.as_mut().ok_or("no state")?;
            s.store
                .directory()
                .set(&mem, &mut frames, SELFTEST_DID, &ctx)
                .map_err(|e| format!("ddt set: {e:?}"))?;
        }
        fence(
            &[
                Command::iodir_inval_ddt(Some(SELFTEST_DID)),
                Command::fence(),
            ],
            Some(SELFTEST_DID),
        )
        .map_err(|()| "the fence after the context did not complete")?;
        let probes = [
            (base, Op::Read),
            (base + 0x123, Op::Write),
            (base + 0x20_0000, Op::Read),
            (base + 0x20_0000, Op::Write),
            (base + 0x40_0000, Op::Read),
            (base + 0x1000, Op::Write),
            (0x1000, Op::Read), // (below every window: unmapped)
        ];
        let bound = crate::timer::cycles_from_ns(FENCE_BOUND_NS);
        for (iova, op) in probes {
            let want = iopt.translate(&mem, iova, op).ok().map(|pa| pa & !0xfff);
            // (the request is written under the IOMMU's lock and waited for without it: nothing else uses the debug interface,
            // so its response is this request's)
            {
                let _g = STATE.lock();
                r.write64(regs::TR_REQ_IOVA, iova & !0xfff);
                r.write64(
                    regs::TR_REQ_CTL,
                    regs::tr_req_ctl(SELFTEST_DID, op == Op::Write),
                );
            }
            let t0 = crate::timer::now();
            while regs::tr_busy(r.read64(regs::TR_REQ_CTL)) {
                if crate::timer::now().wrapping_sub(t0) > bound {
                    return Err(format!(
                        "the debug translation of {iova:#x} never completed"
                    ));
                }
            }
            let raw = r.read64(regs::TR_RESPONSE);
            let got = regs::decode_tr_response(raw);
            if got != want {
                return Err(format!(
                    "{iova:#x} {op:?}: the IOMMU says {got:x?} (tr_response {raw:#x}), the tables say {want:x?}"
                ));
            }
        }
        Ok(())
    })();
    // the context goes, then everything of the self-test's soft-context id, fenced, before any frame is given back
    {
        let mut g = STATE.lock();
        if let Some(s) = g.as_mut() {
            s.store.directory().clear(&mem, &mut frames, SELFTEST_DID);
        }
    }
    let fenced = fence(
        &[
            Command::iodir_inval_ddt(Some(SELFTEST_DID)),
            Command::iotinval_gvma(soft_id as u16, None),
            Command::fence(),
        ],
        Some(SELFTEST_DID),
    );
    if fenced.is_err() {
        // IO8: what the fence guarded is leaked, never reused
        return Err(
            "the fence after the clear did not complete: the self-test's frames are leaked".into(),
        );
    }
    give_back_root(iopt.destroy(&mem, &mut frames));
    for pa in frames.deferred.drain(..).chain([rw, ro]) {
        crate::kmem::frame_free_sole(pa / 4096);
    }
    if result.is_ok() && FENCE_TIMEOUTS.load(Ordering::Relaxed) != 0 {
        result = Err("a fence timed out".into());
    }
    result
}
