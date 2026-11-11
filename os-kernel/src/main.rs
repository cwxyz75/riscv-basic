//! The OS-track kernel, milestone S0: a stub that proves the harness. It boots in
//! S-mode under OpenSBI, runs a few self-tests, reports them as structured lines
//! on the serial port (`kcore::testlog`) and ends QEMU through the test finisher
//! with a pass/fail exit status. The real kernel grows from here (S1 onwards).

#![no_std]
#![no_main]

mod console;
mod finisher;

use core::arch::global_asm;
use core::fmt::Write;
use core::panic::PanicInfo;
use kcore::latch::Latch;
use kcore::testlog;

// Entry: OpenSBI jumps here in S-mode with a0 = hart id, a1 = device tree.
// Set up the stack, zero .bss, call into Rust (a0/a1 arrive as kmain's arguments).
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

/// A zero-initialised static: it lives in .bss, so this checks `_start` cleared it.
static BSS_PROBE: [u64; 64] = [0; 64];

struct Tally {
    pass: u32,
    fail: u32,
}

impl Tally {
    fn record(&mut self, name: &str, result: Result<(), &str>) {
        let _ = testlog::test(&mut console::Console, name, result);
        if result.is_ok() {
            self.pass += 1;
        } else {
            self.fail += 1;
        }
    }
}

#[no_mangle]
pub extern "C" fn kmain(hart_id: usize, _dtb: usize) -> ! {
    let mut out = console::Console;
    let _ = testlog::boot(&mut out, "os-kernel", "s0");
    let _ = writeln!(out, "hello from the OS track, hart {hart_id}");

    let mut tally = Tally { pass: 0, fail: 0 };
    tally.record(
        "bss_is_zeroed",
        if BSS_PROBE.iter().all(|&w| w == 0) {
            Ok(())
        } else {
            Err("a .bss word was not zero")
        },
    );
    tally.record("stack_is_16_byte_aligned", check_sp());
    tally.record("latch_works_on_the_target", {
        let latch = Latch::new();
        latch.set();
        if latch.take() && !latch.take() {
            Ok(())
        } else {
            Err("latch semantics broke on riscv64")
        }
    });
    #[cfg(feature = "fail-demo")]
    tally.record(
        "deliberate_failure",
        Err("fail-demo build: failing on purpose"),
    );

    let _ = testlog::done(&mut out, tally.pass, tally.fail);
    finisher::exit(tally.fail == 0)
}

fn check_sp() -> Result<(), &'static str> {
    let sp: usize;
    // SAFETY: reads the stack pointer register; no memory is touched.
    unsafe { core::arch::asm!("mv {}, sp", out(reg) sp) };
    if sp.is_multiple_of(16) {
        Ok(())
    } else {
        Err("sp is not 16-byte aligned")
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    let _ = testlog::panic(&mut console::Console, info);
    finisher::exit(false)
}
