//! The signal table of a program: the dispositions, the masks and the flags set by `sigaction`, the blocked set and the pending set,
//! and the one step that decides what is delivered next. Pure (no handler is called here); the C library runs the handler and puts
//! the blocked set back. Tested on the host against the host's own `sigaction` and `sigprocmask` (nesting, masks, `SA_NODEFER`,
//! `SA_RESETHAND`).

pub const SIG_DFL: usize = 0;
pub const SIG_IGN: usize = 1;

#[derive(Clone, Copy)]
pub struct SigTable {
    pub handler: [usize; 65],
    pub mask: [u64; 65],
    pub flags: [i32; 65],
    pub blocked: u64,
    pub pending: u64,
}

/// A signal chosen for delivery and what the table was before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ready {
    pub sig: i32,
    pub handler: usize,
    pub flags: i32,
    /// The blocked set to put back when a handler returns.
    pub old_blocked: u64,
}

pub fn bit(sig: i32) -> u64 {
    1u64 << (sig - 1)
}

impl SigTable {
    pub const fn new() -> Self {
        SigTable {
            handler: [SIG_DFL; 65],
            mask: [0; 65],
            flags: [0; 65],
            blocked: 0,
            pending: 0,
        }
    }

    /// Take the lowest-numbered pending signal that is not blocked, if any. When it has a handler (neither default nor ignored),
    /// the blocked set grows by the handler's mask and, unless `nodefer` is set in its flags, the signal itself, and `resethand`
    /// makes the disposition the default again, as `sigaction` says; the caller runs the handler and then sets `blocked` back to
    /// `old_blocked`. The flag values are the caller's (they differ between systems).
    pub fn take_ready(&mut self, nodefer: i32, resethand: i32) -> Option<Ready> {
        let ready = self.pending & !self.blocked;
        if ready == 0 {
            return None;
        }
        let sig = ready.trailing_zeros() as i32 + 1;
        self.pending &= !bit(sig);
        let (handler, mask, flags) = (
            self.handler[sig as usize],
            self.mask[sig as usize],
            self.flags[sig as usize],
        );
        let old_blocked = self.blocked;
        if handler > SIG_IGN {
            self.blocked |= mask | if flags & nodefer == 0 { bit(sig) } else { 0 };
            if flags & resethand != 0 {
                self.handler[sig as usize] = SIG_DFL;
            }
        }
        Some(Ready {
            sig,
            handler,
            flags,
            old_blocked,
        })
    }
}

/// What a signal does to a program that has not asked for anything else (POSIX 2.4.3, the table of default actions).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// T: end the program as killed by the signal.
    Terminate,
    /// A: end it as killed by the signal, "with additional actions" (a core file on Unix; there are none here, so the same).
    Abort,
    /// I: nothing.
    Ignore,
    /// S: stop the program (needs the kernel's suspend; until then the library treats it as `Ignore`).
    Stop,
    /// C: continue a stopped program, otherwise nothing.
    Continue,
}

/// The default action of signal `sig` (Linux numbers). Signals with no number of their own (the real-time range) terminate; the
/// non-POSIX `SIGWINCH` is ignored, as everywhere.
pub fn default_action(sig: i32) -> Action {
    match sig {
        1 | 2 | 9 | 10 | 12 | 13 | 14 | 15 | 26 | 27 | 29 => Action::Terminate,
        3 | 4 | 5 | 6 | 7 | 8 | 11 | 24 | 25 | 31 => Action::Abort,
        17 | 23 | 28 => Action::Ignore,
        19..=22 => Action::Stop,
        18 => Action::Continue,
        _ => Action::Terminate,
    }
}

/// The signal a hardware fault (the supervisor `scause`) stands for, as Linux maps it on RISC-V: misaligned access SIGBUS,
/// illegal instruction SIGILL, breakpoint SIGTRAP, every access and page fault SIGSEGV.
pub fn fault_signal(scause: u64) -> i32 {
    match scause {
        0 | 4 | 6 => 7,
        2 => 4,
        3 => 5,
        _ => 11,
    }
}

impl Default for SigTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Action::*;

    /// POSIX 2.4.3's table (and `SIGSTKFLT`'s absence): every signal of the base and XSI lists, Linux numbers.
    #[test]
    fn default_actions_are_the_posix_table() {
        let table: &[(i32, &str, Action)] = &[
            (6, "ABRT", Abort),
            (14, "ALRM", Terminate),
            (7, "BUS", Abort),
            (17, "CHLD", Ignore),
            (18, "CONT", Continue),
            (8, "FPE", Abort),
            (1, "HUP", Terminate),
            (4, "ILL", Abort),
            (2, "INT", Terminate),
            (9, "KILL", Terminate),
            (13, "PIPE", Terminate),
            (3, "QUIT", Abort),
            (11, "SEGV", Abort),
            (19, "STOP", Stop),
            (15, "TERM", Terminate),
            (20, "TSTP", Stop),
            (21, "TTIN", Stop),
            (22, "TTOU", Stop),
            (10, "USR1", Terminate),
            (12, "USR2", Terminate),
            (29, "POLL", Terminate),
            (27, "PROF", Terminate),
            (31, "SYS", Abort),
            (5, "TRAP", Abort),
            (23, "URG", Ignore),
            (26, "VTALRM", Terminate),
            (24, "XCPU", Abort),
            (25, "XFSZ", Abort),
        ];
        for &(n, name, want) in table {
            assert_eq!(default_action(n), want, "SIG{name}");
        }
        assert_eq!(default_action(28), Ignore); // SIGWINCH
        for rt in 34..=64 {
            assert_eq!(default_action(rt), Terminate, "real-time {rt}");
        }
    }

    #[test]
    fn faults_become_the_signals_linux_gives() {
        for (scause, sig) in [
            (0, 7),
            (4, 7),
            (6, 7),
            (2, 4),
            (3, 5),
            (1, 11),
            (5, 11),
            (7, 11),
            (12, 11),
            (13, 11),
            (15, 11),
        ] {
            assert_eq!(fault_signal(scause), sig, "scause {scause}");
        }
    }
}
