//! Signals (S11, OD119): a subset delivered *where a program waits or calls the library*, not asynchronously into a running
//! loop. The shell writes one byte (the signal number) to the channel a program holds as `Role::SignalChannel`; the library
//! reads it whenever the program blocks (a stream read or write, `sleep`, `pause`, `sigsuspend`, `waitpid`, `poll`) and on
//! the calls that can unblock one (`raise`, `sigprocmask`, `kill` of itself), runs the handler on the calling thread, and a
//! blocking call a handler interrupted returns `EINTR` (unless the handler was installed with `SA_RESTART`, which restarts
//! `read`, `write` and `waitpid` but never `sleep`, `pause`, `sigsuspend` or `poll`). `alarm` and `setitimer` are deadlines
//! the same waits honour. A default action that ends the program ends it with the exit code `-2000 - signal`, which
//! `waitpid` reports as "killed by that signal"; SIGKILL is the shell ending the process (a program cannot catch it).
//! Not here: signals into a loop that never waits, per-thread masks, `SIGCHLD`, job control (stop and continue are ignored).

use crate::errno;
use core::ffi::{c_int, c_long};
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use rt::abi::port_flags;
use rt::abi::signals as sg;
use rt::abi::status::Status;
use rt::sync::Mutex;
use rt::sys;

pub type Handler = usize;
pub const SIG_DFL: Handler = 0;
pub const SIG_IGN: Handler = 1;
pub const SIG_ERR: Handler = usize::MAX;

pub const SIGKILL: c_int = 9;
pub const SIGSTOP: c_int = 19;
pub const SIGALRM: c_int = 14;
const SA_RESTART: c_int = 0x1000_0000;
const SA_NODEFER: c_int = 0x4000_0000;
const SA_RESETHAND: c_int = 0x8000_0000u32 as c_int;
const SIG_BLOCK: c_int = 0;
const SIG_UNBLOCK: c_int = 1;
const SIG_SETMASK: c_int = 2;
const EINTR: c_int = 4;

/// The exit code of a program ended by the default action of signal `sig`.
pub const SIGNALED_BASE: i32 = -2000;

static CHAN: AtomicU32 = AtomicU32::new(0);
static OWN_PID: AtomicU32 = AtomicU32::new(0);
static PARENT_PID: AtomicU32 = AtomicU32::new(0);
/// The absolute time (ms) the pending `alarm` fires, 0 for none, and its repeat interval (0 for none).
static ALARM_AT: AtomicU64 = AtomicU64::new(0);
static ALARM_EVERY: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
struct Table {
    handler: [Handler; 65],
    mask: [u64; 65],
    flags: [c_int; 65],
    blocked: u64,
    pending: u64,
}

static TABLE: Mutex<Table> = Mutex::new(Table {
    handler: [SIG_DFL; 65],
    mask: [0; 65],
    flags: [0; 65],
    blocked: 0,
    pending: 0,
});

/// The signal state a `vfork` child must not change for its parent: saved when `vfork` is called, put back when the child ends
/// (the signals that arrived meanwhile stay pending).
static VFORK_SAVED: Mutex<Option<Table>> = Mutex::new(None);

pub fn vfork_save() {
    let now = *TABLE.lock();
    *VFORK_SAVED.lock() = Some(now);
}

pub fn vfork_restore() {
    if let Some(saved) = VFORK_SAVED.lock().take() {
        let mut t = TABLE.lock();
        let pending = t.pending;
        *t = saved;
        t.pending |= pending;
        let handled = t.handler[2] != SIG_DFL;
        drop(t);
        notify_int(handled);
    }
}

/// Whether the shell has been told that this program handles SIGINT (so that Ctrl-C is passed to it as a signal, not an end).
static INT_HANDLED: AtomicU8 = AtomicU8::new(0);

/// Tell the shell when SIGINT's disposition changes between the default and something else (a handler, or ignoring it): the
/// console server ends the foreground programs on Ctrl-C, and the shell keeps off its list the ones that handle the signal and
/// sends them SIGINT instead (T47).
fn notify_int(handled: bool) {
    let ch = channel();
    if ch == 0 || INT_HANDLED.swap(u8::from(handled), Ordering::Relaxed) == u8::from(handled) {
        return;
    }
    sys::channel_write(
        ch,
        [b'H', 2, u8::from(handled)].as_ptr(),
        3,
        core::ptr::null(),
        0,
    );
}

fn bit(sig: c_int) -> u64 {
    1u64 << (sig - 1)
}

/// Install the signal channel and the pids (start-up); 0 means none.
pub fn init(chan: u32, pid: u32, ppid: u32) {
    CHAN.store(chan, Ordering::Relaxed);
    OWN_PID.store(pid, Ordering::Relaxed);
    PARENT_PID.store(ppid, Ordering::Relaxed);
}

pub fn own_pid() -> u32 {
    OWN_PID.load(Ordering::Relaxed)
}

/// The signal channel's handle, for a wait that wants to be woken by a signal (0 if there is none).
pub fn channel() -> u32 {
    CHAN.load(Ordering::Relaxed)
}

fn post(sig: c_int) {
    if (1..=64).contains(&sig) {
        TABLE.lock().pending |= bit(sig);
    }
}

/// Move what the shell has sent into the pending set.
fn drain() {
    let ch = channel();
    if ch == 0 {
        return;
    }
    loop {
        let mut b = [0u8; 8];
        let r = sys::channel_read(ch, b.as_mut_ptr(), b.len(), core::ptr::null_mut(), 0);
        if r.status != 0 {
            return;
        }
        if r.a1 == 1 {
            post(c_int::from(b[0]));
        }
    }
}

/// `alarm` or `setitimer` expired: post SIGALRM, and re-arm an interval timer.
fn check_alarm() {
    let at = ALARM_AT.load(Ordering::Relaxed);
    if at == 0 || sys::clock_get() < at {
        return;
    }
    let every = ALARM_EVERY.load(Ordering::Relaxed);
    ALARM_AT.store(if every == 0 { 0 } else { at + every }, Ordering::Relaxed);
    post(SIGALRM);
}

/// What `deliver` did.
pub struct Delivered {
    /// At least one handler ran.
    pub ran: bool,
    /// Every handler that ran was installed with `SA_RESTART`.
    pub restart: bool,
}

fn terminate(sig: c_int) -> ! {
    rt::sys::exit(SIGNALED_BASE - sig)
}

/// Run the handlers of the pending signals that are not blocked (lowest number first), apply default actions, and say whether
/// a handler ran. Called with no lock held.
pub fn deliver() -> Delivered {
    let mut out = Delivered {
        ran: false,
        restart: true,
    };
    loop {
        drain();
        check_alarm();
        if crate::spawn::newly_exited() {
            post(17); // SIGCHLD (its default is to do nothing; hush's `wait` and `sigsuspend` callers wait for it)
        }
        // a process waiting for the program it `exec`'d passes its signals on to it
        let fwd = crate::spawn::FORWARD_TO.load(Ordering::Relaxed);
        if fwd != 0 {
            let all = core::mem::take(&mut TABLE.lock().pending);
            for sig in 1..=64 {
                if all & bit(sig) != 0 {
                    crate::spawn::kill_pid(fwd as c_int, sig);
                }
            }
            return out;
        }
        let (sig, h, flags, old) = {
            let mut t = TABLE.lock();
            let ready = t.pending & !t.blocked;
            if ready == 0 {
                return out;
            }
            let sig = ready.trailing_zeros() as c_int + 1;
            t.pending &= !bit(sig);
            let (h, mask, flags) = (
                t.handler[sig as usize],
                t.mask[sig as usize],
                t.flags[sig as usize],
            );
            let old = t.blocked;
            if h > SIG_IGN {
                t.blocked |= mask | if flags & SA_NODEFER == 0 { bit(sig) } else { 0 };
                if flags & SA_RESETHAND != 0 {
                    t.handler[sig as usize] = SIG_DFL;
                }
            }
            (sig, h, flags, old)
        };
        match h {
            SIG_IGN => {}
            SIG_DFL => {
                // the default is to end the program, except for the signals about children, windows, urgent data and
                // stopping and continuing (nothing stops a program here)
                if !matches!(sig, 17..=23 | 28) {
                    terminate(sig);
                }
            }
            f => {
                // SAFETY: a function pointer the program registered with `signal` or `sigaction`.
                let f: extern "C" fn(c_int) = unsafe { core::mem::transmute(f) };
                f(sig);
                TABLE.lock().blocked = old;
                out.ran = true;
                if flags & SA_RESTART == 0 {
                    out.restart = false;
                }
            }
        }
    }
}

/// Port keys from here up are children's ends (`wait_any`'s own objects count up from 0).
const CHILD_KEY: u64 = 1 << 32;

/// How a wait ended.
#[allow(dead_code)]
pub enum Wait {
    /// The `i`th object had one of its signals.
    Ready(usize),
    Timeout,
    /// A handler ran: the call returns `EINTR`.
    Interrupted,
}

/// Wait until one of `items` (object, signal mask) is ready, `deadline` (absolute ms; `u64::MAX` for never) passes, or a
/// signal's handler has run. With `restart`, a handler installed with `SA_RESTART` does not end the wait.
pub fn wait_any(items: &[(u32, u32)], deadline: u64, restart: bool) -> Wait {
    loop {
        let d = deliver();
        if d.ran && !(restart && d.restart) {
            return Wait::Interrupted;
        }
        let now = sys::clock_get();
        if now >= deadline {
            return Wait::Timeout;
        }
        let alarm = ALARM_AT.load(Ordering::Relaxed);
        let until = if alarm != 0 {
            deadline.min(alarm)
        } else {
            deadline
        };
        let p = sys::port_create();
        if p.status != 0 {
            // no port: look again soon (a signal still gets through)
            if let Some(&(h, m)) = items.first() {
                if sys::object_wait(h, m, now + 5).status == 0 {
                    return Wait::Ready(0);
                }
            } else {
                rt::io::nap(5);
            }
            continue;
        }
        let port = p.a1 as u32;
        for (i, &(h, m)) in items.iter().enumerate() {
            sys::port_bind_flags(port, h, m, i as u64, port_flags::ONESHOT);
        }
        let ch = channel();
        if ch != 0 {
            sys::port_bind_flags(port, ch, sg::READABLE, u64::MAX, port_flags::ONESHOT);
        }
        // a child's end wakes the wait too (SIGCHLD is posted at the top of the loop)
        for (k, h) in crate::spawn::live_child_handles().into_iter().enumerate() {
            sys::port_bind_flags(
                port,
                h,
                sg::TERMINATED,
                CHILD_KEY + k as u64,
                port_flags::ONESHOT,
            );
        }
        let r = sys::port_wait(port, until);
        sys::handle_close(port);
        if r.status == 0 && r.a1 != u64::MAX as usize && (r.a1 as u64) < CHILD_KEY {
            return Wait::Ready(r.a1);
        }
    }
}

// ---- blocking stream I/O that signals can interrupt ----

/// Read from a stream socket; `Ok(0)` is end of stream; `Err(errno)`.
pub fn stream_read(h: u32, buf: &mut [u8]) -> Result<usize, c_int> {
    if buf.is_empty() {
        return Ok(0);
    }
    loop {
        let r = sys::socket_read(h, buf.as_mut_ptr(), buf.len());
        match r.status() {
            Some(Status::Ok) => return Ok(r.a1),
            Some(Status::PeerClosed) => return Ok(0),
            Some(Status::ShouldWait) => {
                if let Wait::Interrupted = wait_any(
                    &[(h, sg::READABLE | sg::PEER_CLOSED | sg::WRITE_CLOSED)],
                    u64::MAX,
                    true,
                ) {
                    return Err(EINTR);
                }
            }
            _ => return Err(5),
        }
    }
}

/// Write to a stream socket, waiting for room: the number written, which is less than `data.len()` only if a signal's handler
/// interrupted after something was written; `Err(EPIPE)` for a closed stream, `Err(EINTR)` if nothing was written.
pub fn stream_write(h: u32, data: &[u8]) -> Result<usize, c_int> {
    let mut done = 0;
    let mut nomem_since: Option<u64> = None;
    while done < data.len() {
        let n = (data.len() - done).min(rt::abi::limits::SOCKET_BUFFER);
        let r = sys::socket_write(h, data[done..].as_ptr(), n);
        match r.status() {
            Some(Status::Ok) => {
                nomem_since = None;
                done += r.a1;
            }
            Some(Status::PeerClosed) => return Err(32),
            Some(Status::ShouldWait) => {
                if let Wait::Interrupted =
                    wait_any(&[(h, sg::WRITABLE | sg::PEER_CLOSED)], u64::MAX, true)
                {
                    return if done > 0 { Ok(done) } else { Err(EINTR) };
                }
            }
            Some(Status::NoMemory) => {
                let now = sys::clock_get();
                if now.saturating_sub(*nomem_since.get_or_insert(now)) >= 1000 {
                    return Err(12);
                }
                rt::io::nap(10);
            }
            _ => return Err(5),
        }
    }
    Ok(done)
}

// ---- the C interface ----

#[repr(C)]
pub struct SigAction {
    handler: Handler,
    mask: u64,
    flags: c_int,
}

fn valid(sig: c_int) -> bool {
    (1..=64).contains(&sig)
}

#[no_mangle]
pub unsafe extern "C" fn sigaction(
    sig: c_int,
    act: *const SigAction,
    old: *mut SigAction,
) -> c_int {
    if !valid(sig) || (!act.is_null() && (sig == SIGKILL || sig == SIGSTOP)) {
        errno::set(22);
        return -1;
    }
    let mut t = TABLE.lock();
    if !old.is_null() {
        *old = SigAction {
            handler: t.handler[sig as usize],
            mask: t.mask[sig as usize],
            flags: t.flags[sig as usize],
        };
    }
    if !act.is_null() {
        t.handler[sig as usize] = (*act).handler;
        t.mask[sig as usize] = (*act).mask;
        t.flags[sig as usize] = (*act).flags;
    }
    let int_handled = t.handler[2] != SIG_DFL;
    drop(t);
    if sig == 2 {
        notify_int(int_handled);
    }
    0
}

#[no_mangle]
pub extern "C" fn signal(sig: c_int, h: Handler) -> Handler {
    if !valid(sig) || sig == SIGKILL || sig == SIGSTOP {
        errno::set(22);
        return SIG_ERR;
    }
    let mut t = TABLE.lock();
    let old = t.handler[sig as usize];
    t.handler[sig as usize] = h;
    t.mask[sig as usize] = 0;
    t.flags[sig as usize] = SA_RESTART;
    drop(t);
    if sig == 2 {
        notify_int(h != SIG_DFL);
    }
    old
}

#[no_mangle]
pub extern "C" fn raise(sig: c_int) -> c_int {
    if sig == 0 {
        return 0;
    }
    if !valid(sig) {
        errno::set(22);
        return -1;
    }
    post(sig);
    deliver();
    0
}

#[no_mangle]
pub unsafe extern "C" fn sigprocmask(how: c_int, set: *const u64, old: *mut u64) -> c_int {
    let mut t = TABLE.lock();
    if !old.is_null() {
        *old = t.blocked;
    }
    if !set.is_null() {
        let s = *set & !(bit(SIGKILL) | bit(SIGSTOP));
        match how {
            SIG_BLOCK => t.blocked |= s,
            SIG_UNBLOCK => t.blocked &= !s,
            SIG_SETMASK => t.blocked = s,
            _ => {
                errno::set(22);
                return -1;
            }
        }
    }
    drop(t);
    deliver();
    0
}

#[no_mangle]
pub unsafe extern "C" fn sigpending(set: *mut u64) -> c_int {
    drain();
    check_alarm();
    let t = TABLE.lock();
    *set = t.pending & t.blocked;
    0
}

#[no_mangle]
pub unsafe extern "C" fn sigsuspend(mask: *const u64) -> c_int {
    let old = {
        let mut t = TABLE.lock();
        let old = t.blocked;
        t.blocked = *mask & !(bit(SIGKILL) | bit(SIGSTOP));
        old
    };
    while !matches!(wait_any(&[], u64::MAX, false), Wait::Interrupted) {}
    TABLE.lock().blocked = old;
    errno::set(EINTR);
    -1
}

#[no_mangle]
pub extern "C" fn pause() -> c_int {
    while !matches!(wait_any(&[], u64::MAX, false), Wait::Interrupted) {}
    errno::set(EINTR);
    -1
}

#[no_mangle]
pub unsafe extern "C" fn sigemptyset(set: *mut u64) -> c_int {
    *set = 0;
    0
}

#[no_mangle]
pub unsafe extern "C" fn sigfillset(set: *mut u64) -> c_int {
    *set = u64::MAX;
    0
}

#[no_mangle]
pub unsafe extern "C" fn sigaddset(set: *mut u64, sig: c_int) -> c_int {
    if !valid(sig) {
        errno::set(22);
        return -1;
    }
    *set |= bit(sig);
    0
}

#[no_mangle]
pub unsafe extern "C" fn sigdelset(set: *mut u64, sig: c_int) -> c_int {
    if !valid(sig) {
        errno::set(22);
        return -1;
    }
    *set &= !bit(sig);
    0
}

#[no_mangle]
pub unsafe extern "C" fn sigismember(set: *const u64, sig: c_int) -> c_int {
    if !valid(sig) {
        errno::set(22);
        return -1;
    }
    c_int::from(*set & bit(sig) != 0)
}

#[no_mangle]
pub unsafe extern "C" fn sigisemptyset(set: *const u64) -> c_int {
    c_int::from(*set == 0)
}

fn remaining_ms() -> u64 {
    let at = ALARM_AT.load(Ordering::Relaxed);
    if at == 0 {
        return 0;
    }
    at.saturating_sub(sys::clock_get())
}

#[no_mangle]
pub extern "C" fn alarm(seconds: u32) -> u32 {
    let left = remaining_ms().div_ceil(1000) as u32;
    ALARM_EVERY.store(0, Ordering::Relaxed);
    ALARM_AT.store(
        if seconds == 0 {
            0
        } else {
            sys::clock_get() + u64::from(seconds) * 1000
        },
        Ordering::Relaxed,
    );
    left
}

#[repr(C)]
pub struct Timeval {
    sec: c_long,
    usec: c_long,
}

#[repr(C)]
pub struct Itimerval {
    interval: Timeval,
    value: Timeval,
}

fn tv_ms(t: &Timeval) -> u64 {
    (t.sec.max(0) as u64) * 1000 + (t.usec.max(0) as u64).div_ceil(1000)
}

fn ms_tv(ms: u64) -> Timeval {
    Timeval {
        sec: (ms / 1000) as c_long,
        usec: ((ms % 1000) * 1000) as c_long,
    }
}

/// `setitimer(ITIMER_REAL, ...)`: the same deadline as `alarm`, with an interval. The other timers are not supported.
#[no_mangle]
pub unsafe extern "C" fn setitimer(
    which: c_int,
    new: *const Itimerval,
    old: *mut Itimerval,
) -> c_int {
    if which != 0 {
        errno::set(22);
        return -1;
    }
    if !old.is_null() {
        *old = Itimerval {
            interval: ms_tv(ALARM_EVERY.load(Ordering::Relaxed)),
            value: ms_tv(remaining_ms()),
        };
    }
    if !new.is_null() {
        let v = tv_ms(&(*new).value);
        ALARM_EVERY.store(tv_ms(&(*new).interval), Ordering::Relaxed);
        ALARM_AT.store(
            if v == 0 { 0 } else { sys::clock_get() + v },
            Ordering::Relaxed,
        );
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn getitimer(which: c_int, cur: *mut Itimerval) -> c_int {
    if which != 0 {
        errno::set(22);
        return -1;
    }
    *cur = Itimerval {
        interval: ms_tv(ALARM_EVERY.load(Ordering::Relaxed)),
        value: ms_tv(remaining_ms()),
    };
    0
}

#[no_mangle]
pub extern "C" fn getpid() -> c_int {
    match own_pid() {
        0 => 1,
        p => p as c_int,
    }
}

/// The pid of the program that started this one (the shell's own command line reads as 1). In a `vfork` child it is the pid of
/// the process the child is running in, as it is for a real one.
#[no_mangle]
pub extern "C" fn getppid() -> c_int {
    if crate::spawn::in_vfork_child() {
        return getpid();
    }
    match PARENT_PID.load(Ordering::Relaxed) {
        0 => 1,
        p => p as c_int,
    }
}
