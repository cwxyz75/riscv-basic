//! The boot menu: one keypress chooses which language the machine runs.
//! The choice lasts until the next boot; `BYE` reboots back here.

use crate::terminal::Console;
use crate::uart;
use core::fmt::Write;

/// The languages the machine can run.
pub enum Language {
    Basic,
}

/// Show the menu and wait for a valid choice. `1` selects BASIC. `2` (LOGO)
/// is announced as not installed yet and the prompt comes back; any other key
/// is ignored.
pub fn choose() -> Language {
    let mut out = Console;
    let _ = writeln!(out, "\nChoose a language:\n  1) BASIC\n  2) LOGO");
    loop {
        let _ = write!(out, "Your choice: ");
        loop {
            match uart::getc() {
                b'1' => {
                    let _ = writeln!(out, "1");
                    return Language::Basic;
                }
                b'2' => {
                    let _ = writeln!(out, "2\nLOGO is not installed yet.");
                    break;
                }
                _ => {}
            }
        }
    }
}
