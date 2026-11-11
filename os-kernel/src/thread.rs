//! Kernel threads: per-thread kernel stacks, the context switch, and the glue between
//! the pure scheduler core (`kcore::sched`) and the machine (OS-ARCHITECTURE.md, 8).
//!
//! All scheduler state lives in one `Kernel` value that is only touched with
//! interrupts disabled (one hart; S5 turns this into real locks). The core decides
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
}

struct Kernel {
    sched: Sched<Class>,
    threads: BTreeMap<Tid, KThread>,
    running: Tid,
    exits: WaitQueue,
    queues: Vec<WaitQueue>,
}

static mut KERNEL: Option<Kernel> = None;
static TICKS: AtomicU64 = AtomicU64::new(0);
/// Counts iterations of the idle loop (so a test can see that idle ran).
pub static IDLE_SPINS: AtomicU64 = AtomicU64::new(0);

/// The scheduler state. Callers hold interrupts disabled (so the timer interrupt
/// cannot re-enter) and do not keep the reference across a context switch.
fn kernel() -> &'static mut Kernel {
    debug_assert!(
        !irq::enabled(),
        "scheduler state touched with interrupts on"
    );
    // SAFETY: one hart, interrupts off (checked above in debug builds), and every
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
    // SAFETY: clears sstatus.FS only.
    unsafe { core::arch::asm!("csrc sstatus, {}", in(reg) 3usize << 13) };
    let mut sched = Sched::new(new_class());
    let boot = sched.adopt_current(boot_prio).expect("boot thread");
    let mut threads = BTreeMap::new();
    threads.insert(
        boot,
        KThread {
            ctx: Box::default(),
            stack: None,
            name: "boot",
        },
    );
    // SAFETY: called once, interrupts off, before any other use of KERNEL.
    unsafe {
        *addr_of_mut!(KERNEL) = Some(Kernel {
            sched,
            threads,
            running: boot,
            exits: WaitQueue::new(),
            queues: Vec::new(),
        });
    }
    spawn("idle", PRIO_IDLE, idle_main, 0);
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
    // A new thread starts inside a context switch, with interrupts off.
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
        let tid = k.sched.spawn(prio).expect("valid priority");
        k.threads.insert(
            tid,
            KThread {
                ctx,
                stack: Some(stack),
                name,
            },
        );
        reschedule();
        tid
    })
}

/// Switch to whichever thread the core says should run, if it is not this one.
/// Interrupts must be off.
fn reschedule() {
    let (old, new) = {
        let k = kernel();
        let next = k.sched.current().expect("the idle thread always exists");
        if next == k.running {
            return;
        }
        let outgoing = k
            .threads
            .get_mut(&k.running)
            .expect("running thread exists");
        if let Some(stack) = &outgoing.stack {
            assert!(
                guard_intact(stack),
                "stack overflow in thread {}",
                outgoing.name
            );
        }
        let old: *mut Context = &mut *outgoing.ctx;
        k.running = next;
        let incoming = k.threads.get(&next).expect("next thread exists");
        if let Some(stack) = &incoming.stack {
            assert!(
                guard_intact(stack),
                "stack of thread {} was overwritten while it slept",
                incoming.name
            );
        }
        let new: *const Context = &*incoming.ctx;
        (old, new)
    };
    // SAFETY: `old` and `new` point into boxed contexts that stay alive and in
    // place (threads are only reaped once exited and switched away from); the
    // switch saves this thread's state and resumes the other's, as `context_switch`
    // documents. Interrupts are off, and no reference to KERNEL is live.
    unsafe { context_switch(old, new) };
}

/// The timer interrupt: advance time, and preempt if the core says so.
pub fn timer_interrupt() {
    TICKS.fetch_add(1, Ordering::SeqCst);
    crate::timer::next_tick();
    kernel().sched.tick();
    reschedule();
}

/// Ticks since the timer started (1 ms each).
pub fn ticks() -> u64 {
    TICKS.load(Ordering::SeqCst)
}

pub fn current() -> Tid {
    irq::without(|| kernel().running)
}

/// Give up the rest of the time slice.
pub fn yield_now() {
    irq::without(|| {
        kernel().sched.yield_current().expect("running thread");
        reschedule();
    });
}

/// Block until woken (or return at once if a wake already arrived).
pub fn block() {
    irq::without(|| {
        if kernel().sched.block_current().expect("running thread") {
            reschedule();
        }
    });
}

/// Wake a thread (or leave it a token if it is not blocked yet).
pub fn wake(tid: Tid) {
    irq::without(|| {
        let _ = kernel().sched.wake(tid);
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
            if k.sched.sleep_until(deadline).expect("running thread") {
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
        let _ = kernel().sched.set_priority(tid, prio);
        reschedule();
    });
}

/// End the calling thread. Its stack is freed by whoever `join`s it.
pub fn exit() -> ! {
    irq::without(|| {
        let k = kernel();
        k.sched.exit_current().expect("running thread");
        let Kernel { sched, exits, .. } = k;
        exits.wake_all(sched);
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
            if tid == k.running {
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
                    if exits.wait(sched).expect("running thread") {
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
    loop {
        let result = irq::without(|| {
            if ready() {
                return Some(true);
            }
            let k = kernel();
            if k.sched.now() >= deadline {
                return Some(false);
            }
            let Kernel { sched, queues, .. } = k;
            if queues[q]
                .wait_until(sched, deadline)
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

/// Block on queue `q` until `ready()` holds. `ready` is evaluated with interrupts
/// off, so checking and queueing are one atomic step with respect to wakers: this is
/// the "no lost wakeup" path.
pub fn wait_until(q: usize, mut ready: impl FnMut() -> bool) {
    loop {
        let go = irq::without(|| {
            if ready() {
                return true;
            }
            let k = kernel();
            let Kernel { sched, queues, .. } = k;
            if queues[q].wait(sched).expect("running thread") {
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
        let woke = queues[q].wake_one(sched).is_some();
        reschedule();
        woke
    })
}

pub fn wake_all(q: usize) {
    irq::without(|| {
        let k = kernel();
        let Kernel { sched, queues, .. } = k;
        queues[q].wake_all(sched);
        reschedule();
    });
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
    v >> 13 & 3 == 0
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
        let t = k.threads.get(&k.running).expect("running thread exists");
        let stack = t
            .stack
            .as_ref()
            .expect("a user thread has its own kernel stack");
        ((stack.as_ptr() as usize + STACK_BYTES) & !15) as u64
    })
}
