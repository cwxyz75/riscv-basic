//! Threads (S7c-3b, S7-SPEC section 5): `pthread` on the kernel's futexes and `thread_create`,
//! per-thread `errno` and keys through a thread control block (TCB) that `tp` points to, and the
//! big main stack.
//!
//! The kernel saves and restores a user thread's `tp` across traps and starts new threads with
//! `tp` = 0: `run_main` and the thread trampoline set it, so library calls from a thread this
//! library did not create fall back to process-wide state (a single static `errno`).

use alloc::boxed::Box;
use core::arch::{asm, global_asm};
use core::ffi::{c_int, c_long, c_void};
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use rt::abi::signals;
use rt::sync::Mutex;

pub const EAGAIN: c_int = 11;
pub const EBUSY: c_int = 16;
pub const EINVAL: c_int = 22;
pub const EDEADLK: c_int = 35;
pub const EPERM: c_int = 1;
pub const ETIMEDOUT: c_int = 110;

const KEYS: usize = 32;
/// Thread stacks: 256 KiB mapped (committed lazily) at the top of a 512 KiB slot, so the lower half
/// of every slot is an unmapped guard.
const STACK_BYTES: u64 = 256 * 1024;
const SLOT_BYTES: u64 = 512 * 1024;
const SLOT_BASE: u64 = 0x10_0100_0000;
const MAX_SLOTS: usize = 64;
/// The main thread's stack: 1 MiB, mapped lazily below `MAIN_STACK_TOP`.
const MAIN_STACK_BYTES: u64 = 1024 * 1024;
const MAIN_STACK_TOP: u64 = 0x10_0100_0000;

const EXITED: u32 = 1;
const DETACHED: u32 = 2;

#[repr(C)]
pub struct Tcb {
    me: *mut Tcb,
    errno: c_int,
    state: AtomicU32,
    retval: *mut c_void,
    /// The thread object's handle: 0 until the creator has stored it (a thread can start, and even
    /// exit, before `pthread_create` returns).
    handle: AtomicU32,
    slot: usize,
    stack_base: u64,
    start: Option<unsafe extern "C" fn(*mut c_void) -> *mut c_void>,
    arg: *mut c_void,
    keys: [*mut c_void; KEYS],
    main: bool,
}

/// Who owns a lock: the thread's TCB address, or 1 for a thread without one (never 0, which means
/// "unowned").
fn owner_id() -> usize {
    match tcb() as usize {
        0 => 1,
        p => p,
    }
}

fn tcb() -> *mut Tcb {
    let p: usize;
    // SAFETY: reads the thread pointer register.
    unsafe { asm!("mv {}, tp", out(reg) p, options(nomem, nostack)) };
    p as *mut Tcb
}

fn set_tcb(p: *mut Tcb) {
    // SAFETY: writes the thread pointer register; nothing in Rust code here uses `tp`.
    unsafe { asm!("mv tp, {}", in(reg) p as usize, options(nomem, nostack)) };
}

/// The fallback for a thread without a TCB: one racy int for the whole process.
static mut FALLBACK_ERRNO: c_int = 0;

/// `errno`'s address: the calling thread's, or the process-wide fallback when `tp` is not set.
pub fn errno_ptr() -> *mut c_int {
    let t = tcb();
    if t.is_null() {
        core::ptr::addr_of_mut!(FALLBACK_ERRNO)
    } else {
        // SAFETY: a TCB made by this library lives until its thread is joined.
        unsafe { core::ptr::addr_of_mut!((*t).errno) }
    }
}

fn new_tcb(main: bool, handle: u32, slot: usize) -> *mut Tcb {
    Box::into_raw(Box::new(Tcb {
        me: core::ptr::null_mut(),
        errno: 0,
        state: AtomicU32::new(0),
        retval: core::ptr::null_mut(),
        handle: AtomicU32::new(handle),
        slot,
        stack_base: 0,
        start: None,
        arg: core::ptr::null_mut(),
        keys: [core::ptr::null_mut(); KEYS],
        main,
    }))
}

/// Give the calling (main) thread its TCB.
pub fn init_main_thread() {
    let t = new_tcb(true, 0, usize::MAX);
    // SAFETY: fresh allocation.
    unsafe { (*t).me = t };
    set_tcb(t);
}

// ---- the big main stack ----

global_asm!(
    r#"
    .section .text.os_call_on_stack,"ax",@progbits
    .globl os_call_on_stack
os_call_on_stack:
    addi sp, sp, -32
    sd ra, 24(sp)
    sd s0, 16(sp)
    mv s0, sp
    mv sp, a0
    mv t1, a1
    mv a0, a2
    jalr t1
    mv sp, s0
    ld ra, 24(sp)
    ld s0, 16(sp)
    addi sp, sp, 32
    ret
"#
);

extern "C" {
    fn os_call_on_stack(sp: usize, f: extern "C" fn(usize) -> c_int, arg: usize) -> c_int;
}

/// Run `f(arg)` on a fresh 1 MiB stack (the loader gives a program 256 KiB, too tight for Lua's
/// nested C calls); on any failure to map one, on the current stack.
pub fn on_big_stack(f: extern "C" fn(usize) -> c_int, arg: usize) -> c_int {
    let base = MAIN_STACK_TOP - MAIN_STACK_BYTES;
    let vmo = rt::sys::vmo_create(MAIN_STACK_BYTES / 4096);
    if vmo.status == 0 {
        let me = rt::sys::process_self().a1 as u32;
        let m = rt::sys::process_map(me, vmo.a1 as u32, base, 0, MAIN_STACK_BYTES, 3);
        rt::sys::handle_close(vmo.a1 as u32);
        rt::sys::handle_close(me);
        if m.status == 0 {
            // SAFETY: the stack was just mapped; `f` is a plain function.
            return unsafe { os_call_on_stack(MAIN_STACK_TOP as usize, f, arg) };
        }
    }
    f(arg)
}

// ---- thread creation ----

struct Slots {
    used: [bool; MAX_SLOTS],
}
static SLOTS: Mutex<Slots> = Mutex::new(Slots {
    used: [false; MAX_SLOTS],
});

fn slot_top(slot: usize) -> u64 {
    SLOT_BASE + (slot as u64 + 1) * SLOT_BYTES
}

#[repr(C)]
pub struct Attr {
    stacksize: u64,
    detached: c_int,
}

pub type PthreadT = usize;

extern "C" fn thread_main(tcb_addr: usize) -> ! {
    let t = tcb_addr as *mut Tcb;
    set_tcb(t);
    // SAFETY: `t` was filled by `pthread_create` before the thread started.
    let ret = unsafe { ((*t).start.expect("a thread has an entry"))((*t).arg) };
    // SAFETY: as above.
    unsafe { exit_thread(t, ret) }
}

unsafe fn exit_thread(t: *mut Tcb, ret: *mut c_void) -> ! {
    (*t).retval = ret;
    (*t).state.fetch_or(EXITED, Ordering::SeqCst);
    rt::sys::futex_wake((*t).state.as_ptr(), u32::MAX);
    rt::sys::thread_exit(0)
}

#[no_mangle]
pub unsafe extern "C" fn pthread_create(
    out: *mut PthreadT,
    attr: *const Attr,
    start: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    arg: *mut c_void,
) -> c_int {
    let (detached, size) = if attr.is_null() {
        (false, STACK_BYTES)
    } else {
        (
            (*attr).detached != 0,
            if (*attr).stacksize == 0 {
                STACK_BYTES
            } else {
                (*attr).stacksize
            },
        )
    };
    if size > STACK_BYTES {
        return EINVAL;
    }
    let size = size.max(16 * 1024).next_multiple_of(4096);
    let slot = {
        let mut s = SLOTS.lock();
        match s.used.iter().position(|u| !u) {
            Some(i) => {
                s.used[i] = true;
                i
            }
            None => return EAGAIN,
        }
    };
    let release = |slot: usize| SLOTS.lock().used[slot] = false;
    let top = slot_top(slot);
    let base = top - size;
    let vmo = rt::sys::vmo_create(size / 4096);
    if vmo.status != 0 {
        release(slot);
        return EAGAIN;
    }
    let me = rt::sys::process_self().a1 as u32;
    let m = rt::sys::process_map(me, vmo.a1 as u32, base, 0, size, 3);
    rt::sys::handle_close(vmo.a1 as u32);
    if m.status != 0 {
        rt::sys::handle_close(me);
        release(slot);
        return EAGAIN;
    }
    let t = new_tcb(false, 0, slot);
    (*t).me = t;
    (*t).stack_base = base;
    (*t).start = Some(start);
    (*t).arg = arg;
    if detached {
        (*t).state.store(DETACHED, Ordering::SeqCst);
    }
    // the creator's `pthread_t` is stored before the thread can run (a thread may read it at once)
    *out = t as PthreadT;
    let r = rt::sys::thread_create(me, thread_main as *const () as usize as u64, top, t as u64);
    rt::sys::handle_close(me);
    if r.status != 0 {
        let me = rt::sys::process_self().a1 as u32;
        rt::sys::process_unmap(me, base);
        rt::sys::handle_close(me);
        release(slot);
        drop(Box::from_raw(t));
        return EAGAIN;
    }
    (*t).handle.store(r.a1 as u32, Ordering::SeqCst);
    0
}

#[no_mangle]
pub unsafe extern "C" fn pthread_join(th: PthreadT, retval: *mut *mut c_void) -> c_int {
    let t = th as *mut Tcb;
    if t == tcb() {
        return EDEADLK;
    }
    loop {
        let s = (*t).state.load(Ordering::SeqCst);
        if s & DETACHED != 0 {
            return EINVAL;
        }
        if s & EXITED != 0 {
            break;
        }
        rt::sys::futex_wait((*t).state.as_ptr(), s, u64::MAX);
    }
    // it announced its exit; wait until the thread object is really finished before its stack goes
    // (the creator publishes the handle right after `thread_create`; a joiner can beat it)
    let handle = loop {
        let h = (*t).handle.load(Ordering::SeqCst);
        if h != 0 {
            break h;
        }
        rt::sys::thread_yield();
    };
    loop {
        let w = rt::sys::object_wait(handle, signals::TERMINATED, u64::MAX);
        if w.status == 0 && w.a1 as u32 & signals::TERMINATED != 0 {
            break;
        }
    }
    if !retval.is_null() {
        *retval = (*t).retval;
    }
    let me = rt::sys::process_self().a1 as u32;
    rt::sys::process_unmap(me, (*t).stack_base);
    rt::sys::handle_close(me);
    rt::sys::handle_close(handle);
    SLOTS.lock().used[(*t).slot] = false;
    drop(Box::from_raw(t));
    0
}

#[no_mangle]
pub unsafe extern "C" fn pthread_detach(th: PthreadT) -> c_int {
    // (a detached thread's stack and TCB are not reclaimed before the process ends: a thread cannot
    // unmap its own stack, and nobody joins it; TIDY-UP T52)
    (*(th as *mut Tcb))
        .state
        .fetch_or(DETACHED, Ordering::SeqCst);
    0
}

#[no_mangle]
pub unsafe extern "C" fn pthread_exit(ret: *mut c_void) -> ! {
    let t = tcb();
    if t.is_null() || (*t).main {
        crate::stdlib::exit(0); // the last thread out ends the process
    }
    exit_thread(t, ret)
}

#[no_mangle]
pub extern "C" fn pthread_self() -> PthreadT {
    tcb() as PthreadT
}

#[no_mangle]
pub extern "C" fn pthread_equal(a: PthreadT, b: PthreadT) -> c_int {
    c_int::from(a == b)
}

#[no_mangle]
pub unsafe extern "C" fn pthread_attr_init(a: *mut Attr) -> c_int {
    *a = Attr {
        stacksize: 0,
        detached: 0,
    };
    0
}
#[no_mangle]
pub extern "C" fn pthread_attr_destroy(_a: *mut Attr) -> c_int {
    0
}
#[no_mangle]
pub unsafe extern "C" fn pthread_attr_setstacksize(a: *mut Attr, size: usize) -> c_int {
    (*a).stacksize = size as u64;
    0
}
#[no_mangle]
pub unsafe extern "C" fn pthread_attr_setdetachstate(a: *mut Attr, state: c_int) -> c_int {
    (*a).detached = state;
    0
}

// ---- mutexes ----

#[repr(C)]
pub struct Mutex_ {
    state: AtomicU32,
    kind: u32,
    count: u32,
    owner: AtomicUsize,
}

#[repr(C)]
pub struct MutexAttr {
    kind: u32,
}

#[no_mangle]
pub unsafe extern "C" fn pthread_mutexattr_init(a: *mut MutexAttr) -> c_int {
    (*a).kind = 0;
    0
}
#[no_mangle]
pub extern "C" fn pthread_mutexattr_destroy(_a: *mut MutexAttr) -> c_int {
    0
}
#[no_mangle]
pub unsafe extern "C" fn pthread_mutexattr_settype(a: *mut MutexAttr, kind: c_int) -> c_int {
    if !(0..=2).contains(&kind) {
        return EINVAL;
    }
    (*a).kind = kind as u32;
    0
}

#[no_mangle]
pub unsafe extern "C" fn pthread_mutex_init(m: *mut Mutex_, attr: *const MutexAttr) -> c_int {
    (*m).state.store(0, Ordering::Relaxed);
    (*m).kind = if attr.is_null() { 0 } else { (*attr).kind };
    (*m).count = 0;
    (*m).owner.store(0, Ordering::Relaxed);
    0
}
#[no_mangle]
pub extern "C" fn pthread_mutex_destroy(_m: *mut Mutex_) -> c_int {
    0
}

fn lock_word(w: &AtomicU32, deadline: u64) -> bool {
    if w.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
        .is_ok()
    {
        return true;
    }
    loop {
        // announce a sleeper (2); if the lock just became free we own it (as contended)
        if w.swap(2, Ordering::Acquire) == 0 {
            return true;
        }
        let r = rt::sys::futex_wait(w.as_ptr(), 2, deadline);
        if r.status() == Some(rt::abi::status::Status::TimedOut) {
            return false;
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn pthread_mutex_lock(m: *mut Mutex_) -> c_int {
    let me = owner_id();
    let kind = (*m).kind;
    if kind != 0 && (*m).owner.load(Ordering::Relaxed) == me {
        if kind == 1 {
            (*m).count += 1;
            return 0;
        }
        return EDEADLK;
    }
    lock_word(&(*m).state, u64::MAX);
    (*m).owner.store(me, Ordering::Relaxed);
    (*m).count = 1;
    0
}

#[no_mangle]
pub unsafe extern "C" fn pthread_mutex_trylock(m: *mut Mutex_) -> c_int {
    let me = owner_id();
    if (*m).kind == 1 && (*m).owner.load(Ordering::Relaxed) == me {
        (*m).count += 1;
        return 0;
    }
    if (*m)
        .state
        .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
        .is_ok()
    {
        (*m).owner.store(me, Ordering::Relaxed);
        (*m).count = 1;
        0
    } else {
        EBUSY
    }
}

#[no_mangle]
pub unsafe extern "C" fn pthread_mutex_unlock(m: *mut Mutex_) -> c_int {
    let me = owner_id();
    if (*m).kind != 0 && (*m).owner.load(Ordering::Relaxed) != me {
        return EPERM;
    }
    if (*m).kind == 1 && (*m).count > 1 {
        (*m).count -= 1;
        return 0;
    }
    (*m).owner.store(0, Ordering::Relaxed);
    (*m).count = 0;
    if (*m).state.swap(0, Ordering::Release) == 2 {
        rt::sys::futex_wake((*m).state.as_ptr(), 1);
    }
    0
}

// ---- condition variables ----

#[repr(C)]
pub struct Cond {
    seq: AtomicU32,
}

#[no_mangle]
pub unsafe extern "C" fn pthread_cond_init(c: *mut Cond, _attr: *const c_void) -> c_int {
    (*c).seq.store(0, Ordering::Relaxed);
    0
}
#[no_mangle]
pub extern "C" fn pthread_cond_destroy(_c: *mut Cond) -> c_int {
    0
}

unsafe fn cond_wait(c: *mut Cond, m: *mut Mutex_, deadline: u64) -> c_int {
    let seq = (*c).seq.load(Ordering::Relaxed);
    // release the mutex completely (a recursive one is released to depth 0 and restored after)
    let depth = (*m).count;
    (*m).count = 1;
    pthread_mutex_unlock(m);
    let r = rt::sys::futex_wait((*c).seq.as_ptr(), seq, deadline);
    pthread_mutex_lock(m);
    (*m).count = depth.max(1);
    if r.status() == Some(rt::abi::status::Status::TimedOut) {
        ETIMEDOUT
    } else {
        0
    }
}

#[no_mangle]
pub unsafe extern "C" fn pthread_cond_wait(c: *mut Cond, m: *mut Mutex_) -> c_int {
    cond_wait(c, m, u64::MAX)
}

#[repr(C)]
pub struct Timespec {
    pub tv_sec: i64,
    pub tv_nsec: c_long,
}

/// An absolute `CLOCK_REALTIME` time as a kernel tick deadline (a tick is a millisecond; time since
/// boot is the wall clock here).
fn deadline_of(ts: &Timespec) -> u64 {
    let ms = ts
        .tv_sec
        .saturating_mul(1000)
        .saturating_add(ts.tv_nsec / 1_000_000);
    ms.max(1) as u64
}

#[no_mangle]
pub unsafe extern "C" fn pthread_cond_timedwait(
    c: *mut Cond,
    m: *mut Mutex_,
    abs: *const Timespec,
) -> c_int {
    cond_wait(c, m, deadline_of(&*abs))
}

#[no_mangle]
pub unsafe extern "C" fn pthread_cond_signal(c: *mut Cond) -> c_int {
    (*c).seq.fetch_add(1, Ordering::Release);
    rt::sys::futex_wake((*c).seq.as_ptr(), 1);
    0
}

#[no_mangle]
pub unsafe extern "C" fn pthread_cond_broadcast(c: *mut Cond) -> c_int {
    (*c).seq.fetch_add(1, Ordering::Release);
    rt::sys::futex_wake((*c).seq.as_ptr(), u32::MAX);
    0
}

// ---- once and keys ----

#[no_mangle]
pub unsafe extern "C" fn pthread_once(once: *mut AtomicU32, f: extern "C" fn()) -> c_int {
    loop {
        match (*once).compare_exchange(0, 1, Ordering::Acquire, Ordering::Acquire) {
            Ok(_) => {
                f();
                (*once).store(2, Ordering::Release);
                rt::sys::futex_wake((*once).as_ptr(), u32::MAX);
                return 0;
            }
            Err(2) => return 0,
            Err(_) => {
                rt::sys::futex_wait((*once).as_ptr(), 1, u64::MAX);
            }
        }
    }
}

static KEY_USED: Mutex<[bool; KEYS]> = Mutex::new([false; KEYS]);

#[no_mangle]
pub unsafe extern "C" fn pthread_key_create(key: *mut u32, _destructor: *const c_void) -> c_int {
    let mut used = KEY_USED.lock();
    match used.iter().position(|u| !u) {
        Some(i) => {
            used[i] = true;
            *key = i as u32;
            // a re-used index must not show the old key's value (in this thread; others: T52)
            let t = tcb();
            if !t.is_null() {
                (*t).keys[i] = core::ptr::null_mut();
            }
            0
        }
        None => EAGAIN,
    }
}

#[no_mangle]
pub extern "C" fn pthread_key_delete(key: u32) -> c_int {
    if (key as usize) < KEYS {
        KEY_USED.lock()[key as usize] = false;
        0
    } else {
        EINVAL
    }
}

#[no_mangle]
pub unsafe extern "C" fn pthread_setspecific(key: u32, value: *const c_void) -> c_int {
    let t = tcb();
    if t.is_null() || key as usize >= KEYS {
        return EINVAL;
    }
    (*t).keys[key as usize] = value as *mut c_void;
    0
}

#[no_mangle]
pub unsafe extern "C" fn pthread_getspecific(key: u32) -> *mut c_void {
    let t = tcb();
    if t.is_null() || key as usize >= KEYS {
        return core::ptr::null_mut();
    }
    (*t).keys[key as usize]
}

// ---- time and scheduling helpers ----

#[no_mangle]
pub extern "C" fn sched_yield() -> c_int {
    rt::sys::thread_yield();
    0
}

#[no_mangle]
pub unsafe extern "C" fn clock_gettime(_clock: c_int, ts: *mut Timespec) -> c_int {
    let ms = rt::sys::clock_get();
    (*ts).tv_sec = (ms / 1000) as i64;
    (*ts).tv_nsec = ((ms % 1000) * 1_000_000) as c_long;
    0
}

#[no_mangle]
pub unsafe extern "C" fn nanosleep(req: *const Timespec, rem: *mut Timespec) -> c_int {
    let ms = (*req)
        .tv_sec
        .saturating_mul(1000)
        .saturating_add(((*req).tv_nsec + 999_999) / 1_000_000);
    if ms > 0 {
        rt::io::nap(ms as u64);
    }
    if !rem.is_null() {
        (*rem).tv_sec = 0;
        (*rem).tv_nsec = 0;
    }
    0
}

#[no_mangle]
pub extern "C" fn sleep(s: u32) -> u32 {
    rt::io::nap(u64::from(s) * 1000);
    0
}

#[no_mangle]
pub extern "C" fn usleep(us: u32) -> c_int {
    rt::io::nap(u64::from(us).div_ceil(1000));
    0
}
