//! Kernel traps. S1 handles page faults (the lazy-commit path through
//! `kcore::aspace`) and treats everything else as fatal. No user mode yet, so every
//! trap comes from the kernel itself, on the kernel stack.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::panic,
    clippy::unreachable
)] // KT3 (KERNEL-V1-SPEC 7.4): no panic a program can cause; remaining sites carry `#[allow] // INVARIANT:` (scripts/kernel_allows.py)

use crate::arch::{Fault, FaultKind, UserTrap};
use crate::{console::Console, finisher, kmem};
use core::arch::global_asm;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use kcore::aspace::{Access, FaultError};
use kcore::testlog;

/// Bytes of a saved trap frame: where a thread's frame sits below its kernel stack's top (`trap_entry`, `enter_user_frame`).
pub const FRAME_BYTES: u64 = 288;
const _: () = assert!(core::mem::size_of::<TrapFrame>() == FRAME_BYTES as usize);

/// Saved registers: x0 is unused (slot kept so the index is the register number),
/// then the trap CSRs.
#[repr(C)]
pub struct TrapFrame {
    x: [u64; 32],
    pub sepc: u64,
    pub scause: u64,
    pub stval: u64,
    pub sstatus: u64,
}

impl TrapFrame {
    /// An all-zero frame (a hart's scratch frame, before it is filled).
    pub const ZERO: TrapFrame = TrapFrame {
        x: [0; 32],
        sepc: 0,
        scause: 0,
        stval: 0,
        sstatus: 0,
    };

    /// Was this frame saved by a trap from user mode?
    #[inline(always)]
    pub fn is_from_user(&self) -> bool {
        self.sstatus & (1 << 8) == 0
    }

    /// Is this trap from user mode a system call? (The hot path: cheaper than building the whole decoded trap.)
    #[inline(always)]
    pub fn is_syscall(&self) -> bool {
        self.scause == 8
    }

    /// Decode a trap taken from user mode.
    pub fn decode_user(&self) -> UserTrap {
        let (kind, access) = match self.scause {
            8 => return UserTrap::Syscall,
            12 => (FaultKind::PageFault, Some(Access::Exec)),
            13 => (FaultKind::PageFault, Some(Access::Read)),
            15 => (FaultKind::PageFault, Some(Access::Write)),
            2 => (FaultKind::Illegal, None),
            3 => (FaultKind::Breakpoint, None),
            0 | 4 | 6 => (FaultKind::Misaligned, None),
            1 | 5 | 7 => (FaultKind::Access, None),
            _ => (FaultKind::Other, None),
        };
        UserTrap::Fault(Fault {
            kind,
            address: self.stval,
            pc: self.sepc,
            access,
            code: self.scause,
        })
    }

    /// Step past the instruction that trapped (a system call returns to the next instruction).
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: the program counter of a user trap is a user address, far from u64::MAX
    #[inline(always)]
    pub fn skip_trap_instruction(&mut self) {
        self.sepc += 4;
    }

    /// The user's register `x{n}` (a0 is 10, a7 is 17).
    #[allow(clippy::indexing_slicing)] // INVARIANT: n < 32, the register count of the trap frame (callers pass constants)
    pub fn arg(&self, n: usize) -> u64 {
        self.x[n]
    }

    /// The registers of a thread that has not run yet (or of a frame being built for one): the GENERAL layout of `abi::thread_state` (x1 to x31,
    /// then pc), user mode with interrupts on at the `sret`, the FP and vector units off.
    #[allow(clippy::indexing_slicing)] // INVARIANT: the layout has 32 words; word 31 is the pc, words 0 to 30 are x1 to x31 (x has 32 slots)
    pub fn from_general(words: &[u64; 32]) -> TrapFrame {
        let mut f = TrapFrame {
            x: [0; 32],
            sepc: words[31],
            scause: 0,
            stval: 0,
            sstatus: (1 << 5) | (2 << 32), // SPIE, and UXL 64-bit (a trap's own frame has it: a WARL field written as it reads)
        };
        f.x[1..].copy_from_slice(&words[..31]);
        f
    }

    /// The user's registers in the GENERAL layout (x1 to x31, then pc).
    pub fn general(&self) -> [u64; 32] {
        let mut w = [0u64; 32];
        w[..31].copy_from_slice(&self.x[1..]);
        w[31] = self.sepc;
        w
    }

    /// Replace the user's registers with the GENERAL layout (the caller has checked the pc).
    pub fn set_general(&mut self, words: &[u64; 32]) {
        self.x[1..].copy_from_slice(&words[..31]);
        self.sepc = words[31];
    }

    /// Put syscall results in a0 (and a1, a2 when given).
    pub fn set_return(&mut self, a0: u64, a1: Option<u64>, a2: Option<u64>) {
        self.x[10] = a0;
        if let Some(v) = a1 {
            self.x[11] = v;
        }
        if let Some(v) = a2 {
            self.x[12] = v;
        }
    }
}

/// A trap was taken with `sp` in a kernel stack's guard: a kernel stack overflow. Runs on the hart's
/// emergency stack; reports through the panic handler (the structured `##PANIC` line the boot harness
/// reads) and ends the run.
#[no_mangle]
#[allow(clippy::panic)] // INVARIANT: a kernel stack overflow is fatal by design: this is the panic that reports it
extern "C" fn stack_overflow(sepc: u64, stval: u64, sp: u64) -> ! {
    panic!("kernel stack overflow: sp {sp:#x} in a guard page (sepc {sepc:#x}, stval {stval:#x})")
}

global_asm!(
    r#"
    .section .text
    .balign 4
    .globl trap_entry
trap_entry:
    csrrw tp, sscratch, tp       # user mode: tp = &PerHart, sscratch = the user's tp
                                 # kernel: tp = 0 (sscratch is 0 in the kernel), sscratch = &PerHart
    bnez  tp, 2f                 # nonzero: we came from user mode
    csrrw tp, sscratch, tp       # from the kernel: undo (tp = &PerHart again, sscratch = 0)
    # Kernel stack overflow check (slice T): is sp inside the guard half of a kernel-stack slot? Pure
    # arithmetic, no memory: the window is the top-level slot 0xffffffe000000000 (arithmetic shift by 30
    # gives -128 for every address in it), and in a 32 KiB slot the lower 16 KiB are the unmapped guard
    # (bit 14 of the address is clear). t0 is parked in sscratch (0 in the kernel) while it is used.
    csrw sscratch, t0
    srai t0, sp, 30
    addi t0, t0, 128
    bnez t0, 5f                  # not in the window: a normal kernel trap
    slli t0, sp, 49
    srli t0, t0, 63              # bit 14 of sp (0: guard half, 1: stack half)
    bnez t0, 5f
    # a kernel stack has overflowed: the stack cannot take a trap frame. Report on the emergency stack.
    csrw sscratch, zero          # (a trap from here on must still look like a kernel trap)
    csrr a0, sepc
    csrr a1, stval
    mv   a2, sp
    ld   sp, 16(tp)              # PerHart.emergency_top
    call stack_overflow
5:  csrr t0, sscratch
    csrw sscratch, zero
    addi sp, sp, -288
    sd   x1, 8(sp)
    sd   x3, 24(sp)
    sd   x5, 40(sp)
    sd   x6, 48(sp)
    sd   x7, 56(sp)
    sd   x8, 64(sp)
    sd   x9, 72(sp)
    sd   x10, 80(sp)
    sd   x11, 88(sp)
    sd   x12, 96(sp)
    sd   x13, 104(sp)
    sd   x14, 112(sp)
    sd   x15, 120(sp)
    sd   x16, 128(sp)
    sd   x17, 136(sp)
    sd   x18, 144(sp)
    sd   x19, 152(sp)
    sd   x20, 160(sp)
    sd   x21, 168(sp)
    sd   x22, 176(sp)
    sd   x23, 184(sp)
    sd   x24, 192(sp)
    sd   x25, 200(sp)
    sd   x26, 208(sp)
    sd   x27, 216(sp)
    sd   x28, 224(sp)
    sd   x29, 232(sp)
    sd   x30, 240(sp)
    sd   x31, 248(sp)
    sd   x4, 32(sp)              # (kept for the frame's sake; never restored on a return to the kernel)
    addi t0, sp, 288
    sd   t0, 16(sp)              # the interrupted sp
    j    3f
2:  # from user mode: tp = &PerHart; switch to this hart's kernel stack
    sd   sp, 8(tp)               # PerHart.scratch_sp = the user's sp
    ld   sp, 0(tp)               # PerHart.kstack_top
    addi sp, sp, -288
    sd   x1, 8(sp)
    sd   x3, 24(sp)
    sd   x5, 40(sp)
    sd   x6, 48(sp)
    sd   x7, 56(sp)
    sd   x8, 64(sp)
    sd   x9, 72(sp)
    sd   x10, 80(sp)
    sd   x11, 88(sp)
    sd   x12, 96(sp)
    sd   x13, 104(sp)
    sd   x14, 112(sp)
    sd   x15, 120(sp)
    sd   x16, 128(sp)
    sd   x17, 136(sp)
    sd   x18, 144(sp)
    sd   x19, 152(sp)
    sd   x20, 160(sp)
    sd   x21, 168(sp)
    sd   x22, 176(sp)
    sd   x23, 184(sp)
    sd   x24, 192(sp)
    sd   x25, 200(sp)
    sd   x26, 208(sp)
    sd   x27, 216(sp)
    sd   x28, 224(sp)
    sd   x29, 232(sp)
    sd   x30, 240(sp)
    sd   x31, 248(sp)
    csrr t0, sscratch
    sd   t0, 32(sp)              # the user's tp
    ld   t0, 8(tp)
    sd   t0, 16(sp)              # the user's sp
    csrw sscratch, zero          # in the kernel now: a trap here is a kernel trap
3:
    csrr t0, sepc
    sd   t0, 256(sp)
    csrr t0, scause
    sd   t0, 264(sp)
    csrr t0, stval
    sd   t0, 272(sp)
    csrr t0, sstatus
    sd   t0, 280(sp)
    mv   a0, sp
    call trap_handler
    .globl trap_epilogue
trap_epilogue:                   # a frame at sp (also entered by `enter_user_frame`): restore it and return
    ld   t0, 256(sp)
    csrw sepc, t0
    ld   t0, 280(sp)
    csrw sstatus, t0
    andi t1, t0, 256             # SPP: was the trap taken from the kernel?
    bnez t1, 4f
    # returning to user mode: the next trap needs this hart's kernel stack top and its PerHart
    addi t1, sp, 288
    sd   t1, 0(tp)               # (tp is this hart's PerHart: kernel code never changes it)
    csrw sscratch, tp
    ld   x4, 32(sp)              # the user's tp
    ld   x1, 8(sp)
    ld   x3, 24(sp)
    ld   x5, 40(sp)
    ld   x6, 48(sp)
    ld   x7, 56(sp)
    ld   x8, 64(sp)
    ld   x9, 72(sp)
    ld   x10, 80(sp)
    ld   x11, 88(sp)
    ld   x12, 96(sp)
    ld   x13, 104(sp)
    ld   x14, 112(sp)
    ld   x15, 120(sp)
    ld   x16, 128(sp)
    ld   x17, 136(sp)
    ld   x18, 144(sp)
    ld   x19, 152(sp)
    ld   x20, 160(sp)
    ld   x21, 168(sp)
    ld   x22, 176(sp)
    ld   x23, 184(sp)
    ld   x24, 192(sp)
    ld   x25, 200(sp)
    ld   x26, 208(sp)
    ld   x27, 216(sp)
    ld   x28, 224(sp)
    ld   x29, 232(sp)
    ld   x30, 240(sp)
    ld   x31, 248(sp)
    ld   sp, 16(sp)
    sret
4:  # returning to the kernel: tp stays as it is (a thread may have moved to another hart)
    ld   x1, 8(sp)
    ld   x3, 24(sp)
    ld   x5, 40(sp)
    ld   x6, 48(sp)
    ld   x7, 56(sp)
    ld   x8, 64(sp)
    ld   x9, 72(sp)
    ld   x10, 80(sp)
    ld   x11, 88(sp)
    ld   x12, 96(sp)
    ld   x13, 104(sp)
    ld   x14, 112(sp)
    ld   x15, 120(sp)
    ld   x16, 128(sp)
    ld   x17, 136(sp)
    ld   x18, 144(sp)
    ld   x19, 152(sp)
    ld   x20, 160(sp)
    ld   x21, 168(sp)
    ld   x22, 176(sp)
    ld   x23, 184(sp)
    ld   x24, 192(sp)
    ld   x25, 200(sp)
    ld   x26, 208(sp)
    ld   x27, 216(sp)
    ld   x28, 224(sp)
    ld   x29, 232(sp)
    ld   x30, 240(sp)
    ld   x31, 248(sp)
    ld   sp, 16(sp)
    sret
"#
);

extern "C" {
    fn trap_entry();
}

/// Point `stvec` at the trap entry (direct mode).
pub fn init() {
    let entry = trap_entry as *const () as usize;
    // SAFETY: writing stvec only changes where traps go; `trap_entry` is 4-byte
    // aligned code in the kernel text, valid at its upper-half address.
    unsafe { core::arch::asm!("csrw stvec, {}", "csrw sscratch, zero", in(reg) entry) };
    // KT5: test builds let user mode read the retired-instruction counter (`rdinstret`, bit 2 of `scounteren`) so the suite can count the
    // instructions of a syscall path. The release build leaves it closed: a program has no use for it and it is a timing channel.
    #[cfg(not(feature = "release"))]
    // SAFETY: `scounteren` only selects which counters user mode may read; no memory is touched.
    unsafe {
        core::arch::asm!("csrs scounteren, {}", in(reg) 4usize);
    }
}

// A self-test can declare that the next fault is expected to be refused; the
// handler then records it and steps over the faulting instruction instead of
// dying. (The refusal itself is what is being tested.)
static EXPECT: AtomicBool = AtomicBool::new(false);
static LOG_ACCESS: AtomicU64 = AtomicU64::new(0);
static LOG_ADDR: AtomicU64 = AtomicU64::new(0);
static LOG_ERROR: AtomicU64 = AtomicU64::new(u64::MAX);

/// A refused fault, as the handler saw it.
#[derive(Debug, PartialEq, Eq)]
pub struct Refused {
    pub access: Access,
    pub addr: u64,
    pub error: FaultError,
}

fn error_code(e: FaultError) -> u64 {
    match e {
        FaultError::NoRegion => 0,
        FaultError::Denied => 1,
        FaultError::VmoGone => 2,
        FaultError::NoMemory => 3,
        FaultError::MapFailed(_) => 4,
        // (the kernel's own table has no pagers)
        FaultError::NeedPager(_) => 5,
    }
}

/// Run `access` (which should touch memory the fault handler will refuse) and
/// report the refusal, or `None` if no fault was refused.
pub fn expect_refused(access: impl FnOnce()) -> Option<Refused> {
    LOG_ERROR.store(u64::MAX, Ordering::SeqCst);
    EXPECT.store(true, Ordering::SeqCst);
    access();
    EXPECT.store(false, Ordering::SeqCst);
    let error = match LOG_ERROR.load(Ordering::SeqCst) {
        0 => FaultError::NoRegion,
        1 => FaultError::Denied,
        2 => FaultError::VmoGone,
        3 => FaultError::NoMemory,
        4 => FaultError::MapFailed(kcore::pagetable::MapError::AlreadyMapped),
        _ => return None,
    };
    Some(Refused {
        access: match LOG_ACCESS.load(Ordering::SeqCst) {
            1 => Access::Write,
            2 => Access::Exec,
            _ => Access::Read,
        },
        addr: LOG_ADDR.load(Ordering::SeqCst),
        error,
    })
}

/// Flush the whole TLB (after unmapping).
pub fn flush_tlb() {
    // SAFETY: `sfence.vma` with no operands orders and flushes all address
    // translations; it has no other effect.
    unsafe { core::arch::asm!("sfence.vma zero, zero") };
}

#[no_mangle]
extern "C" fn trap_handler(frame: &mut TrapFrame) {
    // (a trap runs with interrupts off from entry to return, except where a thread sleeps inside it: the latency
    // trace closes the span at every context switch, so only the stretches really spent with interrupts off count)
    let span = crate::latency::off_begin(true);
    trap_handler_inner(frame);
    crate::latency::off_end(span, core::panic::Location::caller());
}

/// `clock_get`, `thread_yield` (L12c) and `futex_wake` (L14) from user mode without the big kernel lock. False: not one of those, or the return is not
/// quiet (a kill or stop to look for) or dirty FP or vector registers could not be saved without the lock (`fp::light_save`): the full path.
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: sepc + 4 steps past an ecall
fn lock_free_syscall(frame: &mut TrapFrame) -> bool {
    use abi::syscall as nr;
    let n = frame.arg(17) as usize;
    if (n != nr::CLOCK_GET && n != nr::THREAD_YIELD && n != nr::FUTEX_WAKE && n != nr::FUTEX_WAIT)
        || !crate::fp::light_save(frame)
        || !crate::user::user_return_is_quiet()
    {
        return false;
    }
    frame.sepc += 4; // past the ecall
    if n == nr::CLOCK_GET {
        frame.set_return(0, Some(crate::thread::now_ns()), None);
    } else if n == nr::FUTEX_WAIT {
        // L19, T213: the whole call with no big lock, from the translation of the word to the wake; false: nothing was done (or the wait ended
        // for a reason the full path reports), and the call starts again there
        if !crate::user::futex_wait_quiet(frame) {
            frame.sepc -= 4;
            return false;
        }
    } else if n == nr::FUTEX_WAKE {
        // L14: the bucket lock and the scheduler's own locks, no big lock; the lock only for a switch the wake asked for
        crate::user::futex_wake_quiet(frame);
        if crate::thread::need_resched_pending() {
            crate::bkl::lock();
            crate::thread::switch_if_designated();
            crate::user::commit_wake();
            crate::thread::preempt_point();
            crate::user::activate_current(frame);
            crate::bkl::unlock();
        }
    } else {
        frame.set_return(0, None, None);
        if crate::thread::yield_fast() {
            crate::bkl::lock();
            crate::thread::switch_if_designated();
            crate::user::commit_wake();
            crate::thread::preempt_point();
            crate::user::activate_current(frame);
            crate::heap::service();
            crate::bkl::unlock();
        }
    }
    true
}

fn trap_handler_inner(frame: &mut TrapFrame) {
    let from_user = frame.sstatus & (1 << 8) == 0;
    if from_user {
        crate::user::user_mode_left();
        // The kernel runs with the FP unit off (INVARIANTS FP3) on every path, including the light
        // IPI one that saves nothing: the frame keeps the user's FS for the return.
        // SAFETY: clears sstatus.FS only.
        unsafe { core::arch::asm!("csrc sstatus, {}", in(reg) (3usize << 13) | (3usize << 9)) };
    }
    // Every trap holds the big kernel lock while the kernel handles it (S5, K2), except the quiet ones of L12b and L12c (INVARIANTS K6); interrupts are
    // off (the trap cleared SIE). The interrupted code held it only if it was in a locked region,
    // which the lock's reentrancy covers.
    //
    let cause = frame.scause;
    let ready = crate::thread::scheduler_ready();
    // An inter-hart interrupt (an external one carrying only message 1, `external_trap`) takes the lock only when it has scheduling to do; a secondary
    // hart's timer before the scheduler exists touches nothing shared.
    let light = cause == (1 << 63) | 5 && !ready;
    // L12c, L14: `clock_get`, `thread_yield` and `futex_wake` from user mode need no big lock (a yield takes it only to switch), when the return is quiet
    // (no kill or stop recorded) and the user's FP state is not dirty.
    if from_user && cause == 8 && ready && lock_free_syscall(frame) {
        // handled
    } else if from_user
        && ready
        && matches!(cause, 12 | 13 | 15)
        && crate::fp::light_save(frame)
        && crate::user::user_return_is_quiet()
        && crate::user::page_fault_quiet(frame)
    {
        // L17 part c: a page fault resolved under the process's AS lock and the memory locks, with no big lock; anything else takes the full path
    } else if cause == (1 << 63) | 5 && ready {
        // The scheduler's tick (SCHED-SMP-SPEC task 12). Its work is done under the scheduler's own lock. A tick that
        // interrupted **kernel code** (an idle hart, a kernel thread) takes the big kernel lock only if the hart has to
        // switch threads. A tick that interrupted **user mode** takes it only when something needs it (L12b, below): the return
        // to user mode is where a killed process dies (`activate_current`), which needs the lock, so a kill moves an epoch the tick looks at.
        // (A thread whose state is in this hart's FP or vector registers returns with no lock: `fp::return_to_user`, T166.)
        crate::percpu::this()
            .timer_ticks
            .fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        // L12b: a tick that interrupted user mode takes the big lock only when something needs it: the user's FP registers are dirty
        // (saving them touches the FP tables), the hart must switch threads, a kill or stop was recorded since this hart last
        // returned to user mode, or the heap is low. Otherwise the thread resumes with no lock at all.
        if from_user && crate::fp::light_save(frame) {
            let switch = crate::thread::timer_interrupt();
            if !switch
                && crate::user::user_return_is_quiet()
                && !crate::heap::needs_service()
                && !crate::thread::need_resched_pending()
            {
                // nothing to do under the lock
            } else {
                crate::bkl::lock();
                crate::thread::switch_if_designated();
                crate::user::commit_wake();
                crate::thread::preempt_point();
                crate::user::activate_current(frame);
                crate::heap::service();
                crate::bkl::unlock();
            }
        } else if from_user {
            crate::bkl::lock();
            crate::fp::trap_entry(frame);
            let _ = crate::thread::timer_interrupt();
            crate::thread::switch_if_designated();
            crate::user::commit_wake();
            crate::thread::preempt_point();
            crate::user::activate_current(frame);
            crate::heap::service();
            crate::bkl::unlock();
        } else if crate::thread::timer_interrupt() {
            crate::bkl::lock();
            crate::thread::switch_if_designated();
            crate::heap::service();
            crate::bkl::unlock();
        }
    } else if cause == (1 << 63) | 9 {
        external_trap(frame, from_user);
    } else if light {
        trap_body(frame);
    } else {
        crate::bkl::lock();
        crate::fp::trap_entry(frame);
        trap_body(frame);
        if from_user {
            // whatever this trap touched is committed before the lock is released (W1)
            crate::user::commit_wake();
            // the return to user mode is a defined point for the switch a wake asked for (X7)
            crate::thread::preempt_point();
            // (a switch away and back: load this thread's address space again, as every other path through `trap_body` does)
            crate::user::activate_current(frame);
        }
        crate::heap::service(); // (a safe point: headroom for the next allocations, see heap.rs)
        crate::bkl::unlock();
    }
    // (the trap exit returns to user mode if this frame came from it)
    if from_user {
        crate::fp::return_to_user(frame);
        crate::user::user_mode_entered();
    }
}

/// An inter-hart interrupt on its own (no device message came with it): the quiet paths of L12b, the lock only when the hart has scheduling to do.
#[inline(never)] // (a frame of its own: folded into `trap_handler_inner` it deepens every kernel path that starts with a trap, KT13)
fn ipi_trap(frame: &mut TrapFrame) {
    let reasons = crate::smp::ipi_interrupt();
    if reasons & crate::percpu::IPI_RESCHED != 0 && crate::thread::scheduler_ready() {
        if frame.sstatus & (1 << 8) == 0 {
            // interrupted user mode (L12b): no big lock when the hart need not switch and no kill or stop was recorded (the
            // kill IPI follows the epoch bump, so it takes the full path, where a killed process dies at the return to user mode)
            if crate::fp::light_save(frame)
                && !crate::thread::reschedule_fast()
                && crate::user::user_return_is_quiet()
                && !crate::thread::need_resched_pending()
            {
                return;
            }
            crate::bkl::lock();
            crate::fp::trap_entry(frame);
            crate::thread::reschedule_this_hart();
            crate::thread::preempt_point();
            crate::user::activate_current(frame);
            crate::bkl::unlock();
        } else if crate::thread::reschedule_fast() {
            // interrupted kernel code and the hart must switch: only now the big kernel lock
            crate::bkl::lock();
            crate::thread::switch_if_designated();
            crate::bkl::unlock();
        }
    }
}

/// A supervisor external interrupt: take every message in this hart's IMSIC file. The inter-hart interrupt (message 1) is the hot one and needs
/// no lock unless the hart must switch; a device message goes to its interrupt object under the big lock (S6a-2).
#[inline(never)] // (as `ipi_trap`)
fn external_trap(frame: &mut TrapFrame, from_user: bool) {
    let ids = crate::aia::claim();
    let devices = ids & !crate::aia::IPI_BIT;
    if devices == 0 {
        if ids & crate::aia::IPI_BIT != 0 {
            ipi_trap(frame);
        }
        return;
    }
    // (the inter-hart part first, with no lock: its test jobs probe the lock)
    let reasons = if ids & crate::aia::IPI_BIT != 0 {
        crate::smp::ipi_interrupt()
    } else {
        0
    };
    crate::bkl::lock();
    crate::fp::trap_entry(frame);
    let any = crate::aia::deliver(devices, crate::user::irq_fired);
    if (any || reasons & crate::percpu::IPI_RESCHED != 0) && crate::thread::scheduler_ready() {
        if any {
            crate::user::wake_signal_waiters();
        }
        crate::thread::reschedule_this_hart();
    }
    if from_user {
        crate::user::commit_wake();
        crate::thread::preempt_point();
        crate::user::activate_current(frame);
    }
    crate::heap::service();
    crate::bkl::unlock();
}

#[allow(clippy::arithmetic_side_effects)] // INVARIANT: the kernel's own fault path: va steps over the pages a fault mapped; sepc plus an instruction length stays in kernel text
fn trap_body(frame: &mut TrapFrame) {
    let cause = frame.scause;
    if cause == (1 << 63) | 5 && !crate::thread::scheduler_ready() {
        // A secondary hart's timer before the scheduler exists: count and re-arm.
        crate::percpu::this()
            .timer_ticks
            .fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        // (a hart parked until the scheduler exists polls for it once a millisecond: nothing else would wake it)
        crate::timer::program(Some(crate::timer::now_ns() + 1_000_000));
        return;
    }
    if frame.sstatus & (1 << 8) == 0 {
        // SPP = 0: the trap came from user mode.
        crate::user::user_trap(frame);
        crate::user::activate_current(frame);
        return;
    }
    let access = match cause {
        12 => Access::Exec,
        13 => Access::Read,
        15 => Access::Write,
        _ => fatal(frame, format_args!("unexpected trap")),
    };
    // (a fault taken while a lock of class VMO or later is held cannot be handled: the handler would take them again. Say what it was.)
    if crate::bkl::violates(crate::bkl::Class::Vmo) {
        fatal(
            frame,
            format_args!("kernel page fault with a memory lock held"),
        );
    }
    let k = kmem::get();
    match k
        .aspace
        .handle_fault_mapped(k.mm, k.vmos, k.jobs, frame.stval, access)
    {
        // (the kernel's own table: every ASID, the pages the fault mapped)
        Ok(m) => {
            let mut va = m.first;
            while va <= m.last {
                // SAFETY: `sfence.vma` with an address drops the cached translations of that page for every ASID; no other effect.
                unsafe { core::arch::asm!("sfence.vma {}, zero", in(reg) va) };
                va += 4096;
            }
        }
        // Only data faults can be stepped over: after an instruction-fetch fault
        // `sepc` is the unmapped target, not an instruction we can skip.
        Err(error) if cause != 12 && EXPECT.swap(false, Ordering::SeqCst) => {
            LOG_ACCESS.store(
                match access {
                    Access::Read => 0,
                    Access::Write => 1,
                    Access::Exec => 2,
                },
                Ordering::SeqCst,
            );
            LOG_ADDR.store(frame.stval, Ordering::SeqCst);
            LOG_ERROR.store(error_code(error), Ordering::SeqCst);
            frame.sepc += instruction_length(frame.sepc);
        }
        Err(error) => fatal(frame, format_args!("page fault refused: {error:?}")),
    }
}

/// 2 for a compressed instruction, 4 otherwise (RISC-V encodes it in the low bits).
fn instruction_length(pc: u64) -> u64 {
    // SAFETY: `pc` is the address of the instruction that just trapped, in
    // kernel text that is mapped and readable.
    let half = unsafe { core::ptr::read_volatile(pc as *const u16) };
    if half & 3 == 3 {
        4
    } else {
        2
    }
}

fn fatal(frame: &TrapFrame, what: core::fmt::Arguments) -> ! {
    crate::console::break_lock();
    let _ = testlog::panic(
        &mut Console,
        &format_args!(
            "{what}: scause={:#x} sepc={:#x} stval={:#x}",
            frame.scause, frame.sepc, frame.stval
        ),
    );
    finisher::exit(false)
}
