//! The line editor: a byte-at-a-time state machine. Echo and editing keys follow
//! the "Console model" in `DESIGN.md` and the session rules in `BASIC.md`.

/// Longest line the editor will hold; further characters are ignored.
pub const MAX_LEN: usize = 255;

const BS: u8 = 0x08;
const DEL: u8 = 0x7f;
const CTRL_C: u8 = 0x03;
const CTRL_U: u8 = 0x15;
const ESC: u8 = 0x1b;
const CR: u8 = b'\r';
const LF: u8 = b'\n';

/// Rub out the character before the cursor on a terminal.
const ERASE: &[u8] = b"\x08 \x08";

/// What the last byte fed to the editor meant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Still editing; nothing for the caller to do.
    Pending,
    /// The user pressed Enter; [`LineEditor::line`] holds the finished line.
    Line,
    /// The user pressed Ctrl-C; the line being typed was abandoned.
    Break,
}

/// Where we are inside a terminal escape sequence (arrow keys and friends),
/// which must be swallowed rather than typed into the line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Escape {
    None,
    /// Saw ESC; the next byte says which kind of sequence follows.
    Seen,
    /// Inside `ESC [ ...`: skip until the final byte (0x40 to 0x7e).
    Csi,
    /// Inside `ESC O x`: skip one more byte.
    Ss3,
}

#[derive(Debug, Clone)]
pub struct LineEditor {
    buf: [u8; MAX_LEN],
    len: usize,
    /// The previous event ended a line; the next byte starts a new one.
    finished: bool,
    /// The previous byte was CR, so a directly following LF is part of it.
    after_cr: bool,
    escape: Escape,
}

impl Default for LineEditor {
    fn default() -> Self {
        Self::new()
    }
}

impl LineEditor {
    pub const fn new() -> Self {
        LineEditor {
            buf: [0; MAX_LEN],
            len: 0,
            finished: false,
            after_cr: false,
            escape: Escape::None,
        }
    }

    /// The text typed so far. After [`Event::Line`] it is the finished line
    /// (valid until the next byte is fed); after [`Event::Break`] it is empty.
    /// Always printable ASCII.
    pub fn line(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// Feed one received byte. Anything the terminal should display is passed
    /// to `echo`, possibly several times, in order.
    pub fn feed(&mut self, byte: u8, echo: &mut dyn FnMut(&[u8])) -> Event {
        if self.finished {
            self.len = 0;
            self.finished = false;
        }
        let was_cr = core::mem::replace(&mut self.after_cr, false);

        match self.escape {
            Escape::None => {}
            Escape::Seen => {
                self.escape = match byte {
                    b'[' => Escape::Csi,
                    b'O' => Escape::Ss3,
                    _ => Escape::None,
                };
                return Event::Pending;
            }
            Escape::Csi => {
                if (0x40..=0x7e).contains(&byte) {
                    self.escape = Escape::None;
                }
                return Event::Pending;
            }
            Escape::Ss3 => {
                self.escape = Escape::None;
                return Event::Pending;
            }
        }

        match byte {
            LF if was_cr => Event::Pending,
            CR | LF => {
                echo(b"\r\n");
                self.finished = true;
                self.after_cr = byte == CR;
                Event::Line
            }
            BS | DEL => {
                if self.len > 0 {
                    self.len -= 1;
                    echo(ERASE);
                }
                Event::Pending
            }
            CTRL_U => {
                for _ in 0..self.len {
                    echo(ERASE);
                }
                self.len = 0;
                Event::Pending
            }
            CTRL_C => {
                echo(b"^C\r\n");
                self.len = 0;
                Event::Break
            }
            ESC => {
                self.escape = Escape::Seen;
                Event::Pending
            }
            0x20..=0x7e => {
                if self.len < MAX_LEN {
                    self.buf[self.len] = byte;
                    self.len += 1;
                    echo(&[byte]);
                }
                Event::Pending
            }
            // Other control characters and everything outside ASCII are ignored.
            _ => Event::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::string::String;
    use std::vec::Vec;

    /// Feed `bytes`; return the events that were not `Pending`, the lines
    /// finished (in order), and everything echoed.
    fn run(ed: &mut LineEditor, bytes: &[u8]) -> (Vec<Event>, Vec<String>, Vec<u8>) {
        let mut events = Vec::new();
        let mut lines = Vec::new();
        let mut echoed = Vec::new();
        for &b in bytes {
            match ed.feed(b, &mut |s| echoed.extend_from_slice(s)) {
                Event::Pending => {}
                Event::Line => {
                    events.push(Event::Line);
                    lines.push(String::from_utf8(ed.line().to_vec()).unwrap());
                }
                Event::Break => events.push(Event::Break),
            }
        }
        (events, lines, echoed)
    }

    #[test]
    fn plain_line_is_echoed_with_crlf() {
        let (events, lines, echo) = run(&mut LineEditor::new(), b"hi\r");
        assert_eq!(events, [Event::Line]);
        assert_eq!(lines, ["hi"]);
        assert_eq!(echo, b"hi\r\n");
    }

    #[test]
    fn backspace_and_delete_rub_out() {
        let (_, lines, echo) = run(&mut LineEditor::new(), b"AB\x7fC\x08D\r");
        assert_eq!(lines, ["AD"]);
        assert_eq!(echo, b"AB\x08 \x08C\x08 \x08D\r\n");
    }

    #[test]
    fn backspace_on_empty_line_does_nothing() {
        let (_, lines, echo) = run(&mut LineEditor::new(), b"\x7f\x08z\r");
        assert_eq!(lines, ["z"]);
        assert_eq!(echo, b"z\r\n");
    }

    #[test]
    fn ctrl_u_erases_the_whole_line() {
        let (_, lines, echo) = run(&mut LineEditor::new(), b"xyz\x15Q\r");
        assert_eq!(lines, ["Q"]);
        assert_eq!(echo, b"xyz\x08 \x08\x08 \x08\x08 \x08Q\r\n");
    }

    #[test]
    fn ctrl_c_abandons_the_line() {
        let mut ed = LineEditor::new();
        let (events, _, echo) = run(&mut ed, b"abc\x03");
        assert_eq!(events, [Event::Break]);
        assert_eq!(echo, b"abc^C\r\n");
        assert_eq!(ed.line(), b"");
        let (_, lines, _) = run(&mut ed, b"ok\r");
        assert_eq!(lines, ["ok"]);
    }

    #[test]
    fn crlf_and_lone_lf_each_end_one_line() {
        let (events, lines, _) = run(&mut LineEditor::new(), b"x\r\ny\nz\r");
        assert_eq!(events, [Event::Line; 3]);
        assert_eq!(lines, ["x", "y", "z"]);
    }

    #[test]
    fn cr_cr_is_two_empty_lines() {
        let (_, lines, _) = run(&mut LineEditor::new(), b"\r\r");
        assert_eq!(lines, ["", ""]);
    }

    #[test]
    fn escape_sequences_are_swallowed() {
        let (_, lines, echo) = run(&mut LineEditor::new(), b"a\x1b[Ab\x1b[1;5Cc\x1bOPd\x1bxe\r");
        assert_eq!(lines, ["abcde"]);
        assert_eq!(echo, b"abcde\r\n");
    }

    #[test]
    fn line_is_limited_to_max_len() {
        let mut input = std::vec![b'a'; MAX_LEN + 45];
        input.push(b'\r');
        let (_, lines, echo) = run(&mut LineEditor::new(), &input);
        assert_eq!(lines[0].len(), MAX_LEN);
        assert_eq!(echo.len(), MAX_LEN + 2);
    }

    #[test]
    fn non_ascii_and_stray_controls_are_ignored() {
        let (_, lines, echo) = run(&mut LineEditor::new(), b"a\xc3\xa9\x01\x07b\r");
        assert_eq!(lines, ["ab"]);
        assert_eq!(echo, b"ab\r\n");
    }

    #[test]
    fn editor_is_reusable_after_a_line() {
        let mut ed = LineEditor::new();
        let (_, first, _) = run(&mut ed, b"one\r");
        let (_, second, _) = run(&mut ed, b"two\r");
        assert_eq!((first[0].as_str(), second[0].as_str()), ("one", "two"));
    }
}
