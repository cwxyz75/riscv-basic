//! Kernel threads: per-thread kernel stacks, the context switch, and the glue between
//! the pure scheduler core (`kcore::sched_smp::Sched2`: one run queue per hart, SCHED-SMP-SPEC 4.4) and the machine
//! (OS-ARCHITECTURE.md, 8).
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
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::panic,
    clippy::unreachable
)] // KT3 (KERNEL-V1-SPEC 7.4): no panic a program can cause; remaining sites carry `#[allow] // INVARIANT:` (scripts/kernel_allows.py)

use crate::{heap, irq};
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::arch::global_asm;
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicU64, Ordering};
use kcore::sched::{Prio, State, Tid};
use kcore::sched2::{HartRq, SchedAttr, MS};
use kcore::sched_smp::{Rqs, Sched2, Table, Waiters2};

#[cfg(not(feature = "sched-round-robin"))]
pub type Class = kcore::sched2::FixedPrio;
#[cfg(feature = "sched-round-robin")]
pub type Class = kcore::sched2::RoundRobin;

/// The scheduler: one run queue per hart, placement, push and pull (`kcore::sched_smp`, SCHED-SMP-SPEC 4.4).
type KSched = Sched2<Class, KRqs>;

/// The per-hart run queues, each behind its own lock of class `RQ` (instance: the hart), and the thread table behind one of class
/// `TT` (L11b). `Sched2` takes at most two `RQ` locks, in ascending hart order, and never the table while it holds one.
struct KRqs {
    rq: Vec<klock::Spin<HartRq<Class>, crate::bkl::Machine>>,
    table: klock::Spin<Table, crate::bkl::Machine>,
}

impl Rqs<Class> for KRqs {
    type Guard<'a> = klock::Guard<'a, HartRq<Class>, crate::bkl::Machine>;
    type TableGuard<'a> = klock::Guard<'a, Table, crate::bkl::Machine>;
    fn len(&self) -> usize {
        self.rq.len()
    }
    #[allow(clippy::indexing_slicing)] // INVARIANT: x is a hart index below the hart count (callers pass harts they were given)
    fn lock(&self, x: usize) -> Self::Guard<'_> {
        self.rq[x].lock()
    }
    fn table(&self) -> Self::TableGuard<'_> {
        self.table.lock()
    }
}

fn new_class() -> Class {
    Class::default()
}

/// Kernel stack size per thread. 16 KiB since slice T: the deepest any stack went in the whole suite (debug
/// build, every test and the Lua test suite) was about 5.6 KiB (`stack_high_water`), and a test fails if one
/// ever passes three quarters of this.
pub const STACK_BYTES: usize = abi::layout::KSTACK_BYTES as usize;
const CANARY: u64 = 0xdead_c0de_5ca1_ab1e;
/// Words of guard pattern at the bottom of every stack (the overflow detector).
const GUARD_WORDS: usize = 8;

#[allow(clippy::indexing_slicing)] // INVARIANT: the stack is a whole stack (many words): GUARD_WORDS is far below its length
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
    stack: Option<crate::kstack::KStack>,
    name: &'static str,
    /// Nobody will `join` this thread: when it exits, the hart that switches away from it frees it (`finish_switch`).
    detached: bool,
    /// The big kernel lock's nesting depth this thread had when it was switched out (K3); a new
    /// thread starts at 1: it is born inside the context switch that first runs it, and its
    /// bootstrap leaves that region.
    bkl_depth: u32,
}

struct Kernel {
    threads: BTreeMap<Tid, KThread>,
    exits: Waiters2,
    queues: Vec<Waiters2>,
}

static mut KERNEL: Option<Kernel> = None;

/// The scheduler core: its run queues are behind their own per-hart locks (class `RQ`, SCHED-SMP-SPEC task 11), so it is shared by
/// every hart with no lock of its own; each call takes the locks it needs and says which. Set once, by `init`.
static mut SCHED: Option<KSched> = None;

/// The scheduler (every method takes `&self`; interrupts must be off).
#[allow(clippy::expect_used)] // INVARIANT: SCHED is set by `init` before any other hart starts or any scheduler call is made
fn sched() -> &'static KSched {
    // SAFETY: written once by `init` before any other hart is started or any scheduler call is made, and never again; after that
    // it is only read through a shared reference.
    unsafe {
        (*core::ptr::addr_of!(SCHED))
            .as_ref()
            .expect("thread::init was not called")
    }
}

/// The thread this hart is running (its own record: only this hart writes it).
fn running_here() -> Tid {
    crate::percpu::this().running.load(Ordering::Relaxed)
}
fn set_running(tid: Tid) {
    crate::percpu::this().running.store(tid, Ordering::Relaxed);
}
static TICKS: AtomicU64 = AtomicU64::new(0);
/// Counts iterations of the idle loop (so a test can see that idle ran).
pub static IDLE_SPINS: AtomicU64 = AtomicU64::new(0);

/// The scheduler state. Callers hold interrupts disabled (so the timer interrupt
/// cannot re-enter) and do not keep the reference across a context switch.
#[track_caller]
#[allow(clippy::expect_used)] // INVARIANT: KERNEL is set by `init` before the first use (boot order)
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

/// Turn the code that is running (the boot thread) into a thread (its id follows the harts' idle ids) and start the idle
/// thread. Needs the heap. Interrupts must still be off.
#[allow(clippy::expect_used)] // INVARIANT: boot code: the boot thread's priority is valid and the idle and boot records are created once
pub fn init(boot_prio: Prio) {
    // The kernel does no floating point and thread switches do not save FP state:
    // keep the FPU off (sstatus.FS = 0) so any accidental use traps instead of
    // silently corrupting another thread's registers.
    // The same for the vector unit (sstatus.VS, bits 10:9; mandatory in RVA23, OD47): no vector
    // state is saved per thread, so user mode must not be able to use it either.
    // SAFETY: clears sstatus.FS and sstatus.VS only.
    unsafe { core::arch::asm!("csrc sstatus, {}", in(reg) (3usize << 13) | (3usize << 9)) };
    let harts = crate::smp::started();
    let sched = KSched::over(KRqs {
        rq: (0..harts)
            .map(|h| klock::Spin::new(klock::Class::Rq, h as u32, HartRq::new(new_class())))
            .collect(),
        table: klock::Spin::new(klock::Class::Tt, 0, Table::new()),
    });
    // a wake never switches: the hart looks at its queues at its next defined point (`preempt_point`, SCHED-SMP-SPEC X7)
    sched.set_deferred(true);
    // only hart 0 is part of the scheduler until the others join (`join_scheduler`)
    for h in 1..harts {
        sched.set_online(h, false);
    }
    let boot = sched
        .adopt_current_on(
            0,
            SchedAttr {
                prio: boot_prio,
                rr: false,
            },
            Some(0),
        )
        .expect("boot thread");
    let idle0 = sched.idle(0);
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
            threads,
            exits: Waiters2::new(),
            queues: Vec::new(),
        });
    }
    // SAFETY: the one write (see `sched`): interrupts off, one hart.
    unsafe { *addr_of_mut!(SCHED) = Some(sched) };
    set_running(boot);
    // hart 0's idle thread (the other harts use their own boot context when they join): the scheduler made its id, this
    // gives it a stack and a first instruction
    let idle_thread = new_kthread("idle", idle_main, 0, false);
    kernel().threads.insert(idle0, idle_thread);
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
#[allow(clippy::unreachable)] // INVARIANT: a secondary hart that cannot join the scheduler cannot run; the loop never returns
pub fn join_scheduler() -> ! {
    irq::without(|| {
        let h = hart();
        let k = kernel();
        // the hart's boot context becomes its idle thread (the scheduler made the id when it was built)
        let tid = sched().idle(h);
        sched().set_clock(h, ticks_to_ns(TICKS.load(Ordering::SeqCst)));
        sched().set_online(h, true);
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
        set_running(tid);
        crate::percpu::this()
            .in_sched
            .store(true, Ordering::Release);
        JOINED.fetch_add(1, Ordering::SeqCst);
        // Anything that became runnable for this hart while it was starting (its poke was forgotten or
        // arrived before it was ready) is looked at now, not at the next tick (T65).
        reschedule_this_hart();
    });
    irq::enable();
    idle_main(0);
    unreachable!()
}

/// The idle loop of SCHED-SMP-SPEC 3.9: with interrupts off, look at the hart's queues; if anything is queued or a mark is set,
/// switch; otherwise `wfi`, which returns when an enabled interrupt is pending even with `sstatus.SIE` clear, so a wake that
/// lands after the check (it has enqueued and sent its IPI) cannot be slept through, and then enable interrupts, which takes
/// the interrupt that woke the hart. (Today's 1 ms tick would end the sleep anyway; with no tick, L13, this is the whole protocol.)
fn idle_main(_: usize) {
    loop {
        IDLE_SPINS.fetch_add(1, Ordering::Relaxed);
        let was = irq::save_and_disable();
        if sched().idle_has_work(hart()) {
            irq::restore(was);
            irq::without(reschedule_this_hart);
            continue;
        }
        if crate::heap::needs_service() {
            irq::without(crate::heap::service); // an idle hart is a safe point too
        }
        // SAFETY: `wfi` idles the hart until an interrupt is pending; it is legal with interrupts disabled (the architecture
        // resumes on any pending and individually enabled interrupt), which is the point of the protocol above.
        unsafe { core::arch::asm!("wfi") };
        irq::restore(was);
    }
}

/// First Rust code of every new thread (called from the assembly trampoline).
#[no_mangle]
extern "C" fn thread_bootstrap(entry: usize, arg: usize) -> ! {
    // A new thread starts inside a context switch, with interrupts off and the big kernel lock
    // handed over at depth 1: it leaves that region now and runs outside the lock (K4).
    crate::latency::switch_in();
    finish_switch();
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

/// A thread nobody joins: when it exits, the hart that switches away from it frees its stack and record.
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

#[allow(clippy::expect_used)] // INVARIANT: the priority passed by kernel code is valid (it was chosen in this crate)
fn spawn_with_affinity(
    name: &'static str,
    prio: Prio,
    entry: fn(usize),
    arg: usize,
    detached: bool,
    affinity: Option<usize>,
) -> Tid {
    let thread = new_kthread(name, entry, arg, detached);
    irq::without(|| {
        let k = kernel();
        let tid = sched()
            .spawn_on(hart(), SchedAttr { prio, rr: false }, affinity)
            .expect("valid priority");
        k.threads.insert(tid, thread);
        note_resched();
        tid
    })
}

/// A kernel thread's stack and first context. The stack comes from frames, not the heap, and has a guard page below it
/// (`kstack.rs`).
#[allow(clippy::expect_used, clippy::indexing_slicing)] // INVARIANT: memory for a kernel thread's stack at boot or at thread creation by kernel code: running out is the kernel's own bug (the user thread limit and the job's stack charge are checked first); GUARD_WORDS is far below the stack's length
fn new_kthread(name: &'static str, entry: fn(usize), arg: usize, detached: bool) -> KThread {
    let mut stack = crate::kstack::alloc().expect("memory for a kernel stack");
    stack[..GUARD_WORDS].fill(CANARY);
    let top = stack.top();
    let mut ctx = Box::<Context>::default();
    ctx.ra = thread_trampoline as *const () as u64;
    ctx.sp = (top & !15) as u64;
    ctx.s[0] = entry as usize as u64;
    ctx.s[1] = arg as u64;
    KThread {
        ctx,
        stack: Some(stack),
        name,
        detached,
        bkl_depth: 1,
    }
}

/// This hart's context switch is complete (the thread it left has been saved), or it decided not to switch: the thread it
/// left, if any, is now available to the other harts (`Sched2::switched`). Interrupts are off.
fn finish_switch() {
    let h = hart();
    let pokes = {
        let sc = sched();
        sc.switched(h);
        sc.take_pokes()
    };
    send_pokes(pokes, h);
    // a mark that is still set (a wake that came after this hart's last settle) must not be left without its flag
    if sched().resched_pending(h) {
        crate::percpu::this()
            .need_resched
            .store(true, Ordering::Relaxed);
    }
    // The thread this hart replaced, if it has exited and nobody will join it, is dead now: its context is saved, so its stack
    // is in nobody's use (SCHED-SMP-SPEC 3.6: `Dead`, then reap by the hart; the reaper thread this replaces needed a wake and a
    // switch for every thread that ended). The big kernel lock is held (it follows the thread), so the table is ours.
    let prev = crate::percpu::this().prev.swap(0, Ordering::Relaxed);
    if prev != 0 {
        reap_if_dead(prev);
    }
}

/// Free `tid` if it is an exited detached thread (a joined one is freed by its joiner). The caller has completed the switch away
/// from it and holds the big kernel lock.
fn reap_if_dead(tid: Tid) {
    let k = kernel();
    let dead = k.threads.get(&tid).is_some_and(|t| t.detached)
        && sched().state(tid) == Some(State::Exited);
    if dead {
        let reaped = sched().reap(tid).is_ok();
        // (it cannot fail under the big lock: a failure would leak the thread's stack, so it is loud in a debug build)
        debug_assert!(reaped, "reap of exited thread {tid} failed");
        if reaped {
            if let Some(t) = k.threads.remove(&tid) {
                note_stack_use(&t);
            }
        }
    }
}

/// A wake (or a priority change, or a new thread) may have designated another thread for this hart: send the pokes it owes, and if
/// the designation is no longer the thread that is running, remember it (`need_resched`). **A wake never switches** (SCHED-SMP-SPEC
/// X7): the switch happens at the next defined point, `preempt_point`: the end of the outermost critical section (`irq::without`),
/// the return to user mode, or the thread's own block, yield or exit. Interrupts are off and the big kernel lock is held.
fn note_resched() {
    let h = hart();
    let (pokes, switch) = {
        let sc = sched();
        (sc.take_pokes(), sc.resched_pending(h))
    };
    send_pokes(pokes, h);
    if switch {
        crate::percpu::this()
            .need_resched
            .store(true, Ordering::Relaxed);
    }
}

/// A defined point at which a thread may be replaced (X7): switch if a wake asked for it. Cheap when nothing did. Interrupts off,
/// big kernel lock held.
#[inline]
pub fn preempt_point() {
    if crate::percpu::this().need_resched.load(Ordering::Relaxed) {
        // (the hart makes its own designation right, then switches: the settle a wake deferred)
        reschedule_this_hart();
    }
}

/// Switch to whichever thread the core says should run, if it is not this one.
/// Interrupts must be off.
#[allow(clippy::expect_used)] // INVARIANT: the running and the next thread are threads the scheduler knows and the kernel table holds (every scheduler thread has a KThread: the same insert)
fn reschedule() {
    crate::percpu::this()
        .need_resched
        .store(false, Ordering::Relaxed);
    // First, tell the harts that should look at the run queues (a wake or a priority change on
    // this hart found something better for them). They are poked, never changed from here.
    let (old, new) = {
        let k = kernel();
        let h = hart();
        let (pokes, next) = {
            let sc = sched();
            (sc.take_pokes(), sc.current_on(h))
        };
        send_pokes(pokes, h);
        let running = running_here();
        if next == running {
            finish_switch();
            return;
        }
        // (a double dispatch would panic here instead of running one thread on two harts)
        debug_assert!(
            (0..crate::smp::started())
                .all(|y| y == h || crate::percpu::hart(y).running.load(Ordering::Relaxed) != next),
            "thread {next} is already running on another hart"
        );
        let outgoing = k.threads.get_mut(&running).expect("running thread exists");
        if let Some(stack) = &outgoing.stack {
            assert!(
                guard_intact(stack),
                "stack overflow in thread {}",
                outgoing.name
            );
        }
        let old: *mut Context = &mut *outgoing.ctx;
        set_running(next);
        crate::percpu::this().prev.store(running, Ordering::Relaxed);
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
    crate::latency::switch_out();
    // SAFETY: `old` and `new` point into boxed contexts that stay alive and in
    // place (threads are only reaped once exited and switched away from); the
    // switch saves this thread's state and resumes the other's, as `context_switch`
    // documents. Interrupts are off, the big kernel lock is held (and handed over), and no
    // reference to KERNEL is live.
    unsafe { context_switch(old, new) };
    // (resumed: on whichever hart now runs this thread, the switch that brought it here has finished saving the one before)
    finish_switch();
    crate::latency::switch_in();
}

/// Switch threads if the scheduler core has designated another one for this hart (the caller holds the big kernel lock).
pub fn switch_if_designated() {
    reschedule();
}

/// Send a reschedule IPI to every hart in the bitmask (by dense index) except `me`.
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: pokes is a non-zero mask of harts (the loop condition), so pokes - 1 cannot underflow
fn send_pokes(mut pokes: u32, me: usize) {
    // A poke for this hart itself (another hart marked it after this hart's own settle began, and this hart took the mask first):
    // the mark is real and `pending` is set, so the hart must look again: an IPI to itself, taken as soon as interrupts are on.
    if pokes & (1 << me) != 0 {
        crate::smp::send_ipi(me, crate::percpu::IPI_RESCHED);
    }
    pokes &= !(1 << me);
    while pokes != 0 {
        let i = pokes.trailing_zeros() as usize;
        pokes &= pokes - 1;
        // (the scheduler marks only harts that have joined it, so every poke is for a hart that is up)
        if i < crate::smp::started() && crate::percpu::hart(i).online.load(Ordering::Acquire) {
            crate::smp::send_ipi(i, crate::percpu::IPI_RESCHED);
        }
    }
}

/// The reschedule IPI (or any other reason to look again): this hart makes its own designation
/// right, then switches if needed. Called with the big kernel lock held.
pub fn reschedule_this_hart() {
    let h = hart();
    sched().reschedule_on(h);
    reschedule();
}

/// The reschedule IPI's scheduler work **without the big kernel lock** (SCHED-SMP-SPEC task 12): the hart makes its own
/// designation right under the scheduler lock and sends the pokes that owes. Returns whether it must now switch threads
/// (the caller then takes the big lock and calls `reschedule`); most of the time it need not.
pub fn reschedule_fast() -> bool {
    let h = hart();
    let (pokes, switch) = {
        let sc = sched();
        sc.reschedule_on(h);
        let switch = sc.current_on(h) != running_here();
        if !switch {
            sc.switched(h); // no switch after all: nothing is in transit
        }
        (sc.take_pokes(), switch)
    };
    send_pokes(pokes, h);
    switch
}

/// The starvation watchdog (SCHED-SMP-SPEC 3.9; debug builds): every 100 ms, each hart at its own phase, a thread queued for more than
/// 500 ms while a hart that may run it idles is a lost wake, not a slow system: the report is a panic (`##PANIC starved thread`).
#[cfg(debug_assertions)]
#[allow(clippy::arithmetic_side_effects, clippy::panic)] // INVARIANT: h is a hart index below 32 and now counts 1 ms ticks; the panic is the detector's report
fn starvation_watchdog(h: usize, now: u64) {
    if now % 100 == h as u64 {
        if let Some((t, on, idle)) = sched().starved(500 * MS) {
            panic!("starved thread {t}: queued on hart {on} for more than 500 ms while hart {idle} idles");
        }
    }
}

/// The timer interrupt's scheduler work **without the big kernel lock** (SCHED-SMP-SPEC task 12): advance this hart's clock,
/// fire its due timers, account the running thread's slice. Returns whether the hart must now switch threads (a rotation, a
/// timer wake that outranks the running thread, a mark served): the caller then takes the big lock and calls `reschedule`.
/// A tick that changes nothing needs no lock but the scheduler's own.
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: TICKS counts 1 ms ticks: far below u64::MAX
pub fn timer_tick_fast() -> bool {
    let h = hart();
    crate::timer::next_tick();
    // Hart 0 keeps the clock (sleepers' deadlines are in its ticks); every hart runs its own timers and slice from it.
    let now = if h == 0 {
        TICKS.fetch_add(1, Ordering::SeqCst) + 1
    } else {
        TICKS.load(Ordering::SeqCst)
    };
    #[cfg(debug_assertions)]
    starvation_watchdog(h, now);
    let (pokes, switch) = {
        let sc = sched();
        sc.tick_on(h, ticks_to_ns(now));
        let switch = sc.current_on(h) != running_here();
        if !switch {
            sc.switched(h); // the designation came back to the running thread: nothing is in transit
        }
        (sc.take_pokes(), switch)
    };
    send_pokes(pokes, h);
    switch
}

/// Ticks (1 ms) to the scheduler's nanoseconds; "never" (`u64::MAX`) stays never.
fn ticks_to_ns(t: u64) -> u64 {
    t.saturating_mul(MS)
}

/// Ticks since the timer started (1 ms each).
pub fn ticks() -> u64 {
    TICKS.load(Ordering::SeqCst)
}

pub fn current() -> Tid {
    // (interrupts off so the thread cannot be moved to another hart between reading `tp` and the record; no lock)
    let was = irq::save_and_disable();
    let t = running_here();
    irq::restore(was);
    t
}

/// Give up the rest of the time slice.
#[allow(clippy::expect_used)] // INVARIANT: the caller is a running thread, which the scheduler always has on its hart
pub fn yield_now() {
    irq::without(|| {
        sched().yield_on(hart()).expect("running thread");
        reschedule();
    });
}

/// About to wait (SCHED-SMP-SPEC 3.7 step 1): from now on a wake counts. Call it before registering anywhere; end the
/// wait with `block` (which returns at once if a wake arrived meanwhile).
#[allow(clippy::expect_used)] // INVARIANT: the caller is a running thread, which the scheduler always has on its hart
pub fn prepare_block() {
    irq::without(|| {
        sched().prepare_block_on(hart()).expect("running thread");
    });
}

/// The park word of a thread (a test hook).
pub fn park_state(tid: Tid) -> Option<kcore::park::ParkState> {
    irq::without(|| sched().park_state(tid))
}

/// Block until woken (or return at once if a wake already arrived after `prepare_block`).
#[allow(clippy::expect_used)] // INVARIANT: the caller is a running thread, which the scheduler always has on its hart
pub fn block() {
    irq::without(|| {
        if sched().block_on(hart()).expect("running thread") {
            reschedule();
        }
    });
}

/// Wake a thread: a blocked one runs again; one that has prepared to wait will not block; otherwise nothing happens
/// (a wake is never remembered).
pub fn wake(tid: Tid) {
    irq::without(|| {
        let _ = sched().wake_on(hart(), tid);
        note_resched();
    });
}

/// Sleep for `n` ticks (or until woken).
#[allow(clippy::arithmetic_side_effects, clippy::expect_used)] // INVARIANT: ticks() + n: ticks far below u64::MAX and n is a kernel-chosen duration; the caller is a running thread
pub fn sleep_ticks(n: u64) {
    let deadline = ticks() + n;
    // A loop: a stale entry on a wait queue can wake a sleeper early, and the sleep must not
    // end before its deadline.
    loop {
        let done = irq::without(|| {
            if ticks() >= deadline {
                return true;
            }
            if sched()
                .sleep_until_on(hart(), ticks_to_ns(deadline))
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

/// A thread's current priority (its base: nothing donates yet).
pub fn priority_of(tid: Tid) -> Option<Prio> {
    irq::without(|| sched().priority(tid))
}

/// Set several threads' priorities in one critical section and reschedule once (a clamp must reach every thread
/// before anything can run on the new, lower limit; review of task 8).
pub fn set_priorities(changes: &[(Tid, Prio)]) {
    irq::without(|| {
        for &(tid, prio) in changes {
            let _ = sched().set_priority_on(hart(), tid, prio);
        }
        note_resched();
    });
}

pub fn set_priority(tid: Tid, prio: Prio) {
    irq::without(|| {
        let _ = sched().set_priority_on(hart(), tid, prio);
        note_resched();
    });
}

/// End the calling thread. Its stack is freed by whoever `join`s it, or, if it is detached, by the hart that switches away from it
/// (`finish_switch`); `join` of a detached thread therefore returns false.
#[allow(clippy::expect_used, clippy::unreachable)] // INVARIANT: the caller is a running thread; an exited thread is never scheduled again
pub fn exit() -> ! {
    irq::without(|| {
        let k = kernel();
        let h = hart();
        let Kernel { exits, .. } = k;
        {
            let sc = sched();
            sc.exit_on(h).expect("running thread");
            exits.wake_all_on(sc, h);
            // (a thread nobody joins is freed by the hart that switches away from it: `finish_switch`)
            // a joiner just woken on this hart marked it (deferred mode): the hart makes its own designation right before the
            // switch, or the mark would stay set with no flag and no poke owed (PR 263 review)
            sc.reschedule_on(h);
        }
        reschedule();
    });
    unreachable!("an exited thread was scheduled again");
}

/// Wait for `tid` to exit, then free its stack and record. Returns false, without
/// waiting, if there is nothing to join: the thread is the caller itself, unknown, or
/// already joined by someone else (a thread can be joined once).
#[allow(clippy::expect_used)] // INVARIANT: the thread being reaped has exited (checked a few lines above) and the caller is a running thread
pub fn join(tid: Tid) -> bool {
    loop {
        let result = irq::without(|| {
            let k = kernel();
            if tid == running_here() {
                return Some(false);
            }
            // (the state is read first: a guard in the scrutinee would live through every arm)
            let state = sched().state(tid);
            match state {
                None => Some(false),
                Some(State::Exited) => {
                    sched().reap(tid).expect("exited thread");
                    if let Some(t) = k.threads.remove(&tid) {
                        note_stack_use(&t);
                    }
                    Some(true)
                }
                Some(_) => {
                    let blocked = {
                        let sc = sched();
                        k.exits.wait_on(sc, hart()).expect("running thread")
                    };
                    if blocked {
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
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: queues.len() >= 1 after the push just above
pub fn new_queue() -> usize {
    irq::without(|| {
        let k = kernel();
        k.queues.push(Waiters2::new());
        k.queues.len() - 1
    })
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
#[allow(clippy::expect_used)] // INVARIANT: the caller is a running thread, which the scheduler always has on its hart
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
            if ticks() >= deadline {
                return Some(false);
            }
            // (prepared before registering: the order of 3.7 — under the big lock nothing can interleave, but the
            // protocol is the one the later locks need)
            let _ = sched().prepare_block_on(hart());
            register();
            count_sleep(&mut slept);
            if sched()
                .sleep_until_on(hart(), ticks_to_ns(deadline))
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

/// Wake every thread in `tids` that is blocked, **then** reschedule once (SMP review finding 1, T62): all of
/// them are runnable before any of them can run, so a woken high-priority thread cannot preempt the waker
/// before the next one was woken (which, with a mid-priority hog on the hart, would leave the next one
/// blocked until the waker ran again). A thread that is not blocked is left alone (harmless to waiters that
/// re-check).
pub fn wake_many(tids: &[Tid]) {
    irq::without(|| {
        for &tid in tids {
            if sched().state(tid) == Some(State::Blocked) {
                WAKES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            }
            let _ = sched().wake_on(hart(), tid);
        }
        note_resched();
    });
}

/// Is `tid` blocked right now (a test hook)?
pub fn is_blocked(tid: Tid) -> bool {
    irq::without(|| sched().state(tid) == Some(State::Blocked))
}

/// Block on queue `q` until `ready()` holds. `ready` is evaluated with interrupts
/// off, so checking and queueing are one atomic step with respect to wakers: this is
/// the "no lost wakeup" path.
#[allow(clippy::expect_used, clippy::indexing_slicing)] // INVARIANT: q is a queue id `new_queue` returned; the caller is a running thread
pub fn wait_until(q: usize, mut ready: impl FnMut() -> bool) {
    let mut slept = false;
    loop {
        let go = irq::without(|| {
            if ready() {
                return true;
            }
            count_sleep(&mut slept);
            let k = kernel();
            let blocked = {
                let sc = sched();
                k.queues[q].wait_on(sc, hart()).expect("running thread")
            };
            if blocked {
                reschedule();
            }
            false
        });
        if go {
            return;
        }
    }
}

#[allow(clippy::indexing_slicing)] // INVARIANT: q is a queue id `new_queue` returned
pub fn wake_one(q: usize) -> bool {
    irq::without(|| {
        let k = kernel();
        let woke = {
            let sc = sched();
            k.queues[q].wake_one_on(sc, hart()).is_some()
        };
        note_resched();
        woke
    })
}

#[allow(clippy::indexing_slicing)] // INVARIANT: q is a queue id `new_queue` returned
pub fn wake_all(q: usize) {
    irq::without(|| {
        let k = kernel();
        let n = {
            let sc = sched();
            k.queues[q].wake_all_on(sc, hart())
        };
        WAKES.fetch_add(n as u64, core::sync::atomic::Ordering::Relaxed);
        note_resched();
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
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: the sums are on `which` in the arms that match a range (16..=23 and the like) and on a hart index below the hart count
pub fn wake_stat(which: u64) -> u64 {
    use core::sync::atomic::Ordering::Relaxed;
    match which {
        0 => SLEEPS.load(Relaxed),
        1 => RESLEEPS.load(Relaxed),
        2 => WAKES.load(Relaxed),
        3..=5 => crate::aia::stat(which),
        // 6: the `time` counter (10 MHz on QEMU `virt`), so a benchmark need not depend on user-mode counter access
        // or on the 1 ms tick; 7: 1 if the kernel command line has `bench=full` (SCHED-SMP-SPEC 6.1)
        6 => crate::timer::now(),
        // 8: the longest interrupts-off span seen (cycles; 0 unless built with `latency-trace`)
        8 => crate::latency::irq_off_max().0,
        7 => u64::from(crate::dtb::boot_arg_present("bench=full")),
        // 9: 1 if the kernel command line has `net=lossy` (the harness runs the guest behind the lossy relay, S9-SPEC Q30)
        9 => u64::from(crate::dtb::boot_arg_present("net=lossy")),
        // 13: which part of the suite the kernel line asks for: 0 all of it, 1 `shard=a` (the larger part), 2
        // `shard=b` (the network tests and the heaviest stress tests; the list is in the suite), 3 `shard=k` (the kernel-only boot: the Kernel tests, `init/ktests.txt`); CI runs the two as parallel jobs (TESTING.md, OD121)
        13 => {
            if crate::dtb::boot_arg_present("shard=a") {
                1
            } else if crate::dtb::boot_arg_present("shard=b") {
                2
            } else if crate::dtb::boot_arg_present("shard=k") {
                // 3: the kernel-only boot; 4: the same without the tests marked `slow` (`kfast`)
                if crate::dtb::boot_arg_present("kfast") {
                    4
                } else {
                    3
                }
            } else {
                0
            }
        }
        // 10 to 12: scheduler statistics (pulls, settles, marks)
        10..=12 => sched_stat(which),
        // 16 + h: how often hart h took the big kernel lock from free
        16..=23 => crate::percpu::hart((which - 16) as usize)
            .bkl_takes
            .load(Relaxed),
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

#[allow(clippy::arithmetic_side_effects)] // INVARIANT: the heap's total free bytes are at least its grown bytes
pub fn heap_free() -> usize {
    // (growth at a safe point adds free bytes that are not a leak)
    heap::free_bytes() - heap::grown_bytes()
}

pub fn thread_count() -> usize {
    irq::without(|| kernel().threads.len())
}

/// How many threads the scheduler core tracks (must equal `thread_count`).
pub fn sched_thread_count() -> usize {
    irq::without(|| sched().thread_count())
}

/// Test hook: damage (or repair) a thread's stack guard, to show the overflow
/// detector sees it.
#[allow(clippy::indexing_slicing)] // INVARIANT: a test hook: the thread's stack has at least one word
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
    // (the names the tests and the boot banner know; the classes' own names are `fixed-prio` and `round-robin`)
    if cfg!(feature = "sched-round-robin") {
        "round-robin"
    } else {
        "fixed-priority"
    }
}

/// The top of the calling thread's kernel stack (where a trap from user mode starts).
#[allow(clippy::expect_used)] // INVARIANT: the running thread is in the kernel's table, with a stack unless it is the boot thread
pub fn current_stack_top() -> u64 {
    irq::without(|| {
        let k = kernel();
        let t = k
            .threads
            .get(&running_here())
            .expect("running thread exists");
        let stack = t
            .stack
            .as_ref()
            .expect("a user thread has its own kernel stack");
        (stack.top() & !15) as u64
    })
}

/// The deepest any kernel stack has been, in bytes (a measurement for slice T: stacks start zeroed, so
/// the lowest non-zero word past the guard is the deepest write; a stack that held zeros there reads as
/// shallower, so this is a lower bound, close in practice).
static STACK_HIGH_WATER: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

#[allow(clippy::arithmetic_side_effects)] // INVARIANT: first < the stack's words (it is the index of a changed word)
fn note_stack_use(t: &KThread) {
    if let Some(stack) = t.stack.as_ref() {
        let used = stack
            .iter()
            .skip(GUARD_WORDS)
            .position(|&w| w != 0)
            .map_or(0, |first| STACK_BYTES - (GUARD_WORDS + first) * 8);
        STACK_HIGH_WATER.fetch_max(used, core::sync::atomic::Ordering::Relaxed);
    }
}

/// The deepest kernel stack use seen so far among threads that have ended.
pub fn stack_high_water() -> usize {
    STACK_HIGH_WATER.load(core::sync::atomic::Ordering::Relaxed)
}

/// The scheduler's view of a thread (diagnostics for the SMP tests).
pub fn debug_state(tid: Tid) -> alloc::string::String {
    irq::without(|| {
        let sc = sched();
        alloc::format!(
            "{:?} prio {:?} aff {:?} cur {:?} pending {:?}",
            sc.state(tid),
            sc.priority(tid),
            sc.affinity(tid),
            (0..sc.harts())
                .map(|h| Some(sc.current_on(h)))
                .collect::<Vec<_>>(),
            (0..sc.harts())
                .map(|h| sc.resched_pending(h))
                .collect::<Vec<_>>()
        )
    })
}

/// Scheduler statistics for the benchmarks: 10 pulls, 11 calls that make a hart's designation right, 12 marks.
fn sched_stat(which: u64) -> u64 {
    let sc = sched();
    match which {
        10 => sc.pulls(),
        11 => sc.settles(),
        _ => sc.marks(),
    }
}
