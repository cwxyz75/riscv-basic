//! The pure half of the shell (S4-SPEC section 8): the line grammar and the lexical path
//! arithmetic of `cd`/`pwd`. `no_std` + `alloc`, no unsafe, no I/O.
//!
//! Grammar: a line is `stage ('|' stage)*`. Blanks (space, tab) separate words; single or
//! double quotes group (no escapes, no expansion, no variables, no globbing; a quoted empty
//! string is an empty word); `|` outside quotes separates stages and needs no blanks around
//! it; `#` at the start of a word (outside quotes) starts a comment that runs to the end of the
//! line. A line with no words at all (empty, blanks, comment) is not an error: it has no stages.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;

pub const MAX_STAGES: usize = 16;
pub const MAX_WORDS: usize = 64;

/// One stage of a pipeline: the command name followed by its arguments.
pub type Stage = Vec<Vec<u8>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// A quote was opened and never closed.
    UnterminatedQuote,
    /// A `|` with no command on one side of it.
    EmptyStage,
    TooManyStages,
    TooManyWords,
}

impl ParseError {
    /// The shell's message for the error (S4-SPEC: printed as `sh: <message>`).
    pub fn message(self) -> &'static str {
        match self {
            ParseError::UnterminatedQuote => "unterminated quote",
            ParseError::EmptyStage => "empty command in pipeline",
            ParseError::TooManyStages => "too many commands in pipeline",
            ParseError::TooManyWords => "too many words",
        }
    }
}

/// Parse one line (without its newline) into pipeline stages.
pub fn parse(line: &[u8]) -> Result<Vec<Stage>, ParseError> {
    let mut stages: Vec<Stage> = Vec::new();
    let mut words: Stage = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    let mut in_word = false; // a word is being built (possibly still empty, if quoted)
    let mut quote: Option<u8> = None;
    let mut seen_pipe = false;
    let mut i = 0;
    let end_word =
        |words: &mut Stage, cur: &mut Vec<u8>, in_word: &mut bool| -> Result<(), ParseError> {
            if *in_word {
                if words.len() >= MAX_WORDS {
                    return Err(ParseError::TooManyWords);
                }
                words.push(core::mem::take(cur));
                *in_word = false;
            }
            Ok(())
        };
    while i < line.len() {
        let b = line[i];
        i += 1;
        if let Some(q) = quote {
            if b == q {
                quote = None;
            } else {
                cur.push(b);
            }
            continue;
        }
        match b {
            b'\'' | b'"' => {
                quote = Some(b);
                in_word = true;
            }
            b' ' | b'\t' => end_word(&mut words, &mut cur, &mut in_word)?,
            b'|' => {
                end_word(&mut words, &mut cur, &mut in_word)?;
                if words.is_empty() {
                    return Err(ParseError::EmptyStage);
                }
                if stages.len() + 1 >= MAX_STAGES {
                    return Err(ParseError::TooManyStages);
                }
                stages.push(core::mem::take(&mut words));
                seen_pipe = true;
            }
            b'#' if !in_word => break, // a comment: the rest of the line is ignored
            _ => {
                cur.push(b);
                in_word = true;
            }
        }
    }
    if quote.is_some() {
        return Err(ParseError::UnterminatedQuote);
    }
    end_word(&mut words, &mut cur, &mut in_word)?;
    if words.is_empty() {
        if seen_pipe {
            return Err(ParseError::EmptyStage); // `a |`
        }
        return Ok(stages); // an empty or comment-only line
    }
    if stages.len() >= MAX_STAGES {
        return Err(ParseError::TooManyStages);
    }
    stages.push(words);
    Ok(stages)
}

/// Resolve `arg` against the lexical working directory `cwd` (an absolute path without a
/// trailing slash, `/` for the root). Returns the new absolute path, or `None` when `..` would
/// climb above `/`. `.` and empty components are ignored; the result has no trailing slash.
pub fn resolve(cwd: &[u8], arg: &[u8]) -> Option<Vec<u8>> {
    let mut parts: Vec<&[u8]> = Vec::new();
    if arg.first() != Some(&b'/') {
        for c in cwd.split(|&b| b == b'/') {
            if !c.is_empty() {
                parts.push(c);
            }
        }
    }
    for c in arg.split(|&b| b == b'/') {
        match c {
            b"" | b"." => {}
            b".." => {
                parts.pop()?;
            }
            _ => parts.push(c),
        }
    }
    if parts.is_empty() {
        return Some(alloc::vec![b'/']);
    }
    let mut out = Vec::new();
    for p in parts {
        out.push(b'/');
        out.extend_from_slice(p);
    }
    Some(out)
}

/// A command name the shell will look up in `/bin`: non-empty, no `/`.
pub fn valid_command_name(name: &[u8]) -> bool {
    !name.is_empty() && !name.contains(&b'/')
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::string::String;
    use std::vec;

    fn p(line: &str) -> Result<std::vec::Vec<std::vec::Vec<String>>, ParseError> {
        parse(line.as_bytes()).map(|stages| {
            stages
                .into_iter()
                .map(|s| {
                    s.into_iter()
                        .map(|w| String::from_utf8(w).unwrap())
                        .collect()
                })
                .collect()
        })
    }

    fn st(v: &[&[&str]]) -> std::vec::Vec<std::vec::Vec<String>> {
        v.iter()
            .map(|s| s.iter().map(|w| String::from(*w)).collect())
            .collect()
    }

    #[test]
    fn simple_commands() {
        assert_eq!(p("echo hi"), Ok(st(&[&["echo", "hi"]])));
        assert_eq!(p("  ls\t -l   dir "), Ok(st(&[&["ls", "-l", "dir"]])));
        assert_eq!(p(""), Ok(vec![]));
        assert_eq!(p("   \t "), Ok(vec![]));
    }

    #[test]
    fn pipelines() {
        assert_eq!(p("a | b | c"), Ok(st(&[&["a"], &["b"], &["c"]])));
        assert_eq!(p("a|b"), Ok(st(&[&["a"], &["b"]])));
        assert_eq!(
            p("cat x|echo y z"),
            Ok(st(&[&["cat", "x"], &["echo", "y", "z"]]))
        );
    }

    #[test]
    fn quotes_group_and_hide_syntax() {
        assert_eq!(p("echo 'a b' \"c d\""), Ok(st(&[&["echo", "a b", "c d"]])));
        assert_eq!(p("echo 'a|b' \"#x\""), Ok(st(&[&["echo", "a|b", "#x"]])));
        assert_eq!(
            p("echo \"it's\" 'say \"hi\"'"),
            Ok(st(&[&["echo", "it's", "say \"hi\""]]))
        );
        // adjacent quoted and bare text is one word; a quoted empty string is an empty word
        assert_eq!(p("echo a'b c'd"), Ok(st(&[&["echo", "ab cd"]])));
        assert_eq!(p("echo '' x"), Ok(st(&[&["echo", "", "x"]])));
        assert_eq!(p("echo \"\""), Ok(st(&[&["echo", ""]])));
        // no escapes: a backslash is an ordinary byte
        assert_eq!(p("echo a\\b"), Ok(st(&[&["echo", "a\\b"]])));
    }

    #[test]
    fn comments() {
        assert_eq!(p("# nothing"), Ok(vec![]));
        assert_eq!(p("echo a # b | c"), Ok(st(&[&["echo", "a"]])));
        assert_eq!(p("echo a#b"), Ok(st(&[&["echo", "a#b"]])));
        assert_eq!(p("echo a | # b"), Err(ParseError::EmptyStage));
    }

    #[test]
    fn errors() {
        assert_eq!(p("echo 'x"), Err(ParseError::UnterminatedQuote));
        assert_eq!(p("echo \"x"), Err(ParseError::UnterminatedQuote));
        assert_eq!(p("| a"), Err(ParseError::EmptyStage));
        assert_eq!(p("a |"), Err(ParseError::EmptyStage));
        assert_eq!(p("a || b"), Err(ParseError::EmptyStage));
        assert_eq!(p("|"), Err(ParseError::EmptyStage));
    }

    #[test]
    fn limits() {
        let ok = vec!["a"; MAX_STAGES].join("|");
        assert_eq!(p(&ok).unwrap().len(), MAX_STAGES);
        let too_many = vec!["a"; MAX_STAGES + 1].join("|");
        assert_eq!(p(&too_many), Err(ParseError::TooManyStages));
        let words = vec!["w"; MAX_WORDS].join(" ");
        assert_eq!(p(&words).unwrap()[0].len(), MAX_WORDS);
        let too_many_words = vec!["w"; MAX_WORDS + 1].join(" ");
        assert_eq!(p(&too_many_words), Err(ParseError::TooManyWords));
        // the word limit is per stage
        let two = std::format!("{} | {}", words, words);
        assert_eq!(p(&two).unwrap().len(), 2);
    }

    #[test]
    fn every_input_is_handled_without_panicking() {
        // all strings over a small alphabet up to length 6
        let alphabet = [b'a', b' ', b'|', b'\'', b'"', b'#', b'\t'];
        let mut buf = std::vec::Vec::new();
        fn go(buf: &mut std::vec::Vec<u8>, alphabet: &[u8], depth: usize) {
            let _ = parse(buf);
            if depth == 0 {
                return;
            }
            for &c in alphabet {
                buf.push(c);
                go(buf, alphabet, depth - 1);
                buf.pop();
            }
        }
        go(&mut buf, &alphabet, 6);
    }

    fn r(cwd: &str, arg: &str) -> Option<String> {
        resolve(cwd.as_bytes(), arg.as_bytes()).map(|v| String::from_utf8(v).unwrap())
    }

    #[test]
    fn lexical_paths() {
        assert_eq!(r("/home", "sub"), Some("/home/sub".into()));
        assert_eq!(r("/home", "sub/"), Some("/home/sub".into()));
        assert_eq!(r("/home", "."), Some("/home".into()));
        assert_eq!(r("/home", ".."), Some("/".into()));
        assert_eq!(r("/home/sub", "../.."), Some("/".into()));
        assert_eq!(r("/home/sub", "../x/./y"), Some("/home/x/y".into()));
        assert_eq!(r("/home", "/bin"), Some("/bin".into()));
        assert_eq!(r("/home", "/"), Some("/".into()));
        assert_eq!(r("/home", "//a//b"), Some("/a/b".into()));
        assert_eq!(r("/home", ""), Some("/home".into()));
        assert_eq!(r("/", ".."), None);
        assert_eq!(r("/home", "../.."), None);
        assert_eq!(r("/home", "/.."), None);
        assert_eq!(r("/", "a/../.."), None);
    }

    #[test]
    fn command_names() {
        assert!(valid_command_name(b"ls"));
        assert!(!valid_command_name(b""));
        assert!(!valid_command_name(b"bin/ls"));
        assert!(!valid_command_name(b"/ls"));
        assert!(!valid_command_name(b"../x"));
    }
}
