//! Shared start-up and output helpers of the small user programs (`echo`, `cat`, `ls`, `probe`,
//! `spin`): included with `#[path]`, because each program is its own binary.
#![allow(dead_code)]

use alloc::vec::Vec;
use proto::startup::Role;
use rt::io::{self, IoError};
use rt::startup::{self, Env, EXIT_BAD_STARTUP, EXIT_NO_STARTUP};

/// Exit code of a program whose output pipe was closed (128 + SIGPIPE): the shell does not
/// report it.
pub const EXIT_PIPE: i32 = 141;

pub struct Prog {
    pub env: Env,
    pub stdin: Option<u32>,
    pub stdout: Option<u32>,
}

/// Read the startup message; `Err` is the exit code to return.
pub fn start() -> Result<Prog, i32> {
    let env = match startup::read() {
        Ok(e) => e,
        Err(startup::EnvError::NoMessage) => return Err(EXIT_NO_STARTUP),
        Err(_) => return Err(EXIT_BAD_STARTUP),
    };
    let stdin = env.handle(Role::Stdin);
    let stdout = env.handle(Role::Stdout);
    Ok(Prog { env, stdin, stdout })
}

impl Prog {
    /// The arguments after the program name.
    pub fn args(&self) -> &[Vec<u8>] {
        self.env.startup.args.get(1..).unwrap_or(&[])
    }

    pub fn name(&self) -> &[u8] {
        self.env.startup.args.first().map_or(&b"?"[..], |a| &a[..])
    }

    /// Write to STDOUT. `Err` is the exit code to return (141 when the reader is gone).
    pub fn out(&self, bytes: &[u8]) -> Result<(), i32> {
        let Some(h) = self.stdout else {
            return Err(1);
        };
        match io::write_all(h, bytes) {
            Ok(()) => Ok(()),
            Err(IoError::PeerClosed) => Err(EXIT_PIPE),
            Err(_) => Err(1),
        }
    }

    /// `name: message` on STDOUT (errors go to the same stream as output, S4-SPEC section 8).
    pub fn complain(&self, what: &[u8], why: &str) -> Result<(), i32> {
        let mut line: Vec<u8> = Vec::new();
        line.extend_from_slice(self.name());
        line.extend_from_slice(b": ");
        if !what.is_empty() {
            line.extend_from_slice(what);
            line.extend_from_slice(b": ");
        }
        line.extend_from_slice(why.as_bytes());
        line.push(b'\n');
        self.out(&line)
    }

    /// The connection a path is relative to: `/` for absolute paths when the program was given a
    /// root, else the working directory `.`. Returns the connection handle and the path to open
    /// there (an absolute path loses its leading slashes). `None`: nothing to resolve against.
    pub fn base_for<'a>(&self, path: &'a [u8]) -> Option<(u32, &'a [u8])> {
        if path.first() == Some(&b'/') {
            let h = self.env.namespace(b"/")?;
            let mut p = path;
            while p.first() == Some(&b'/') {
                p = &p[1..];
            }
            Some((h, p))
        } else {
            Some((self.env.namespace(b".")?, path))
        }
    }
}

/// The word for a file-protocol status in the programs' messages.
pub fn why(status: i32) -> &'static str {
    use proto::file::status as st;
    match status {
        st::NOT_FOUND => "not found",
        st::IS_DIRECTORY => "not a file",
        st::NOT_DIRECTORY => "not a directory",
        st::BAD_REQUEST | st::NAME_TOO_LONG => "bad path",
        -3 => "access denied",
        _ => "error",
    }
}
