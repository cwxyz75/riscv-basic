//! The banner and the program-memory accounting behind its BYTES FREE figure.

use basic::{Interpreter, Io, Line};

/// A console that only records output.
#[derive(Default)]
struct RecordIo {
    output: String,
}

impl Io for RecordIo {
    fn write_str(&mut self, s: &str) {
        self.output.push_str(s);
    }
    fn read_line(&mut self) -> Line {
        Line::Eof
    }
    fn break_pressed(&mut self) -> bool {
        false
    }
    fn millis(&mut self) -> u64 {
        0
    }
    fn peek(&mut self, _: u64) -> u8 {
        0
    }
    fn poke(&mut self, _: u64, _: u8) {}
}

fn banner(interp: &mut Interpreter) -> String {
    let mut io = RecordIo::default();
    interp.banner(&mut io);
    io.output
}

#[test]
fn fresh_banner() {
    assert_eq!(
        banner(&mut Interpreter::new()),
        "RISCV BASIC V0.1\n32768 BYTES FREE\n\n"
    );
}

#[test]
fn bytes_free_counts_tokens_plus_five_per_line() {
    let mut interp = Interpreter::new();
    let mut io = RecordIo::default();
    // `PRINT 1` stores as PRINT, space, '1' = 3 bytes, plus 5 overhead.
    interp.handle_line(&mut io, "10 PRINT 1");
    assert_eq!(interp.bytes_free(), 32768 - 8);
    // Replacing it with a one-byte body (just REM) costs 1 + 5.
    interp.handle_line(&mut io, "10 REM");
    assert_eq!(interp.bytes_free(), 32768 - 6);
    interp.handle_line(&mut io, "10");
    assert_eq!(interp.bytes_free(), 32768);
    assert_eq!(io.output, "");
}

#[test]
fn banner_shows_current_free_space() {
    let mut interp = Interpreter::new();
    interp.handle_line(&mut RecordIo::default(), "10 PRINT 1");
    assert_eq!(
        banner(&mut interp),
        "RISCV BASIC V0.1\n32760 BYTES FREE\n\n"
    );
}
