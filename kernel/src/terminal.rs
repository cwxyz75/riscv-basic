//! The serial console: formatted output, and line input through the editor in
//! the `console` crate.

use crate::uart;
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

/// Input side: owns the line editor.
pub struct Input {
    editor: LineEditor,
}

impl Input {
    pub const fn new() -> Self {
        Input {
            editor: LineEditor::new(),
        }
    }

    /// Block until the user finishes a line (Enter) or presses Ctrl-C, echoing
    /// as they type.
    pub fn read_line(&mut self) -> Line<'_> {
        loop {
            let byte = uart::getc();
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
