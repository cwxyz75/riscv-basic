//! Golden transcripts: each `tests/golden/*.t` is a script with the exact stdout, stderr and exit status the shell must
//! produce against the fake host (SHELL-SPEC section 10, SH0/SH2). Format:
//!
//! ```text
//! # comment lines before the first section are ignored
//! --- file /path          (optional, repeatable: a file in the fake file system)
//! content
//! --- file-hex /path     (optional, repeatable: a file given as hexadecimal digits, blanks ignored: bytes that are not text)
//! --- args a b c        (optional: the script's arguments, split on blanks)
//! --- env NAME=value      (optional, repeatable)
//! --- stdin               (optional)
//! --- script
//! source
//! --- lines               (instead of `script`: each line is run as an interactive line)
//! --- term               (instead of `script`: interactive; one line per chunk of terminal input, with the escapes
//!                         \e \b \r \t \n \a \\ \xNN; a line `@interrupt` is the console's interrupt event)
//! --- tty                (the expected terminal output of an interactive run, control bytes escaped the same way;
//!                         a real newline follows each `\n`; one trailing newline of the section is not part of the output)
//! --- batch              (the script is read from `stdin` by the non-interactive mode)
//! --- interrupt-after N  (the user presses Ctrl-C at the Nth poll)
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
    hex_files: Vec<(String, Vec<u8>)>,
    env: Vec<(String, String)>,
    args: Vec<String>,
    stdin: String,
    script: String,
    interactive: bool,
    stdout: String,
    stderr: String,
    status: i32,
    term: Option<String>,
    tty: Option<String>,
    batch: bool,
    legacy: bool,
    interrupt_after: u32,
}

fn parse(text: &str) -> Case {
    let mut c = Case {
        files: vec![],
        hex_files: vec![],
        env: vec![],
        args: vec![],
        stdin: String::new(),
        script: String::new(),
        interactive: false,
        stdout: String::new(),
        stderr: String::new(),
        status: 0,
        term: None,
        tty: None,
        batch: false,
        legacy: false,
        interrupt_after: 0,
    };
    let mut section: Option<String> = None;
    let mut body = String::new();
    let flush = |c: &mut Case, section: &Option<String>, body: &mut String| {
        if let Some(s) = section {
            let b = std::mem::take(body);
            match s.split_once(' ').unwrap_or((s.as_str(), "")) {
                ("file", p) => c.files.push((p.to_string(), b)),
                ("file-hex", p) => {
                    let digits: Vec<u8> = b.bytes().filter(|x| !x.is_ascii_whitespace()).collect();
                    let bytes = digits
                        .chunks(2)
                        .map(|d| {
                            u8::from_str_radix(core::str::from_utf8(d).unwrap(), 16).expect("hex")
                        })
                        .collect();
                    c.hex_files.push((p.to_string(), bytes));
                }
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
                ("term", _) => c.term = Some(b),
                ("tty", _) => c.tty = Some(b),
                ("batch", _) => c.batch = true,
                ("legacy", _) => c.legacy = true,
                ("interrupt-after", n) => c.interrupt_after = n.trim().parse().expect("count"),
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

fn unescape(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            i += 1;
            match b[i] {
                b'e' => out.push(0x1b),
                b'b' => out.push(0x08),
                b'r' => out.push(b'\r'),
                b't' => out.push(b'\t'),
                b'n' => out.push(b'\n'),
                b'a' => out.push(0x07),
                b'\\' => out.push(b'\\'),
                b'x' => {
                    out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).expect("hex"));
                    i += 2;
                }
                c => panic!("unknown escape \\{}", c as char),
            }
        } else {
            out.push(b[i]);
        }
        i += 1;
    }
    out
}

fn escape(bytes: &[u8]) -> String {
    let mut out = String::new();
    for ch in String::from_utf8_lossy(bytes).chars() {
        match ch {
            '\x1b' => out.push_str("\\e"),
            '\x08' => out.push_str("\\b"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x07' => out.push_str("\\a"),
            '\n' => out.push_str("\\n\n"),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out
}

fn run(c: &Case) -> (String, String, i32, usize) {
    let mut host = FakeHost::new();
    for (p, content) in &c.files {
        host = host.with_file(p, content);
    }
    for (p, bytes) in &c.hex_files {
        host = host.with_file_bytes(p, bytes);
    }
    host.set_stdin(c.stdin.as_bytes());
    for (k, v) in &c.env {
        host.set_env(k, v);
    }
    let mut sh = Shell::new(host);
    sh.set_args(c.args.clone());
    if c.legacy {
        sh.name = "sh".into();
        sh.legacy = true;
    }
    sh.host.interrupt_after_polls = c.interrupt_after;
    let status = if let Some(term) = &c.term {
        for l in term.lines() {
            if l == "@interrupt" {
                sh.host.term_in.push_back(ish::host::TermIn::Interrupt);
            } else {
                sh.host
                    .term_in
                    .push_back(ish::host::TermIn::Bytes(unescape(l)));
            }
        }
        ish::repl::run_interactive(&mut sh)
    } else if c.batch {
        ish::repl::run_batch(&mut sh)
    } else if c.interactive {
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
        let out = if c.term.is_some() {
            escape(out.as_bytes())
        } else {
            out
        };
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if bless && c.term.is_none() {
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
        let (want_out, got_out) = if c.term.is_some() {
            let w = c.tty.clone().unwrap_or_default();
            let w = w.strip_suffix('\n').unwrap_or(&w).to_string();
            let g = out.strip_suffix('\n').unwrap_or(&out).to_string();
            (w, g)
        } else {
            (c.stdout.clone(), out.clone())
        };
        if want_out != got_out {
            failures.push(format!(
                "{name}: output\n  expected: {:?}\n  actual:   {:?}",
                want_out, got_out
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
