//! Kernel traps. S1 handles page faults (the lazy-commit path through
//! `kcore::aspace`) and treats everything else as fatal. No user mode yet, so every
//! trap comes from the kernel itself, on the kernel stack.

use crate::{console::Console, finisher, kmem};
use core::arch::global_asm;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use kcore::aspace::{Access, FaultError};
use kcore::testlog;

/// Saved registers: x0 is unused (slot kept so the index is the register number),
/// then the trap CSRs.
#[repr(C)]
pub struct TrapFrame {
    x: [u64; 32],
    sepc: u64,
    scause: u64,
    stval: u64,
    sstatus: u64,
}

global_asm!(
    r#"
    .section .text
    .balign 4
    .globl trap_entry
trap_entry:
    addi sp, sp, -288
    sd   x1, 8(sp)
    sd   x3, 24(sp)
    sd   x4, 32(sp)
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
    addi t0, sp, 288
    sd   t0, 16(sp)
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
    ld   t0, 256(sp)
    csrw sepc, t0
    ld   t0, 280(sp)
    csrw sstatus, t0
    ld   x1, 8(sp)
    ld   x3, 24(sp)
    ld   x4, 32(sp)
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
    unsafe { core::arch::asm!("csrw stvec, {}", in(reg) entry) };
}

// A self-test can declare that the next fault is expected to be refused; the
// handler then records it and steps over the faulting instruction instead of
// dying. (The refusal itself is what is being tested.)
static EXPECT: AtomicBool = AtomicBool::new(false);
static LOG_CAUSE: AtomicU64 = AtomicU64::new(0);
static LOG_ADDR: AtomicU64 = AtomicU64::new(0);
static LOG_ERROR: AtomicU64 = AtomicU64::new(u64::MAX);

/// A refused fault, as the handler saw it.
#[derive(Debug, PartialEq, Eq)]
pub struct Refused {
    pub cause: u64,
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
        cause: LOG_CAUSE.load(Ordering::SeqCst),
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
    let cause = frame.scause;
    if cause == (1 << 63) | 5 {
        // Supervisor timer interrupt: the scheduler's tick (and preemption).
        crate::thread::timer_interrupt();
        return;
    }
    let access = match cause {
        12 => Access::Exec,
        13 => Access::Read,
        15 => Access::Write,
        _ => fatal(frame, format_args!("unexpected trap")),
    };
    let k = kmem::get();
    match k
        .aspace
        .handle_fault(&mut k.mm, &mut k.vmos, frame.stval, access)
    {
        Ok(()) => flush_tlb(),
        // Only data faults can be stepped over: after an instruction-fetch fault
        // `sepc` is the unmapped target, not an instruction we can skip.
        Err(error) if cause != 12 && EXPECT.swap(false, Ordering::SeqCst) => {
            LOG_CAUSE.store(cause, Ordering::SeqCst);
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
    let _ = testlog::panic(
        &mut Console,
        &format_args!(
            "{what}: scause={:#x} sepc={:#x} stval={:#x}",
            frame.scause, frame.sepc, frame.stval
        ),
    );
    finisher::exit(false)
}
