//! Driver for the NS16550A UART that QEMU's `virt` machine maps at `0x1000_0000`.
//! Polling only: no interrupts until a milestone needs them.

use core::ptr::{read_volatile, write_volatile};

const BASE: usize = 0x1000_0000;

// Register offsets (with DLAB = 0).
const RBR_THR: usize = 0; // receive buffer (read) / transmit holding (write)
const IER: usize = 1; // interrupt enable
const LCR: usize = 3; // line control
const LSR: usize = 5; // line status

const LSR_DATA_READY: u8 = 1 << 0;
const LSR_THR_EMPTY: u8 = 1 << 5;

fn read(offset: usize) -> u8 {
    // SAFETY: BASE + offset is one of the 16550's byte-wide memory-mapped
    // registers on QEMU's virt machine; volatile one-byte accesses are how the
    // device is used, and nothing else in the kernel aliases these addresses.
    unsafe { read_volatile((BASE + offset) as *const u8) }
}

fn write(offset: usize, value: u8) {
    // SAFETY: as for `read`: a volatile byte write to a device register.
    unsafe { write_volatile((BASE + offset) as *mut u8, value) }
}

/// Put the UART in a known state: no interrupts, 8 data bits, no parity, one
/// stop bit. The FIFO control register is left as OpenSBI set it (polling works
/// either way, and rewriting it resets the FIFOs). QEMU ignores the baud-rate
/// divisor, so none is programmed.
pub fn init() {
    write(IER, 0x00);
    write(LCR, 0x03);
}

/// Send one byte, waiting for the transmitter to be ready.
pub fn putc(byte: u8) {
    while read(LSR) & LSR_THR_EMPTY == 0 {}
    write(RBR_THR, byte);
}

/// A received byte, if one is waiting.
pub fn try_getc() -> Option<u8> {
    if read(LSR) & LSR_DATA_READY != 0 {
        Some(read(RBR_THR))
    } else {
        None
    }
}

/// Wait for and return the next received byte.
pub fn getc() -> u8 {
    loop {
        if let Some(byte) = try_getc() {
            return byte;
        }
        core::hint::spin_loop();
    }
}
