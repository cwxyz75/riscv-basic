//! The serial console: formatted output, and line input through the editor in
//! the `console` crate.

use crate::uart;
use alloc::collections::VecDeque;
use console::{Event, LineEditor};
use core::fmt;

/// Output side: implements `fmt::Write`, turning `\n` into `\r\n` for the terminal.
pub struct Console;

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            if byte == b'\n' {
                uart::putc(b'\r');
            }
            uart::putc(byte);
        }
        Ok(())
    }
}

/// What reading a line produced. Mirrors `basic::Line`, which the kernel will
/// adopt when the interpreter is integrated.
pub enum Line<'a> {
    Text(&'a str),
    Break,
}

/// Most typed-ahead bytes kept while a program runs; more are dropped.
const TYPEAHEAD_LIMIT: usize = 8192;

const CTRL_C: u8 = 0x03;

/// Input side: owns the line editor and the typeahead buffer.
pub struct Input {
    editor: LineEditor,
    /// Bytes received while the interpreter was busy (see `poll_break`), to be
    /// read before anything new from the UART.
    typeahead: VecDeque<u8>,
}

impl Input {
    pub const fn new() -> Self {
        Input {
            editor: LineEditor::new(),
            typeahead: VecDeque::new(),
        }
    }

    /// Check for Ctrl-C without waiting. Everything received is taken from the
    /// UART; Ctrl-C is acted on (and dropped) and every other byte is kept for
    /// the next `read_line`, so text typed or pasted ahead is not lost.
    pub fn poll_break(&mut self) -> bool {
        let mut pressed = false;
        while let Some(byte) = uart::try_getc() {
            if byte == CTRL_C {
                pressed = true;
            } else if self.typeahead.len() < TYPEAHEAD_LIMIT {
                self.typeahead.push_back(byte);
            }
        }
        pressed
    }

    /// The next received byte: typeahead first, then wait on the UART.
    fn next_byte(&mut self) -> u8 {
        self.typeahead.pop_front().unwrap_or_else(uart::getc)
    }

    /// Block until the user finishes a line (Enter) or presses Ctrl-C, echoing
    /// as they type.
    pub fn read_line(&mut self) -> Line<'_> {
        loop {
            let byte = self.next_byte();
            let event = self.editor.feed(byte, &mut |bytes| {
                for &b in bytes {
                    uart::putc(b);
                }
            });
            match event {
                Event::Pending => {}
                Event::Break => return Line::Break,
                // The editor only ever holds printable ASCII, so this cannot fail.
                Event::Line => {
                    return Line::Text(core::str::from_utf8(self.editor.line()).unwrap_or(""))
                }
            }
        }
    }
}
