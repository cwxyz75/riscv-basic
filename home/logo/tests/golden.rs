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

use cartfs::{Disk, Files, Kind, MemDisk, Unavailable};
use logo::{Engine, Interpreter, Io, Line};
use std::collections::VecDeque;
use std::fs;
use std::path::Path;
use turtle::Bitmap;

struct ScriptIo {
    input: VecDeque<String>,
    output: String,
    clock_ms: u64,
    break_after: Option<u64>,
    files: Option<Box<dyn Files>>,
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

    fn files(&mut self) -> Option<&mut dyn Files> {
        match &mut self.files {
            Some(files) => Some(&mut **files),
            None => None,
        }
    }
}

/// Disk directives, anywhere in a script (none: no disk): `<DISK SECTORS n>` a
/// freshly formatted disk of n sectors (n - 6 blocks of data), `<NO DISK>`,
/// `<BAD DISK>` (unformatted), `<DISK ERROR>` (reads fail), and `<FILE BASIC
/// NAME>` / `<FILE LOGO NAME>` through `<END FILE>` to put a file on the disk.
/// Returns the disk and the remaining lines.
type DiskMaker = Box<dyn Fn() -> Option<Box<dyn Files>>>;

fn take_disk(script: &str) -> (DiskMaker, String) {
    let mut kind_of_disk = DiskKind::None;
    let mut sectors = 0;
    let mut files: Vec<(Kind, String, String)> = Vec::new();
    let mut open: Option<(Kind, String, String)> = None;
    let mut rest = String::new();
    for line in script.lines() {
        if let Some(file) = open.as_mut() {
            if line == "<END FILE>" {
                files.push(open.take().unwrap());
            } else {
                file.2.push_str(line);
                file.2.push('\n');
            }
        } else if let Some(n) = line
            .strip_prefix("<DISK SECTORS ")
            .and_then(|r| r.strip_suffix('>'))
        {
            kind_of_disk = DiskKind::Formatted;
            sectors = n.parse::<u32>().expect("sector count");
        } else if line == "<NO DISK>" {
            kind_of_disk = DiskKind::None;
        } else if line == "<BAD DISK>" {
            kind_of_disk = DiskKind::Unformatted;
        } else if line == "<DISK ERROR>" {
            kind_of_disk = DiskKind::Broken;
        } else if let Some(r) = line
            .strip_prefix("<FILE ")
            .and_then(|r| r.strip_suffix('>'))
        {
            let (kind, name) = r.split_once(' ').expect("<FILE KIND NAME>");
            let kind = match kind {
                "BASIC" => Kind::Basic,
                "LOGO" => Kind::Logo,
                other => panic!("unknown file kind {other}"),
            };
            open = Some((kind, name.to_owned(), String::new()));
        } else {
            rest.push_str(line);
            rest.push('\n');
        }
    }
    // Every engine run gets its own fresh disk.
    let make = move || -> Option<Box<dyn Files>> {
        match kind_of_disk {
            DiskKind::None => None,
            DiskKind::Unformatted => Some(Box::new(Unavailable(cartfs::Error::NotFormatted))),
            DiskKind::Broken => Some(Box::new(Unavailable(cartfs::Error::Device))),
            DiskKind::Formatted => {
                let mut d = Disk::format(MemDisk::new(sectors)).expect("format");
                for (kind, name, text) in &files {
                    d.save(name, *kind, text.as_bytes()).expect("preload");
                }
                Some(Box::new(d))
            }
        }
    };
    (Box::new(make), rest)
}

enum DiskKind {
    None,
    Unformatted,
    Broken,
    Formatted,
}

/// Replace `<LOAD examples/logo/NAME.logo>` lines with that file's lines (path
/// relative to the repository root), so a demo program is typed in from the one
/// copy that ships in `examples/`.
fn expand_loads(script: &str) -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut out = String::new();
    for line in script.lines() {
        match line
            .strip_prefix("<LOAD ")
            .and_then(|rest| rest.strip_suffix('>'))
        {
            Some(path) => out.push_str(
                &fs::read_to_string(root.join(path))
                    .unwrap_or_else(|e| panic!("cannot load {path}: {e}")),
            ),
            None => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out
}

fn run(script: &str, engine: Engine) -> (String, String) {
    let (make_disk, script) = take_disk(&expand_loads(script));
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
        files: make_disk(),
    };
    let mut interpreter = match engine {
        // Compiled code runs in the RV64 emulator.
        Engine::Jit => Interpreter::with_executor(logo::emulated_execute),
        other => Interpreter::with_engine(other),
    };
    interpreter.run_session(&mut io, &mut canvas);
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
        let script = fs::read_to_string(case).unwrap();
        // Developer aid: with LOGO_DUMP=DIR, write the pictures actually drawn
        // (AST engine) into DIR for inspection and comparison with an independent
        // renderer, and check nothing. Never into the repository directly.
        if let Some(dir) = std::env::var_os("LOGO_DUMP") {
            let (_, picture) = run(&script, Engine::Ast);
            let name = case.file_stem().unwrap().to_string_lossy().into_owned();
            fs::write(Path::new(&dir).join(format!("{name}.img")), picture).unwrap();
            continue;
        }
        let out = case.with_extension("out");
        let want = fs::read_to_string(&out).unwrap_or_else(|_| panic!("missing {}", out.display()));
        let img = fs::read_to_string(case.with_extension("img")).ok();
        // The AST interpreter, the IR interpreter and the compiled engine (RV64
        // code in the emulator) must all match the hand-derived files, and
        // therefore each other.
        for engine in [Engine::Ast, Engine::Ir, Engine::Jit] {
            let (transcript, picture) = run(&script, engine);
            if transcript != want {
                failures.push(format!(
                    "--- {} ({engine:?}) ---\nexpected:\n{want}\ngot:\n{transcript}",
                    case.display()
                ));
            }
            if let Some(want) = &img {
                if &picture != want {
                    failures.push(format!(
                        "--- {} ({engine:?}) image ---\nexpected:\n{want}got:\n{picture}",
                        case.display()
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
