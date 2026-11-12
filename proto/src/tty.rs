//! The line discipline of the console server (S4-SPEC section 8), as a pure state machine:
//! `consoled` feeds it serial bytes and client output and does the I/O itself.
//!
//! - Serial input goes through `console::LineEditor` unchanged (echo, backspace, Ctrl-U,
//!   Ctrl-C, at most 255 bytes per line). Each finished line is delivered to the client with
//!   its `\n`.
//! - **Ctrl-D (0x04)** at an empty line delivers the single byte `0x04` (the shell treats it
//!   as `exit`); in the middle of a line it is ignored.
//! - **Ctrl-C** asks the caller to kill the foreground set (`Input::kill`), discards the rest
//!   of the bytes that arrived with it (type-ahead), echoes `^C\r\n` and delivers the single
//!   byte `0x03` so the shell can reprint its prompt.
//! - Client output has `\n` expanded to `\r\n` and is queued for the serial line in a bounded
//!   queue: when it is full the *oldest* bytes are dropped and counted; once the queue has
//!   drained, a one-line notice reports the count.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::vec::Vec;
use console::{Event, LineEditor};

/// What a chunk of serial input asks the server to do.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Input {
    /// Bytes to send to the client, in order (lines with their `\n`, `0x04`, `0x03`).
    pub deliver: Vec<u8>,
    /// Ctrl-C was seen: kill the foreground set.
    pub kill: bool,
}

pub struct Tty {
    editor: LineEditor,
    /// Nothing is being edited (the line is empty), so Ctrl-D means end of input.
    fresh: bool,
    out: VecDeque<u8>,
    cap: usize,
    dropped: u64,
    dropped_total: u64,
}

impl Tty {
    /// `cap` is the most bytes held for the serial line.
    pub fn new(cap: usize) -> Tty {
        Tty {
            editor: LineEditor::new(),
            fresh: true,
            out: VecDeque::new(),
            cap,
            dropped: 0,
            dropped_total: 0,
        }
    }

    fn push_out(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if self.out.len() >= self.cap {
                self.out.pop_front();
                self.dropped += 1;
                self.dropped_total += 1;
            }
            self.out.push_back(b);
        }
    }

    /// Bytes read from the serial line.
    pub fn serial_in(&mut self, bytes: &[u8]) -> Input {
        let mut input = Input::default();
        for &b in bytes {
            if b == 0x04 {
                if self.fresh {
                    input.deliver.push(0x04);
                }
                continue;
            }
            let mut echo: Vec<u8> = Vec::new();
            let ev = self.editor.feed(b, &mut |s| echo.extend_from_slice(s));
            self.push_out(&echo);
            match ev {
                Event::Pending => self.fresh = self.editor.line().is_empty(),
                Event::Line => {
                    input.deliver.extend_from_slice(self.editor.line());
                    input.deliver.push(b'\n');
                    self.fresh = true;
                }
                Event::Break => {
                    input.kill = true;
                    input.deliver.push(0x03);
                    self.fresh = true;
                    // type-ahead behind the Ctrl-C is discarded
                    return input;
                }
            }
        }
        input
    }

    /// Output written by the client: `\n` becomes `\r\n`.
    pub fn client_out(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if b == b'\n' {
                self.push_out(b"\r\n");
            } else {
                self.push_out(&[b]);
            }
        }
    }

    /// Take up to `max` bytes for the serial line. When the queue is empty and bytes were
    /// dropped, the notice goes out first.
    pub fn take_out(&mut self, max: usize) -> Vec<u8> {
        if self.out.is_empty() && self.dropped > 0 {
            let notice = format!("\r\n[console: {} bytes dropped]\r\n", self.dropped);
            self.dropped = 0;
            // the notice is not itself droppable (it is short and the queue is empty)
            self.out.extend(notice.bytes());
        }
        let n = max.min(self.out.len());
        self.out.drain(..n).collect()
    }

    pub fn has_output(&self) -> bool {
        !self.out.is_empty() || self.dropped > 0
    }

    /// Bytes dropped since the start.
    pub fn dropped_total(&self) -> u64 {
        self.dropped_total
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::string::String;

    fn drain(t: &mut Tty) -> String {
        let mut s = Vec::new();
        while t.has_output() {
            s.extend(t.take_out(7)); // small chunks: chunking must not matter
        }
        String::from_utf8(s).unwrap()
    }

    #[test]
    fn a_line_is_echoed_and_delivered_with_its_newline() {
        let mut t = Tty::new(1024);
        let i = t.serial_in(b"ls -l\r");
        assert_eq!(i.deliver, b"ls -l\n");
        assert!(!i.kill);
        assert_eq!(drain(&mut t), "ls -l\r\n");
    }

    #[test]
    fn crlf_and_lf_each_end_one_line() {
        let mut t = Tty::new(1024);
        assert_eq!(t.serial_in(b"a\r\nb\nc\r").deliver, b"a\nb\nc\n");
    }

    #[test]
    fn a_line_split_across_reads_is_delivered_once_complete() {
        let mut t = Tty::new(1024);
        assert_eq!(t.serial_in(b"he").deliver, b"");
        assert_eq!(t.serial_in(b"llo").deliver, b"");
        assert_eq!(t.serial_in(b"\r").deliver, b"hello\n");
    }

    #[test]
    fn editing_keys_work() {
        let mut t = Tty::new(1024);
        let i = t.serial_in(b"abx\x7fc\r");
        assert_eq!(i.deliver, b"abc\n");
        assert_eq!(drain(&mut t), "abx\x08 \x08c\r\n");
        let mut t = Tty::new(1024);
        assert_eq!(t.serial_in(b"junk\x15ok\r").deliver, b"ok\n");
    }

    #[test]
    fn escape_sequences_are_swallowed() {
        let mut t = Tty::new(1024);
        assert_eq!(t.serial_in(b"a\x1b[Ab\x1bOPc\r").deliver, b"abc\n");
    }

    #[test]
    fn a_line_is_capped_at_255_bytes() {
        let mut t = Tty::new(4096);
        let mut input = std::vec![b'x'; 300];
        input.push(b'\r');
        let i = t.serial_in(&input);
        assert_eq!(i.deliver.len(), 256);
        assert_eq!(*i.deliver.last().unwrap(), b'\n');
    }

    #[test]
    fn ctrl_d_on_an_empty_line_delivers_one_eof_byte() {
        let mut t = Tty::new(1024);
        let i = t.serial_in(b"\x04");
        assert_eq!(i.deliver, [0x04]);
        assert_eq!(drain(&mut t), ""); // no echo
                                       // also right after a finished line
        assert_eq!(t.serial_in(b"x\r\x04").deliver, b"x\n\x04");
        // and after a line that was erased down to nothing
        assert_eq!(t.serial_in(b"ab\x7f\x7f\x04").deliver, [0x04]);
    }

    #[test]
    fn ctrl_d_in_the_middle_of_a_line_is_ignored() {
        let mut t = Tty::new(1024);
        assert_eq!(t.serial_in(b"ab\x04c\r").deliver, b"abc\n");
    }

    #[test]
    fn ctrl_c_kills_discards_type_ahead_echoes_and_wakes_the_shell() {
        let mut t = Tty::new(1024);
        let i = t.serial_in(b"abc\x03def\rghi\r");
        assert!(i.kill);
        assert_eq!(i.deliver, [0x03]); // one byte, nothing from the type-ahead
        assert_eq!(drain(&mut t), "abc^C\r\n");
        // the editor is clean afterwards
        assert_eq!(t.serial_in(b"ok\r").deliver, b"ok\n");
    }

    #[test]
    fn ctrl_c_at_an_empty_prompt_still_reports() {
        let mut t = Tty::new(1024);
        let i = t.serial_in(b"\x03");
        assert!(i.kill);
        assert_eq!(i.deliver, [0x03]);
    }

    #[test]
    fn ctrl_d_after_ctrl_c_is_end_of_input() {
        let mut t = Tty::new(1024);
        assert_eq!(t.serial_in(b"ab\x03").deliver, [0x03]);
        assert_eq!(t.serial_in(b"\x04").deliver, [0x04]);
    }

    #[test]
    fn client_output_gets_crlf() {
        let mut t = Tty::new(1024);
        t.client_out(b"one\ntwo\n\nend");
        assert_eq!(drain(&mut t), "one\r\ntwo\r\n\r\nend");
    }

    #[test]
    fn echo_and_client_output_interleave_in_arrival_order() {
        let mut t = Tty::new(1024);
        t.client_out(b"$ ");
        t.serial_in(b"hi\r");
        t.client_out(b"out\n");
        assert_eq!(drain(&mut t), "$ hi\r\nout\r\n");
    }

    #[test]
    fn a_full_queue_drops_the_oldest_and_reports_once_drained() {
        let mut t = Tty::new(10);
        t.client_out(b"0123456789ABCDE"); // 15 bytes, room for 10: "01234" dropped
        assert_eq!(t.dropped_total(), 5);
        assert_eq!(
            drain(&mut t),
            "56789ABCDE\r\n[console: 5 bytes dropped]\r\n"
        );
        assert!(!t.has_output());
        // the report is made once
        t.client_out(b"x");
        assert_eq!(drain(&mut t), "x");
    }

    #[test]
    fn the_queue_never_exceeds_its_cap_whatever_arrives() {
        let mut t = Tty::new(64);
        for _ in 0..1000 {
            t.client_out(b"flood\n");
            t.serial_in(b"abc\r");
        }
        let mut total = 0;
        while t.has_output() {
            let c = t.take_out(1000);
            assert!(c.len() <= 64 + 64); // the cap plus the notice
            total += c.len();
        }
        assert!(total > 0);
    }
}
