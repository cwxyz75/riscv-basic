//! Golden-file tests.
//!
//! Every `tests/golden/NAME.bas` is fed to the interpreter line by line, as if
//! typed at the prompt, and the full console transcript (prompts included,
//! typed input NOT echoed) must equal `tests/golden/NAME.out`.
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
}

impl Io for ScriptIo {
    fn write_str(&mut self, s: &str) {
        self.output.push_str(s);
    }

    fn read_line(&mut self) -> Line {
        match self.input.pop_front() {
            Some(line) => Line::Text(line),
            None => Line::Eof,
        }
    }

    fn break_pressed(&mut self) -> bool {
        false
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

fn transcript(source: &str) -> String {
    let mut io = ScriptIo {
        input: source.lines().map(str::to_owned).collect(),
        output: String::new(),
        memory: HashMap::new(),
        clock_ms: 0,
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
