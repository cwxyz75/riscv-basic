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
    // SAFETY: `UART_BASE` is the ns16550 `reg` of `/chosen/stdout-path` (`fdt::console_base` refuses a register shift or width other than byte), mapped read-write
    // in the direct map by the early table and by `kmem::init`; the line-status and receive registers are bytes at offsets 5 and 0 inside that page.
    // Like every use of the base it trusts the device tree: the address is bounded by `DIRECT_MAP_PHYS_LIMIT` only, not checked against RAM or the image as
    // the AIA's pages are (T241, part 3), so on a wrong tree this reads kernel memory. No UART lock is taken: the callers are the serial bridge's own
    // thread, and a stale read costs a character, not memory safety (T242).
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

/// The panic handler's writer: its own line state and no UART lock (`break_lock` has let it through, and another hart may be inside `LINE` still: a panic
/// must not form a second `&mut` to it, T240).
pub struct PanicConsole {
    line: kcore::linestate::LineState,
}

impl PanicConsole {
    pub const fn new() -> PanicConsole {
        PanicConsole {
            line: kcore::linestate::LineState::new(),
        }
    }
}

impl fmt::Write for PanicConsole {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.line.kernel_text(s.as_bytes(), &mut put);
        Ok(())
    }
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
    // SAFETY: `UART_BASE` is the ns16550 `reg` of `/chosen/stdout-path` (`fdt::console_base` refuses a register shift or width other than byte), mapped read-write in
    // the direct map by the early table and by `kmem::init`'s device pages; the line-status and transmit registers are bytes at offsets 5 and 0 inside that page,
    // and one-byte volatile accesses are how the device is used. It trusts the device tree like the receive path does: the address is bounded by
    // `DIRECT_MAP_PHYS_LIMIT` only (T241, part 3), so on a wrong tree this writes kernel memory. Only the output path (inside `with_uart`) and `try_read` touch the
    // registers (T242).
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
            // SAFETY: the UART lock is held (`with_uart`), which makes this the only access to `LINE` on any hart: the panic and fatal-trap paths no longer
            // reach it (they write through `PanicConsole`, which has a line state of its own, T240), so breaking the lock cannot alias this reference.
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
        // SAFETY: the UART lock is held, and `LINE` is touched only under it (see `write_str`).
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
