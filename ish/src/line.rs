//! The line editor of the interactive shell (SHELL-SPEC section 9, step SH5): a pure state machine that turns the bytes a
//! terminal in raw mode sends into the bytes to write back and the events the shell acts on. No I/O and no machine
//! knowledge, so the whole behaviour is tested on the host against exact byte streams.
//!
//! It works on a dumb serial console: the only escape sequences it writes are cursor left (`ESC [ n D`), cursor right
//! (`ESC [ n C`) and erase to end of line (`ESC [ K`), plus `ESC [ 2 J ESC [ H` for Ctrl-L. It never asks the terminal where
//! the cursor is, and it assumes every character is one column wide.
//!
//! Keys: printable characters (UTF-8) insert at the cursor; Backspace (0x7f, 0x08), Delete (`ESC [ 3 ~`, Ctrl-D in the
//! middle of a line), Left/Right (`ESC [ D`/`C`, Ctrl-B/F), Home/End (`ESC [ H`/`F`, `ESC [ 1 ~`/`4 ~`, `ESC O H`/`F`, Ctrl-A/E),
//! Up/Down (`ESC [ A`/`B`, Ctrl-P/N) walk the history, Ctrl-K kills to the end, Ctrl-U to the start, Ctrl-W the word before the
//! cursor, Ctrl-Y yanks what was killed, Ctrl-L redraws, Tab completes, Enter (CR, LF or CRLF) accepts the line, Ctrl-D on an
//! empty line is end of input and Ctrl-C abandons the line. Anything else is ignored; an escape sequence that is not known is
//! swallowed whole.

use alloc::string::String;
use alloc::vec::Vec;

/// What a chunk of input made the shell do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A line was accepted (without its newline).
    Line(String),
    /// Ctrl-D at an empty line.
    Eof,
    /// Ctrl-C: the line was abandoned (`^C` was echoed and the buffer is empty).
    Interrupt,
}

/// The answer of a completer: the characters of the buffer from `start` to the cursor are the word being completed, and
/// `items` are the whole replacements for it (a directory ends with `/`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Completion {
    pub start: usize,
    pub items: Vec<String>,
}

/// The result of feeding bytes: what to write to the terminal, and the events in order.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Fed {
    pub out: Vec<u8>,
    pub events: Vec<Event>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Esc {
    None,
    /// Saw ESC.
    Escape,
    /// Saw `ESC [`; the parameter bytes so far are in `params`.
    Csi,
    /// Saw `ESC O`.
    Ss3,
}

pub const MAX_HISTORY: usize = 200;
pub const MAX_LINE: usize = 4096;

pub struct LineEditor {
    prompt: String,
    buf: Vec<char>,
    cur: usize,
    history: Vec<String>,
    /// Browsing: the index into `history`, and what was being typed.
    browse: Option<(usize, Vec<char>)>,
    killed: Vec<char>,
    esc: Esc,
    params: Vec<u8>,
    /// UTF-8 bytes of a character in progress.
    pending: Vec<u8>,
    /// The last byte was a CR (so a following LF is part of the same Enter).
    after_cr: bool,
    /// A Tab that made no progress (a second one lists the candidates).
    tab_again: bool,
}

impl LineEditor {
    pub fn new(prompt: &str) -> LineEditor {
        LineEditor {
            prompt: String::from(prompt),
            buf: Vec::new(),
            cur: 0,
            history: Vec::new(),
            browse: None,
            killed: Vec::new(),
            esc: Esc::None,
            params: Vec::new(),
            pending: Vec::new(),
            after_cr: false,
            tab_again: false,
        }
    }

    /// Change the prompt (a continuation line); the next [`LineEditor::begin`] prints it.
    pub fn set_prompt(&mut self, p: &str) {
        self.prompt = String::from(p);
    }

    /// Start a line: the bytes that print the prompt. The buffer is empty.
    pub fn begin(&mut self) -> Vec<u8> {
        self.buf.clear();
        self.cur = 0;
        self.browse = None;
        self.esc = Esc::None;
        self.pending.clear();
        self.tab_again = false;
        self.prompt.as_bytes().to_vec()
    }

    /// The text of the line so far.
    pub fn text(&self) -> String {
        self.buf.iter().collect()
    }

    pub fn cursor(&self) -> usize {
        self.cur
    }

    /// Remember a command in the history (not an empty one, nor a repeat of the last).
    pub fn remember(&mut self, line: &str) {
        if line.trim().is_empty() || self.history.last().is_some_and(|l| l == line) {
            return;
        }
        if self.history.len() >= MAX_HISTORY {
            self.history.remove(0);
        }
        self.history.push(String::from(line));
    }

    pub fn history(&self) -> &[String] {
        &self.history
    }

    // ---- rendering ----

    fn left(out: &mut Vec<u8>, n: usize) {
        if n > 0 {
            out.extend_from_slice(alloc::format!("\x1b[{n}D").as_bytes());
        }
    }

    /// Move left after erasing: a plain backspace for one column (what the console's own editor does), else the sequence.
    fn back(out: &mut Vec<u8>, n: usize) {
        if n == 1 {
            out.push(0x08);
        } else {
            Self::left(out, n);
        }
    }

    fn right(out: &mut Vec<u8>, n: usize) {
        if n > 0 {
            out.extend_from_slice(alloc::format!("\x1b[{n}C").as_bytes());
        }
    }

    fn put(out: &mut Vec<u8>, chars: &[char]) {
        let s: String = chars.iter().collect();
        out.extend_from_slice(s.as_bytes());
    }

    /// Replace the whole line by `new`, the cursor ending at its end.
    fn replace_line(&mut self, out: &mut Vec<u8>, new: Vec<char>) {
        Self::left(out, self.cur);
        Self::put(out, &new);
        if new.len() < self.buf.len() {
            out.extend_from_slice(b"\x1b[K");
        }
        self.buf = new;
        self.cur = self.buf.len();
    }

    fn insert(&mut self, out: &mut Vec<u8>, c: char) {
        if self.buf.len() >= MAX_LINE {
            return;
        }
        self.buf.insert(self.cur, c);
        self.cur += 1;
        Self::put(out, &self.buf[self.cur - 1..]);
        Self::left(out, self.buf.len() - self.cur);
    }

    fn delete_at(&mut self, out: &mut Vec<u8>, at: usize) {
        // remove the character at index `at` (the cursor is after it when it was a backspace)
        self.buf.remove(at);
        if at < self.cur {
            self.cur -= 1;
            out.push(0x08);
        }
        Self::put(out, &self.buf[self.cur..]);
        out.push(b' ');
        Self::back(out, self.buf.len() - self.cur + 1);
    }

    fn kill_range(&mut self, out: &mut Vec<u8>, from: usize, to: usize) {
        if from >= to {
            return;
        }
        self.killed = self.buf[from..to].to_vec();
        if self.cur > from {
            Self::left(out, self.cur - from);
        } else {
            Self::right(out, from - self.cur);
        }
        self.buf.drain(from..to);
        self.cur = from;
        Self::put(out, &self.buf[from..]);
        out.extend_from_slice(b"\x1b[K");
        Self::left(out, self.buf.len() - from);
    }

    // ---- input ----

    /// Feed bytes from the terminal. `complete` is asked for candidates when Tab is pressed; it gets the text before the
    /// cursor and returns the word being completed.
    pub fn feed(&mut self, bytes: &[u8], complete: &mut dyn FnMut(&str) -> Completion) -> Fed {
        let mut fed = Fed::default();
        for &b in bytes {
            self.byte(b, complete, &mut fed);
        }
        fed
    }

    fn byte(&mut self, b: u8, complete: &mut dyn FnMut(&str) -> Completion, fed: &mut Fed) {
        match self.esc {
            Esc::Escape => {
                match b {
                    b'[' => {
                        self.esc = Esc::Csi;
                        self.params.clear();
                    }
                    b'O' => self.esc = Esc::Ss3,
                    _ => self.esc = Esc::None, // ESC and one more byte: not a key we know
                }
                return;
            }
            Esc::Csi => {
                if (0x30..=0x3f).contains(&b) {
                    if self.params.len() < 16 {
                        self.params.push(b);
                    }
                } else if (0x20..=0x2f).contains(&b) {
                    // intermediate bytes: keep reading
                } else {
                    self.esc = Esc::None;
                    let params = core::mem::take(&mut self.params);
                    self.csi(b, &params, &mut fed.out);
                }
                return;
            }
            Esc::Ss3 => {
                self.esc = Esc::None;
                match b {
                    b'H' => self.home(&mut fed.out),
                    b'F' => self.end(&mut fed.out),
                    b'A' => self.history_up(&mut fed.out),
                    b'B' => self.history_down(&mut fed.out),
                    b'C' => self.move_right(&mut fed.out),
                    b'D' => self.move_left(&mut fed.out),
                    _ => {}
                }
                return;
            }
            Esc::None => {}
        }
        // a continuation byte of a UTF-8 character in progress
        if !self.pending.is_empty() {
            if (0x80..=0xbf).contains(&b) {
                self.pending.push(b);
                let need = utf8_len(self.pending[0]);
                if self.pending.len() == need {
                    let bytes = core::mem::take(&mut self.pending);
                    if let Some(c) = core::str::from_utf8(&bytes)
                        .ok()
                        .and_then(|s| s.chars().next())
                    {
                        self.after_cr = false;
                        self.tab_again = false;
                        self.insert(&mut fed.out, c);
                    }
                }
                return;
            }
            self.pending.clear(); // a broken character: drop it and read this byte afresh
        }
        let was_cr = core::mem::replace(&mut self.after_cr, false);
        if b != 0x09 {
            self.tab_again = false;
        }
        match b {
            0x1b => self.esc = Esc::Escape,
            0x0a if was_cr => {} // the LF of a CRLF
            0x0d | 0x0a => {
                self.after_cr = b == 0x0d;
                let line = self.text();
                // end the line on the terminal (past any text after the cursor)
                Self::right(&mut fed.out, self.buf.len() - self.cur);
                fed.out.push(b'\n');
                self.buf.clear();
                self.cur = 0;
                self.browse = None;
                fed.events.push(Event::Line(line));
            }
            0x01 => self.home(&mut fed.out),
            0x02 => self.move_left(&mut fed.out),
            0x03 => {
                fed.out.extend_from_slice(b"^C\n");
                self.buf.clear();
                self.cur = 0;
                self.browse = None;
                fed.events.push(Event::Interrupt);
            }
            0x04 => {
                if self.buf.is_empty() {
                    fed.events.push(Event::Eof);
                } else if self.cur < self.buf.len() {
                    self.delete_at(&mut fed.out, self.cur);
                }
            }
            0x05 => self.end(&mut fed.out),
            0x06 => self.move_right(&mut fed.out),
            0x07 | 0x00 => {}
            0x08 | 0x7f => {
                if self.cur > 0 {
                    self.delete_at(&mut fed.out, self.cur - 1);
                }
            }
            0x09 => self.tab(complete, &mut fed.out),
            0x0b => {
                let (c, n) = (self.cur, self.buf.len());
                self.kill_range(&mut fed.out, c, n);
            }
            0x0c => {
                fed.out.extend_from_slice(b"\x1b[2J\x1b[H");
                fed.out.extend_from_slice(self.prompt.as_bytes());
                Self::put(&mut fed.out, &self.buf);
                Self::left(&mut fed.out, self.buf.len() - self.cur);
            }
            0x0e => self.history_down(&mut fed.out),
            0x10 => self.history_up(&mut fed.out),
            0x15 => {
                let c = self.cur;
                self.kill_range(&mut fed.out, 0, c);
            }
            0x17 => {
                let mut i = self.cur;
                while i > 0 && self.buf[i - 1] == ' ' {
                    i -= 1;
                }
                while i > 0 && self.buf[i - 1] != ' ' {
                    i -= 1;
                }
                let c = self.cur;
                self.kill_range(&mut fed.out, i, c);
            }
            0x19 => {
                let k = self.killed.clone();
                for c in k {
                    self.insert(&mut fed.out, c);
                }
            }
            0x20..=0x7e => self.insert(&mut fed.out, b as char),
            0xc2..=0xf4 => self.pending.push(b),
            _ => {}
        }
    }

    fn csi(&mut self, fin: u8, params: &[u8], out: &mut Vec<u8>) {
        match (fin, params) {
            (b'A', _) => self.history_up(out),
            (b'B', _) => self.history_down(out),
            (b'C', _) => self.move_right(out),
            (b'D', _) => self.move_left(out),
            (b'H', _) => self.home(out),
            (b'F', _) => self.end(out),
            (b'~', b"1" | b"7") => self.home(out),
            (b'~', b"4" | b"8") => self.end(out),
            (b'~', b"3") if self.cur < self.buf.len() => self.delete_at(out, self.cur),
            _ => {} // an unknown sequence is swallowed
        }
    }

    fn move_left(&mut self, out: &mut Vec<u8>) {
        if self.cur > 0 {
            self.cur -= 1;
            Self::left(out, 1);
        }
    }

    fn move_right(&mut self, out: &mut Vec<u8>) {
        if self.cur < self.buf.len() {
            self.cur += 1;
            Self::right(out, 1);
        }
    }

    fn home(&mut self, out: &mut Vec<u8>) {
        Self::left(out, self.cur);
        self.cur = 0;
    }

    fn end(&mut self, out: &mut Vec<u8>) {
        Self::right(out, self.buf.len() - self.cur);
        self.cur = self.buf.len();
    }

    fn history_up(&mut self, out: &mut Vec<u8>) {
        let idx = match &self.browse {
            None if self.history.is_empty() => return,
            None => {
                self.browse = Some((self.history.len() - 1, self.buf.clone()));
                self.history.len() - 1
            }
            Some((0, _)) => return,
            Some((i, saved)) => {
                let (i, saved) = (*i - 1, saved.clone());
                self.browse = Some((i, saved));
                i
            }
        };
        let new: Vec<char> = self.history[idx].chars().collect();
        self.replace_line(out, new);
    }

    fn history_down(&mut self, out: &mut Vec<u8>) {
        let Some((i, saved)) = self.browse.clone() else {
            return;
        };
        if i + 1 < self.history.len() {
            self.browse = Some((i + 1, saved));
            let new: Vec<char> = self.history[i + 1].chars().collect();
            self.replace_line(out, new);
        } else {
            self.browse = None;
            self.replace_line(out, saved);
        }
    }

    fn tab(&mut self, complete: &mut dyn FnMut(&str) -> Completion, out: &mut Vec<u8>) {
        let before: String = self.buf[..self.cur].iter().collect();
        let c = complete(&before);
        if c.items.is_empty() || c.start > self.cur {
            return;
        }
        let word: Vec<char> = self.buf[c.start..self.cur].to_vec();
        let common = common_prefix(&c.items);
        let progress = common.chars().count() > word.len();
        if progress {
            let mut add: Vec<char> = common.chars().skip(word.len()).collect();
            // a single candidate is finished off: a space after a file, nothing after a directory
            if c.items.len() == 1 && !common.ends_with('/') {
                add.push(' ');
            }
            for ch in add {
                self.insert(out, ch);
            }
            self.tab_again = false;
            return;
        }
        if c.items.len() == 1 {
            if !common.ends_with('/') && self.buf.get(self.cur) != Some(&' ') {
                self.insert(out, ' ');
            }
            return;
        }
        if !self.tab_again {
            self.tab_again = true;
            return;
        }
        // a second Tab with nothing to add: list the candidates under the line, then redraw it
        Self::right(out, self.buf.len() - self.cur);
        out.push(b'\n');
        let mut first = true;
        for it in &c.items {
            if !first {
                out.extend_from_slice(b"  ");
            }
            first = false;
            out.extend_from_slice(it.as_bytes());
        }
        out.push(b'\n');
        out.extend_from_slice(self.prompt.as_bytes());
        Self::put(out, &self.buf);
        Self::left(out, self.buf.len() - self.cur);
        self.tab_again = false;
    }
}

fn utf8_len(first: u8) -> usize {
    match first {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

fn common_prefix(items: &[String]) -> String {
    let mut it = items.iter();
    let Some(first) = it.next() else {
        return String::new();
    };
    let mut p: Vec<char> = first.chars().collect();
    for s in it {
        let n = p
            .iter()
            .zip(s.chars())
            .take_while(|(a, b)| **a == *b)
            .count();
        p.truncate(n);
    }
    p.into_iter().collect()
}
