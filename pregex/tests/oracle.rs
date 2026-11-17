//! The oracle: the host's `grep` (BSD on macOS, GNU on Linux; both POSIX leftmost-longest) finds the same matches. `grep -o -b`
//! prints the byte offset and text of every non-empty match; the first line is compared with this crate's leftmost-longest match.
//! Skipped with a message when `grep` is absent.

use pregex::{compile, Flags};
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

/// The oracle's first non-empty match: `Some((offset, text))`, `None` when it printed nothing, `Err` when grep is absent or
/// refused the pattern.
fn grep_first(
    ere: bool,
    icase: bool,
    pattern: &str,
    text: &str,
) -> Result<Option<(usize, String)>, ()> {
    let mut cmd = Command::new("grep");
    cmd.args(["-o", "-b", if ere { "-E" } else { "-G" }]);
    if icase {
        cmd.arg("-i");
    }
    cmd.args(["-e", pattern])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().map_err(|_| ())?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{text}\n").as_bytes())
        .map_err(|_| ())?;
    let out = child.wait_with_output().map_err(|_| ())?;
    if out.status.code() == Some(2) {
        return Err(());
    }
    let s = String::from_utf8_lossy(&out.stdout).into_owned();
    Ok(s.lines()
        .next()
        .and_then(|l| l.split_once(':'))
        .map(|(o, m)| (o.parse().unwrap(), m.to_string())))
}

fn ours(ere: bool, icase: bool, pattern: &str, text: &str) -> Option<(usize, usize)> {
    let re = compile(
        pattern.as_bytes(),
        Flags {
            extended: ere,
            icase,
            newline: false,
        },
    )
    .unwrap_or_else(|e| panic!("{pattern:?}: {e:?}"));
    re.exec(text.as_bytes(), false, false)
        .map(|c| c[0].unwrap())
}

fn check(ere: bool, icase: bool, pattern: &str, text: &str) {
    let want = match grep_first(ere, icase, pattern, text) {
        Ok(w) => w,
        Err(()) => return,
    };
    let got = ours(ere, icase, pattern, text);
    match (got, want) {
        // an empty match (or a match the oracle skips because it is empty) is not comparable
        (Some((s, e)), _) if s == e => {}
        (Some((s, e)), Some((o, m))) => assert_eq!(
            (s, &text[s..e]),
            (o, m.as_str()),
            "{} {pattern:?} on {text:?}",
            if ere { "ERE" } else { "BRE" }
        ),
        (Some((s, e)), None) => panic!("{pattern:?} on {text:?}: we match {s}..{e}, grep does not"),
        (None, Some((o, m))) => panic!("{pattern:?} on {text:?}: grep matches {o}:{m}, we do not"),
        (None, None) => {}
    }
}

fn grep_present() -> bool {
    Command::new("grep").arg("--version").output().is_ok()
}

#[test]
fn curated_patterns_agree_with_grep() {
    if !grep_present() {
        eprintln!("skipped: grep is not available");
        return;
    }
    let ere = [
        "a",
        "a+",
        "a*b",
        "(ab)+",
        "a|ab|abc",
        "(a|ab)(c|bcd)",
        "[a-c]+",
        "[^a-c]+",
        "x{2,3}",
        "x{2}",
        "(a+)(b+)",
        "^ab",
        "ab$",
        "[[:digit:]]+",
        "[[:alpha:]_][[:alnum:]_]*",
        "a.c",
        "colou?r",
        "(foo|bar)baz",
        "a{0,1}b",
        ".*",
        "(a*)*b",
        "(a|b)*abb",
        "\\.",
        "[.]",
        "[]a]+",
        "[a-]+",
        "\\w+",
        "\\bfoo\\b",
        "\\<ab",
        "(ab)\\1",
        "(a*)b\\1",
    ];
    let texts = [
        "",
        "a",
        "ab",
        "abc",
        "aabb",
        "xxabab yy",
        "colour color",
        "foo.bar",
        "foobarbaz",
        "x xx xxx xxxx",
        "abcd",
        "a1_b2 c3",
        "aaa b",
        "the foo bar",
        "a-b]c",
        "babb",
    ];
    for p in ere {
        for t in texts {
            check(true, false, p, t);
        }
    }
    let bre = [
        "a",
        "a*b",
        "\\(ab\\)*",
        "\\(a\\)\\(b\\)",
        "a\\{2\\}",
        "a\\{1,2\\}b",
        "^ab",
        "ab$",
        "[a-c]*",
        "a.c",
        "\\(ab\\)\\1",
        "*a",
        "a**",
        "\\(a*\\)b\\1",
        "[[:upper:]]\\{2,\\}",
    ];
    for p in bre {
        for t in texts.iter().chain(["AAbB", "ABCabc"].iter()) {
            check(false, false, p, t);
        }
    }
    // case folding
    for p in ["abc", "[a-c]+", "(AB)+", "a.B"] {
        for t in ["xABCx", "aBc", "abAB"] {
            check(true, true, p, t);
        }
    }
}

fn atom(r: &mut Rng, depth: u32) -> String {
    match r.below(if depth == 0 { 5 } else { 8 }) {
        0 => "a".into(),
        1 => "b".into(),
        2 => ".".into(),
        3 => ["[ab]", "[^a]", "[a-c]", "c"][r.below(4)].into(),
        4 => "a".into(),
        5 => format!("({})", alt(r, depth - 1)),
        6 => format!("({})", alt(r, depth - 1)),
        _ => "b".into(),
    }
}

fn piece(r: &mut Rng, depth: u32) -> String {
    let a = atom(r, depth);
    match r.below(7) {
        0 => format!("{a}*"),
        1 => format!("{a}+"),
        2 => format!("{a}?"),
        3 => format!("{a}{{1,2}}"),
        _ => a,
    }
}

fn concat(r: &mut Rng, depth: u32) -> String {
    (0..1 + r.below(3)).map(|_| piece(r, depth)).collect()
}

fn alt(r: &mut Rng, depth: u32) -> String {
    let n = 1 + r.below(3);
    (0..n)
        .map(|_| concat(r, depth))
        .collect::<Vec<_>>()
        .join("|")
}

#[test]
fn random_extended_patterns_agree_with_grep() {
    if !grep_present() {
        eprintln!("skipped: grep is not available");
        return;
    }
    let mut r = Rng(0x00c0_ffee_1234);
    let alphabet = ['a', 'b', 'c', ' ', 'a', 'b'];
    for _ in 0..400 {
        let p = alt(&mut r, 2);
        let t: String = (0..r.below(9))
            .map(|_| alphabet[r.below(alphabet.len())])
            .collect();
        check(true, false, &p, &t);
    }
}

#[test]
fn bad_patterns_are_refused() {
    for (ere, p) in [
        (true, "("),
        (true, "a)"),
        (true, "*a"),
        (true, "[a"),
        (true, "a{2"),
        (true, "a{3,1}"),
        (true, "(a)\\2"),
        (false, "\\(a"),
        (false, "a\\{1"),
        (true, "[b-a]"),
        (true, "a\\"),
        (true, "[[:nope:]]"),
    ] {
        let r = compile(
            p.as_bytes(),
            Flags {
                extended: ere,
                ..Flags::default()
            },
        );
        // (`a)` is a literal paren in ERE: accepted)
        if p == "a)" {
            assert!(r.is_ok());
        } else {
            assert!(r.is_err(), "{p:?} should be refused");
        }
    }
}

#[test]
fn captures_and_anchors() {
    let re = compile(
        b"(a+)(b+)?c",
        Flags {
            extended: true,
            ..Flags::default()
        },
    )
    .unwrap();
    let m = re.exec(b"xaabbc", false, false).unwrap();
    assert_eq!(m[0], Some((1, 6)));
    assert_eq!(m[1], Some((1, 3)));
    assert_eq!(m[2], Some((3, 5)));
    let m = re.exec(b"ac", false, false).unwrap();
    assert_eq!((m[1], m[2]), (Some((0, 1)), None));
    let bol = compile(
        b"^a",
        Flags {
            extended: true,
            ..Flags::default()
        },
    )
    .unwrap();
    assert!(bol.exec(b"a", false, false).is_some());
    assert!(bol.exec(b"a", true, false).is_none());
    let nl = compile(
        b"^b$",
        Flags {
            extended: true,
            newline: true,
            ..Flags::default()
        },
    )
    .unwrap();
    assert!(nl.exec(b"a\nb\nc", false, false).is_some());
    let nonl = compile(
        b"^b$",
        Flags {
            extended: true,
            ..Flags::default()
        },
    )
    .unwrap();
    assert!(nonl.exec(b"a\nb\nc", false, false).is_none());
}

#[test]
fn a_hostile_pattern_ends() {
    let re = compile(
        b"(a*)*(a*)*(a*)*b",
        Flags {
            extended: true,
            ..Flags::default()
        },
    )
    .unwrap();
    let text = "a".repeat(200);
    let _ = re.exec(text.as_bytes(), false, false); // must return (the step budget), not run for ever
}
