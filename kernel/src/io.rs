//! The kernel's implementation of the interpreter's `Io` trait, over the UART,
//! the timer and raw memory.

use crate::terminal::{self, Console, Input};
use crate::timer;
use alloc::string::String;
use core::fmt::Write;
use core::ptr::{read_volatile, write_volatile};

pub struct KernelIo {
    input: Input,
}

impl KernelIo {
    pub const fn new() -> Self {
        KernelIo {
            input: Input::new(),
        }
    }
}

impl basic::Io for KernelIo {
    fn write_str(&mut self, s: &str) {
        let _ = Console.write_str(s);
    }

    fn read_line(&mut self) -> basic::Line {
        match self.input.read_line() {
            terminal::Line::Text(text) => basic::Line::Text(String::from(text)),
            terminal::Line::Break => basic::Line::Break,
        }
    }

    /// Has Ctrl-C been pressed? Other typed-ahead characters are kept for the
    /// next line read.
    fn break_pressed(&mut self) -> bool {
        self.input.poll_break()
    }

    fn reboot(&mut self) {
        crate::sbi::reboot()
    }

    fn millis(&mut self) -> u64 {
        timer::millis()
    }

    fn peek(&mut self, addr: u64) -> u8 {
        // SAFETY: none, deliberately. BASIC's PEEK reads any physical address
        // (decision D12); a bad address raises a load access fault that the trap
        // handler reports before powering off. The interpreter has already
        // checked that `addr` is an exactly-representable non-negative integer.
        unsafe { read_volatile(addr as *const u8) }
    }

    fn poke(&mut self, addr: u64, value: u8) {
        // SAFETY: none, deliberately; see `peek`. POKEing kernel memory
        // corrupts it, as on the machines this imitates.
        unsafe { write_volatile(addr as *mut u8, value) }
    }
}
