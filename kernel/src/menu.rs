//! The boot menu: one keypress chooses which language the machine runs.
//! The choice lasts until the next boot; `BYE` (BASIC) and `bye` (LOGO) reboot
//! back here.

use crate::terminal::Console;
use crate::uart;
use core::fmt::Write;

/// The languages the machine can run.
pub enum Language {
    Basic,
    Logo,
}

/// Show the menu and wait for a choice: `1` BASIC or `2` LOGO. Any other key is
/// ignored.
pub fn choose() -> Language {
    let mut out = Console;
    let _ = writeln!(out, "\nChoose a language:\n  1) BASIC\n  2) LOGO");
    let _ = write!(out, "Your choice: ");
    loop {
        match uart::getc() {
            b'1' => {
                let _ = writeln!(out, "1");
                return Language::Basic;
            }
            b'2' => {
                let _ = writeln!(out, "2");
                return Language::Logo;
            }
            _ => {}
        }
    }
}
