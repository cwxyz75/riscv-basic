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

impl Default for SigTable {
    fn default() -> Self {
        Self::new()
    }
}
