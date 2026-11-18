//! Starting programs from C (UV8, OD118). A program the shell started with a spawn grant holds a channel to the shell
//! (`Role::ServiceSpawn`, `proto::spawn`); with it `posix_spawn`, `execve` and `vfork` work, and `waitpid` and `kill` act on the
//! children it started. Without the grant they fail with `ENOSYS`, as before.
//!
//! There is no `fork` and no address-space replacement here. `execve` is "start the program, wait for it, exit with its status",
//! except in the child of a `vfork`: the child runs in the caller until its first `execve` (or `_exit`), and that call returns
//! to the caller of `vfork` with the new program's pid (a `longjmp` to where `vfork` was called), which is what the caller of a
//! real `vfork` sees. The child must not return from the function that called `vfork`, change locals other than the pid, or
//! call anything but `exec*` and `_exit` (the same rules as POSIX). One `vfork` at a time per process, and the child must be the only thread to call `exec*` or `_exit` meanwhile.

use crate::errno;
use alloc::vec::Vec;
use core::ffi::{c_char, c_int, c_void, CStr};
use core::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use proto::file::status as ps;
use rt::abi::rights::Rights;
use rt::abi::signals;
use rt::sync::Mutex;
use rt::sys;

static SPAWN_CH: AtomicU32 = AtomicU32::new(0);
/// Requests and replies on the spawn channel are one exchange at a time.
static EXCHANGE: Mutex<()> = Mutex::new(());
/// Transaction numbers for requests, and pids for the children that ended before they began (a failed `exec` in a `vfork`
/// child); a started program's pid is the shell's.
static NEXT_TX: AtomicU32 = AtomicU32::new(1);
static NEXT_PSEUDO: AtomicU32 = AtomicU32::new(1_000_000);

/// Install the spawn channel (start-up); 0 means the program holds no grant.
pub fn init(ch: u32) {
    SPAWN_CH.store(ch, Ordering::Relaxed);
}

struct Child {
    /// SIGCHLD has been posted for this child's end.
    notified: bool,
    pid: c_int,
    /// The process handle, or 0 for a child that ended before it began (a `vfork` child whose `exec` failed).
    h: u32,
    done: Option<i32>,
}

static CHILDREN: Mutex<Vec<Child>> = Mutex::new(Vec::new());

fn register(pid: c_int, h: u32, done: Option<i32>) -> c_int {
    let pid = if pid != 0 {
        pid
    } else {
        NEXT_PSEUDO.fetch_add(1, Ordering::Relaxed) as c_int
    };
    CHILDREN.lock().push(Child {
        notified: false,
        pid,
        h,
        done,
    });
    pid
}

/// Have children ended since the last look? (Each end is reported once; the library posts SIGCHLD for it.)
pub fn newly_exited() -> bool {
    let mut any = false;
    for c in CHILDREN.lock().iter_mut() {
        if !c.notified
            && (c.done.is_some()
                || (c.h != 0 && sys::object_wait(c.h, signals::TERMINATED, 0).status == 0))
        {
            c.notified = true;
            any = true;
        }
    }
    any
}

/// The process handles of the children that have not ended yet, for a wait that a child's end should wake.
pub fn live_child_handles() -> Vec<u32> {
    CHILDREN
        .lock()
        .iter()
        .filter(|c| !c.notified && c.h != 0)
        .map(|c| c.h)
        .collect()
}

/// Is a `vfork` child running (in this process)?
pub fn in_vfork_child() -> bool {
    VFORK_ACTIVE.load(Ordering::Relaxed) != 0
}

/// The wait status of a child: exited with `code`, or killed. Killed is the shell's Ctrl-C (-130: SIGINT), a kill (-1:
/// SIGKILL), a default signal action (`-2000 - signal`, `sig.rs`) or a fault the kernel ended the program for (`-1000 -
/// scause`: SIGILL, SIGBUS or SIGSEGV).
fn wait_status(code: i32) -> c_int {
    match code {
        -130 => 2,
        -1 => 9,
        c if c < crate::sig::SIGNALED_BASE && c > crate::sig::SIGNALED_BASE - 65 => {
            crate::sig::SIGNALED_BASE - c
        }
        c if (-1100..=-1000).contains(&c) => cfmt::sigtab::fault_signal((-1000 - c) as u64),
        c => (c & 0xff) << 8,
    }
}

/// A handle for a child's standard stream from our descriptor `fd`: a duplicate, or a stand-in when `fd` is closed (an input
/// at its end, an output nobody reads).
fn std_handle(fd: c_int, input: bool) -> Option<u32> {
    if let Some(h) = crate::fd::stream_handle(fd) {
        let d = sys::handle_duplicate(h, Rights::SAME.0);
        return (d.status == 0).then_some(d.a1 as u32);
    }
    let s = sys::socket_create();
    if s.status != 0 {
        return None;
    }
    let (a, b) = (s.a1 as u32, s.a2 as u32);
    // the end the child holds; ours is closed, so the child sees the end of input or a closed output
    let (keep, drop_) = if input { (a, b) } else { (b, a) };
    sys::handle_close(drop_);
    Some(keep)
}

fn status_errno(st: i32) -> c_int {
    match st {
        s if s == ps::NOT_FOUND => 2,
        s if s == ps::QUOTA => 11,
        s if s == ps::BAD_REQUEST => 22,
        _ => 5,
    }
}

/// Ask the shell to start `argv` with `env` and the three standard streams from `fds`; the process handle, or an `errno`.
fn start(
    prog: &[u8],
    argv: &[Vec<u8>],
    env: &[Vec<u8>],
    fds: [c_int; 3],
) -> Result<(u32, c_int), c_int> {
    let ch = SPAWN_CH.load(Ordering::Relaxed);
    if ch == 0 {
        return Err(38);
    }
    let a: Vec<&[u8]> = argv.iter().map(|v| v.as_slice()).collect();
    let _x = EXCHANGE.lock();
    let txid = NEXT_TX.fetch_add(1, Ordering::Relaxed);
    // the standard streams: the environment says which are the terminal and which are files (the shell passes it on), whatever the
    // caller's own environment said about its own
    let mut env2: Vec<Vec<u8>> = env
        .iter()
        .filter(|v| {
            !(v.starts_with(b"STDIN=") || v.starts_with(b"STDOUT=") || v.starts_with(b"STDERR="))
        })
        .cloned()
        .collect();
    let mut hs = [0u32; 3];
    for (i, fd) in fds.iter().enumerate() {
        let got = match crate::fd::stdio_for_child(*fd) {
            Some((h, marker)) => {
                if let Some(m) = marker {
                    env2.push(
                        [b"STDIN=" as &[u8], b"STDOUT=", b"STDERR="][i]
                            .iter()
                            .chain(m.as_bytes())
                            .copied()
                            .collect(),
                    );
                }
                Some(h)
            }
            None => std_handle(*fd, i == 0),
        };
        match got {
            Some(h) => hs[i] = h,
            None => {
                for &h in &hs[..i] {
                    sys::handle_close(h);
                }
                return Err(24);
            }
        }
    }
    let e: Vec<&[u8]> = env2.iter().map(|v| v.as_slice()).collect();
    let req = match proto::spawn::encode(txid, prog, &a, &e) {
        Ok(r) => r,
        Err(_) => {
            // E2BIG: the duplicated handles were not sent
            for &h in &hs {
                sys::handle_close(h);
            }
            return Err(7);
        }
    };
    let w = sys::channel_write(ch, req.as_ptr(), req.len(), hs.as_ptr(), hs.len());
    if w.status != 0 {
        for &h in &hs {
            sys::handle_close(h);
        }
        return Err(5);
    }
    let deadline = sys::clock_get() + 120_000;
    loop {
        let wt = sys::object_wait(ch, signals::READABLE | signals::PEER_CLOSED, deadline);
        if wt.status != 0 {
            return Err(110); // ETIMEDOUT
        }
        let mut buf = [0u8; 16];
        let mut h = [0u32; 1];
        let r = sys::channel_read(ch, buf.as_mut_ptr(), buf.len(), h.as_mut_ptr(), h.len());
        if r.status != 0 {
            return Err(5);
        }
        match proto::spawn::decode_reply(&buf[..r.a1]) {
            Some((id, 0, pid)) if id == txid && r.a2 == 1 => return Ok((h[0], pid as c_int)),
            Some((id, st, _)) if id == txid => {
                for &x in &h[..r.a2] {
                    sys::handle_close(x);
                }
                return Err(status_errno(st));
            }
            _ => {
                for &x in &h[..r.a2] {
                    sys::handle_close(x);
                }
            }
        }
    }
}

/// Ask the shell to deliver `sig` to the program with pid `pid` in this chain (itself, what it started, what started it); an
/// `errno` (`ESRCH` for no such program).
fn signal_via_shell(pid: c_int, sig: c_int) -> Result<(), c_int> {
    let ch = SPAWN_CH.load(Ordering::Relaxed);
    if ch == 0 {
        return Err(3);
    }
    let _x = EXCHANGE.lock();
    let txid = NEXT_TX.fetch_add(1, Ordering::Relaxed);
    let req = proto::spawn::encode_signal(txid, pid as u32, sig as u32);
    if sys::channel_write(ch, req.as_ptr(), req.len(), core::ptr::null(), 0).status != 0 {
        return Err(3);
    }
    let deadline = sys::clock_get() + 30_000;
    loop {
        if sys::object_wait(ch, signals::READABLE | signals::PEER_CLOSED, deadline).status != 0 {
            return Err(110);
        }
        let mut buf = [0u8; 16];
        let mut h = [0u32; 1];
        let r = sys::channel_read(ch, buf.as_mut_ptr(), buf.len(), h.as_mut_ptr(), h.len());
        if r.status != 0 {
            return Err(5);
        }
        for &x in &h[..r.a2] {
            sys::handle_close(x);
        }
        match proto::spawn::decode_reply(&buf[..r.a1]) {
            Some((id, 0, _)) if id == txid => return Ok(()),
            Some((id, st, _)) if id == txid => {
                return Err(if st == ps::NOT_FOUND { 3 } else { 22 });
            }
            _ => {}
        }
    }
}

/// While this process is an `exec`'d program's wait (`exec_with`): the pid its signals are passed on to.
pub static FORWARD_TO: AtomicU32 = AtomicU32::new(0);

/// `kill`: a signal to this process runs through `raise`; one to a program in the chain goes through the shell, and SIGKILL
/// to a child this process holds ends it at once.
pub fn kill_pid(pid: c_int, sig: c_int) -> c_int {
    if !(0..=64).contains(&sig) {
        errno::set(22);
        return -1;
    }
    if pid <= 0 {
        errno::set(3); // process groups are not emulated
        return -1;
    }
    if crate::sig::own_pid() != 0 && pid as u32 == crate::sig::own_pid() {
        return crate::sig::raise(sig);
    }
    if sig == 9 {
        let g = CHILDREN.lock();
        if let Some(c) = g.iter().find(|c| c.pid == pid && c.h != 0) {
            sys::process_kill(c.h, -1);
            return 0;
        }
    }
    // a child this process holds exists until it is waited for, whatever the shell says (it has no grant of its own below the
    // depth limit, so the shell may not know it as a program of the chain)
    let held = CHILDREN.lock().iter().find(|c| c.pid == pid).map(|c| c.h);
    if sig == 0 && held.is_some() {
        return 0;
    }
    match signal_via_shell(pid, sig) {
        Ok(()) => 0,
        Err(3) if held.is_some_and(|h| h != 0) => {
            // the shell has no channel to it: a signal that would end it ends it (the others have nowhere to go)
            if !matches!(sig, 17..=23 | 28) {
                sys::process_kill(held.unwrap_or(0), -1);
            }
            0
        }
        Err(e) => {
            errno::set(e);
            -1
        }
    }
}

/// Ask the shell about the terminal (it answers a program of the foreground job): the answer (`proto::spawn::TERM_*`), or an `errno`
/// (`ENOTTY` when the program is not the foreground job or is not on a terminal).
pub fn term_via_shell(what: u16, value: u32) -> Result<u32, c_int> {
    let ch = SPAWN_CH.load(Ordering::Relaxed);
    if ch == 0 {
        return Err(25);
    }
    let _x = EXCHANGE.lock();
    let txid = NEXT_TX.fetch_add(1, Ordering::Relaxed);
    let req = proto::spawn::encode_term(txid, what, value);
    if sys::channel_write(ch, req.as_ptr(), req.len(), core::ptr::null(), 0).status != 0 {
        return Err(25);
    }
    let deadline = sys::clock_get() + 30_000;
    loop {
        if sys::object_wait(ch, signals::READABLE | signals::PEER_CLOSED, deadline).status != 0 {
            return Err(110);
        }
        let mut buf = [0u8; 16];
        let mut h = [0u32; 1];
        let r = sys::channel_read(ch, buf.as_mut_ptr(), buf.len(), h.as_mut_ptr(), h.len());
        if r.status != 0 {
            return Err(5);
        }
        for &x in &h[..r.a2] {
            sys::handle_close(x);
        }
        match proto::spawn::decode_reply(&buf[..r.a1]) {
            Some((id, 0, v)) if id == txid => return Ok(v),
            Some((id, _, _)) if id == txid => return Err(25),
            _ => {}
        }
    }
}

/// `argv[0]` as the shell knows commands: a bare name, or `/bin/name`.
fn command_name(file: &[u8]) -> Option<&[u8]> {
    let n = file.strip_prefix(b"/bin/").unwrap_or(file);
    (!n.is_empty() && !n.contains(&b'/')).then_some(n)
}

/// What to ask the shell to start for `exec*(file, args)` or `posix_spawn(file, args)`: the program's name in the image, and its
/// `argv`. A bare name or `/bin/name` is a program of the image; **`sh` is the compat shell** (BusyBox `hush`, `ish` being the system
/// shell, which programs do not talk to: T139), so `execl("/bin/sh", "sh", "-c", cmd)` and `system` work the way programs expect;
/// a path to a file that starts with `#!` is that interpreter run on the file (`#!/bin/sh`, `#!/bin/busybox sh`).
unsafe fn plan(file: &[u8], args: Vec<Vec<u8>>, depth: u32) -> Result<(Vec<u8>, Vec<Vec<u8>>), c_int> {
    if let Some(name) = command_name(file) {
        if name == b"sh" {
            let mut v: Vec<Vec<u8>> = alloc::vec![b"busybox".to_vec(), b"hush".to_vec()];
            v.extend(args.into_iter().skip(1));
            return Ok((b"busybox".to_vec(), v));
        }
        return Ok((name.to_vec(), args));
    }
    if depth >= 4 {
        return Err(40); // ELOOP: interpreters naming scripts
    }
    // a script: its first line names the interpreter
    let mut path = file.to_vec();
    path.push(0);
    let fd = crate::fd::open(path.as_ptr() as *const c_char, 0, 0);
    if fd < 0 {
        return Err(2);
    }
    let mut head = [0u8; 256];
    let n = crate::fd::read(fd, head.as_mut_ptr() as *mut c_void, head.len());
    crate::fd::close(fd);
    let n = n.max(0) as usize;
    if n < 3 || &head[..2] != b"#!" {
        return Err(8); // ENOEXEC
    }
    let line = head[2..n].split(|&b| b == b'\n').next().unwrap_or(&[]);
    let mut words = line
        .split(|&b| b == b' ' || b == b'\t')
        .filter(|w| !w.is_empty());
    let Some(interp) = words.next() else {
        return Err(8);
    };
    let mut argv: Vec<Vec<u8>> = alloc::vec![interp.to_vec()];
    if let Some(opt) = words.next() {
        argv.push(opt.to_vec()); // (one optional argument, as on Linux: the rest of the line is not split)
    }
    argv.push(file.to_vec());
    argv.extend(args.into_iter().skip(1));
    plan(interp, argv, depth + 1)
}

// SAFETY (callers): `p` is a NULL-terminated array of NUL-terminated strings.
unsafe fn string_array(p: *const *const c_char) -> Vec<Vec<u8>> {
    let mut v = Vec::new();
    if p.is_null() {
        return v;
    }
    let mut i = 0;
    while !(*p.add(i)).is_null() {
        v.push(CStr::from_ptr(*p.add(i)).to_bytes().to_vec());
        i += 1;
    }
    v
}

unsafe fn current_env() -> Vec<Vec<u8>> {
    string_array(crate::stdlib::ENVIRON as *const *const c_char)
}

// ---- vfork ----

/// Where `vfork` was called (the `setjmp` layout), and whether a `vfork` child is running.
#[no_mangle]
pub static mut VFORK_BUF: [u64; 26] = [0; 26];
#[no_mangle]
pub static VFORK_ACTIVE: AtomicU8 = AtomicU8::new(0);

core::arch::global_asm!(
    r#"
    .section .text.vfork,"ax",@progbits
    .globl vfork
vfork:
    addi sp, sp, -16
    sd ra, 8(sp)
    call vfork_prepare
    ld ra, 8(sp)
    addi sp, sp, 16
    la a0, VFORK_BUF
    tail setjmp
"#
);

/// Called by `vfork` before it records where to come back to: a child is about to run, and what it does to the signal
/// dispositions and the mask is its own, not its parent's.
#[no_mangle]
extern "C" fn vfork_prepare() {
    crate::sig::vfork_save();
    crate::fd::vfork_enter();
    VFORK_ACTIVE.store(1, Ordering::Relaxed);
}

extern "C" {
    fn longjmp(buf: *mut u64, val: c_int) -> !;
}

/// The `vfork` child is done (it started a program or ended): back to the caller of `vfork`, with the child's pid.
fn vfork_return(spid: c_int, h: u32, done: Option<i32>) -> ! {
    VFORK_ACTIVE.store(0, Ordering::Relaxed);
    crate::sig::vfork_restore();
    crate::fd::vfork_leave();
    let pid = register(spid, h, done);
    // SAFETY: `VFORK_BUF` was filled by `vfork` and the frame it describes is still live: the child may not return from it.
    unsafe { longjmp(core::ptr::addr_of_mut!(VFORK_BUF) as *mut u64, pid) }
}

/// `_exit` in a `vfork` child: the child ended with `code`; the caller of `vfork` resumes.
pub fn vfork_child_exit(code: c_int) {
    if VFORK_ACTIVE.load(Ordering::Relaxed) != 0 {
        vfork_return(0, 0, Some(code));
    }
}

// ---- exec ----

/// Ask for the program. Everything that allocates ends here, before any `longjmp` out of `exec_with`.
unsafe fn exec_start(
    file: *const c_char,
    argv: *const *const c_char,
    envp: Option<*const *const c_char>,
) -> Result<(u32, c_int), c_int> {
    if file.is_null() || argv.is_null() {
        return Err(14);
    }
    let (name, args) = plan(CStr::from_ptr(file).to_bytes(), string_array(argv), 0)?;
    let env = match envp {
        Some(e) => string_array(e),
        None => current_env(),
    };
    // the program is named by the file; `argv[0]` is what the program sees (an applet name, usually)
    start(&name, &args, &env, [0, 1, 2])
}

unsafe fn exec_with(
    file: *const c_char,
    argv: *const *const c_char,
    envp: Option<*const *const c_char>,
) -> c_int {
    match exec_start(file, argv, envp) {
        Err(e) => {
            errno::set(e);
            -1
        }
        Ok((h, pid)) => {
            if VFORK_ACTIVE.load(Ordering::Relaxed) != 0 {
                vfork_return(pid, h, None)
            }
            // not a `vfork` child: this process becomes the program (it runs, and this one ends with its status); what is
            // sent to this process meanwhile is the program's, so it is passed on
            FORWARD_TO.store(pid as u32, Ordering::Relaxed);
            while !matches!(
                crate::sig::wait_any(&[(h, signals::TERMINATED)], u64::MAX, true),
                crate::sig::Wait::Ready(_)
            ) {}
            let code = sys::process_info(h).a1 as i64 as i32;
            sys::handle_close(h);
            crate::compat::_exit(code)
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn execve(
    file: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> c_int {
    exec_with(file, argv, Some(envp))
}

#[no_mangle]
pub unsafe extern "C" fn execv(file: *const c_char, argv: *const *const c_char) -> c_int {
    exec_with(file, argv, None)
}

#[no_mangle]
pub unsafe extern "C" fn execvp(file: *const c_char, argv: *const *const c_char) -> c_int {
    exec_with(file, argv, None)
}

#[no_mangle]
pub unsafe extern "C" fn execvpe(
    file: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> c_int {
    exec_with(file, argv, Some(envp))
}

// ---- posix_spawn ----

/// `posix_spawn_file_actions_t` is `{ void *actions }`: a boxed list of (kind, fd, newfd).
#[repr(C)]
pub struct FileActions {
    list: *mut Vec<(u8, c_int, c_int)>,
}

#[no_mangle]
pub unsafe extern "C" fn posix_spawn_file_actions_init(fa: *mut FileActions) -> c_int {
    (*fa).list = alloc::boxed::Box::into_raw(alloc::boxed::Box::new(Vec::new()));
    0
}

#[no_mangle]
pub unsafe extern "C" fn posix_spawn_file_actions_destroy(fa: *mut FileActions) -> c_int {
    if !(*fa).list.is_null() {
        drop(alloc::boxed::Box::from_raw((*fa).list));
        (*fa).list = core::ptr::null_mut();
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn posix_spawn_file_actions_adddup2(
    fa: *mut FileActions,
    fd: c_int,
    to: c_int,
) -> c_int {
    if (*fa).list.is_null() {
        return 22;
    }
    (*(*fa).list).push((0, fd, to));
    0
}

#[no_mangle]
pub unsafe extern "C" fn posix_spawn_file_actions_addclose(
    fa: *mut FileActions,
    fd: c_int,
) -> c_int {
    if (*fa).list.is_null() {
        return 22;
    }
    (*(*fa).list).push((1, fd, 0));
    0
}

unsafe fn spawn_impl(
    pid: *mut c_int,
    file: *const c_char,
    fa: *const FileActions,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> c_int {
    let (name, args) = match plan(CStr::from_ptr(file).to_bytes(), string_array(argv), 0) {
        Ok(p) => p,
        Err(e) => return e,
    };
    // the child's standard streams: ours, then what the file actions say (dup2 of one standard stream onto another, close)
    let mut map = [0 as c_int, 1, 2];
    let mut closed = [false; 3];
    if !fa.is_null() && !(*fa).list.is_null() {
        for &(kind, fd, to) in (*(*fa).list).iter() {
            match kind {
                0 if (0..3).contains(&to) && (0..3).contains(&fd) => {
                    map[to as usize] = map[fd as usize];
                    closed[to as usize] = closed[fd as usize];
                }
                1 if (0..3).contains(&fd) => closed[fd as usize] = true,
                _ => return 38, // a file action on anything but the standard streams
            }
        }
    }
    for i in 0..3 {
        if closed[i] {
            map[i] = -1;
        }
    }
    match start(&name, &args, &string_array(envp), map) {
        Ok((h, spid)) => {
            let p = register(spid, h, None);
            if !pid.is_null() {
                *pid = p;
            }
            0
        }
        Err(e) => e,
    }
}

#[no_mangle]
pub unsafe extern "C" fn posix_spawn(
    pid: *mut c_int,
    file: *const c_char,
    fa: *const FileActions,
    _attr: *const c_void,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> c_int {
    spawn_impl(pid, file, fa, argv, envp)
}

#[no_mangle]
pub unsafe extern "C" fn posix_spawnp(
    pid: *mut c_int,
    file: *const c_char,
    fa: *const FileActions,
    _attr: *const c_void,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> c_int {
    spawn_impl(pid, file, fa, argv, envp)
}

// ---- waiting and killing ----

/// `WNOHANG`.
const WNOHANG: c_int = 1;

#[no_mangle]
pub unsafe extern "C" fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int {
    loop {
        let handles: Vec<u32> = {
            let mut g = CHILDREN.lock();
            let mut any = false;
            let mut found = None;
            for (i, c) in g.iter().enumerate() {
                if pid > 0 && c.pid != pid {
                    continue;
                }
                any = true;
                let over = match c.done {
                    Some(_) => true,
                    None => sys::object_wait(c.h, signals::TERMINATED, 0).status == 0,
                };
                if over {
                    found = Some(i);
                    break;
                }
            }
            if let Some(i) = found {
                let c = g.remove(i);
                let code = match c.done {
                    Some(code) => code,
                    None => {
                        let code = sys::process_info(c.h).a1 as i64 as i32;
                        sys::handle_close(c.h);
                        code
                    }
                };
                if !status.is_null() {
                    *status = wait_status(code);
                }
                return c.pid;
            }
            if !any {
                errno::set(10); // ECHILD
                return -1;
            }
            if options & WNOHANG != 0 {
                return 0;
            }
            g.iter()
                .filter(|c| pid <= 0 || c.pid == pid)
                .map(|c| c.h)
                .collect()
        };
        let items: Vec<(u32, u32)> = handles.iter().map(|&h| (h, signals::TERMINATED)).collect();
        if let crate::sig::Wait::Interrupted = crate::sig::wait_any(&items, u64::MAX, true) {
            errno::set(4); // EINTR
            return -1;
        }
    }
}

/// `struct rusage` is 144 bytes (two `timeval`s and 14 `long`s); nothing is accounted, so it is all zero.
const RUSAGE_BYTES: usize = 144;

#[no_mangle]
pub unsafe extern "C" fn wait4(
    pid: c_int,
    status: *mut c_int,
    options: c_int,
    ru: *mut c_void,
) -> c_int {
    let r = waitpid(pid, status, options);
    if r > 0 && !ru.is_null() {
        core::ptr::write_bytes(ru as *mut u8, 0, RUSAGE_BYTES);
    }
    r
}

#[no_mangle]
pub unsafe extern "C" fn wait3(status: *mut c_int, options: c_int, ru: *mut c_void) -> c_int {
    wait4(-1, status, options, ru)
}

#[no_mangle]
pub unsafe extern "C" fn wait(status: *mut c_int) -> c_int {
    waitpid(-1, status, 0)
}

#[no_mangle]
pub extern "C" fn kill(pid: c_int, sig: c_int) -> c_int {
    kill_pid(pid, sig)
}

// ---- system and popen: the command goes to the compat shell (BusyBox `hush`, work item T139) ----

/// Start `busybox hush -c cmd` with the standard streams from `fds`.
unsafe fn start_shell(cmd: *const c_char, fds: [c_int; 3]) -> Result<(u32, c_int), c_int> {
    let line = CStr::from_ptr(cmd).to_bytes().to_vec();
    let argv: Vec<Vec<u8>> =
        alloc::vec![b"busybox".to_vec(), b"hush".to_vec(), b"-c".to_vec(), line];
    start(b"busybox", &argv, &current_env(), fds)
}

/// `system`: run the command with `hush` and wait; a null command asks whether there is a shell (there is, if this program may
/// start programs).
#[no_mangle]
pub unsafe extern "C" fn system(cmd: *const c_char) -> c_int {
    if cmd.is_null() {
        return c_int::from(SPAWN_CH.load(Ordering::Relaxed) != 0);
    }
    match start_shell(cmd, [0, 1, 2]) {
        Ok((h, pid)) => {
            register(pid, h, None);
            let mut status = 0;
            if waitpid(pid, &mut status, 0) < 0 {
                return -1;
            }
            status
        }
        Err(e) => {
            errno::set(e);
            -1
        }
    }
}

/// The pipes `popen` made: the stream and the shell's pid, for `pclose`.
static POPEN: Mutex<Vec<(usize, c_int)>> = Mutex::new(Vec::new());

#[no_mangle]
pub unsafe extern "C" fn popen(cmd: *const c_char, mode: *const c_char) -> *mut c_void {
    let read = match CStr::from_ptr(mode).to_bytes().first() {
        Some(b'r') => true,
        Some(b'w') => false,
        _ => {
            errno::set(22);
            return core::ptr::null_mut();
        }
    };
    let mut fds = [0 as c_int; 2];
    if crate::fd::pipe(fds.as_mut_ptr()) != 0 {
        return core::ptr::null_mut();
    }
    // reading: the shell writes into the pipe (its stdout); writing: it reads from it (its stdin)
    let (mine, theirs) = if read {
        (fds[0], fds[1])
    } else {
        (fds[1], fds[0])
    };
    let map = if read { [0, theirs, 2] } else { [theirs, 1, 2] };
    let started = start_shell(cmd, map);
    crate::fd::close(theirs);
    match started {
        Ok((h, pid)) => {
            register(pid, h, None);
            let f = crate::file::fdopen(mine, mode);
            if f.is_null() {
                crate::fd::close(mine);
                return core::ptr::null_mut();
            }
            POPEN.lock().push((f as usize, pid));
            f as *mut c_void
        }
        Err(e) => {
            crate::fd::close(mine);
            errno::set(e);
            core::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn pclose(f: *mut c_void) -> c_int {
    let pid = {
        let mut g = POPEN.lock();
        match g.iter().position(|&(p, _)| p == f as usize) {
            Some(i) => g.remove(i).1,
            None => {
                errno::set(22);
                return -1;
            }
        }
    };
    crate::file::fclose(f as *mut crate::file::File);
    let mut status = 0;
    if waitpid(pid, &mut status, 0) < 0 {
        return -1;
    }
    status
}
