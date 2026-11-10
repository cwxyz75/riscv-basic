//! Milestone M0: prove the toolchain. Boot in S-mode under OpenSBI, say hello
//! over the serial port, and shut the machine down cleanly through SBI.
//!
//! STATUS: verified locally (`make boot-test` passes under QEMU on macOS).

#![no_std]
#![no_main]

use core::arch::{asm, global_asm};
use core::fmt::Write;
use core::panic::PanicInfo;

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

/// QEMU `virt` maps an NS16550A UART here. QEMU transmits a byte as soon as it
/// is written to the transmit register, so no initialisation is needed yet.
const UART0: *mut u8 = 0x1000_0000 as *mut u8;

fn uart_putc(byte: u8) {
    // SAFETY: UART0 is the memory-mapped transmit register on QEMU's virt
    // machine; a volatile one-byte write is the documented way to use it.
    unsafe { core::ptr::write_volatile(UART0, byte) }
}

struct Console;

impl Write for Console {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for byte in s.bytes() {
            if byte == b'\n' {
                uart_putc(b'\r');
            }
            uart_putc(byte);
        }
        Ok(())
    }
}

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
    let mut console = Console;
    let _ = writeln!(console, "riscv-basic: hello from S-mode (hart {hart_id})");
    shutdown()
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    let mut console = Console;
    let _ = writeln!(console, "\nKERNEL PANIC: {info}");
    shutdown()
}
