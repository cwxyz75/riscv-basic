//! Golden tests for the LOGO interpreter. Each `tests/golden/NAME.logo` is
//! typed at the prompt line by line; the transcript (prompts included, typed
//! text NOT echoed) must equal `NAME.out`. If `NAME.img` exists, the picture the
//! turtle drew must equal it too (same text format as the `turtle` crate's
//! golden images).
//!
//! A first line `;canvas W H` (a LOGO comment) sizes the canvas (default 21x21)
//! and is not typed. A line `<BREAK AFTER n>` is not typed either: the n-th
//! following check for Ctrl-C reports "pressed". Expected files are worked out
//! by hand from `LOGO.md`; never bless them blindly.

use logo::{Interpreter, Io, Line};
use std::collections::VecDeque;
use std::fs;
use std::path::Path;
use turtle::Bitmap;

struct ScriptIo {
    input: VecDeque<String>,
    output: String,
    clock_ms: u64,
    break_after: Option<u64>,
}

impl Io for ScriptIo {
    fn write_str(&mut self, s: &str) {
        self.output.push_str(s);
    }

    fn read_line(&mut self) -> Line {
        while let Some(line) = self.input.pop_front() {
            let directive = line
                .strip_prefix("<BREAK AFTER ")
                .and_then(|rest| rest.strip_suffix('>'))
                .and_then(|n| n.parse::<u64>().ok());
            match directive {
                Some(n) => self.break_after = Some(n),
                None => return Line::Text(line),
            }
        }
        Line::Eof
    }

    fn break_pressed(&mut self) -> bool {
        match self.break_after {
            Some(n) if n <= 1 => {
                self.break_after = None;
                true
            }
            Some(n) => {
                self.break_after = Some(n - 1);
                false
            }
            None => false,
        }
    }

    fn millis(&mut self) -> u64 {
        let now = self.clock_ms;
        self.clock_ms += 250;
        now
    }

    fn reboot(&mut self) {
        self.output.push_str("[reboot]\n");
    }
}

fn run(script: &str) -> (String, String) {
    let mut lines: Vec<&str> = script.lines().collect();
    let (mut width, mut height) = (21, 21);
    if let Some(dims) = lines.first().and_then(|l| l.strip_prefix(";canvas ")) {
        let parts: Vec<i32> = dims
            .split_whitespace()
            .map(|n| n.parse().unwrap())
            .collect();
        (width, height) = (parts[0], parts[1]);
        lines.remove(0);
    }
    let mut canvas = Bitmap::new(width, height);
    let mut io = ScriptIo {
        input: lines.into_iter().map(str::to_owned).collect(),
        output: String::new(),
        clock_ms: 0,
        break_after: None,
    };
    Interpreter::new().run_session(&mut io, &mut canvas);
    (io.output, canvas.to_ascii())
}

#[test]
fn golden() {
    // The interpreter recurses (procedure depth 400), so run on a roomy stack.
    std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(golden_cases)
        .unwrap()
        .join()
        .unwrap();
}

fn golden_cases() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let mut cases: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "logo"))
        .collect();
    cases.sort();
    assert!(!cases.is_empty());
    let mut failures = Vec::new();
    for case in &cases {
        let (transcript, picture) = run(&fs::read_to_string(case).unwrap());
        let out = case.with_extension("out");
        let want = fs::read_to_string(&out).unwrap_or_else(|_| panic!("missing {}", out.display()));
        if transcript != want {
            failures.push(format!(
                "--- {} ---\nexpected:\n{want}\ngot:\n{transcript}",
                case.display()
            ));
        }
        let img = case.with_extension("img");
        if let Ok(want) = fs::read_to_string(&img) {
            if picture != want {
                failures.push(format!(
                    "--- {} ---\nexpected:\n{want}got:\n{picture}",
                    img.display()
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
