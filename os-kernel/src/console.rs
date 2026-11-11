//! The serial port (QEMU `virt`'s NS16550A at 0x1000_0000), output only, polled.

use core::fmt;
use core::ptr::{read_volatile, write_volatile};

const BASE: usize = 0x1000_0000;
const LSR: usize = 5;
const LSR_THR_EMPTY: u8 = 1 << 5;

pub struct Console;

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            // SAFETY: BASE + LSR and BASE are the 16550's byte-wide registers on
            // QEMU's virt machine; volatile one-byte accesses are how the device
            // is used and nothing else in this kernel touches them.
            unsafe {
                while read_volatile((BASE + LSR) as *const u8) & LSR_THR_EMPTY == 0 {}
                write_volatile(BASE as *mut u8, byte);
            }
        }
        Ok(())
    }
}
