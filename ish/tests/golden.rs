//! Golden transcripts: each `tests/golden/*.t` is a script with the exact stdout, stderr and exit status the shell must
//! produce against the fake host (SHELL-SPEC section 10, SH0/SH2). Format:
//!
//! ```text
//! # comment lines before the first section are ignored
//! --- file /path          (optional, repeatable: a file in the fake file system)
//! content
//! --- args a b c        (optional: the script's arguments, split on blanks)
//! --- env NAME=value      (optional, repeatable)
//! --- stdin               (optional)
//! --- script
//! source
//! --- lines               (instead of `script`: each line is run as an interactive line)
//! --- stdout
//! --- stderr
//! --- status
//! 0
//! ```
//! A missing `stdout`/`stderr` section means empty; a missing `status` means 0. `BLESS=1` rewrites the three expected
//! sections from the actual run: read the diff before committing it.

use ish::fake::FakeHost;
use ish::Shell;
use std::fs;
use std::path::PathBuf;

struct Case {
    files: Vec<(String, String)>,
    env: Vec<(String, String)>,
    args: Vec<String>,
    stdin: String,
    script: String,
    interactive: bool,
    stdout: String,
    stderr: String,
    status: i32,
}

fn parse(text: &str) -> Case {
    let mut c = Case {
        files: vec![],
        env: vec![],
        args: vec![],
        stdin: String::new(),
        script: String::new(),
        interactive: false,
        stdout: String::new(),
        stderr: String::new(),
        status: 0,
    };
    let mut section: Option<String> = None;
    let mut body = String::new();
    let flush = |c: &mut Case, section: &Option<String>, body: &mut String| {
        if let Some(s) = section {
            let b = std::mem::take(body);
            match s.split_once(' ').unwrap_or((s.as_str(), "")) {
                ("file", p) => c.files.push((p.to_string(), b)),
                ("env", kv) => {
                    let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
                    c.env.push((k.to_string(), v.to_string()));
                }
                ("args", a) => c.args = a.split_whitespace().map(String::from).collect(),
                ("stdin", _) => c.stdin = b,
                ("script", _) => c.script = b,
                ("lines", _) => {
                    c.script = b;
                    c.interactive = true;
                }
                ("stdout", _) => c.stdout = b,
                ("stderr", _) => c.stderr = b,
                ("status", _) => c.status = b.trim().parse().expect("status"),
                (other, _) => panic!("unknown section {other}"),
            }
        }
    };
    for line in text.split_inclusive('\n') {
        if let Some(rest) = line.strip_prefix("--- ") {
            flush(&mut c, &section, &mut body);
            section = Some(rest.trim_end().to_string());
        } else if section.is_some() {
            body.push_str(line);
        }
    }
    flush(&mut c, &section, &mut body);
    c
}

fn run(c: &Case) -> (String, String, i32, usize) {
    let mut host = FakeHost::new();
    for (p, content) in &c.files {
        host = host.with_file(p, content);
    }
    host.set_stdin(c.stdin.as_bytes());
    let mut sh = Shell::new(host);
    for (k, v) in &c.env {
        use ish::Host;
        sh.host.setenv(k, v);
    }
    sh.set_args(c.args.clone());
    let status = if c.interactive {
        let mut last = 0;
        for l in c.script.lines() {
            last = sh.run_line(l);
        }
        last
    } else {
        sh.run_script(&c.script)
    };
    let leaked = sh.host.open_streams();
    (sh.host.stdout_text(), sh.host.stderr_text(), status, leaked)
}

#[test]
fn golden_transcripts() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let bless = std::env::var("BLESS").is_ok();
    let mut names: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "t"))
        .collect();
    names.sort();
    assert!(!names.is_empty());
    let mut failures = Vec::new();
    for path in &names {
        let text = fs::read_to_string(path).unwrap();
        let c = parse(&text);
        let (out, err, st, leaked) = run(&c);
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if bless {
            let head: String = text
                .split("--- stdout")
                .next()
                .unwrap()
                .split("--- stderr")
                .next()
                .unwrap()
                .split("--- status")
                .next()
                .unwrap()
                .to_string();
            let mut new = head.trim_end_matches('\n').to_string();
            new.push('\n');
            new.push_str(&format!(
                "--- stdout\n{out}--- stderr\n{err}--- status\n{st}\n"
            ));
            fs::write(path, new).unwrap();
            continue;
        }
        if out != c.stdout {
            failures.push(format!(
                "{name}: stdout\n  expected: {:?}\n  actual:   {:?}",
                c.stdout, out
            ));
        }
        if err != c.stderr {
            failures.push(format!(
                "{name}: stderr\n  expected: {:?}\n  actual:   {:?}",
                c.stderr, err
            ));
        }
        if st != c.status {
            failures.push(format!("{name}: status expected {} actual {st}", c.status));
        }
        if leaked != 0 {
            failures.push(format!("{name}: {leaked} stream(s) left open"));
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}
