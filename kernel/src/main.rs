//! Milestone M1: boot in S-mode under OpenSBI, say hello over the serial port,
//! and run a line-edited echo console (UART receive, backspace, Ctrl-U, Ctrl-C).
//! The machine shuts down cleanly through SBI when asked to `exit`.
//!
//! STATUS: verified locally (`make boot-test` passes under QEMU on macOS).

#![no_std]
#![no_main]

mod cpu;
mod sbi;
mod terminal;
mod timer;
mod trap;
mod uart;

use console::ansi;
use core::arch::{asm, global_asm};
use core::fmt::Write;
use core::hint::black_box;
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

#[no_mangle]
pub extern "C" fn kmain(hart_id: usize, _dtb: usize) -> ! {
    // Before anything can use floating point (or take a trap).
    cpu::enable_fpu();
    trap::init();
    uart::init();
    let mut out = Console;
    let _ = writeln!(out, "riscv-basic: hello from S-mode (hart {hart_id})");
    let product = black_box(1.5_f64) * black_box(2.0_f64);
    let _ = writeln!(out, "fpu: 1.5 * 2 = {product}");
    timer::init();
    cpu::enable_interrupts();

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
                match text {
                    "exit" => {
                        let _ = writeln!(out, "bye");
                        sbi::shutdown();
                    }
                    "ticks" => {
                        let _ = writeln!(out, "ticks={} ms={}", timer::ticks(), timer::millis());
                    }
                    "crash" => {
                        // SAFETY: deliberately executes the architecturally
                        // illegal `unimp` instruction to exercise the trap
                        // handler; it reports the fault and powers off.
                        unsafe { asm!("unimp") };
                    }
                    "badload" => {
                        // SAFETY: deliberately reads an address with nothing
                        // mapped behind it, to provoke a load access fault
                        // whose `stval` is that address; the trap handler
                        // reports it and powers off.
                        let _ = unsafe { core::ptr::read_volatile(0x1234_5678 as *const u8) };
                    }
                    _ => {}
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
    sbi::shutdown()
}
