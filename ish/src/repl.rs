//! The interactive loop and the batch mode (SHELL-SPEC section 9, step SH5). The loop owns the line editor and the
//! history; the host supplies bytes (`term_read`) and takes bytes (`write` to standard output). Nothing here knows what a
//! terminal is: the OS host switches the terminal to raw mode for editing and back to cooked for the programs it runs.

use crate::eval::Shell;
use crate::host::{Host, TermIn};
use crate::line::{Completion, Event, LineEditor};
use alloc::collections::VecDeque;
use alloc::string::String;

pub const PROMPT: &str = "$ ";
pub const CONTINUATION: &str = "> ";

fn write<H: Host>(sh: &mut Shell<H>, bytes: &[u8]) {
    let out = sh.host.stdout();
    let _ = sh.host.write(out, bytes);
}

/// Read commands from the terminal and run them until end of input, `exit` or the terminal going away. Returns the status
/// of the shell. A command that is not finished (an open block, bracket or pipe) continues on the next line with the
/// continuation prompt; Ctrl-C abandons what is being typed.
pub fn run_interactive<H: Host>(sh: &mut Shell<H>) -> i32 {
    let mut ed = LineEditor::new(PROMPT);
    let mut pending = String::new();
    // events typed ahead (several lines in one read) wait here
    let mut queue: VecDeque<Event> = VecDeque::new();
    'prompt: loop {
        ed.set_prompt(if pending.is_empty() {
            PROMPT
        } else {
            CONTINUATION
        });
        let p = ed.begin();
        write(sh, &p);
        let line = 'read: loop {
            if let Some(ev) = queue.pop_front() {
                match ev {
                    Event::Line(l) => break 'read l,
                    Event::Interrupt => {
                        pending.clear();
                        queue.clear();
                        continue 'prompt;
                    }
                    Event::Eof => {
                        if pending.is_empty() {
                            return 0;
                        }
                        pending.clear();
                        continue 'prompt;
                    }
                }
            }
            match sh.host.term_read() {
                TermIn::Gone => return 0,
                TermIn::Interrupt => {
                    // the terminal said so (the editor is idle: the typed text is abandoned)
                    write(sh, b"^C\n");
                    pending.clear();
                    continue 'prompt;
                }
                TermIn::Bytes(bytes) => {
                    let fed = ed.feed(&bytes, &mut |before: &str| -> Completion {
                        sh.complete(before)
                    });
                    write(sh, &fed.out);
                    queue.extend(fed.events);
                }
            }
        };
        let mut text = core::mem::take(&mut pending);
        text.push_str(&line);
        text.push('\n');
        if let Err(e) = crate::parser::parse(&text) {
            if e.incomplete {
                pending = text;
                continue 'prompt;
            }
        }
        if !line.trim().is_empty() && text.matches('\n').count() == 1 {
            ed.remember(&line);
        }
        sh.run_line(&text);
        if let Some(code) = sh.take_exit() {
            return code;
        }
    }
}

/// Run all of standard input as a script: no prompt, no echo, no banner (the mode for a shell that is not on a terminal).
pub fn run_batch<H: Host>(sh: &mut Shell<H>) -> i32 {
    let mut bytes = alloc::vec::Vec::new();
    let mut chunk = [0u8; 1024];
    let stdin = sh.host.stdin();
    loop {
        match sh.host.read(stdin, &mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if bytes.len() + n > sh.cap {
                    write(sh, b"sh: input too large\n");
                    return 2;
                }
                bytes.extend_from_slice(&chunk[..n]);
            }
        }
    }
    match String::from_utf8(bytes) {
        Ok(text) => sh.run_script(&text),
        Err(_) => {
            write(sh, b"sh: input is not valid UTF-8\n");
            2
        }
    }
}
