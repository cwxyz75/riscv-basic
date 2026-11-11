//! The serial port (QEMU `virt`'s NS16550A, physical 0x1000_0000), output only,
//! polled, reached through the direct map.

use abi::layout::DIRECT_BASE;
use core::fmt;
use core::ptr::{read_volatile, write_volatile};

const BASE: usize = (DIRECT_BASE + 0x1000_0000) as usize;
const LSR: usize = 5;
const LSR_THR_EMPTY: u8 = 1 << 5;

/// One received byte, if the UART has one (polled; the S4 serial bridge calls this every tick).
#[cfg_attr(not(feature = "interactive"), allow(dead_code))]
pub fn try_read() -> Option<u8> {
    // SAFETY: the 16550's line-status and receive registers, mapped like the transmit ones.
    unsafe {
        if read_volatile((BASE + LSR) as *const u8) & 1 != 0 {
            Some(read_volatile(BASE as *const u8))
        } else {
            None
        }
    }
}

pub struct Console;

/// The physical line state shared by every writer (kernel and bridge): see
/// `kcore::linestate`. Accessed only with interrupts off.
static mut LINE: kcore::linestate::LineState = kcore::linestate::LineState::new();

fn put(byte: u8) {
    // SAFETY: BASE + LSR and BASE are the 16550's byte-wide registers, mapped by the early
    // boot table and by `kmem::init`'s device pages; volatile one-byte accesses are how the
    // device is used and nothing else in this kernel touches them.
    unsafe {
        while read_volatile((BASE + LSR) as *const u8) & LSR_THR_EMPTY == 0 {}
        write_volatile(BASE as *mut u8, byte);
    }
}

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        crate::irq::without(|| {
            // SAFETY: interrupts are off and this is the only access path to `LINE`.
            let line = unsafe { &mut *core::ptr::addr_of_mut!(LINE) };
            line.kernel_text(s.as_bytes(), &mut put);
        });
        Ok(())
    }
}

/// Bytes a *user program* wrote, forwarded to the UART byte for byte (no text conversion) with
/// line-start `##` rewritten to `# #`, through the same line state as the kernel's own output.
#[cfg_attr(not(feature = "interactive"), allow(dead_code))]
pub fn write_user(bytes: &[u8]) {
    crate::irq::without(|| {
        // SAFETY: as in `write_str`.
        let line = unsafe { &mut *core::ptr::addr_of_mut!(LINE) };
        for &b in bytes {
            line.user_byte(b, &mut put);
        }
    });
}
