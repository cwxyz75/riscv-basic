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

/// `##PAUSE <test> hart <h> late <us> us` (T284): a window of a timing test that a pause of the host covered (a timer interrupt of
/// hart `h` taken `us` microseconds late inside it), so the window is taken again or, the third in a row, the test fails.
pub fn pause(out: &mut impl Write, test: &str, hart: usize, late_us: u64) -> fmt::Result {
    debug_assert!(!test.is_empty() && !test.contains(char::is_whitespace));
    writeln!(out, "##PAUSE {test} hart {hart} late {late_us} us")
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

/// The host ports of the test harness's fixtures for one boot (T195), from the value of `hostports=<tcp>,<dns>,<http>` on the kernel
/// line: the host TCP server, the scripted DNS server (UDP) and the HTTP server, which the guest reaches at 10.0.2.2 through slirp.
/// The harness binds each where the host has room (port 0), so two boots on one host get different ports. Packed for `debug_stat` 15 as
/// `tcp | dns << 16 | http << 32`; 0 (no fixtures: the tests that need them skip) unless the value is exactly three ports from 1 to 65535.
pub fn host_ports(value: &str) -> u64 {
    let mut parts = value.split(',');
    let mut packed = 0u64;
    for shift in [0u32, 16, 32] {
        // (digits only: `u16::from_str` would also take a leading `+`)
        let port = parts
            .next()
            .filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|p| p.parse::<u16>().ok());
        match port {
            Some(port) if port != 0 => packed |= u64::from(port) << shift,
            _ => return 0,
        }
    }
    if parts.next().is_some() {
        return 0;
    }
    packed
}

/// One port out of [`host_ports`]'s packing: `which` 0 the TCP server, 1 the DNS server, 2 the HTTP server; `None` when there is none
/// (or `which` names no fixture).
pub fn host_port(packed: u64, which: u32) -> Option<u16> {
    let shift = match which {
        0 => 0,
        1 => 16,
        2 => 32,
        _ => return None,
    };
    let port = (packed >> shift) as u16;
    (port != 0).then_some(port)
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
