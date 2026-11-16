//! Completion for the line editor (SHELL-SPEC section 9): command names at the start of a command, variable names after `$`,
//! and paths elsewhere, from the shell's own table (the host lists directories, so only what the shell can reach is offered).

use crate::eval::Shell;
use crate::host::Host;
use crate::line::Completion;
use alloc::string::String;
use alloc::vec::Vec;

fn is_word_break(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '(' | ')' | '[' | ']' | '{' | '}' | ';' | '|' | '&' | '>' | '<' | '^' | ',' | ':'
        )
}

impl<H: Host> Shell<H> {
    /// Candidates for the word that ends at the end of `before` (the text left of the cursor).
    pub fn complete(&mut self, before: &str) -> Completion {
        let chars: Vec<char> = before.chars().collect();
        // inside an open quote: no completion
        let (mut single, mut double) = (false, false);
        for &c in &chars {
            match c {
                '\'' if !double => single = !single,
                '"' if !single => double = !double,
                _ => {}
            }
        }
        if single || double {
            return Completion::default();
        }
        let mut start = chars.len();
        while start > 0 && !is_word_break(chars[start - 1]) {
            start -= 1;
        }
        let word: String = chars[start..].iter().collect();
        // is this the first word of a command?
        let head: String = chars[..start].iter().collect();
        let trimmed = head.trim_end();
        let last_word = trimmed
            .rsplit(|c: char| c.is_whitespace())
            .next()
            .unwrap_or("");
        let command_position = trimmed.is_empty()
            || trimmed.ends_with(['|', ';', '(', '&'])
            || matches!(last_word, "do" | "else" | "!" | "then");
        let mut items: Vec<String> = Vec::new();
        if let Some(prefix) = word.strip_prefix('$') {
            for n in self.var_names() {
                if n.starts_with(prefix) {
                    items.push(alloc::format!("${n}"));
                }
            }
        } else if command_position && !word.contains('/') {
            for n in crate::builtins::names() {
                if n.starts_with(&word) {
                    items.push(String::from(*n));
                }
            }
            for n in self.fn_names() {
                if n.starts_with(&word) {
                    items.push(n);
                }
            }
            if let Ok(list) = self.host.list_dir("/bin") {
                for (n, is_dir) in list {
                    if !is_dir && n.starts_with(&word) {
                        items.push(n);
                    }
                }
            }
        } else {
            let (dir, prefix) = match word.rfind('/') {
                Some(i) => (&word[..=i], &word[i + 1..]),
                None => ("", word.as_str()),
            };
            let path = if dir.is_empty() {
                String::from(".")
            } else {
                String::from(dir)
            };
            if let Ok(list) = self.host.list_dir(&path) {
                for (n, is_dir) in list {
                    if n.starts_with(prefix) && (!n.starts_with('.') || prefix.starts_with('.')) {
                        items.push(alloc::format!("{dir}{n}{}", if is_dir { "/" } else { "" }));
                    }
                }
            }
        }
        items.sort();
        items.dedup();
        Completion { start, items }
    }
}
