use crate::io::{Io, Line};

/// The interpreter state: program store, variables, stacks, ...
///
/// Milestone status: this is a stub. It prompts, reads lines, and answers
/// every non-empty line with `?SYNTAX ERROR`. Real behaviour arrives with the
/// interpreter milestones in `ROADMAP.md`, specified by `BASIC.md`.
#[derive(Debug, Default)]
pub struct Interpreter {}

impl Interpreter {
    pub fn new() -> Self {
        Self::default()
    }

    /// The top-level loop: print `READY.`, read a line, handle it, repeat.
    /// Returns only when the console reports end of input.
    pub fn run_session<I: Io>(&mut self, io: &mut I) {
        loop {
            io.write_str("READY.\n");
            match io.read_line() {
                Line::Text(text) => self.handle_line(io, &text),
                Line::Break => {}
                Line::Eof => return,
            }
        }
    }

    /// Handle one line typed at the prompt: either a program line to store
    /// (starts with a line number) or a statement/command to run immediately.
    pub fn handle_line<I: Io>(&mut self, io: &mut I, line: &str) {
        if line.trim().is_empty() {
            return;
        }
        io.write_str("?SYNTAX ERROR\n");
    }
}
