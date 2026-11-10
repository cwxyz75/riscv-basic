use alloc::string::String;

/// What the console handed back when the interpreter asked for a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// A finished line of text, without the line terminator. Line editing
    /// (backspace, echo, ...) is the `Io` implementation's job, not ours.
    Text(String),
    /// The user pressed BREAK (Ctrl-C) while the line was being typed.
    Break,
    /// No more input. Never happens on real hardware; lets scripted tests end.
    Eof,
}

/// Everything the interpreter needs from the outside world.
///
/// The kernel implements this on top of the UART; tests implement it with
/// in-memory buffers. Screen primitives (CLS, COLOR, LOCATE) and PEEK/POKE
/// access will be added as default or required methods in later milestones.
pub trait Io {
    /// Write text to the console.
    fn write_str(&mut self, s: &str);

    /// Block until a full line is available.
    fn read_line(&mut self) -> Line;

    /// Non-blocking check: has the user pressed BREAK since the last call?
    /// Polled by `RUN` between statements.
    fn break_pressed(&mut self) -> bool;

    /// Milliseconds since boot, for `TIMER`.
    fn millis(&mut self) -> u64;

    /// Reboot the machine (`BYE`). On real hardware this does not return; the
    /// interpreter ends the current run right after calling it, for the sake of
    /// implementations that do (such as the test console).
    fn reboot(&mut self);

    /// Clear the screen and home the cursor (`CLS`). The default sends the ANSI
    /// escape sequence; an implementation with a real screen may override it.
    fn clear_screen(&mut self) {
        let mut text = String::new();
        let _ = console::ansi::clear_screen(&mut text);
        self.write_str(&text);
    }

    /// Set the text colour and optionally the background (`COLOR`), each 0 to 7.
    fn set_color(&mut self, fg: u8, bg: Option<u8>) {
        let mut text = String::new();
        let _ = console::ansi::color(&mut text, fg, bg);
        self.write_str(&text);
    }

    /// Move the cursor to `row`, `col`, both from 1 (`LOCATE`).
    fn move_cursor(&mut self, row: u16, col: u16) {
        let mut text = String::new();
        let _ = console::ansi::cursor_to(&mut text, row, col);
        self.write_str(&text);
    }

    /// Read one byte from physical address `addr` (`PEEK`). Unchecked: the
    /// interpreter has validated the number, nothing more.
    fn peek(&mut self, addr: u64) -> u8;

    /// Write one byte to physical address `addr` (`POKE`). Unchecked.
    fn poke(&mut self, addr: u64, value: u8);
}
