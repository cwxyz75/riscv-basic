//! Kernel threads: per-thread kernel stacks, the context switch, and the glue between
//! the pure scheduler core (`kcore::sched`) and the machine (OS-ARCHITECTURE.md, 8).
//!
//! All scheduler state lives in one `Kernel` value that is only touched with
//! interrupts disabled and the big kernel lock held (`irq::without`). The core decides
//! which thread *should* run; [`reschedule`] compares that with the thread that *is*
//! running and switches stacks if they differ. A switch can happen
//!   * voluntarily (block, sleep, yield, exit, or waking something better), from
//!     ordinary code with interrupts off, or
//!   * on preemption, from the timer interrupt handler, in the middle of the
//!     interrupted thread's trap frame.
//!
//! Either way the thread that is switched out resumes exactly where it left off.

use crate::{heap, irq};
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::arch::global_asm;
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicU64, Ordering};
use kcore::sched::{Prio, Sched, State, Tid, WaitQueue, PRIO_IDLE};

#[cfg(not(feature = "sched-round-robin"))]
pub type Class = kcore::sched::FixedPriority;
#[cfg(feature = "sched-round-robin")]
pub type Class = kcore::sched::RoundRobin;

fn new_class() -> Class {
    Class::default()
}

/// Kernel stack size per thread.
pub const STACK_BYTES: usize = 32 * 1024;
const STACK_WORDS: usize = STACK_BYTES / 8;
const CANARY: u64 = 0xdead_c0de_5ca1_ab1e;
/// Words of guard pattern at the bottom of every stack (the overflow detector).
const GUARD_WORDS: usize = 8;

fn guard_intact(stack: &[u64]) -> bool {
    stack[..GUARD_WORDS].iter().all(|&w| w == CANARY)
}

/// Callee-saved state, saved and restored by `context_switch`.
#[repr(C)]
#[derive(Default)]
struct Context {
    ra: u64,
    sp: u64,
    s: [u64; 12],
}

global_asm!(
    r#"
    .section .text
    .globl context_switch
    .balign 4
context_switch:                  # a0 = old *mut Context, a1 = new *const Context
    sd   ra,   0(a0)
    sd   sp,   8(a0)
    sd   s0,  16(a0)
    sd   s1,  24(a0)
    sd   s2,  32(a0)
    sd   s3,  40(a0)
    sd   s4,  48(a0)
    sd   s5,  56(a0)
    sd   s6,  64(a0)
    sd   s7,  72(a0)
    sd   s8,  80(a0)
    sd   s9,  88(a0)
    sd   s10, 96(a0)
    sd   s11, 104(a0)
    ld   ra,   0(a1)
    ld   sp,   8(a1)
    ld   s0,  16(a1)
    ld   s1,  24(a1)
    ld   s2,  32(a1)
    ld   s3,  40(a1)
    ld   s4,  48(a1)
    ld   s5,  56(a1)
    ld   s6,  64(a1)
    ld   s7,  72(a1)
    ld   s8,  80(a1)
    ld   s9,  88(a1)
    ld   s10, 96(a1)
    ld   s11, 104(a1)
    ret

    .globl thread_trampoline
    .balign 4
thread_trampoline:               # a new thread's first instruction (via `ret` above)
    mv   a0, s0                  # entry
    mv   a1, s1                  # argument
    call thread_bootstrap
    ebreak
"#
);

extern "C" {
    fn context_switch(old: *mut Context, new: *const Context);
    fn thread_trampoline();
}

struct KThread {
    ctx: Box<Context>,
    /// `None` for the boot thread, which runs on the boot stack.
    stack: Option<Box<[u64]>>,
    name: &'static str,
    /// Nobody will `join` this thread: when it exits the reaper frees it.
    detached: bool,
    /// The big kernel lock's nesting depth this thread had when it was switched out (K3); a new
    /// thread starts at 1: it is born inside the context switch that first runs it, and its
    /// bootstrap leaves that region.
    bkl_depth: u32,
}

struct Kernel {
    sched: Sched<Class>,
    threads: BTreeMap<Tid, KThread>,
    /// The thread running on each hart (by dense hart index).
    running: Vec<Tid>,
    exits: WaitQueue,
    queues: Vec<WaitQueue>,
    /// Exited detached threads waiting for the reaper.
    zombies: Vec<Tid>,
    reaper_queue: usize,
}

static mut KERNEL: Option<Kernel> = None;
static TICKS: AtomicU64 = AtomicU64::new(0);
/// Counts iterations of the idle loop (so a test can see that idle ran).
pub static IDLE_SPINS: AtomicU64 = AtomicU64::new(0);

/// The scheduler state. Callers hold interrupts disabled (so the timer interrupt
/// cannot re-enter) and do not keep the reference across a context switch.
#[track_caller]
fn kernel() -> &'static mut Kernel {
    debug_assert!(
        !irq::enabled() && crate::bkl::held(),
        "scheduler state touched without the big kernel lock"
    );
    // SAFETY: the big kernel lock is held (checked above in debug builds), and every
    // caller drops the reference before any switch, so no two live `&mut` alias.
    unsafe {
        (*addr_of_mut!(KERNEL))
            .as_mut()
            .expect("thread::init was not called")
    }
}

/// Turn the code that is running (the boot thread) into thread 1 and start the idle
/// thread. Needs the heap. Interrupts must still be off.
pub fn init(boot_prio: Prio) {
    // The kernel does no floating point and thread switches do not save FP state:
    // keep the FPU off (sstatus.FS = 0) so any accidental use traps instead of
    // silently corrupting another thread's registers.
    // The same for the vector unit (sstatus.VS, bits 10:9; mandatory in RVA23, OD47): no vector
    // state is saved per thread, so user mode must not be able to use it either.
    // SAFETY: clears sstatus.FS and sstatus.VS only.
    unsafe { core::arch::asm!("csrc sstatus, {}", in(reg) (3usize << 13) | (3usize << 9)) };
    let harts = crate::smp::started();
    let mut sched = Sched::with_harts(new_class(), harts);
    let boot = sched
        .adopt_current_on(0, boot_prio, Some(0))
        .expect("boot thread");
    let mut threads = BTreeMap::new();
    threads.insert(
        boot,
        KThread {
            ctx: Box::default(),
            stack: None,
            name: "boot",
            detached: false,
            bkl_depth: 0,
        },
    );
    // SAFETY: called once, interrupts off, before any other use of KERNEL.
    unsafe {
        *addr_of_mut!(KERNEL) = Some(Kernel {
            sched,
            threads,
            running: alloc::vec![boot; harts],
            exits: WaitQueue::new(),
            queues: Vec::new(),
            zombies: Vec::new(),
            reaper_queue: 0,
        });
    }
    // hart 0's idle thread (the other harts adopt their own boot context when they join)
    spawn_with_affinity("idle", PRIO_IDLE, idle_main, 0, false, Some(0));
    let q = new_queue();
    kernel().reaper_queue = q;
    spawn("reaper", REAPER_PRIO, reaper_main, 0);
    crate::percpu::this()
        .in_sched
        .store(true, Ordering::Release);
    SCHEDULER_READY.store(true, Ordering::Release);
}

/// Set once `init` has finished: the secondary harts then join the scheduler.
static SCHEDULER_READY: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// How many harts have joined (hart 0 counts from the start).
static JOINED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(1);

/// The dense index of the calling hart.
fn hart() -> usize {
    crate::percpu::this().index.load(Ordering::Relaxed)
}

/// Is the calling hart part of the scheduler (it has an idle thread and may run threads)?
pub fn scheduler_ready() -> bool {
    crate::percpu::this().in_sched.load(Ordering::Acquire)
}

/// Has the boot hart finished `init` (so the other harts may join)?
pub fn scheduler_exists() -> bool {
    SCHEDULER_READY.load(Ordering::Acquire)
}

pub fn joined() -> usize {
    JOINED.load(Ordering::SeqCst)
}

/// A secondary hart joins the scheduler: its boot context becomes its idle thread (pinned to it)
/// and it never returns.
pub fn join_scheduler() -> ! {
    irq::without(|| {
        let h = hart();
        let k = kernel();
        let tid = k
            .sched
            .adopt_current_on(h, PRIO_IDLE, Some(h))
            .expect("a secondary hart joins once");
        k.threads.insert(
            tid,
            KThread {
                ctx: Box::default(),
                stack: None,
                name: "idle",
                detached: false,
                bkl_depth: 0,
            },
        );
        k.running[h] = tid;
        crate::percpu::this()
            .in_sched
            .store(true, Ordering::Release);
        JOINED.fetch_add(1, Ordering::SeqCst);
    });
    irq::enable();
    idle_main(0);
    unreachable!()
}

fn idle_main(_: usize) {
    loop {
        IDLE_SPINS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: `wfi` idles the hart until an interrupt; interrupts are enabled
        // here (thread_bootstrap enabled them).
        unsafe { core::arch::asm!("wfi") };
    }
}

/// First Rust code of every new thread (called from the assembly trampoline).
#[no_mangle]
extern "C" fn thread_bootstrap(entry: usize, arg: usize) -> ! {
    // A new thread starts inside a context switch, with interrupts off and the big kernel lock
    // handed over at depth 1: it leaves that region now and runs outside the lock (K4).
    crate::bkl::unlock();
    irq::enable();
    // SAFETY: `entry` is the `fn(usize)` that `spawn` stored, round-tripped through
    // a register as an address.
    let entry: fn(usize) = unsafe { core::mem::transmute(entry) };
    entry(arg);
    exit()
}

/// Create a thread. It runs `entry(arg)` and then exits. It may preempt the caller
/// at once if it has a higher priority.
pub fn spawn(name: &'static str, prio: Prio, entry: fn(usize), arg: usize) -> Tid {
    spawn_with(name, prio, entry, arg, false)
}

/// A thread nobody joins: when it exits the reaper thread frees its stack and record.
#[allow(dead_code)]
pub fn spawn_detached(name: &'static str, prio: Prio, entry: fn(usize), arg: usize) -> Tid {
    spawn_with(name, prio, entry, arg, true)
}

/// The affinity new threads get unless asked otherwise: hart 0 (S5c1; S5c2 lets them migrate).
pub static DEFAULT_AFFINITY: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

fn default_affinity() -> Option<usize> {
    let a = DEFAULT_AFFINITY.load(Ordering::Relaxed);
    (a != usize::MAX).then_some(a)
}

/// Like `spawn_detached`, free to run on any hart.
pub fn spawn_free_detached(name: &'static str, prio: Prio, entry: fn(usize), arg: usize) -> Tid {
    spawn_with_affinity(name, prio, entry, arg, true, None)
}

/// Create a thread that may run on any hart, whatever the default is.
pub fn spawn_free(name: &'static str, prio: Prio, entry: fn(usize), arg: usize) -> Tid {
    spawn_with_affinity(name, prio, entry, arg, false, None)
}

fn spawn_with(name: &'static str, prio: Prio, entry: fn(usize), arg: usize, detached: bool) -> Tid {
    spawn_with_affinity(name, prio, entry, arg, detached, default_affinity())
}

fn spawn_with_affinity(
    name: &'static str,
    prio: Prio,
    entry: fn(usize),
    arg: usize,
    detached: bool,
    affinity: Option<usize>,
) -> Tid {
    let mut stack: Box<[u64]> = alloc::vec![0u64; STACK_WORDS].into_boxed_slice();
    stack[..GUARD_WORDS].fill(CANARY);
    let top = stack.as_ptr() as usize + STACK_BYTES;
    let mut ctx = Box::<Context>::default();
    ctx.ra = thread_trampoline as *const () as u64;
    ctx.sp = (top & !15) as u64;
    ctx.s[0] = entry as usize as u64;
    ctx.s[1] = arg as u64;
    irq::without(|| {
        let k = kernel();
        let tid = k
            .sched
            .spawn_on(hart(), prio, affinity)
            .expect("valid priority");
        k.threads.insert(
            tid,
            KThread {
                ctx,
                stack: Some(stack),
                name,
                detached,
                bkl_depth: 1,
            },
        );
        reschedule();
        tid
    })
}

/// Switch to whichever thread the core says should run, if it is not this one.
/// Interrupts must be off.
fn reschedule() {
    // First, tell the harts that should look at the run queues (a wake or a priority change on
    // this hart found something better for them). They are poked, never changed from here.
    let (old, new) = {
        let k = kernel();
        let h = hart();
        let pokes = k.sched.take_pokes();
        send_pokes(pokes, h);
        let next = k
            .sched
            .current_on(h)
            .expect("the idle thread always exists");
        let running = k.running[h];
        if next == running {
            return;
        }
        let outgoing = k.threads.get_mut(&running).expect("running thread exists");
        if let Some(stack) = &outgoing.stack {
            assert!(
                guard_intact(stack),
                "stack overflow in thread {}",
                outgoing.name
            );
        }
        let old: *mut Context = &mut *outgoing.ctx;
        k.running[h] = next;
        let incoming = k.threads.get(&next).expect("next thread exists");
        if let Some(stack) = &incoming.stack {
            assert!(
                guard_intact(stack),
                "stack of thread {} was overwritten while it slept",
                incoming.name
            );
        }
        let new: *const Context = &*incoming.ctx;
        // The lock stays with this hart across the switch, but its nesting depth now belongs to
        // the incoming thread (K3); the outgoing thread keeps its own for when it resumes.
        let in_depth = incoming.bkl_depth;
        let out_depth = crate::bkl::swap_depth(in_depth);
        k.threads
            .get_mut(&running)
            .expect("outgoing thread exists")
            .bkl_depth = out_depth;
        (old, new)
    };
    // A hart must never keep a user process's page table in `satp` across a switch: another hart
    // may tear that process down and free (and reuse) the root while this one is idle or running
    // a thread of some other process. Back to the kernel's own table; the return to user mode
    // loads the right one (`user::activate_current`).
    crate::kmem::use_kernel_table_if_needed();
    // SAFETY: `old` and `new` point into boxed contexts that stay alive and in
    // place (threads are only reaped once exited and switched away from); the
    // switch saves this thread's state and resumes the other's, as `context_switch`
    // documents. Interrupts are off, the big kernel lock is held (and handed over), and no
    // reference to KERNEL is live.
    unsafe { context_switch(old, new) };
}

/// Send a reschedule IPI to every hart in the bitmask (by dense index) except `me`.
fn send_pokes(mut pokes: u32, me: usize) {
    pokes &= !(1 << me);
    while pokes != 0 {
        let i = pokes.trailing_zeros() as usize;
        pokes &= pokes - 1;
        if i < crate::smp::started() && crate::percpu::hart(i).online.load(Ordering::Acquire) {
            crate::smp::send_ipi(i, crate::percpu::IPI_RESCHED);
        }
    }
}

/// The reschedule IPI (or any other reason to look again): this hart makes its own designation
/// right, then switches if needed. Called with the big kernel lock held.
pub fn reschedule_this_hart() {
    let k = kernel();
    let h = hart();
    k.sched.reschedule_on(h);
    reschedule();
}

/// The timer interrupt: advance time, and preempt if the core says so.
pub fn timer_interrupt() {
    let h = hart();
    let k = kernel();
    crate::timer::next_tick();
    if h == 0 {
        // Hart 0 keeps the clock: sleepers' deadlines are in its ticks.
        let now = TICKS.fetch_add(1, Ordering::SeqCst) + 1;
        k.sched.advance_to(now, 0);
    }
    k.sched.tick_slice(h);
    reschedule();
}

/// Ticks since the timer started (1 ms each).
pub fn ticks() -> u64 {
    TICKS.load(Ordering::SeqCst)
}

pub fn current() -> Tid {
    irq::without(|| kernel().running[hart()])
}

/// Give up the rest of the time slice.
pub fn yield_now() {
    irq::without(|| {
        kernel()
            .sched
            .yield_current_on(hart())
            .expect("running thread");
        reschedule();
    });
}

/// Block until woken (or return at once if a wake already arrived).
pub fn block() {
    irq::without(|| {
        if kernel()
            .sched
            .block_current_on(hart())
            .expect("running thread")
        {
            reschedule();
        }
    });
}

/// Wake a thread (or leave it a token if it is not blocked yet).
pub fn wake(tid: Tid) {
    irq::without(|| {
        let _ = kernel().sched.wake_on(hart(), tid);
        reschedule();
    });
}

/// Sleep for `n` ticks (or until woken).
pub fn sleep_ticks(n: u64) {
    let deadline = irq::without(|| kernel().sched.now() + n);
    // A loop: a stale entry on a wait queue can wake a sleeper early, and the sleep must not
    // end before its deadline.
    loop {
        let done = irq::without(|| {
            let k = kernel();
            if k.sched.now() >= deadline {
                return true;
            }
            if k.sched
                .sleep_until_on(hart(), deadline)
                .expect("running thread")
            {
                reschedule();
            }
            false
        });
        if done {
            return;
        }
    }
}

pub fn set_priority(tid: Tid, prio: Prio) {
    irq::without(|| {
        let _ = kernel().sched.set_priority_on(hart(), tid, prio);
        reschedule();
    });
}

/// End the calling thread. Its stack is freed by whoever `join`s it.
pub fn exit() -> ! {
    irq::without(|| {
        let k = kernel();
        let h = hart();
        k.sched.exit_current_on(h).expect("running thread");
        let me = k.running[h];
        let detached = k.threads.get(&me).is_some_and(|t| t.detached);
        let Kernel {
            sched,
            exits,
            queues,
            zombies,
            reaper_queue,
            ..
        } = k;
        exits.wake_all_on(sched, h);
        if detached {
            // Nobody will join it: hand it to the reaper (which runs once we have switched
            // away, so its stack is no longer in use when it is freed).
            zombies.push(me);
            queues[*reaper_queue].wake_all_on(sched, h);
        }
        reschedule();
    });
    unreachable!("an exited thread was scheduled again");
}

/// Wait for `tid` to exit, then free its stack and record. Returns false, without
/// waiting, if there is nothing to join: the thread is the caller itself, unknown, or
/// already joined by someone else (a thread can be joined once).
pub fn join(tid: Tid) -> bool {
    loop {
        let result = irq::without(|| {
            let k = kernel();
            if tid == k.running[hart()] {
                return Some(false);
            }
            match k.sched.state(tid) {
                None => Some(false),
                Some(State::Exited) => {
                    k.sched.reap(tid).expect("exited thread");
                    k.threads.remove(&tid);
                    Some(true)
                }
                Some(_) => {
                    let Kernel { sched, exits, .. } = k;
                    if exits.wait_on(sched, hart()).expect("running thread") {
                        reschedule();
                    }
                    None
                }
            }
        });
        if let Some(joined) = result {
            return joined;
        }
    }
}

/// A wait queue, by id.
pub fn new_queue() -> usize {
    irq::without(|| {
        let k = kernel();
        k.queues.push(WaitQueue::new());
        k.queues.len() - 1
    })
}

/// Block on queue `q` until `ready()` holds or the clock reaches `deadline` (absolute
/// ticks). Returns whether `ready()` held. `ready` runs with interrupts off, like
/// `wait_until`, so checking and sleeping are one atomic step.
pub fn wait_deadline(q: usize, mut ready: impl FnMut() -> bool, deadline: u64) -> bool {
    let mut slept = false;
    loop {
        let result = irq::without(|| {
            if ready() {
                return Some(true);
            }
            let k = kernel();
            if k.sched.now() >= deadline {
                return Some(false);
            }
            count_sleep(&mut slept);
            let Kernel { sched, queues, .. } = k;
            if queues[q]
                .wait_until_on(sched, hart(), deadline)
                .expect("running thread")
            {
                reschedule();
            }
            None
        });
        if let Some(r) = result {
            return r;
        }
    }
}

/// Sleep until `ready()` holds or `deadline` (absolute ticks), **not queued anywhere**: only
/// `wake_blocked(tid)` or the deadline ends the sleep early (slice W step 2: futex sleepers and
/// other waiters that are woken by id). `ready` runs with interrupts off with the big lock held, so
/// checking and sleeping are one step against any waker. Returns whether `ready()` held.
pub fn wait_deadline_private(ready: impl FnMut() -> bool, deadline: u64) -> bool {
    wait_deadline_registered(ready, || {}, deadline)
}

/// `wait_deadline_private`, calling `register` (in the same interrupts-off region, with the lock held)
/// just before each sleep: the waiter's registration in the wait table, so that the thread that makes
/// its condition true wakes exactly it. The caller removes the registration after the wait returns.
pub fn wait_deadline_registered(
    mut ready: impl FnMut() -> bool,
    mut register: impl FnMut(),
    deadline: u64,
) -> bool {
    let mut slept = false;
    loop {
        let result = irq::without(|| {
            if ready() {
                return Some(true);
            }
            let k = kernel();
            if k.sched.now() >= deadline {
                return Some(false);
            }
            register();
            count_sleep(&mut slept);
            if k.sched
                .sleep_until_on(hart(), deadline)
                .expect("running thread")
            {
                reschedule();
            }
            None
        });
        if let Some(r) = result {
            return r;
        }
    }
}

/// Wake thread `tid` if it is blocked (and leave a token if it is not, which a later queue wait
/// consumes; harmless to waiters that re-check). Counts a wake only for a thread that was blocked.
pub fn wake_blocked(tid: Tid) {
    irq::without(|| {
        let k = kernel();
        if k.sched.state(tid) == Some(State::Blocked) {
            WAKES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
        let _ = k.sched.wake_on(hart(), tid);
        reschedule();
    });
}

/// Block on queue `q` until `ready()` holds. `ready` is evaluated with interrupts
/// off, so checking and queueing are one atomic step with respect to wakers: this is
/// the "no lost wakeup" path.
pub fn wait_until(q: usize, mut ready: impl FnMut() -> bool) {
    let mut slept = false;
    loop {
        let go = irq::without(|| {
            if ready() {
                return true;
            }
            let k = kernel();
            count_sleep(&mut slept);
            let Kernel { sched, queues, .. } = k;
            if queues[q].wait_on(sched, hart()).expect("running thread") {
                reschedule();
            }
            false
        });
        if go {
            return;
        }
    }
}

pub fn wake_one(q: usize) -> bool {
    irq::without(|| {
        let k = kernel();
        let Kernel { sched, queues, .. } = k;
        let woke = queues[q].wake_one_on(sched, hart()).is_some();
        reschedule();
        woke
    })
}

pub fn wake_all(q: usize) {
    irq::without(|| {
        let k = kernel();
        let Kernel { sched, queues, .. } = k;
        let n = queues[q].wake_all_on(sched, hart());
        WAKES.fetch_add(n as u64, core::sync::atomic::Ordering::Relaxed);
        reschedule();
    });
}

// Wake-path counters (slice W step 0): how often a thread blocks, how often it is woken and finds its
// condition still false (a spurious wake: what targeted wake removes), and how many threads are woken.
static SLEEPS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static RESLEEPS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static WAKES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// A thread is about to block; `slept` says it already blocked once in this wait (so it was woken
/// without being satisfied).
fn count_sleep(slept: &mut bool) {
    use core::sync::atomic::Ordering::Relaxed;
    SLEEPS.fetch_add(1, Relaxed);
    if *slept {
        RESLEEPS.fetch_add(1, Relaxed);
    }
    *slept = true;
}

/// The counter `which` (0 sleeps, 1 spurious wakes, 2 wakes); 0 for an unknown index.
pub fn wake_stat(which: u64) -> u64 {
    use core::sync::atomic::Ordering::Relaxed;
    match which {
        0 => SLEEPS.load(Relaxed),
        1 => RESLEEPS.load(Relaxed),
        2 => WAKES.load(Relaxed),
        _ => 0,
    }
}

/// Every thread's stack guard is intact.
pub fn stacks_intact() -> bool {
    irq::without(|| {
        kernel()
            .threads
            .values()
            .all(|t| t.stack.as_ref().is_none_or(|s| guard_intact(s)))
    })
}

pub fn fp_off() -> bool {
    let v: usize;
    // SAFETY: reads a CSR.
    unsafe { core::arch::asm!("csrr {}, sstatus", out(reg) v) };
    v >> 13 & 3 == 0 && v >> 9 & 3 == 0
}

pub fn heap_free() -> usize {
    heap::free_bytes()
}

pub fn thread_count() -> usize {
    irq::without(|| kernel().threads.len())
}

/// How many threads the scheduler core tracks (must equal `thread_count`).
pub fn sched_thread_count() -> usize {
    irq::without(|| kernel().sched.thread_count())
}

/// Test hook: damage (or repair) a thread's stack guard, to show the overflow
/// detector sees it.
pub fn debug_set_guard(tid: Tid, intact: bool) {
    irq::without(|| {
        if let Some(stack) = kernel()
            .threads
            .get_mut(&tid)
            .and_then(|t| t.stack.as_mut())
        {
            stack[0] = if intact { CANARY } else { 0 };
        }
    });
}

/// The scheduling class in use.
pub fn class_name() -> &'static str {
    kcore::sched::SchedClass::name(&new_class())
}

/// The top of the calling thread's kernel stack (where a trap from user mode starts).
pub fn current_stack_top() -> u64 {
    irq::without(|| {
        let k = kernel();
        let t = k
            .threads
            .get(&k.running[hart()])
            .expect("running thread exists");
        let stack = t
            .stack
            .as_ref()
            .expect("a user thread has its own kernel stack");
        ((stack.as_ptr() as usize + STACK_BYTES) & !15) as u64
    })
}

/// Priority of the reaper: above everything that can create work for it.
const REAPER_PRIO: Prio = 50;

/// Frees detached threads once they have exited (stack, context, scheduler record).
fn reaper_main(_: usize) {
    loop {
        let q = irq::without(|| kernel().reaper_queue);
        wait_until(q, || !kernel().zombies.is_empty());
        irq::without(|| {
            let k = kernel();
            let dead: Vec<Tid> = core::mem::take(&mut k.zombies);
            for tid in dead {
                if k.sched.reap(tid).is_ok() {
                    k.threads.remove(&tid);
                }
            }
        });
    }
}

/// The scheduler's view of a thread (diagnostics for the SMP tests).
pub fn debug_state(tid: Tid) -> alloc::string::String {
    irq::without(|| {
        let k = kernel();
        alloc::format!(
            "{:?} prio {:?} aff {:?} cur {:?} pending {:?}",
            k.sched.state(tid),
            k.sched.priority(tid),
            k.sched.affinity(tid),
            (0..k.sched.harts())
                .map(|h| k.sched.current_on(h))
                .collect::<Vec<_>>(),
            (0..k.sched.harts())
                .map(|h| k.sched.resched_pending(h))
                .collect::<Vec<_>>()
        )
    })
}
