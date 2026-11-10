//! Milestone M1: boot in S-mode under OpenSBI, say hello over the serial port,
//! and run a line-edited echo console (UART receive, backspace, Ctrl-U, Ctrl-C).
//! The machine shuts down cleanly through SBI when asked to `exit`.
//!
//! STATUS: verified locally (`make boot-test` passes under QEMU on macOS).

#![no_std]
#![no_main]

mod terminal;
mod uart;

use console::ansi;
use core::arch::{asm, global_asm};
use core::fmt::Write;
use core::panic::PanicInfo;
use terminal::{Console, Input, Line};

// Entry point. OpenSBI jumps here in S-mode with a0 = hart id, a1 = device-tree
// pointer. We set up a stack, zero .bss, and call into Rust. a0/a1 are left
// untouched so they arrive as kmain's arguments.
global_asm!(
    r#"
    .section .text.init
    .globl _start
_start:
    la   sp, __stack_top
    la   t0, __bss_start
    la   t1, __bss_end
1:
    bgeu t0, t1, 2f
    sd   zero, 0(t0)
    addi t0, t0, 8
    j    1b
2:
    call kmain
3:
    wfi
    j    3b
"#
);

/// Ask OpenSBI to power the machine off (System Reset extension, "SRST").
fn shutdown() -> ! {
    const EID_SRST: usize = 0x5352_5354;
    const FID_SYSTEM_RESET: usize = 0;
    const RESET_TYPE_SHUTDOWN: usize = 0;
    const RESET_REASON_NONE: usize = 0;
    // SAFETY: `ecall` from S-mode traps into OpenSBI, which implements the
    // SRST extension. Arguments follow the SBI calling convention.
    unsafe {
        asm!(
            "ecall",
            in("a7") EID_SRST,
            in("a6") FID_SYSTEM_RESET,
            inout("a0") RESET_TYPE_SHUTDOWN => _,
            inout("a1") RESET_REASON_NONE => _,
        );
    }
    loop {
        // SAFETY: wfi just idles the hart until the next interrupt.
        unsafe { asm!("wfi") }
    }
}

#[no_mangle]
pub extern "C" fn kmain(hart_id: usize, _dtb: usize) -> ! {
    uart::init();
    let mut out = Console;
    let _ = writeln!(out, "riscv-basic: hello from S-mode (hart {hart_id})");
    let _ = ansi::color(&mut out, 2, None);
    let _ = write!(out, "console ready; type a line, or `exit` to power off");
    let _ = ansi::reset(&mut out);
    let _ = writeln!(out);

    // Temporary M1 loop: the BASIC interpreter replaces it at integration (M4).
    let mut input = Input::new();
    loop {
        let _ = write!(out, "> ");
        match input.read_line() {
            Line::Text(text) => {
                let _ = writeln!(out, "you typed: [{text}] ({} chars)", text.len());
                if text == "exit" {
                    let _ = writeln!(out, "bye");
                    shutdown();
                }
            }
            Line::Break => {
                let _ = writeln!(out, "(break)");
            }
        }
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    let mut console = Console;
    let _ = writeln!(console, "\nKERNEL PANIC: {info}");
    shutdown()
}
