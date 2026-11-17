//! Independent oracles for the data primitives and the pipeline status (SHELL-SPEC SH2b). Python's `json`, `csv` and `decimal`
//! and `bash -o pipefail` compute the same things from the same inputs; the tests skip with a message when the tool is absent.
//! The inputs are generated here from a fixed seed, so a failure reproduces.

use ish::fake::FakeHost;
use ish::Shell;
use std::io::Write;
use std::process::{Command, Stdio};

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

fn python(code: &str, input: &str) -> Option<String> {
    let mut child = Command::new("python3")
        .args(["-I", "-c", code])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(input.as_bytes()).ok()?;
    let out = child.wait_with_output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run a script on the fake host with these files; (stdout, status).
fn ish(script: &str, files: &[(&str, &str)]) -> (String, String, i32) {
    let mut host = FakeHost::new();
    for (p, c) in files {
        host = host.with_file(p, c);
    }
    let mut sh = Shell::new(host);
    let st = sh.run_script(script);
    (sh.host.stdout_text(), sh.host.stderr_text(), st)
}

const STR_PARTS: &[&str] = &[
    "a", "bc", " ", "\"", "\\", "/", "é", "λ", "✓", "\n", "\t", "x y", "0", "{", "[", "]", ",", ":",
];

fn json_string(r: &mut Rng) -> String {
    let n = r.below(6);
    let raw: String = (0..n)
        .map(|_| STR_PARTS[r.below(STR_PARTS.len())])
        .collect();
    let mut s = String::from("\"");
    for c in raw.chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            '\t' => s.push_str("\\t"),
            c => s.push(c),
        }
    }
    s.push('"');
    s
}

fn json_value(r: &mut Rng, depth: u32) -> String {
    let k = if depth == 0 { r.below(5) } else { r.below(7) };
    match k {
        0 => "null".into(),
        1 => if r.below(2) == 0 { "true" } else { "false" }.into(),
        2 => format!("{}", r.below(2_000_000) as i64 - 1_000_000),
        3 => format!("{}.{:02}", r.below(10_000), 1 + r.below(98)),
        4 => json_string(r),
        5 => {
            let n = r.below(4);
            let v: Vec<String> = (0..n).map(|_| json_value(r, depth - 1)).collect();
            format!("[{}]", v.join(", "))
        }
        _ => {
            let n = r.below(4);
            let v: Vec<String> = (0..n)
                .map(|i| format!("\"k{i}\": {}", json_value(r, depth - 1)))
                .collect();
            format!("{{{}}}", v.join(", "))
        }
    }
}

#[test]
fn json_agrees_with_python() {
    let mut r = Rng(0x1234_5678_9abc);
    let cases: Vec<String> = (0..300).map(|_| json_value(&mut r, 3)).collect();
    let input = cases.join("\n---\n");
    let code = "import sys,json\nfor t in sys.stdin.read().split('\\n---\\n'):\n    print(json.dumps(json.loads(t), separators=(',',':'), ensure_ascii=False))\n";
    let Some(want) = python(code, &input) else {
        eprintln!("skipped: python3 is not available");
        return;
    };
    let want: Vec<&str> = want.lines().collect();
    // (a value with a newline in a string stays on one line in both: it is escaped)
    assert_eq!(want.len(), cases.len());
    for (i, c) in cases.iter().enumerate() {
        let (out, err, st) = ish(
            "print (to-json (from-json (cat /in.json)))",
            &[("/in.json", c)],
        );
        assert_eq!(st, 0, "case {i}: {c}\n{err}");
        assert_eq!(out.trim_end_matches('\n'), want[i], "case {i}: {c}");
    }
}

#[test]
fn csv_agrees_with_python() {
    let mut r = Rng(0xfeed_f00d);
    let cells = [
        "a", "b c", "d,e", "f\"g", "", "h\ni", "é", " lead", "trail ", "x;y",
    ];
    let mut cases = Vec::new();
    for _ in 0..200 {
        let rows = 1 + r.below(4);
        let width = 1 + r.below(4);
        let table: Vec<Vec<&str>> = (0..rows)
            .map(|_| (0..width).map(|_| cells[r.below(cells.len())]).collect())
            .collect();
        cases.push(table);
    }
    // the text is written the way Python's csv module writes it (quoting where needed): an independent writer
    let json_in = format!(
        "[{}]",
        cases
            .iter()
            .map(|t| format!(
                "[{}]",
                t.iter()
                    .map(|row| format!(
                        "[{}]",
                        row.iter()
                            .map(|c| format!("{c:?}"))
                            .collect::<Vec<_>>()
                            .join(",")
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            ))
            .collect::<Vec<_>>()
            .join(",")
    );
    // (Rust's `{:?}` escapes are valid JSON for these cells: quotes, newlines and non-ASCII are kept or escaped as JSON does)
    let code = "import sys,json,csv,io\nfor t in json.load(sys.stdin):\n    b=io.StringIO()\n    csv.writer(b,lineterminator='\\n').writerows(t)\n    print(json.dumps(b.getvalue()))\n";
    let Some(texts) = python(code, &json_in) else {
        eprintln!("skipped: python3 is not available");
        return;
    };
    let texts: Vec<&str> = texts.lines().collect();
    assert_eq!(texts.len(), cases.len());
    for (i, t) in cases.iter().enumerate() {
        let text: String = serde_like_unquote(texts[i]);
        // ish reads Python's CSV back to exactly the table that was written
        let (out, err, st) = ish(
            "print (to-json (from-csv (cat /in.csv)))",
            &[("/in.csv", &text)],
        );
        assert_eq!(st, 0, "case {i}: {text:?}\n{err}");
        let want = format!(
            "[{}]",
            t.iter()
                .map(|row| format!(
                    "[{}]",
                    row.iter()
                        .map(|c| json_quote(c))
                        .collect::<Vec<_>>()
                        .join(",")
                ))
                .collect::<Vec<_>>()
                .join(",")
        );
        assert_eq!(out.trim_end_matches('\n'), want, "case {i}: {text:?}");
    }
}

fn json_quote(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\t' => o.push_str("\\t"),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// The inverse of `json_quote` for the text Python's `json.dumps` printed (ensure_ascii escapes `\uXXXX`).
fn serde_like_unquote(q: &str) -> String {
    let inner = &q[1..q.len() - 1];
    let mut out = String::new();
    let mut it = inner.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('/') => out.push('/'),
            Some('u') => {
                let hex: String = it.by_ref().take(4).collect();
                out.push(char::from_u32(u32::from_str_radix(&hex, 16).unwrap()).unwrap());
            }
            other => panic!("unexpected escape {other:?}"),
        }
    }
    out
}

#[test]
fn decimal_arithmetic_agrees_with_python() {
    let mut r = Rng(0xdec1_a001);
    let mut lines = Vec::new();
    for _ in 0..400 {
        let a = format!(
            "{}{}.{:0w$}",
            if r.below(2) == 0 { "-" } else { "" },
            r.below(100_000),
            r.below(1000),
            w = 3
        );
        let b = format!("{}.{:02}", 1 + r.below(5000), r.below(100));
        lines.push((a, b));
    }
    let input = lines
        .iter()
        .map(|(a, b)| format!("{a} {b}"))
        .collect::<Vec<_>>()
        .join("\n");
    let code = "import sys\nfrom decimal import Decimal, getcontext\ngetcontext().prec=60\nfor l in sys.stdin.read().split('\\n'):\n    a,b=map(Decimal,l.split())\n    print(a+b, a-b, a*b)\n";
    let Some(want) = python(code, &input) else {
        eprintln!("skipped: python3 is not available");
        return;
    };
    for (i, w) in want.lines().enumerate() {
        let (a, b) = &lines[i];
        let (out, err, st) = ish(
            &format!(
                "a = to-number '{a}'\nb = to-number '{b}'\nprint ($a + $b) ($a - $b) ($a * $b)"
            ),
            &[],
        );
        assert_eq!(st, 0, "{a} {b}: {err}");
        // equal as numbers (the trailing zeros a scale keeps are not part of the value)
        let got: Vec<&str> = out.split_whitespace().collect();
        let exp: Vec<&str> = w.split_whitespace().collect();
        assert_eq!(got.len(), 3);
        for (g, e) in got.iter().zip(&exp) {
            assert!(numerically_equal(g, e), "{a} {b}: ish {g}, python {e}");
        }
    }
}

/// Decimal strings equal as numbers: sign and digits compared after dropping trailing fractional zeros.
fn numerically_equal(a: &str, b: &str) -> bool {
    fn norm(s: &str) -> String {
        let (neg, s) = match s.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, s),
        };
        let (int, frac) = s.split_once('.').unwrap_or((s, ""));
        let frac = frac.trim_end_matches('0');
        let int = int.trim_start_matches('0');
        let int = if int.is_empty() { "0" } else { int };
        let zero = int == "0" && frac.is_empty();
        format!(
            "{}{}{}{}",
            if neg && !zero { "-" } else { "" },
            int,
            if frac.is_empty() { "" } else { "." },
            frac
        )
    }
    norm(a) == norm(b)
}

fn bash_available() -> bool {
    Command::new("bash")
        .args(["-c", "exit 0"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[test]
fn pipeline_status_agrees_with_bash_pipefail() {
    if !bash_available() {
        eprintln!("skipped: bash is not available");
        return;
    }
    let codes = [0, 1, 2, 3];
    let mut checked = 0;
    for &a in &codes {
        for &b in &codes {
            for &c in &codes {
                let bash = Command::new("bash")
                    .args([
                        "-c",
                        &format!("set -o pipefail; (exit {a}) | (exit {b}) | (exit {c})"),
                    ])
                    .status()
                    .unwrap()
                    .code()
                    .unwrap();
                // Both fail exactly when some stage fails. Which stage's status is reported differs by design (SHELL-SPEC R3): bash
                // gives the rightmost failing stage's, the shell names the first.
                let (_, _, st) = ish(
                    &format!("exit-with {a} | exit-with {b} | exit-with {c}"),
                    &[],
                );
                assert_eq!(st == 0, bash == 0, "exit {a} | exit {b} | exit {c}");
                let first = [a, b, c].into_iter().find(|&x| x != 0).unwrap_or(0);
                assert_eq!(st, first, "R3: the first failing stage is named");
                checked += 1;
            }
        }
    }
    assert_eq!(checked, 64);
}
