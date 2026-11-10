//! ANSI escape sequences for the serial terminal (decision D5: the terminal is
//! the monitor). Each helper writes its sequence to any [`core::fmt::Write`].

use core::fmt::{self, Write};

/// Clear the screen and move the cursor to the top left.
pub fn clear_screen(w: &mut impl Write) -> fmt::Result {
    w.write_str("\x1b[2J\x1b[H")
}

/// Reset colours and attributes.
pub fn reset(w: &mut impl Write) -> fmt::Result {
    w.write_str("\x1b[0m")
}

/// Set the foreground colour, and optionally the background, from the eight
/// standard colours 0 to 7 (black, red, green, yellow, blue, magenta, cyan,
/// white). Anything above 7 is an error and writes nothing.
pub fn color(w: &mut impl Write, fg: u8, bg: Option<u8>) -> fmt::Result {
    if fg > 7 || bg.is_some_and(|b| b > 7) {
        return Err(fmt::Error);
    }
    write!(w, "\x1b[3{fg}m")?;
    if let Some(bg) = bg {
        write!(w, "\x1b[4{bg}m")?;
    }
    Ok(())
}

/// Move the cursor to `row`, `col`, both counted from 1 as in the ANSI
/// standard. Zero is an error and writes nothing.
pub fn cursor_to(w: &mut impl Write, row: u16, col: u16) -> fmt::Result {
    if row == 0 || col == 0 {
        return Err(fmt::Error);
    }
    write!(w, "\x1b[{row};{col}H")
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::string::String;

    fn out(f: impl FnOnce(&mut String) -> fmt::Result) -> String {
        let mut s = String::new();
        f(&mut s).unwrap();
        s
    }

    #[test]
    fn sequences() {
        assert_eq!(out(clear_screen), "\x1b[2J\x1b[H");
        assert_eq!(out(reset), "\x1b[0m");
        assert_eq!(out(|w| color(w, 2, None)), "\x1b[32m");
        assert_eq!(out(|w| color(w, 7, Some(0))), "\x1b[37m\x1b[40m");
        assert_eq!(out(|w| cursor_to(w, 5, 12)), "\x1b[5;12H");
    }

    #[test]
    fn bad_arguments_write_nothing() {
        let mut s = String::new();
        assert!(color(&mut s, 8, None).is_err());
        assert!(color(&mut s, 1, Some(9)).is_err());
        assert!(cursor_to(&mut s, 0, 1).is_err());
        assert!(cursor_to(&mut s, 1, 0).is_err());
        assert_eq!(s, "");
    }
}
