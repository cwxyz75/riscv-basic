//! The way the run ends (PL5b): QEMU's test finisher in a test build whose tree has one (a headless test says pass or fail with the exit
//! status), otherwise the firmware's system reset (SBI SRST: a shutdown, with a reason for failure).

use abi::finisher::{fail, PASS};
use abi::layout::DIRECT_BASE;
use core::ptr::write_volatile;
use core::sync::atomic::{AtomicU64, Ordering};

/// The test device's physical address from the device tree; 0 when the tree has none (real hardware).
static BASE: AtomicU64 = AtomicU64::new(0);

/// Record the test device the tree names (`kcore::fdt::test_finisher_base`).
pub fn init(base: Option<u64>) {
    BASE.store(base.unwrap_or(0), Ordering::Relaxed);
}

/// The test device's physical address, if the tree has one.
pub fn base() -> Option<u64> {
    Some(BASE.load(Ordering::Relaxed)).filter(|&b| b != 0)
}

/// End the run: exit status 0 for `ok`, 1 otherwise through the test finisher; through the firmware in a release build or when the tree has
/// no test device.
pub fn exit(ok: bool) -> ! {
    if let Some(base) = base().filter(|_| !cfg!(feature = "release")) {
        let value = if ok { PASS } else { fail(1) };
        // SAFETY: the SiFive test device the tree names is mapped through the direct map by the early boot table (all physical space below
        // 64 GiB) and by `kmem::init`; a 32-bit write of one of its documented values ends the run.
        unsafe { write_volatile((DIRECT_BASE + base) as *mut u32, value) };
    } else {
        crate::arch::system_reset(false, !ok);
    }
    loop {
        // SAFETY: `wfi` just idles the hart; the call above ends the run, so this is only reached on a machine whose firmware ignores it.
        unsafe { core::arch::asm!("wfi") }
    }
}
