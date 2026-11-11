//! Keeping verdict lines trustworthy on a shared serial line (S4-SPEC section 3).
//!
//! The kernel's structured output (`##TEST`, `##DONE`, `##PANIC`) and the bytes of user
//! programs share one UART. A program must neither *forge* a line that starts with `##` nor
//! *swallow* a real one by leaving an unterminated line in front of it. [`LineState`] is the one
//! piece of state both writers go through: it knows whether the physical line is at its start
//! and rewrites or separates accordingly. Pure, so the property is tested on the host with an
//! independent oracle.

/// Where the physical output line is.
#[derive(Debug, Clone, Copy, Default)]
pub struct LineState {
    /// Bytes emitted since the last line break.
    pos: usize,
    /// Was the first byte of the current line a `#`?
    first_hash: bool,
}

impl LineState {
    pub const fn new() -> LineState {
        LineState {
            pos: 0,
            first_hash: false,
        }
    }

    /// Is the output at the start of a line?
    pub fn at_line_start(&self) -> bool {
        self.pos == 0
    }

    fn advance(&mut self, b: u8) {
        if b == b'\n' || b == b'\r' {
            self.pos = 0;
            self.first_hash = false;
        } else {
            if self.pos == 0 {
                self.first_hash = b == b'#';
            }
            self.pos += 1;
        }
    }

    /// Emit one byte a *program* wrote. A second `#` directly after a line-start `#` is
    /// preceded by a space (`##` becomes `# #`), so no program line can begin with `##`.
    pub fn user_byte(&mut self, b: u8, out: &mut impl FnMut(u8)) {
        if self.pos == 1 && self.first_hash && b == b'#' {
            out(b' ');
            self.advance(b' ');
        }
        out(b);
        self.advance(b);
    }

    /// Emit text the *kernel* wrote. Kernel text that begins a verdict line (`##...`) is
    /// preceded by a line break if the line is not at its start, so an unterminated program
    /// line cannot swallow it.
    pub fn kernel_text(&mut self, s: &[u8], out: &mut impl FnMut(u8)) {
        if s.starts_with(b"##") && !self.at_line_start() {
            out(b'\r');
            self.advance(b'\r');
            out(b'\n');
            self.advance(b'\n');
        }
        for &b in s {
            out(b);
            self.advance(b);
        }
    }
}
