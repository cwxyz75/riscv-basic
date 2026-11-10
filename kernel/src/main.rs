//! The kernel: boot in S-mode under OpenSBI, bring up the console, traps, timer,
//! FPU and heap, then hand the machine to the BASIC interpreter, which runs the
//! `READY.` prompt forever. See `DESIGN.md` for how the pieces fit.

#![no_std]
#![no_main]

extern crate alloc;

mod cpu;
mod gfx;
mod heap;
mod io;
mod jit;
mod menu;
mod sbi;
mod terminal;
mod timer;
mod trap;
mod uart;

use console::ansi;
use core::arch::global_asm;
use core::fmt::Write;
use core::hint::black_box;
use core::panic::PanicInfo;
use io::KernelIo;
use menu::Language;
use terminal::Console;

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
    heap::init();

    let mut out = Console;
    let _ = writeln!(out, "riscv-basic: hello from S-mode (hart {hart_id})");
    let product = black_box(1.5_f64) * black_box(2.0_f64);
    let _ = writeln!(out, "fpu: 1.5 * 2 = {product}");
    timer::init();
    cpu::enable_interrupts();

    let _ = ansi::color(&mut out, 2, None);
    let _ = write!(
        out,
        "kernel ready: {} KiB heap, timer running",
        heap::HEAP_SIZE / 1024
    );
    let _ = ansi::reset(&mut out);
    let _ = writeln!(out);

    match menu::choose() {
        Language::Basic => {
            let mut io = KernelIo::new();
            let mut interpreter = basic::Interpreter::new();
            interpreter.banner(&mut io);
            interpreter.run_session(&mut io);
        }
        Language::Logo => run_logo(&mut out),
    }
    // The serial console never reports end of input, so this is unreachable in
    // practice; power off cleanly if it ever happens.
    sbi::shutdown()
}

/// LOGO: terminal in, framebuffer out. Without a `ramfb` device it still runs,
/// drawing into memory that nobody sees.
fn run_logo(out: &mut Console) {
    use turtle::Canvas;
    let mut io = KernelIo::new();
    let mut interpreter = logo::Interpreter::new();
    match gfx::init() {
        Ok(pixels) => {
            let mut canvas = gfx::FbCanvas::new(pixels);
            canvas.clear(0);
            interpreter.banner(&mut io);
            interpreter.run_session(&mut io, &mut canvas);
        }
        Err(reason) => {
            let _ = writeln!(out, "(no screen: {reason})");
            let mut canvas = turtle::Bitmap::new(gfx::WIDTH as i32, gfx::HEIGHT as i32);
            interpreter.banner(&mut io);
            interpreter.run_session(&mut io, &mut canvas);
        }
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    let mut console = Console;
    let _ = writeln!(console, "\nKERNEL PANIC: {info}");
    sbi::shutdown()
}
