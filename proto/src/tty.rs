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
//! - **Modes** (step UV4): [`Tty::set_mode`] switches between cooked (the above), cbreak and raw (`console_mode` flags
//!   `ICANON`, `ECHO`, `ISIG`). Without `ICANON` every byte is delivered as it arrives, escape sequences included, with no
//!   editing; `ECHO` echoes it; with `ISIG` Ctrl-C is an interrupt (`Input::interrupt` and `kill`, no byte, type-ahead
//!   discarded) and without it the byte 0x03 is data. In canonical mode Ctrl-C always kills and delivers 0x03, as above.
//! - **Window size**: 80x24 until told otherwise ([`Tty::set_size`]); [`Tty::query_size`] asks the host terminal with a
//!   cursor-position report, and the `ESC [ rows ; cols R` answer is consumed (never delivered) only while a query is
//!   pending; anything else that starts like it is replayed as ordinary input.
//! - Client output has `\n` expanded to `\r\n` and is queued for the serial line in a bounded
//!   queue: when it is full the *oldest* bytes are dropped and counted; once the queue has
//!   drained, a one-line notice reports the count.

use crate::console::mode;
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
    /// Ctrl-C was seen with signals on and the terminal not canonical: tell the owner (no byte is delivered).
    pub interrupt: bool,
    /// Ctrl-\\ was seen with signals on: end the foreground set outright (SIGQUIT's key; the job is not asked, T149).
    pub quit: bool,
    /// A size report answered the pending query: `(cols, rows)`.
    pub resized: Option<(u16, u16)>,
}

/// What the cursor-position query writes: save the cursor, go to the far corner (clamped by the terminal to its real
/// size), ask where it is, restore.
pub const SIZE_QUERY: &[u8] = b"\x1b7\x1b[999;999H\x1b[6n\x1b8";

pub struct Tty {
    editor: LineEditor,
    /// Nothing is being edited (the line is empty), so Ctrl-D means end of input.
    fresh: bool,
    out: VecDeque<u8>,
    cap: usize,
    dropped: u64,
    dropped_total: u64,
    mode: u8,
    size: (u16, u16),
    query_pending: bool,
    /// The partial `ESC [ ...` being matched as a size report (only while a query is pending).
    report: Vec<u8>,
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
            mode: mode::COOKED,
            size: (80, 24),
            query_pending: false,
            report: Vec::new(),
        }
    }

    pub fn mode(&self) -> u8 {
        self.mode
    }

    /// Change the mode (undefined bits are ignored). Whatever was half-edited is dropped.
    pub fn set_mode(&mut self, m: u8) {
        let m = m & mode::ALL;
        if m != self.mode {
            self.mode = m;
            self.editor = LineEditor::new();
            self.fresh = true;
        }
    }

    /// `(cols, rows)`.
    pub fn size(&self) -> (u16, u16) {
        self.size
    }

    pub fn set_size(&mut self, cols: u16, rows: u16) {
        if cols != 0 && rows != 0 {
            self.size = (cols, rows);
        }
    }

    /// Ask the host terminal for its size; the answer shows up in a later `serial_in` as `Input::resized`.
    pub fn query_size(&mut self) {
        self.query_pending = true;
        self.report.clear();
        self.push_out(SIZE_QUERY);
    }

    /// Give up waiting for the answer (it did not come); the bytes held so far are ordinary input after all.
    pub fn cancel_query(&mut self) -> Input {
        let mut input = Input::default();
        self.query_pending = false;
        for b in core::mem::take(&mut self.report) {
            self.plain(b, &mut input);
        }
        input
    }

    pub fn query_pending(&self) -> bool {
        self.query_pending
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
            if input.kill || input.quit {
                break; // type-ahead behind a Ctrl-C or Ctrl-\\ is discarded
            }
            self.byte(b, &mut input);
        }
        input
    }

    /// One byte, through the size-report matcher first.
    fn byte(&mut self, b: u8, input: &mut Input) {
        if !self.query_pending {
            return self.plain(b, input);
        }
        let n = self.report.len();
        let fits = match (n, b) {
            (0, 0x1b) => true,
            (1, b'[') => true,
            (n, b'0'..=b'9') if n >= 2 => true,
            (n, b';') if n >= 3 && !self.report.contains(&b';') => true,
            _ => false,
        };
        if fits {
            self.report.push(b);
            return;
        }
        if n >= 5 && b == b'R' && self.report.contains(&b';') && self.report.last() != Some(&b';') {
            let body = &self.report[2..];
            let mut parts = body.split(|&c| c == b';');
            let num = |p: Option<&[u8]>| {
                p.unwrap_or(&[])
                    .iter()
                    .fold(0u32, |a, &d| (a * 10 + u32::from(d - b'0')).min(65_535))
            };
            let (rows, cols) = (num(parts.next()), num(parts.next()));
            self.report.clear();
            self.query_pending = false;
            if rows != 0 && cols != 0 {
                self.size = (cols as u16, rows as u16);
                input.resized = Some(self.size);
            }
            return;
        }
        // not a report after all: what was held is ordinary input, and so is this byte (which may start a new match)
        if n == 0 {
            return self.plain(b, input);
        }
        for x in core::mem::take(&mut self.report) {
            self.plain(x, input);
        }
        self.byte(b, input);
    }

    fn plain(&mut self, b: u8, input: &mut Input) {
        if input.kill || input.quit {
            return;
        }
        if b == 0x1c && self.mode & mode::ISIG != 0 {
            if self.mode & mode::ECHO != 0 {
                self.push_out(b"^\\\r\n");
            }
            // (a half-typed line is dropped, as for Ctrl-C)
            self.editor = LineEditor::new();
            self.fresh = true;
            input.quit = true;
            return;
        }
        if self.mode & mode::ICANON == 0 {
            if b == 0x03 && self.mode & mode::ISIG != 0 {
                if self.mode & mode::ECHO != 0 {
                    self.push_out(b"^C\r\n");
                }
                input.kill = true;
                input.interrupt = true;
                return;
            }
            input.deliver.push(b);
            if self.mode & mode::ECHO != 0 {
                if b == b'\r' || b == b'\n' {
                    self.push_out(b"\r\n");
                } else {
                    self.push_out(&[b]);
                }
            }
            return;
        }
        if b == 0x04 {
            if self.fresh {
                input.deliver.push(0x04);
            }
            return;
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
            }
        }
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
    fn any_serial_bytes_are_handled() {
        // every byte value in every position of a short stream, with a small queue
        let mut t = Tty::new(32);
        for a in 0..=255u8 {
            for b in 0..=255u8 {
                let i = t.serial_in(&[a, b, b'x', b'\r']);
                assert!(i.deliver.len() <= 8);
                while t.has_output() {
                    let _ = t.take_out(16);
                }
            }
        }
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

    // ---- modes, size, interrupts (UV4) ----

    fn raw() -> Tty {
        let mut t = Tty::new(1024);
        t.set_mode(mode::RAW);
        t
    }

    #[test]
    fn raw_mode_delivers_every_byte_at_once_and_does_not_echo() {
        let mut t = raw();
        let i = t.serial_in(b"a\x7f\x1b[A\x04\r\x15");
        assert_eq!(i.deliver, b"a\x7f\x1b[A\x04\r\x15");
        assert!(!i.kill && !i.interrupt);
        assert_eq!(drain(&mut t), "");
    }

    #[test]
    fn raw_mode_without_signals_delivers_ctrl_c_as_data() {
        let mut t = raw();
        let i = t.serial_in(b"x\x03y");
        assert_eq!(i.deliver, b"x\x03y");
        assert!(!i.kill && !i.interrupt);
    }

    #[test]
    fn cbreak_echoes_and_interrupts_without_a_byte() {
        let mut t = Tty::new(1024);
        t.set_mode(mode::CBREAK);
        let i = t.serial_in(b"ab\rc\x03def");
        assert_eq!(i.deliver, b"ab\rc"); // the interrupt delivers nothing, the type-ahead is dropped
        assert!(i.kill && i.interrupt);
        assert_eq!(drain(&mut t), "ab\r\nc^C\r\n");
        // and later input works
        assert_eq!(t.serial_in(b"z").deliver, b"z");
    }

    #[test]
    fn canonical_ctrl_c_is_not_an_interrupt_event() {
        let mut t = Tty::new(1024);
        let i = t.serial_in(b"\x03");
        assert!(i.kill && !i.interrupt);
        assert_eq!(i.deliver, [0x03]);
        assert_eq!(drain(&mut t), "^C\r\n");
        // canonical without ECHO or ISIG bits dropped is still line-edited (ICANON alone)
        t.set_mode(mode::ICANON);
        assert_eq!(t.serial_in(b"hi\r").deliver, b"hi\n");
        assert_eq!(drain(&mut t), "hi\r\n"); // the editor echoes in canonical mode
    }

    #[test]
    fn ctrl_backslash_is_a_quit_in_cooked_and_cbreak_and_data_without_signals() {
        // cooked: the key asks the console server to end the foreground set, echoes `^\`, delivers nothing and drops type-ahead
        let mut t = Tty::new(1024);
        let i = t.serial_in(b"ab\x1cxyz");
        assert!(i.quit && !i.kill && !i.interrupt);
        assert_eq!(i.deliver, b""); // the half-typed line is gone too
        assert_eq!(drain(&mut t), "ab^\\\r\n");
        assert_eq!(t.serial_in(b"ok\r").deliver, b"ok\n"); // and the editor works afterwards
                                                           // cbreak with signals on: the same
        let mut t = Tty::new(1024);
        t.set_mode(mode::CBREAK);
        let i = t.serial_in(b"a\x1cb");
        assert!(i.quit && !i.kill);
        assert_eq!(i.deliver, b"a");
        // raw, or ICANON alone: no ISIG, so the byte is data
        let mut t = raw();
        let i = t.serial_in(b"x\x1cy");
        assert!(!i.quit);
        assert_eq!(i.deliver, b"x\x1cy");
    }

    #[test]
    fn switching_modes_drops_a_half_edited_line_and_back_again_is_cooked() {
        let mut t = Tty::new(1024);
        t.serial_in(b"half");
        t.set_mode(mode::RAW);
        t.set_mode(mode::COOKED);
        assert_eq!(t.serial_in(b"ok\r").deliver, b"ok\n");
        t.set_mode(0xff); // undefined bits are ignored
        assert_eq!(t.mode(), mode::ALL);
    }

    #[test]
    fn the_default_size_is_80_by_24_and_set_size_refuses_zero() {
        let mut t = Tty::new(64);
        assert_eq!(t.size(), (80, 24));
        t.set_size(132, 43);
        assert_eq!(t.size(), (132, 43));
        t.set_size(0, 10);
        t.set_size(10, 0);
        assert_eq!(t.size(), (132, 43));
    }

    #[test]
    fn a_size_query_writes_the_probe_and_the_report_is_consumed() {
        let mut t = raw();
        t.query_size();
        assert_eq!(drain(&mut t).as_bytes(), SIZE_QUERY);
        let i = t.serial_in(b"\x1b[40;132R");
        assert_eq!(i.resized, Some((132, 40)));
        assert!(i.deliver.is_empty());
        assert_eq!(t.size(), (132, 40));
        assert!(!t.query_pending());
        // the same bytes with no query pending are ordinary input
        assert_eq!(t.serial_in(b"\x1b[41;133R").deliver, b"\x1b[41;133R");
        assert_eq!(t.size(), (132, 40));
    }

    #[test]
    fn a_report_split_across_reads_still_matches() {
        let mut t = raw();
        t.query_size();
        for part in [&b"\x1b"[..], b"[2", b"5;8", b"0"] {
            assert_eq!(t.serial_in(part), Input::default());
        }
        assert_eq!(t.serial_in(b"R").resized, Some((80, 25)));
    }

    #[test]
    fn a_non_report_escape_sequence_during_a_query_is_replayed_as_input() {
        let mut t = raw();
        t.query_size();
        let i = t.serial_in(b"\x1b[Ax");
        assert_eq!(i.deliver, b"\x1b[Ax");
        assert_eq!(i.resized, None);
        assert!(t.query_pending());
        // a malformed report, and a real one right behind it
        let i = t.serial_in(b"\x1b[1;;R\x1b[24;80R");
        assert_eq!(i.deliver, b"\x1b[1;;R");
        assert_eq!(i.resized, Some((80, 24)));
        // a lone ESC held when the query is cancelled comes out as input
        t.query_size();
        assert_eq!(t.serial_in(b"\x1b[2"), Input::default());
        let c = t.cancel_query();
        assert_eq!(c.deliver, b"\x1b[2");
        assert!(!t.query_pending());
    }

    #[test]
    fn a_zero_report_changes_nothing_but_ends_the_query() {
        let mut t = raw();
        t.query_size();
        let i = t.serial_in(b"\x1b[0;0R");
        assert_eq!(i.resized, None);
        assert!(i.deliver.is_empty());
        assert_eq!(t.size(), (80, 24));
        assert!(!t.query_pending());
    }

    #[test]
    fn huge_numbers_in_a_report_saturate_instead_of_overflowing() {
        let mut t = raw();
        t.query_size();
        let i = t.serial_in(b"\x1b[99999999999;1R");
        assert_eq!(i.resized, Some((1, 65535)));
    }

    #[test]
    fn any_bytes_in_any_mode_during_a_query_are_handled() {
        for m in 0..=mode::ALL {
            let mut t = Tty::new(64);
            t.set_mode(m);
            for a in 0..=255u8 {
                t.query_size();
                for b in [0x1b, b'[', b'1', b';', b'R', 0x03, a] {
                    let _ = t.serial_in(&[a, b, b'R']);
                }
                while t.has_output() {
                    let _ = t.take_out(64);
                }
            }
        }
    }
}
