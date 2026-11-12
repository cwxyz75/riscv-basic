//! Secondary harts (S5a, spec section 3): discovery through the SBI hart-state-management
//! extension, the entry code they start in, and the IPI. In S5a the secondaries only idle: each
//! takes its own timer interrupts and software interrupts and counts them; the scheduler and every
//! piece of shared kernel state stay on the boot hart (the big kernel lock arrives in S5b).

use crate::console::Console;
use crate::percpu::{self, IPI_RESCHED, MAX_HARTS};
use crate::{irq, kmem, timer};
use core::arch::{asm, global_asm};
use core::fmt::Write;
use core::sync::atomic::{AtomicUsize, Ordering};

const EID_HSM: usize = 0x48_534D;
const FID_HART_START: usize = 0;
const FID_HART_GET_STATUS: usize = 2;
const EID_IPI: usize = 0x73_5049;
const FID_SEND_IPI: usize = 0;
/// `hart_get_status` result for a hart that exists and is waiting to be started.
const HSM_STOPPED: usize = 1;

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

struct SbiRet {
    error: isize,
    value: usize,
}

fn sbi(eid: usize, fid: usize, a0: usize, a1: usize, a2: usize) -> SbiRet {
    let (error, value): (isize, usize);
    // SAFETY: `ecall` from S-mode into OpenSBI with the standard calling convention (a7 =
    // extension, a6 = function, a0-a2 arguments, results in a0/a1); it changes no kernel state.
    unsafe {
        asm!(
            "ecall",
            in("a7") eid,
            in("a6") fid,
            inlateout("a0") a0 => error,
            inlateout("a1") a1 => value,
            in("a2") a2,
        );
    }
    SbiRet { error, value }
}

global_asm!(
    r#"
    .section .text
    .globl secondary_entry
    .balign 4
secondary_entry:                 # a0 = hart id, a1 = dense hart index; paging off, interrupts off
    mv   s0, a0
    mv   s1, a1
    # The kernel's own table has no identity mapping, so first use the early boot table (still
    # intact in .bss: identity for RAM, plus the upper-half image), then jump to the upper half.
    la   t2, EARLY_ROOT
    srli t3, t2, 12
    li   t4, 8
    slli t4, t4, 60              # satp.MODE = 8 (Sv39)
    or   t3, t3, t4
    csrw satp, t3
    sfence.vma zero, zero
    la   t0, 3f
    li   t1, 0xffffffff00000000  # KERNEL_OFFSET
    add  t0, t0, t1
    jr   t0
3:
    la   t0, SECONDARY_STACK_TOPS
    slli t1, s1, 3
    add  t0, t0, t1
    ld   sp, 0(t0)
    mv   a0, s0
    mv   a1, s1
    call secondary_main
4:
    wfi
    j    4b
"#
);

extern "C" {
    fn secondary_entry();
}

/// What a secondary hart does once it has a stack and the early table (Rust from here).
#[no_mangle]
extern "C" fn secondary_main(hart_id: usize, index: usize) -> ! {
    percpu::bind(index, hart_id);
    let tp: usize;
    // SAFETY: reads `tp`, just bound to this hart's record.
    unsafe { asm!("mv {}, tp", out(reg) tp) };
    percpu::this().tp_seen.store(tp, Ordering::Relaxed);
    crate::trap::init(); // stvec; sscratch = 0
                         // FPU and vector unit off, like the boot hart (thread::init), so any use traps.
                         // SAFETY: clears sstatus.FS and sstatus.VS only.
    unsafe { asm!("csrc sstatus, {}", in(reg) (3usize << 13) | (3usize << 9)) };
    // Leave the early table for the kernel's own.
    let satp = kmem::KERNEL_SATP.load(Ordering::SeqCst);
    // SAFETY: the kernel table maps everything this code runs from and touches (the image, the
    // direct map, the devices in use); the flush drops the early table's translations.
    unsafe { asm!("csrw satp, {}", "sfence.vma zero, zero", in(reg) satp) };
    timer::start();
    irq::enable_timer_interrupt();
    enable_software_interrupt();
    percpu::this().online.store(true, Ordering::Release);
    // Parked until the boot hart has set up the scheduler (interrupts on, touching nothing
    // shared), then this hart joins it for good.
    irq::enable();
    loop {
        if crate::thread::scheduler_exists() {
            crate::thread::join_scheduler();
        }
        // SAFETY: `wfi` idles the hart until an interrupt; interrupts are enabled.
        unsafe { asm!("wfi") };
    }
}

fn enable_software_interrupt() {
    // SAFETY: sets sie.SSIE only (the supervisor software interrupt, used for IPIs).
    unsafe { asm!("csrs sie, {}", in(reg) 1usize << 1) };
}

/// The boot hart's share of `enable_software_interrupt`.
pub fn enable_ipi_on_this_hart() {
    enable_software_interrupt();
}

/// The software interrupt handler (called from the trap handler on any hart): clear the pending
/// bit, take the reasons, count.
pub fn ipi_interrupt() -> u32 {
    // SAFETY: clears sip.SSIP, the pending bit of the software interrupt; nothing else.
    unsafe { asm!("csrc sip, {}", in(reg) 1usize << 1) };
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

/// Send `reason` to hart `index` (an IPI through SBI).
pub fn send_ipi(index: usize, reason: u32) {
    let target = percpu::hart(index);
    target.pending.fetch_or(reason, Ordering::SeqCst);
    let id = target.hart_id.load(Ordering::Relaxed);
    sbi(EID_IPI, FID_SEND_IPI, 1usize << (id % 64), id - id % 64, 0);
}

/// Find the other harts and start them. Needs `kmem::init` (the kernel table) and the heap.
/// Returns how many harts run (the boot hart included).
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
    let entry_phys = secondary_entry as *const () as usize - abi::layout::KERNEL_OFFSET as usize;
    let mut next = 1;
    for id in 0..MAX_HARTS {
        if id == boot_hart_id {
            continue;
        }
        let r = sbi(EID_HSM, FID_HART_GET_STATUS, id, 0, 0);
        if r.error != 0 {
            continue; // no such hart
        }
        if next >= MAX_HARTS {
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
    let deadline = timer::now() + 2 * 10_000_000; // 2 s at 10 MHz
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
