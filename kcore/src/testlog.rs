//! The structured serial lines the OS-track kernel prints (OS-ARCHITECTURE.md,
//! 3.2.1). Formatting only; `scripts/os_boot_test.py` has its own parser.

use core::fmt::{self, Write};

/// `##BOOT <name> <version>`
pub fn boot(out: &mut impl Write, name: &str, version: &str) -> fmt::Result {
    writeln!(out, "##BOOT {name} {version}")
}

/// `##TEST <name> PASS`, or `##TEST <name> FAIL <detail>`. Names have no spaces;
/// a detail is one line (newlines in it become spaces).
pub fn test(out: &mut impl Write, name: &str, result: Result<(), &str>) -> fmt::Result {
    debug_assert!(!name.is_empty() && !name.contains(char::is_whitespace));
    match result {
        Ok(()) => writeln!(out, "##TEST {name} PASS"),
        Err(detail) => {
            write!(out, "##TEST {name} FAIL ")?;
            one_line(out, detail)?;
            writeln!(out)
        }
    }
}

/// `##PANIC <message>`
pub fn panic(out: &mut impl Write, message: &dyn fmt::Display) -> fmt::Result {
    write!(out, "\n##PANIC ")?;
    let mut line = OneLine(out);
    write!(line, "{message}")?;
    writeln!(out)
}

/// `##DONE pass=<n> fail=<n>`
pub fn done(out: &mut impl Write, pass: u32, fail: u32) -> fmt::Result {
    writeln!(out, "##DONE pass={pass} fail={fail}")
}

fn one_line(out: &mut impl Write, text: &str) -> fmt::Result {
    for c in text.chars() {
        out.write_char(if c == '\n' || c == '\r' { ' ' } else { c })?;
    }
    Ok(())
}

struct OneLine<'a, W: Write>(&'a mut W);

impl<W: Write> Write for OneLine<'_, W> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        one_line(self.0, s)
    }
}

#[cfg(all(test, not(any(loom, shuttle))))]
mod tests {
    extern crate std;
    use super::*;
    use std::string::String;

    #[test]
    fn lines_have_the_documented_shape() {
        let mut s = String::new();
        boot(&mut s, "kernel", "s0").unwrap();
        test(&mut s, "a_b", Ok(())).unwrap();
        test(&mut s, "c", Err("bad\nthing")).unwrap();
        done(&mut s, 1, 1).unwrap();
        assert_eq!(
            s,
            "##BOOT kernel s0\n##TEST a_b PASS\n##TEST c FAIL bad thing\n##DONE pass=1 fail=1\n"
        );
    }

    #[test]
    fn a_panic_starts_on_a_fresh_line_and_stays_on_one() {
        let mut s = String::new();
        panic(&mut s, &format_args!("boom\nat line {}", 3)).unwrap();
        assert_eq!(s, "\n##PANIC boom at line 3\n");
    }
}
