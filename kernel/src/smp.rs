//! Secondary harts (S5a, spec section 3): discovery through the SBI hart-state-management
//! extension, the entry code they start in, and the IPI. In S5a the secondaries only idle: each
//! takes its own timer interrupts and software interrupts and counts them; the scheduler and every
//! piece of shared kernel state stay on the boot hart (the big kernel lock arrives in S5b).

use crate::percpu::{self, IPI_RESCHED, MAX_HARTS};
use crate::{irq, kmem, timer};
use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicUsize, Ordering};

const EID_HSM: usize = 0x48_534D;
const FID_HART_START: usize = 0;
const FID_HART_GET_STATUS: usize = 2;
const EID_IPI: usize = 0x73_5049;
const FID_SEND_IPI: usize = 0;
/// `hart_get_status` result for a hart that exists and is waiting to be started.
const HSM_STOPPED: usize = 1;

/// Per-hart boot stacks: 32 KiB each, like every kernel thread, with a canary at the bottom.
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
    // Parked (S5a): idle, taking interrupts, touching nothing shared.
    irq::enable();
    loop {
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
pub fn ipi_interrupt() {
    // SAFETY: clears sip.SSIP, the pending bit of the software interrupt; nothing else.
    unsafe { asm!("csrc sip, {}", in(reg) 1usize << 1) };
    let me = percpu::this();
    me.ipis.fetch_add(1, Ordering::SeqCst);
    let _reasons = me.pending.swap(0, Ordering::SeqCst);
    // (S5c: IPI_RESCHED makes the hart reschedule; in S5a there is nothing to do.)
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
        }
    }
    let entry_phys = secondary_entry as *const () as usize - abi::layout::KERNEL_OFFSET as usize;
    let mut next = 1;
    for id in 0..MAX_HARTS {
        if id == boot_hart_id {
            continue;
        }
        if sbi(EID_HSM, FID_HART_GET_STATUS, id, 0, 0).error != 0 {
            continue; // no such hart
        }
        if next >= MAX_HARTS {
            break;
        }
        let r = sbi(EID_HSM, FID_HART_GET_STATUS, id, 0, 0);
        if r.value != HSM_STOPPED {
            continue; // already running or starting: not ours to start
        }
        // dense index `next`; the record's ids are filled in by the hart itself, but the target
        // of an IPI needs its hart id before then, so set it here too
        percpu::hart(next).hart_id.store(id, Ordering::Relaxed);
        percpu::hart(next).index.store(next, Ordering::Relaxed);
        if sbi(EID_HSM, FID_HART_START, id, entry_phys, next).error == 0 {
            next += 1;
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
