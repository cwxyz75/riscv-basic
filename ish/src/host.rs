//! The machine the shell runs on (SHELL-SPEC section 9): processes, byte streams and files, behind one trait, so the
//! evaluator is pure. The OS implementation (SH4) starts programs through the launcher with an explicit table; the tests
//! use [`crate::fake::FakeHost`].

use alloc::string::String;
use alloc::vec::Vec;

/// A byte stream the host owns: a pipe end, a file, or one of the shell's own standard streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Stream(pub u32);

/// A started program.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pid(pub u32);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostError {
    NotFound,
    Denied,
    /// The other end of a pipe is gone.
    Closed,
    Other(String),
}

impl HostError {
    pub fn message(&self) -> String {
        match self {
            HostError::NotFound => String::from("not found"),
            HostError::Denied => String::from("permission denied"),
            HostError::Closed => String::from("broken pipe"),
            HostError::Other(m) => m.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMode {
    Read,
    /// Create or truncate.
    Write,
    Append,
}

/// The three standard streams of a started program.
#[derive(Debug, Clone, Copy)]
pub struct ProcIo {
    pub stdin: Stream,
    pub stdout: Stream,
    pub stderr: Stream,
}

/// What the terminal gave the shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TermIn {
    /// Bytes typed (the terminal is in raw mode while the shell edits a line).
    Bytes(Vec<u8>),
    /// The terminal reported an interrupt (Ctrl-C) while no job was running.
    Interrupt,
    /// The terminal is gone.
    Gone,
}

pub trait Host {
    /// The shell's own standard streams.
    fn stdin(&self) -> Stream;
    fn stdout(&self) -> Stream;
    fn stderr(&self) -> Stream;

    /// Start the program `argv[0]` with the given streams (the host keeps its own copies; the caller still owns its
    /// handles and closes them). `Err(NotFound)` when there is no such program.
    fn spawn(&mut self, argv: &[String], io: ProcIo) -> Result<Pid, HostError>;
    /// Wait for the program to end; its exit status.
    fn wait(&mut self, pid: Pid) -> i32;

    /// A pipe: (read end, write end).
    fn pipe(&mut self) -> Result<(Stream, Stream), HostError>;
    /// Read up to `buf.len()` bytes; `Ok(0)` is end of input.
    fn read(&mut self, s: Stream, buf: &mut [u8]) -> Result<usize, HostError>;
    /// Write all of `bytes`; `Err(Closed)` when the reader is gone.
    fn write(&mut self, s: Stream, bytes: &[u8]) -> Result<(), HostError>;
    /// Block until one of `streams` has data (or is at end), read it into `buf`: `(index into streams, bytes)`, where 0
    /// bytes means that stream is at end. This lets the shell drain a program's stdout and stderr without deadlock.
    fn poll_read(
        &mut self,
        streams: &[Stream],
        buf: &mut [u8],
    ) -> Result<(usize, usize), HostError>;
    fn close(&mut self, s: Stream);
    fn open(&mut self, path: &str, mode: OpenMode) -> Result<Stream, HostError>;

    /// Whole-file helpers.
    fn read_file(&mut self, path: &str) -> Result<Vec<u8>, HostError>;
    fn write_file(&mut self, path: &str, bytes: &[u8]) -> Result<(), HostError>;
    /// Expand a pattern with `*` and `?` (per path component) into sorted matches.
    fn glob(&mut self, pattern: &str) -> Vec<String>;

    /// All stages of a pipeline are running: make them the foreground job (so an interrupt reaches them). `false` if that
    /// could not be done (the shell then kills them and reports it).
    fn started(&mut self, _pids: &[Pid]) -> bool {
        true
    }
    /// The pipeline is over (every stage waited for): release the foreground job and what was made for it.
    fn finished(&mut self) {}
    /// End a started program that is no longer wanted.
    fn kill(&mut self, _pid: Pid) {}
    /// The `halt` builtin: ask the system to end the session.
    fn halt(&mut self) {}

    /// The namespace table the shell holds: `(path, rights)` with rights written `r`, `rw`.
    fn table(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    /// Narrow what programs started from now on may reach to exactly these grants (`confine`): each is a path in the shell's
    /// table and a rights string. Nested confinement can only narrow (SHELL-SPEC SHI2): a grant the current confinement
    /// does not cover is `Err(Denied)`.
    fn confine(&mut self, _grants: &[(String, String)]) -> Result<(), HostError> {
        Err(HostError::Other(String::from(
            "confine is not supported here",
        )))
    }
    /// End the innermost `confine`.
    fn unconfine(&mut self) {}

    /// Wait for the terminal (interactive hosts only).
    fn term_read(&mut self) -> TermIn {
        TermIn::Gone
    }
    /// The entries of a directory, `(name, is_directory)`, for completion.
    fn list_dir(&mut self, _path: &str) -> Result<Vec<(String, bool)>, HostError> {
        Err(HostError::NotFound)
    }
    /// Has the user interrupted (Ctrl-C) since the last call? Polled between statements and loop iterations, so a loop
    /// made of shell code can be stopped.
    fn interrupted(&mut self) -> bool {
        false
    }

    fn getenv(&self, name: &str) -> Option<String>;
    fn setenv(&mut self, name: &str, value: &str);
    /// All environment variables, in name order.
    fn environ(&self) -> Vec<(String, String)>;
    fn cwd(&self) -> String;
    fn chdir(&mut self, path: &str) -> Result<(), HostError>;
}
