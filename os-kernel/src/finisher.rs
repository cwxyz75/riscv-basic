//! QEMU's test finisher: the way a headless test says pass or fail.

use abi::finisher::{fail, BASE, PASS};
use core::ptr::write_volatile;

/// End the emulation: exit status 0 for `ok`, 1 otherwise.
pub fn exit(ok: bool) -> ! {
    let value = if ok { PASS } else { fail(1) };
    // SAFETY: BASE is the SiFive test device that QEMU's virt machine maps at
    // 0x10_0000; a 32-bit write of one of its documented values ends the run.
    unsafe { write_volatile(BASE as *mut u32, value) };
    loop {
        // SAFETY: `wfi` just idles the hart; the write above ends QEMU, so this
        // is only reached on a machine without the device.
        unsafe { core::arch::asm!("wfi") }
    }
}
