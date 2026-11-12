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

/// The UART lock (S5, K5): held by one hart at a time, taken with interrupts off, reentrant per
/// hart (so `write_fmt` can hold it across a whole line while `write_str` takes it again). The
/// value is the owner's hart index + 1, or 0 when free.
static UART_OWNER: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

fn with_uart<R>(f: impl FnOnce() -> R) -> R {
    use core::sync::atomic::Ordering;
    // (interrupts off, but deliberately *not* the BKL: the console is used outside it, and the
    // lock order is BKL, heap, UART)
    let was = crate::irq::save_and_disable();
    let r = {
        let me = crate::percpu::this().index.load(Ordering::Relaxed) + 1;
        let outer = UART_OWNER.load(Ordering::Relaxed) != me;
        if outer {
            crate::bkl::lock_taken(crate::bkl::Class::Uart);
            while UART_OWNER
                .compare_exchange(0, me, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                core::hint::spin_loop();
            }
        }
        let r = f();
        if outer {
            UART_OWNER.store(0, Ordering::Release);
            crate::bkl::lock_released(crate::bkl::Class::Uart);
        }
        r
    };
    crate::irq::restore(was);
    r
}

/// Break the UART lock (the panic path: whoever held it may never release it).
pub fn break_lock() {
    UART_OWNER.store(0, core::sync::atomic::Ordering::SeqCst);
}

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
    /// A whole formatted print is one critical section, so lines from two harts never interleave.
    fn write_fmt(&mut self, args: fmt::Arguments<'_>) -> fmt::Result {
        with_uart(|| fmt::write(self, args))
    }

    fn write_str(&mut self, s: &str) -> fmt::Result {
        with_uart(|| {
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
    with_uart(|| {
        // SAFETY: as in `write_str`.
        let line = unsafe { &mut *core::ptr::addr_of_mut!(LINE) };
        for &b in bytes {
            line.user_byte(b, &mut put);
        }
    });
}
