//! The serial port: the NS16550A the device tree names (`/chosen/stdout-path`, PL5b), output only, polled, reached through the direct map;
//! the firmware's debug console (SBI DBCN) when the tree names none.

use core::fmt;
use core::ptr::{read_volatile, write_volatile};

/// The UART's address in the direct map; 0 until `init_from_tree` found one (then output goes through the firmware).
static UART_BASE: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
/// Bytes sent through the firmware's debug console, and how many of those it refused (the self-test reads them).
static SBI_BYTES: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static SBI_ERRORS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
/// Set by `use_sbi` (the boot word `console=sbi`): output goes through the firmware even if the tree names a UART.
static FORCE_SBI: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
const LSR: usize = 5;
const LSR_THR_EMPTY: u8 = 1 << 5;

/// One received byte, if the UART has one (polled; the S4 serial bridge calls this every tick).
#[cfg_attr(not(feature = "interactive"), allow(dead_code))]
pub fn try_read() -> Option<u8> {
    let base = UART_BASE.load(core::sync::atomic::Ordering::Relaxed);
    if base == 0 || FORCE_SBI.load(core::sync::atomic::Ordering::Relaxed) {
        return None; // the firmware's console is output only here
    }
    // SAFETY: the 16550's line-status and receive registers, mapped like the transmit ones.
    unsafe {
        if read_volatile((base + LSR) as *const u8) & 1 != 0 {
            Some(read_volatile(base as *const u8))
        } else {
            None
        }
    }
}

/// Find the console in the device tree (allocation-free: the first thing `kmain` does, before the heap exists): the UART `/chosen/stdout-path`
/// names. Returns its physical address, or `None` (then output goes through the firmware's debug console).
pub fn init_from_tree() -> Option<u64> {
    let pa = crate::dtb::dtb_blob()
        .and_then(kcore::fdt::console_base)
        // (an address the early table does not map would fault with no handler: a tree naming one has no usable console)
        .filter(|&pa| pa != 0 && pa.saturating_add(0x1000) <= abi::layout::DIRECT_MAP_PHYS_LIMIT)?;
    UART_BASE.store(
        (crate::kmem::direct_base() + pa) as usize,
        core::sync::atomic::Ordering::Relaxed,
    );
    Some(pa)
}

/// The UART's physical address, if the tree named one the kernel drives.
pub fn base() -> Option<u64> {
    Some(UART_BASE.load(core::sync::atomic::Ordering::Relaxed))
        .filter(|&b| b != 0)
        .map(|b| b as u64 - crate::kmem::direct_base())
}

/// `(bytes sent through the firmware's debug console, bytes it refused)`.
pub fn sbi_counts() -> (usize, usize) {
    (
        SBI_BYTES.load(core::sync::atomic::Ordering::Relaxed),
        SBI_ERRORS.load(core::sync::atomic::Ordering::Relaxed),
    )
}

/// Whether output goes through the firmware's debug console because of `use_sbi`.
pub fn sbi_forced() -> bool {
    FORCE_SBI.load(core::sync::atomic::Ordering::Relaxed)
}

/// Send all output through the firmware's debug console from now on (the boot word `console=sbi`: the fallback path, exercised on a machine
/// that has a UART).
pub fn use_sbi() {
    FORCE_SBI.store(true, core::sync::atomic::Ordering::Relaxed);
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
    let base = UART_BASE.load(core::sync::atomic::Ordering::Relaxed);
    if base == 0 || FORCE_SBI.load(core::sync::atomic::Ordering::Relaxed) {
        SBI_BYTES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        if !crate::arch::debug_console_write_byte(byte) {
            SBI_ERRORS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
        return;
    }
    // SAFETY: base + LSR and base are the 16550's byte-wide registers, mapped by the early
    // boot table and by `kmem::init`'s device pages; volatile one-byte accesses are how the
    // device is used and nothing else in this kernel touches them.
    unsafe {
        while read_volatile((base + LSR) as *const u8) & LSR_THR_EMPTY == 0 {}
        write_volatile(base as *mut u8, byte);
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

/// Take and drop the UART lock (the lock-order test).
pub fn with_uart_for_test() {
    with_uart(|| ());
}
