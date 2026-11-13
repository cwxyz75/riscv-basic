//! `basic`: the BASIC interpreter of the home computer (the `basic` crate, unchanged) as an ordinary
//! program (S7a, S7-SPEC section 3).
//!
//! `basic` runs an interactive session on stdin/stdout: lines come from the stdin byte stream (the
//! console server delivers whole lines, and `0x04` at the start of a line is end of input), output
//! goes to stdout, the clock is `clock_get`. Ctrl-C ends the foreground job (the console server kills
//! it), so `break_pressed` never fires (TIDY-UP T47). PEEK and POKE act on a private sparse map;
//! there is no disk.
//!
//! `basic --golden NAME` runs one script of the golden corpus (`/etc/basic/golden/NAME.bas`, `<LOAD>`
//! paths resolved under `/etc/basic`) through the same scripted console as the host golden test
//! (`basic-script`) and prints the transcript; the caller compares it to `NAME.out`. The program never
//! judges itself.
#![no_std]
#![no_main]

extern crate alloc;

#[path = "../prog.rs"]
mod prog;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use basic::{Interpreter, Io, Line};
use proto::file::rights;
use rt::fs::Fs;
use rt::io;

/// The console behind the program's stdin and stdout.
struct StdioIo {
    stdin: Option<u32>,
    stdout: Option<u32>,
    /// Bytes read from stdin and not yet turned into a line.
    pending: Vec<u8>,
    memory: BTreeMap<u64, u8>,
    eof: bool,
    /// stdin is a terminal: `0x04` at the start of a line is end of input (on a pipe it is data).
    tty_in: bool,
}

impl Io for StdioIo {
    fn write_str(&mut self, s: &str) {
        if let Some(h) = self.stdout {
            if io::write_all(h, s.as_bytes()).is_err() {
                rt::sys::exit(prog::EXIT_PIPE); // the reader is gone
            }
        }
    }

    fn read_line(&mut self) -> Line {
        loop {
            // end of input: 0x04 as the first byte of a line (a terminal's Ctrl-D)
            if self.tty_in && self.pending.first() == Some(&0x04) {
                self.pending.clear();
                self.eof = true;
                return Line::Eof;
            }
            if let Some(i) = self.pending.iter().position(|&b| b == b'\n') {
                let mut line: Vec<u8> = self.pending.drain(..=i).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Line::Text(String::from_utf8_lossy(&line).into_owned());
            }
            if self.eof {
                return Line::Eof;
            }
            let Some(h) = self.stdin else {
                return Line::Eof;
            };
            let mut buf = [0u8; 256];
            match io::read(h, &mut buf) {
                Ok(0) | Err(_) => {
                    self.eof = true;
                    if self.pending.is_empty() {
                        return Line::Eof;
                    }
                    // a last line with no terminator
                    let line = core::mem::take(&mut self.pending);
                    return Line::Text(String::from_utf8_lossy(&line).into_owned());
                }
                Ok(n) => self.pending.extend_from_slice(&buf[..n]),
            }
        }
    }

    fn break_pressed(&mut self) -> bool {
        false
    }

    fn millis(&mut self) -> u64 {
        rt::sys::clock_get()
    }

    fn reboot(&mut self) {
        rt::sys::exit(0)
    }

    fn peek(&mut self, addr: u64) -> u8 {
        self.memory.get(&addr).copied().unwrap_or(0)
    }

    fn poke(&mut self, addr: u64, value: u8) {
        self.memory.insert(addr, value);
    }
}

/// Read a file under the root connection (`/etc/basic/<path>`); `None` if it cannot be read.
fn read_under_home(root: u32, path: &str) -> Option<String> {
    let mut dir = Fs::new(root); // the connection belongs to the program: never closed here
    let mut full = String::from("etc/basic/");
    full.push_str(path);
    let mut f = dir.open(full.as_bytes(), 0, rights::READ).ok()?;
    let data = f.read_all();
    f.close();
    String::from_utf8(data.ok()?).ok()
}

fn golden(p: &prog::Prog, name: &[u8]) -> Result<(), i32> {
    let Some(root) = p.env.namespace(b"/") else {
        p.complain(b"--golden", "no root namespace")?;
        return Err(1);
    };
    let Ok(name) = core::str::from_utf8(name) else {
        p.complain(name, "bad name")?;
        return Err(1);
    };
    let mut script = String::from("golden/");
    script.push_str(name);
    script.push_str(".bas");
    let Some(source) = read_under_home(root, &script) else {
        p.complain(script.as_bytes(), "not found")?;
        return Err(1);
    };
    match basic_script::transcript(&source, &mut |path| read_under_home(root, path)) {
        Ok(out) => p.out(out.as_bytes()),
        Err(e) => {
            p.complain(name.as_bytes(), &e)?;
            Err(1)
        }
    }
}

#[no_mangle]
pub fn app_main() -> i32 {
    let p = match prog::start() {
        Ok(p) => p,
        Err(c) => return c,
    };
    let args = p.args();
    if args.first().map(|a| &a[..]) == Some(&b"--golden"[..]) {
        return match args.get(1) {
            Some(name) if args.len() == 2 => match golden(&p, name) {
                Ok(()) => 0,
                Err(c) => c,
            },
            _ => {
                let _ = p.complain(b"", "usage: basic [--golden NAME]");
                2
            }
        };
    }
    if !args.is_empty() {
        let _ = p.complain(b"", "usage: basic [--golden NAME]");
        return 2;
    }
    let mut io = StdioIo {
        stdin: p.stdin,
        stdout: p.stdout,
        pending: Vec::new(),
        memory: BTreeMap::new(),
        eof: false,
        tty_in: p.env.var(b"STDIN") == Some(&b"tty"[..]),
    };
    Interpreter::new().run_session(&mut io);
    0
}
