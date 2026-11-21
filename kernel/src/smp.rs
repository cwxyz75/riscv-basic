//! Secondary harts (S5a, spec section 3): discovery through the SBI hart-state-management
//! extension, the entry code they start in, and the IPI. In S5a the secondaries only idle: each
//! takes its own timer interrupts and inter-hart interrupts and counts them; the scheduler and every
//! piece of shared kernel state stay on the boot hart (the big kernel lock arrives in S5b).

use crate::arch::sbi::*;
use crate::console::Console;
use crate::percpu::{self, IPI_RESCHED, MAX_HARTS};
use crate::{irq, kmem, timer};
use core::fmt::Write;
use core::sync::atomic::{AtomicUsize, Ordering};

/// Per-hart boot stacks: 32 KiB each, like every kernel thread, with a canary at the bottom
/// (`stack_canaries_intact` checks it).
pub const CANARY: u64 = 0xdead_beef_cafe_f00d;
const STACK_BYTES: usize = 32 * 1024;
#[repr(C, align(16))]
struct BootStacks([[u8; STACK_BYTES]; MAX_HARTS]);
static mut BOOT_STACKS: BootStacks = BootStacks([[0; STACK_BYTES]; MAX_HARTS]);
/// The top of each hart's boot stack (virtual addresses), read by the entry assembly.
#[no_mangle]
static mut SECONDARY_STACK_TOPS: [usize; MAX_HARTS] = [0; MAX_HARTS];

/// Harts found and started (including the boot hart).
static STARTED: AtomicUsize = AtomicUsize::new(1);

/// What a secondary hart does once it has a stack and the early table (Rust from here).
#[no_mangle]
extern "C" fn secondary_main(hart_id: usize, index: usize) -> ! {
    percpu::bind(index, hart_id);
    let tp = crate::arch::thread_pointer();
    percpu::this().tp_seen.store(tp, Ordering::Relaxed);
    crate::trap::init(); // the trap vector
                         // FPU and vector unit off, like the boot hart (thread::init), so any use traps.
                         // SAFETY: a secondary hart's own start, before any user thread has run on it.
    unsafe { crate::arch::lazy_units_off() };
    // Leave the early table for the kernel's own.
    let token = kmem::KERNEL_ASPACE.load(Ordering::SeqCst);
    // SAFETY: the kernel table maps everything this code runs from and touches (the image, the
    // direct map, the devices in use); installing it drops the early table's translations.
    unsafe { crate::arch::aspace_install(token) };
    timer::start();
    irq::enable_timer_interrupt();
    crate::fp::check_hart(); // (PL2: the same vector register length as the boot hart)
    crate::fp::enable_cache_block_ops_or_stop(); // (PL3)
    crate::aia::init_hart(); // (the inter-hart interrupt is a message in this hart's IMSIC file)
    percpu::this().online.store(true, Ordering::Release);
    // Parked until the boot hart has set up the scheduler (interrupts on, touching nothing
    // shared), then this hart joins it for good.
    irq::enable();
    timer::program(Some(timer::now_ns() + 1_000_000)); // (the poll for the scheduler: see the timer branch of the trap handler)
    loop {
        if crate::thread::scheduler_exists() {
            crate::thread::join_scheduler();
        }
        crate::arch::cpu_idle();
    }
}

/// The inter-hart interrupt handler (called from the trap handler on any hart, after the IMSIC message was claimed): take the reasons, count.
pub fn ipi_interrupt() -> u32 {
    let me = percpu::this();
    // take the reasons first, then count: whoever sees the count move sees the bits already gone
    let reasons = me.pending.swap(0, Ordering::SeqCst);
    if reasons & percpu::IPI_WORK != 0 {
        do_work(me);
    }
    if reasons & percpu::IPI_RESCHED != 0 {
        me.resched_ipis.fetch_add(1, Ordering::SeqCst);
    }
    me.ipis.fetch_add(1, Ordering::SeqCst);
    reasons
}

/// Send `reason` to hart `index` (a message to its IMSIC file).
pub fn send_ipi(index: usize, reason: u32) {
    crate::chaos::point(crate::chaos::Point::BeforeIpi);
    let target = percpu::hart(index);
    target.pending.fetch_or(reason, Ordering::SeqCst);
    crate::aia::send_ipi(target.hart_id.load(Ordering::Relaxed));
}

/// Find the other harts and start them. Needs `kmem::init` (the kernel table) and the heap.
/// Returns how many harts run (the boot hart included).
#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
pub fn start_secondaries(boot_hart_id: usize) -> usize {
    // SAFETY: only this function writes the table, once, before any secondary starts; the stacks
    // are private statics.
    unsafe {
        let stacks = core::ptr::addr_of_mut!(BOOT_STACKS) as usize;
        for i in 0..MAX_HARTS {
            (*core::ptr::addr_of_mut!(SECONDARY_STACK_TOPS))[i] = stacks + (i + 1) * STACK_BYTES;
            *((stacks + i * STACK_BYTES) as *mut u64) = CANARY;
        }
    }
    let entry_phys = secondary_entry as *const () as usize - crate::arch::kernel_delta() as usize;
    // the harts the tree lists (any ids, any order; the boot hart is index 0): a hart's index is its place in this list
    let ids: alloc::vec::Vec<u32> = crate::dtb::dtb_blob()
        .and_then(|b| kcore::fdt::parse(b).ok())
        .and_then(|nodes| kcore::harts::hart_ids(&nodes, boot_hart_id as u32))
        .unwrap_or_else(|| {
            let _ = writeln!(
                Console,
                "the tree does not list the boot hart {boot_hart_id}: one hart"
            );
            alloc::vec![boot_hart_id as u32]
        });
    let mut next = 1;
    for &id in ids.iter().skip(1) {
        let id = id as usize;
        let r = sbi(EID_HSM, FID_HART_GET_STATUS, id, 0, 0);
        if r.error != 0 {
            continue; // no such hart
        }
        if next >= MAX_HARTS {
            let _ = writeln!(
                Console,
                "more than {MAX_HARTS} harts: the rest are left stopped"
            );
            break;
        }
        if r.value != HSM_STOPPED {
            // already running or starting: not ours to start
            let _ = writeln!(
                Console,
                "hart {id}: not stopped (state {}), left alone",
                r.value
            );
            continue;
        }
        if !crate::aia::lists_hart(id) {
            // an inter-hart interrupt could never reach it (PL6): it stays stopped
            let _ = writeln!(Console, "hart {id}: not in the IMSIC, left stopped");
            continue;
        }
        // dense index `next`; the record's ids are filled in by the hart itself, but the target
        // of an IPI needs its hart id before then, so set it here too
        percpu::hart(next).hart_id.store(id, Ordering::Relaxed);
        percpu::hart(next).index.store(next, Ordering::Relaxed);
        let start = sbi(EID_HSM, FID_HART_START, id, entry_phys, next);
        if start.error == 0 {
            next += 1;
        } else {
            let _ = writeln!(Console, "hart {id}: hart_start failed ({})", start.error);
        }
    }
    STARTED.store(next, Ordering::SeqCst);
    // wait (bounded) for every started hart
    let deadline = timer::now() + timer::cycles_from_ns(2_000_000_000 + 100_000_000 * next as u64); // 2 s and 0.1 s per hart (emulated harts start slowly)
    for i in 1..next {
        while !percpu::hart(i).online.load(Ordering::Acquire) {
            if timer::now() > deadline {
                break;
            }
            core::hint::spin_loop();
        }
    }
    next
}

/// Are the canaries at the bottom of every started hart's boot stack intact?
pub fn stack_canaries_intact() -> bool {
    let stacks = core::ptr::addr_of!(BOOT_STACKS) as usize;
    // SAFETY: reads the first word of each started hart's stack, a private static.
    (0..started()).all(|i| unsafe { *((stacks + i * STACK_BYTES) as *const u64) } == CANARY)
}

/// How many harts were started (the boot hart included).
pub fn started() -> usize {
    STARTED.load(Ordering::SeqCst)
}

/// How many of them are online.
pub fn online() -> usize {
    (0..started())
        .filter(|&i| percpu::hart(i).online.load(Ordering::Acquire))
        .count()
}

#[allow(dead_code)]
pub const RESCHED: u32 = IPI_RESCHED;

// ---- work run in a hart's IPI handler (the big-lock tests) ----

pub const WORK_BKL_COUNT: u32 = 1;
pub const WORK_BKL_PROBE: u32 = 2;

/// The shared counter of the lock-exclusion test: incremented with a deliberately non-atomic
/// read-modify-write, so only mutual exclusion keeps it exact.
pub static LOCK_COUNTER: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Add one to `LOCK_COUNTER` the racy way (a pause between the load and the store).
pub fn racy_increment() {
    let v = LOCK_COUNTER.load(Ordering::Relaxed);
    core::hint::spin_loop();
    LOCK_COUNTER.store(v + 1, Ordering::Relaxed);
}

fn do_work(me: &percpu::PerHart) {
    let arg = me.work_arg.load(Ordering::SeqCst);
    match me.work_kind.load(Ordering::SeqCst) {
        WORK_BKL_COUNT => {
            for _ in 0..arg {
                crate::bkl::lock();
                racy_increment();
                crate::bkl::unlock();
            }
            me.work_result.store(arg, Ordering::SeqCst);
        }
        WORK_BKL_PROBE => {
            // for `arg` timer cycles, try the lock over and over; report successes and failures
            let end = timer::now() + arg;
            let (mut ok, mut fail) = (0u64, 0u64);
            while timer::now() < end {
                if crate::bkl::try_lock() {
                    ok += 1;
                    crate::bkl::unlock();
                } else {
                    fail += 1;
                }
            }
            me.work_result.store(ok | (fail << 32), Ordering::SeqCst);
        }
        _ => {}
    }
    me.work_done.store(true, Ordering::Release);
}

/// Start `kind`/`arg` on hart `index` (not the caller) through an IPI.
pub fn start_work(index: usize, kind: u32, arg: u64) {
    let h = percpu::hart(index);
    h.work_done.store(false, Ordering::SeqCst);
    h.work_kind.store(kind, Ordering::SeqCst);
    h.work_arg.store(arg, Ordering::SeqCst);
    send_ipi(index, percpu::IPI_WORK);
}

/// Wait (bounded, in timer cycles) for the work on hart `index`; returns its result.
pub fn wait_work(index: usize, timeout_cycles: u64) -> Option<u64> {
    let h = percpu::hart(index);
    let end = timer::now() + timeout_cycles;
    while !h.work_done.load(Ordering::Acquire) {
        if timer::now() > end {
            return None;
        }
        core::hint::spin_loop();
    }
    Some(h.work_result.load(Ordering::SeqCst))
}

/// Make every other hart forget its cached translations (S5d, spec section 5): the caller has
/// just changed or removed a live user mapping, and frames or page tables may be freed and reused
/// as soon as the big kernel lock is released. SBI RFENCE is performed in M-mode and is
/// synchronous, so it works on a hart that is spinning on the lock with interrupts off. Call it
/// inside the locked region that made the change.
pub fn shootdown() {
    if cfg!(feature = "fault-skip-shootdown") {
        return; // negative control: the test that guards this must fail
    }
    let me = percpu::this().index.load(Ordering::Relaxed);
    // every other online hart, as the (mask, base) groups the firmware wants: one call per group of 64 ids, not per hart. No array: the first
    // hart of a group makes the call for the whole group (this runs on the deepest kernel path, the reap on a fresh thread: T173)
    let id_of = |i: usize| percpu::hart(i).hart_id.load(Ordering::Relaxed);
    let online = |i: usize| i != me && percpu::hart(i).online.load(Ordering::Acquire);
    for i in 0..started() {
        if !online(i) {
            continue;
        }
        let base = id_of(i) & !63usize;
        if (0..i).any(|j| online(j) && id_of(j) & !63usize == base) {
            continue;
        }
        let mut mask = 0usize;
        for j in i..started() {
            if online(j) && id_of(j) & !63usize == base {
                mask |= 1usize << (id_of(j) & 63);
            }
        }
        // a whole-address-space flush (start = 0, size = 0) of every ASID on those harts
        let r = sbi5(EID_RFENCE, FID_REMOTE_SFENCE_VMA, [mask, base, 0, 0, 0]);
        assert!(r.error == 0, "SBI remote_sfence_vma failed ({})", r.error);
    }
}

/// Tell every other hart that is part of the scheduler to look at its run state (a kill, for
/// instance, should reach a victim that is running user code on another hart within a tick).
pub fn kick_others() {
    let me = percpu::this().index.load(Ordering::Relaxed);
    for i in 0..started() {
        if i != me && percpu::hart(i).in_sched.load(Ordering::Acquire) {
            send_ipi(i, IPI_RESCHED);
        }
    }
}

/// Does the firmware support a default retentive `hart_suspend`? Learned by trying once: unsupported (or an error) turns it off for good.
static SUSPEND_OFF: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Idle the hart through the firmware's default retentive suspend (SBI HSM `hart_suspend`, KERNEL-V1-SPEC 5.5, PM2): it returns, with
/// all state kept, when an interrupt is pending, like `wfi`. Returns false, having done nothing, if the firmware has no such call (the
/// caller uses `wfi`). Interrupts are off, as for `wfi`.
pub fn suspend_retentive() -> bool {
    if SUSPEND_OFF.load(Ordering::Relaxed) {
        return false;
    }
    let r = sbi(EID_HSM, FID_HART_SUSPEND, 0, 0, 0);
    if r.error != 0 {
        // (not supported, or any other error: off for good, and the caller uses `wfi`)
        let _ = SBI_ERR_NOT_SUPPORTED;
        SUSPEND_OFF.store(true, Ordering::Relaxed);
        return false;
    }
    true
}

/// Whether the firmware's retentive suspend is in use (false until the first idle tries it, and after it failed).
pub fn suspend_supported() -> bool {
    !SUSPEND_OFF.load(Ordering::Relaxed)
}
