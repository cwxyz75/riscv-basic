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
//! Disk directives (anywhere in the script, applied before it runs; a script
//! without one has no disk): `<DISK SECTORS n>` attaches a freshly formatted
//! disk of n sectors (n - 6 blocks of data), `<NO DISK>` (the default),
//! `<BAD DISK>` an unformatted one, `<DISK ERROR>` one whose reads fail, and
//! `<FILE BASIC NAME>` / `<FILE LOGO NAME>` through `<END FILE>` puts the lines
//! between as a file on the disk.
//!
//! To (re)generate expected outputs after an intended behaviour change:
//!     BLESS=1 cargo test -p basic
//! then review the diff of the `.out` files before committing.

use basic_script::transcript;
use std::fs;
use std::path::{Path, PathBuf};

/// Run a script with `<LOAD path>` read relative to the repository root.
fn run(source: &str) -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    transcript(source, &mut |path| fs::read_to_string(root.join(path)).ok())
        .unwrap_or_else(|e| panic!("script error: {e}"))
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
        let got = run(&fs::read_to_string(case).expect("readable .bas"));
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
