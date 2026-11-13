//! The scripted console of the BASIC golden tests (`home/basic/tests/golden/`), as a library: the
//! host test (`home/basic/tests/golden.rs`) and the userland program (`basic --golden`, S7a) run the
//! very same code, and the `.out` files stay the only oracle.
//!
//! A script is fed to the interpreter line by line, as if typed at the prompt; the full console
//! transcript (prompts included, typed input NOT echoed) is the result. Directives:
//!
//! - `<LOAD path>`: replaced by that file's lines (the caller says how to read it).
//! - `<BREAK AFTER n>`: not typed input: the n-th following check for Ctrl-C reports "pressed".
//! - Disk directives (anywhere, applied before the script runs; no directive means no disk):
//!   `<DISK SECTORS n>` a freshly formatted disk of n sectors, `<NO DISK>`, `<BAD DISK>` an
//!   unformatted one, `<DISK ERROR>` one whose reads fail, and `<FILE BASIC NAME>` /
//!   `<FILE LOGO NAME>` through `<END FILE>` puts the lines between as a file on the disk.
//!
//! The memory behind PEEK/POKE is a sparse byte map (unset bytes read 0), and the clock advances
//! 250 ms every time it is read, so a run is deterministic.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use basic::{Interpreter, Io, Line};
use cartfs::{Disk, Files, Kind, MemDisk, Unavailable};

/// A scripted console.
pub struct ScriptIo {
    input: VecDeque<String>,
    output: String,
    memory: BTreeMap<u64, u8>,
    clock_ms: u64,
    /// Checks left until a simulated Ctrl-C.
    break_after: Option<u64>,
    files: Option<Box<dyn Files>>,
}

impl Io for ScriptIo {
    fn write_str(&mut self, s: &str) {
        self.output.push_str(s);
    }

    fn read_line(&mut self) -> Line {
        while let Some(line) = self.input.pop_front() {
            let directive = line
                .strip_prefix("<BREAK AFTER ")
                .and_then(|rest| rest.strip_suffix('>'))
                .and_then(|n| n.parse::<u64>().ok());
            match directive {
                Some(n) => self.break_after = Some(n),
                None => return Line::Text(line),
            }
        }
        Line::Eof
    }

    fn break_pressed(&mut self) -> bool {
        match self.break_after {
            Some(n) if n <= 1 => {
                self.break_after = None;
                true
            }
            Some(n) => {
                self.break_after = Some(n - 1);
                false
            }
            None => false,
        }
    }

    fn millis(&mut self) -> u64 {
        let now = self.clock_ms;
        self.clock_ms += 250;
        now
    }

    fn reboot(&mut self) {
        self.output.push_str("[reboot]\n");
    }

    fn files(&mut self) -> Option<&mut dyn Files> {
        match &mut self.files {
            Some(files) => Some(&mut **files),
            None => None,
        }
    }

    fn peek(&mut self, addr: u64) -> u8 {
        self.memory.get(&addr).copied().unwrap_or(0)
    }

    fn poke(&mut self, addr: u64, value: u8) {
        self.memory.insert(addr, value);
    }
}

/// The script's lines with `<LOAD path>` expanded; `load` returns a file's text.
pub fn expand(
    source: &str,
    load: &mut dyn FnMut(&str) -> Option<String>,
) -> Result<Vec<String>, String> {
    let mut lines = Vec::new();
    for line in source.lines() {
        match line
            .strip_prefix("<LOAD ")
            .and_then(|rest| rest.strip_suffix('>'))
        {
            Some(path) => {
                let text = load(path).ok_or_else(|| format!("cannot load {path}"))?;
                lines.extend(text.lines().map(str::to_string));
            }
            None => lines.push(line.to_string()),
        }
    }
    Ok(lines)
}

/// The disk a script describes (if any) and the lines left to type.
pub type Split = (Option<Box<dyn Files>>, Vec<String>);

/// Split the disk directives out of a script.
pub fn take_disk(lines: Vec<String>) -> Result<Split, String> {
    let mut disk: Option<Box<dyn Files>> = None;
    let mut sectors = None;
    let mut files: Vec<(Kind, String, String)> = Vec::new();
    let mut typed = Vec::new();
    let mut open: Option<(Kind, String, String)> = None;
    for line in lines {
        if let Some(file) = open.as_mut() {
            if line == "<END FILE>" {
                files.extend(open.take());
            } else {
                file.2.push_str(&line);
                file.2.push('\n');
            }
        } else if let Some(n) = line
            .strip_prefix("<DISK SECTORS ")
            .and_then(|r| r.strip_suffix('>'))
        {
            sectors = Some(n.parse::<u32>().map_err(|_| format!("sector count {n}"))?);
        } else if line == "<NO DISK>" {
        } else if line == "<BAD DISK>" {
            disk = Some(Box::new(Unavailable(cartfs::Error::NotFormatted)));
        } else if line == "<DISK ERROR>" {
            disk = Some(Box::new(Unavailable(cartfs::Error::Device)));
        } else if let Some(rest) = line
            .strip_prefix("<FILE ")
            .and_then(|r| r.strip_suffix('>'))
        {
            let (kind, name) = rest
                .split_once(' ')
                .ok_or_else(|| String::from("<FILE KIND NAME>"))?;
            let kind = match kind {
                "BASIC" => Kind::Basic,
                "LOGO" => Kind::Logo,
                other => return Err(format!("unknown file kind {other}")),
            };
            open = Some((kind, name.to_string(), String::new()));
        } else {
            typed.push(line);
        }
    }
    if let Some(n) = sectors {
        let mut d = Disk::format(MemDisk::new(n)).map_err(|_| String::from("format"))?;
        for (kind, name, text) in files {
            d.save(&name, kind, text.as_bytes())
                .map_err(|_| String::from("preload file"))?;
        }
        disk = Some(Box::new(d));
    }
    Ok((disk, typed))
}

/// Run a script and return its console transcript.
pub fn transcript(
    source: &str,
    load: &mut dyn FnMut(&str) -> Option<String>,
) -> Result<String, String> {
    let (files, typed) = take_disk(expand(source, load)?)?;
    let mut io = ScriptIo {
        input: typed.into(),
        output: String::new(),
        memory: BTreeMap::new(),
        clock_ms: 0,
        break_after: None,
        files,
    };
    Interpreter::new().run_session(&mut io);
    Ok(io.output)
}
