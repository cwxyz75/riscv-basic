//! QEMU's test finisher: the way a headless test says pass or fail.

use abi::finisher::{fail, PASS};
use abi::layout::DIRECT_BASE;
use core::ptr::write_volatile;

const BASE: u64 = DIRECT_BASE + abi::finisher::BASE as u64;

/// End the emulation: exit status 0 for `ok`, 1 otherwise.
pub fn exit(ok: bool) -> ! {
    let value = if ok { PASS } else { fail(1) };
    // SAFETY: the SiFive test device (physical 0x10_0000) is mapped through the
    // direct map by the early boot table and by `kmem::init`; a 32-bit write of
    // one of its documented values ends the run.
    unsafe { write_volatile(BASE as *mut u32, value) };
    loop {
        // SAFETY: `wfi` just idles the hart; the write above ends QEMU, so this
        // is only reached on a machine without the device.
        unsafe { core::arch::asm!("wfi") }
    }
}
