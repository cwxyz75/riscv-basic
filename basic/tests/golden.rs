//! Golden-file tests.
//!
//! Every `tests/golden/NAME.bas` is fed to the interpreter line by line, as if
//! typed at the prompt, and the full console transcript (prompts included,
//! typed input NOT echoed) must equal `tests/golden/NAME.out`.
//!
//! A script line of the form `<LOAD examples/NAME.bas>` is replaced by that
//! file's lines (path relative to the repository root), so a demo program is
//! typed in from the one copy that ships in `examples/`.
//!
//! A script line of the form `<BREAK AFTER n>` is not typed input: when the
//! interpreter reads it, the n-th following check for Ctrl-C reports "pressed"
//! (the interpreter checks once before each statement it runs).
//!
//! To (re)generate expected outputs after an intended behaviour change:
//!     BLESS=1 cargo test -p basic
//! then review the diff of the `.out` files before committing.

use basic::{Interpreter, Io, Line};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

/// A scripted console. Memory is a sparse byte map (unset bytes read as 0) and
/// the clock advances 250 ms every time it is read, so tests are deterministic.
struct ScriptIo {
    input: VecDeque<String>,
    output: String,
    memory: HashMap<u64, u8>,
    clock_ms: u64,
    /// Checks left until a simulated Ctrl-C (see the module comment).
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

    fn peek(&mut self, addr: u64) -> u8 {
        self.memory.get(&addr).copied().unwrap_or(0)
    }

    fn poke(&mut self, addr: u64, value: u8) {
        self.memory.insert(addr, value);
    }
}

/// The script's lines, with `<LOAD path>` directives expanded.
fn expand(source: &str) -> Vec<String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut lines = Vec::new();
    for line in source.lines() {
        match line
            .strip_prefix("<LOAD ")
            .and_then(|rest| rest.strip_suffix('>'))
        {
            Some(path) => {
                let text = fs::read_to_string(root.join(path))
                    .unwrap_or_else(|e| panic!("cannot load {path}: {e}"));
                lines.extend(text.lines().map(str::to_owned));
            }
            None => lines.push(line.to_owned()),
        }
    }
    lines
}

fn transcript(source: &str) -> String {
    let mut io = ScriptIo {
        input: expand(source).into(),
        output: String::new(),
        memory: HashMap::new(),
        clock_ms: 0,
        break_after: None,
    };
    Interpreter::new().run_session(&mut io);
    io.output
}

#[test]
fn golden() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let bless = std::env::var_os("BLESS").is_some();

    let mut cases: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("tests/golden should exist")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "bas"))
        .collect();
    cases.sort();
    assert!(
        !cases.is_empty(),
        "no golden cases found in {}",
        dir.display()
    );

    let mut failures = Vec::new();
    for case in &cases {
        let got = transcript(&fs::read_to_string(case).expect("readable .bas"));
        let expected_path = case.with_extension("out");
        if bless {
            fs::write(&expected_path, &got).expect("writable .out");
            continue;
        }
        let want = fs::read_to_string(&expected_path)
            .unwrap_or_else(|_| panic!("missing expected output {}", expected_path.display()));
        if got != want {
            failures.push(format!(
                "--- {} ---\nexpected:\n{want}\ngot:\n{got}",
                case.display()
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
