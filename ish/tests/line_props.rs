//! The line editor against an independent model of a terminal: whatever keys are typed, the bytes the editor writes, played
//! on a minimal terminal (characters, backspace, cursor left and right, erase to end of line), must leave exactly the
//! editor's text on the screen with the cursor where the editor says it is.

use ish::line::{Completion, Event, LineEditor};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// The screen: the editable part of one line (the prompt is not modelled), and the cursor in it.
#[derive(Default)]
struct Screen {
    line: Vec<char>,
    cur: usize,
}

impl Screen {
    fn play(&mut self, out: &[u8]) {
        let s = String::from_utf8_lossy(out).into_owned();
        let c: Vec<char> = s.chars().collect();
        let mut i = 0;
        while i < c.len() {
            match c[i] {
                '\x08' => self.cur = self.cur.saturating_sub(1),
                '\n' => {
                    self.line.clear();
                    self.cur = 0;
                }
                '\x1b' => {
                    // ESC [ params final
                    assert_eq!(
                        c.get(i + 1),
                        Some(&'['),
                        "only CSI sequences are written: {s:?}"
                    );
                    let mut j = i + 2;
                    let mut n = String::new();
                    while c[j].is_ascii_digit() {
                        n.push(c[j]);
                        j += 1;
                    }
                    let n: usize = n.parse().unwrap_or(1);
                    match c[j] {
                        'D' => self.cur = self.cur.saturating_sub(n),
                        'C' => self.cur += n,
                        'K' => self.line.truncate(self.cur),
                        f => panic!("an escape sequence the model does not know: ESC [ {f}"),
                    }
                    i = j;
                }
                ch => {
                    if self.cur < self.line.len() {
                        self.line[self.cur] = ch;
                    } else {
                        while self.line.len() < self.cur {
                            self.line.push(' ');
                        }
                        self.line.push(ch);
                    }
                    self.cur += 1;
                }
            }
            i += 1;
        }
    }
}

const KEYS: &[&[u8]] = &[
    b"a",
    b"b",
    b"c",
    b"x y",
    b" ",
    b"/",
    b"\xc3\xa9",
    b"\x7f",
    b"\x08",
    b"\x1b[D",
    b"\x1b[C",
    b"\x1b[H",
    b"\x1b[F",
    b"\x1b[3~",
    b"\x1b[1~",
    b"\x1b[4~",
    b"\x1bOH",
    b"\x1bOF",
    b"\x01",
    b"\x02",
    b"\x05",
    b"\x06",
    b"\x0b",
    b"\x15",
    b"\x17",
    b"\x19",
    b"\x04",
    b"\x1b[A",
    b"\x1b[B",
    b"\x10",
    b"\x0e",
    b"\x09",
    b"\x1b[2;5~",
    b"\x1bx",
    b"\x00",
    b"\x07",
];

#[test]
fn what_the_editor_writes_leaves_its_text_on_the_screen() {
    let mut r = Rng(0x5eed);
    let mut complete = |before: &str| -> Completion {
        // a completer with a fixed vocabulary
        let start = before.rfind(' ').map_or(0, |i| i + 1);
        let word = &before[start..];
        let items: Vec<String> = ["alpha", "alps", "beta/", "bet"]
            .iter()
            .filter(|w| w.starts_with(word))
            .map(|w| w.to_string())
            .collect();
        Completion {
            start: before[..start].chars().count(),
            items,
        }
    };
    for round in 0..300 {
        let mut ed = LineEditor::new("$ ");
        for h in ["echo first", "ls /", "x"] {
            ed.remember(h);
        }
        ed.begin();
        let mut screen = Screen::default();
        for step in 0..60 {
            let key = KEYS[r.below(KEYS.len())];
            let fed = ed.feed(key, &mut complete);
            // a listing of candidates (second Tab) prints lines and the prompt: replay only what the model understands
            if fed.out.windows(1).any(|w| w == b"\n") {
                // the candidate list: the screen is a new line holding the prompt and the buffer again
                screen = Screen::default();
                let text = ed.text();
                screen.line = text.chars().collect();
                screen.cur = ed.cursor();
                continue;
            }
            screen.play(&fed.out);
            // (a blank is what an erased column looks like: trailing blanks are not text)
            let shown: String = screen.line.iter().collect();
            assert_eq!(
                shown.trim_end_matches(' '),
                ed.text().trim_end_matches(' '),
                "round {round} step {step} after {key:?}"
            );
            assert_eq!(
                screen.cur,
                ed.cursor(),
                "round {round} step {step} after {key:?}: the cursor"
            );
            assert!(
                fed.events.is_empty() || fed.events == vec![Event::Eof],
                "{:?}",
                fed.events
            );
            if fed.events == vec![Event::Eof] {
                break;
            }
        }
    }
}

#[test]
fn any_bytes_are_survived() {
    let mut r = Rng(77);
    let mut none = |_: &str| Completion::default();
    for _ in 0..3000 {
        let mut ed = LineEditor::new("> ");
        ed.begin();
        for _ in 0..40 {
            let n = 1 + r.below(8);
            let bytes: Vec<u8> = (0..n).map(|_| r.next() as u8).collect();
            let fed = ed.feed(&bytes, &mut none);
            let _ = fed;
            assert!(ed.cursor() <= ed.text().chars().count());
        }
    }
}

#[test]
fn history_is_bounded_and_skips_repeats_and_blanks() {
    let mut ed = LineEditor::new("$ ");
    ed.remember("a");
    ed.remember("a");
    ed.remember("   ");
    ed.remember("b");
    assert_eq!(ed.history(), ["a", "b"]);
    for i in 0..500 {
        ed.remember(&format!("cmd {i}"));
    }
    assert_eq!(ed.history().len(), ish::line::MAX_HISTORY);
    assert_eq!(ed.history().last().unwrap(), "cmd 499");
}

#[test]
fn a_crlf_is_one_enter_and_a_long_line_is_capped() {
    let mut ed = LineEditor::new("$ ");
    let mut none = |_: &str| Completion::default();
    ed.begin();
    let fed = ed.feed(b"ab\r\ncd\r", &mut none);
    assert_eq!(
        fed.events,
        vec![Event::Line("ab".into()), Event::Line("cd".into())]
    );
    ed.begin();
    let long = vec![b'x'; ish::line::MAX_LINE + 100];
    ed.feed(&long, &mut none);
    assert_eq!(ed.text().len(), ish::line::MAX_LINE);
}
