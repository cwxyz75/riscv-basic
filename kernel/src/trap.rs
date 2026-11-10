//! Trap entry and handling. Every trap (exception or interrupt) lands in
//! `trap_entry`, which saves all integer and floating-point registers into a
//! frame on the current stack and calls `trap_handler`. A timer interrupt
//! returns to where it happened; anything else is fatal: the handler reports
//! `scause`, `sepc` and `stval` and powers the machine off.

use crate::terminal::Console;
use crate::{cpu, sbi, timer};
use core::arch::global_asm;
use core::fmt::Write;

/// The registers saved by `trap_entry`: `regs[i]` is `x<i>` (slot 0 is unused),
/// `fregs[i]` is `f<i>`. 512 bytes, which keeps `sp` 16-byte aligned.
#[repr(C)]
pub struct TrapFrame {
    pub regs: [usize; 32],
    // Saved so floating-point code interrupted by a tick is not corrupted; only
    // the assembly reads and writes it.
    #[allow(dead_code)]
    pub fregs: [u64; 32],
}

global_asm!(
    r#"
    .section .text.trap
    .balign 4
    .globl trap_entry
trap_entry:
    addi sp, sp, -512
    sd   x1, 8(sp)
    .irp i, 3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
    sd   x\i, \i*8(sp)
    .endr
    addi t0, sp, 512
    sd   t0, 16(sp)
    .irp i, 0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
    fsd  f\i, (32+\i)*8(sp)
    .endr
    mv   a0, sp
    call trap_handler
    .irp i, 0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
    fld  f\i, (32+\i)*8(sp)
    .endr
    ld   x1, 8(sp)
    .irp i, 3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
    ld   x\i, \i*8(sp)
    .endr
    addi sp, sp, 512
    sret
"#
);

extern "C" {
    fn trap_entry();
}

/// Point `stvec` at `trap_entry`.
pub fn init() {
    cpu::write_stvec(trap_entry as unsafe extern "C" fn() as usize);
}

const INTERRUPT_BIT: usize = 1 << (usize::BITS - 1);
const SUPERVISOR_TIMER: usize = 5;

fn exception_name(code: usize) -> &'static str {
    match code {
        0 => "instruction address misaligned",
        1 => "instruction access fault",
        2 => "illegal instruction",
        3 => "breakpoint",
        4 => "load address misaligned",
        5 => "load access fault",
        6 => "store address misaligned",
        7 => "store access fault",
        8 => "environment call from U-mode",
        9 => "environment call from S-mode",
        12 => "instruction page fault",
        13 => "load page fault",
        15 => "store page fault",
        _ => "unknown",
    }
}

fn interrupt_name(code: usize) -> &'static str {
    match code {
        1 => "supervisor software interrupt",
        5 => "supervisor timer interrupt",
        9 => "supervisor external interrupt",
        _ => "unknown interrupt",
    }
}

/// Called from `trap_entry` with the saved registers.
#[no_mangle]
pub extern "C" fn trap_handler(frame: &mut TrapFrame) {
    let scause = cpu::read_scause();
    let is_interrupt = scause & INTERRUPT_BIT != 0;
    let code = scause & !INTERRUPT_BIT;

    if is_interrupt && code == SUPERVISOR_TIMER {
        timer::on_interrupt();
        return;
    }

    let name = if is_interrupt {
        interrupt_name(code)
    } else {
        exception_name(code)
    };
    let mut out = Console;
    let _ = writeln!(
        out,
        "\nTRAP: scause={scause:#x} ({name}) sepc={:#x} stval={:#x}",
        cpu::read_sepc(),
        cpu::read_stval()
    );
    let _ = writeln!(
        out,
        "      ra={:#x} sp={:#x} ticks={} ms={}",
        frame.regs[1],
        frame.regs[2],
        timer::ticks(),
        timer::millis()
    );
    // If the fault happened inside generated code, show that code.
    crate::jit::dump_for_fault(&mut out, cpu::read_sepc());
    sbi::shutdown()
}
