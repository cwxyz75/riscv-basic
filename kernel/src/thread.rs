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

use crate::arch::Context;
use crate::{heap, irq};
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
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

struct KThread {
    ctx: Box<Context>,
    /// `None` for the boot thread, which runs on the boot stack.
    stack: Option<crate::kstack::KStack>,
    name: &'static str,
    /// Nobody will `join` this thread: when it exits, the hart that switches away from it frees it (`finish_switch`).
    detached: bool,
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
#[allow(clippy::large_stack_frames)]
// KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
#[allow(clippy::expect_used)]
// INVARIANT: boot code: the boot thread's priority is valid and the idle and boot records are created once
pub fn init(boot_prio: Prio) {
    // The kernel does no floating point and thread switches do not save FP state:
    // keep the FPU off (sstatus.FS = 0) so any accidental use traps instead of
    // silently corrupting another thread's registers.
    // The same for the vector unit (sstatus.VS, bits 10:9; mandatory in RVA23, OD47): no vector
    // state is saved per thread, so user mode must not be able to use it either.
    // SAFETY: boot, before any user thread has run.
    unsafe { crate::arch::lazy_units_off() };
    let harts = crate::smp::started();
    let sched = KSched::over(KRqs {
        rq: (0..harts)
            .map(|h| klock::Spin::new(klock::Class::Rq, h as u32, HartRq::new(new_class())))
            .collect(),
        table: klock::Spin::new(klock::Class::Tt, 0, Table::new()),
    })
    .with_clock(crate::timer::now_ns); // (charge the running thread at every settle: no tick does it)
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
    if let Some(n) = crate::dtb::ff_value("tid") {
        sched.fast_forward_tid(n); // (AB3, test builds)
    }
    let idle0 = sched.idle(0);
    let mut threads = BTreeMap::new();
    threads.insert(
        boot,
        KThread {
            ctx: Box::default(),
            stack: None,
            name: "boot",
            detached: false,
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
        sched().set_clock(h, now_ns());
        sched().set_online(h, true);
        k.threads.insert(
            tid,
            KThread {
                ctx: Box::default(),
                stack: None,
                name: "idle",
                detached: false,
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
/// A hart whose next timer event is further away than this (or has none) suspends through the firmware instead of `wfi` (PM2).
const SUSPEND_AFTER: u64 = MS;

fn idle_main(_: usize) {
    loop {
        IDLE_SPINS.fetch_add(1, Ordering::Relaxed);
        let was = irq::save_and_disable();
        // (negative control `fault-skip-poke`: the idle hart's only wake is a poke, which that kernel drops; a tick must not rescue it)
        if !cfg!(feature = "fault-skip-poke") && sched().idle_has_work(hart()) {
            irq::restore(was);
            irq::without(reschedule_this_hart);
            continue;
        }
        if crate::heap::needs_service() {
            irq::without(crate::heap::service); // an idle hart is a safe point too
        }
        let ev = program_timer(); // (an idle hart with nothing queued and no sleeper arms nothing: no timer interrupt at all)
        let idle_from = now_ns();
        let me = crate::percpu::this();
        // A long idle (the next event more than `SUSPEND_AFTER` away, or none) goes through the firmware's retentive suspend; a short
        // one, or firmware without it, uses `wfi` (PM2, KERNEL-V1-SPEC 5.5). Both return when an interrupt is pending, with interrupts
        // off, which is the point of the protocol above (the architecture resumes on any pending and individually enabled interrupt).
        if ev.is_none_or(|e| e > idle_from.saturating_add(SUSPEND_AFTER))
            && crate::smp::suspend_retentive()
        {
            me.idle_suspend.fetch_add(1, Ordering::Relaxed);
        } else {
            me.idle_wfi.fetch_add(1, Ordering::Relaxed);
            crate::arch::cpu_idle();
        }
        me.idle_ns
            .fetch_add(now_ns().saturating_sub(idle_from), Ordering::Relaxed);
        irq::restore(was);
    }
}

/// First Rust code of every new thread (called from the assembly trampoline).
#[no_mangle]
extern "C" fn thread_bootstrap(entry: usize, arg: usize) -> ! {
    // A new thread starts inside a context switch, with interrupts off and the big kernel lock
    // released by the thread that switched here (G4): it takes it for its own finish_switch and then runs outside it (K4).
    crate::latency::switch_in();
    // (the lock was released by the thread that switched here: G4)
    switch_done();
    crate::bkl::lock();
    reap_prev();
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

/// Is there room to create a user thread (CNT1)? Checked before the creation commits anything; the margin is far above the number of
/// creations that can be in flight between the check and the spawn (one per hart).
pub fn user_thread_ids_left() -> bool {
    sched().tid_headroom(4096)
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
    let ctx = Context::new(entry, arg, top);
    KThread {
        ctx,
        stack: Some(stack),
        name,
        detached,
    }
}

/// This hart's context switch is complete (the thread it left has been saved), or it decided not to switch: the thread it
/// left, if any, is now available to the other harts (`Sched2::switched`). Interrupts are off.
fn finish_switch() {
    switch_done();
    reap_prev();
}

/// The switch on this hart is complete: the thread it left is available to every hart, and the pokes that owes are sent. Needs no
/// lock but the scheduler's own (interrupts off), so the resumed thread runs it before it waits for the big kernel lock.
fn switch_done() {
    let h = hart();
    let pokes = {
        let sc = sched();
        sc.switched(h);
        sc.take_pokes()
    };
    send_pokes(pokes, h);
    program_timer();
    // a mark that is still set (a wake that came after this hart's last settle) must not be left without its flag
    if sched().resched_pending(h) {
        crate::percpu::this()
            .need_resched
            .store(true, Ordering::Relaxed);
    }
}

/// The thread this hart replaced, if it has exited and nobody will join it, is dead now: its context is saved, so its stack is in
/// nobody's use (SCHED-SMP-SPEC 3.6: `Dead`, then reap by the hart; the reaper thread this replaced needed a wake and a switch for
/// every thread that ended). The big kernel lock is held, so the table is ours.
fn reap_prev() {
    let prev = crate::percpu::this().prev.swap(0, Ordering::Relaxed);
    if prev != 0 {
        reap_if_dead(prev);
    }
}

/// Free `tid` if it is an exited detached thread (a joined one is freed by its joiner). The caller has completed the switch away
/// from it and holds the big kernel lock.
fn reap_if_dead(tid: Tid) {
    let k = kernel();
    // (not while it is in transit: `prev` is consumed after the lock is taken again, which can wait, and by then the thread may have
    // resumed on another hart and exited there; the thread that replaces it on that hart reaps it once that switch is done: PR 266 review)
    let dead = k.threads.get(&tid).is_some_and(|t| t.detached)
        && sched().state(tid) == Some(State::Exited)
        && !sched().is_in_transit(tid);
    if dead {
        let reaped = sched().reap(tid).is_ok();
        // (it cannot fail under the big lock: a failure would leak the thread's stack, so it is loud in a debug build)
        debug_assert!(reaped, "reap of exited thread {tid} failed");
        if reaped {
            if let Some(t) = k.threads.remove(&tid) {
                note_stack_use(tid, &t);
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
    program_timer(); // (a wake or an enqueue here can make a slice end an event)
}

/// Is a switch asked for on this hart (a wake marked it)? Lock-free: the per-hart flag.
pub fn need_resched_pending() -> bool {
    crate::percpu::this().need_resched.load(Ordering::Relaxed)
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
    crate::chaos::point(crate::chaos::Point::Schedule);
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
        (old, new)
    };
    // A hart must never keep a user process's page table in `satp` across a switch: another hart
    // may tear that process down and free (and reuse) the root while this one is idle or running
    // a thread of some other process. Back to the kernel's own table; the return to user mode
    // loads the right one (`user::activate_current`).
    crate::kmem::use_kernel_table_if_needed();
    crate::latency::switch_out();
    // The lock does not follow the thread (SCHED-SMP-SPEC G4): this thread's depth stays on its stack, the lock is released here, and
    // the thread takes it back when it resumes. The thread it leaves is in transit (`Sched2::displaced`: any state) until the
    // incoming thread's `switch_done`, so no other hart can wake-and-run it, pull it or reap it before its context is saved.
    let depth = crate::bkl::release_all();
    // SAFETY: `old` and `new` point into boxed contexts that stay alive and in
    // place (threads are only reaped once exited and switched away from); the
    // switch saves this thread's state and resumes the other's, as `context_switch`
    // documents. Interrupts are off, the big kernel lock was released just above, and no
    // reference to KERNEL is live.
    unsafe { crate::arch::switch(old, new) };
    // (resumed: on whichever hart now runs this thread, the switch that brought it here has finished saving the one before)
    switch_done();
    crate::bkl::reacquire(depth);
    reap_prev();
    crate::latency::switch_in();
}

/// Switch threads if the scheduler core has designated another one for this hart (the caller holds the big kernel lock).
pub fn switch_if_designated() {
    reschedule();
}

/// Send a reschedule IPI to every hart in the bitmask (by dense index) except `me`.
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: pokes is a non-zero mask of harts (the loop condition), so pokes - 1 cannot underflow
fn send_pokes(mut pokes: u64, me: usize) {
    #[cfg(feature = "fault-skip-poke")]
    {
        let _ = (&mut pokes, me);
        return; // negative control (L11d): every poke is lost
    }
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
    program_timer();
    switch
}

/// The starvation watchdog (SCHED-SMP-SPEC 3.9; debug builds), now a timer entry (L13): a hart with queued work arms an event every
/// 100 ms (`program_timer`), and at each timer interrupt after that long a thread queued for more than 500 ms while a hart that may
/// run it idles is a lost wake, not a slow system: the report is a panic (`##PANIC starved thread`). The same thread must be found in two
/// samples in a row: the check reads other harts' idleness from hints at one instant, and a thread preempted after a long run is queued,
/// "late" and briefly in front of an idle hart that is on its way to pull it (PR 264 review). An idle hart with nothing queued
/// arms nothing: it takes no timer interrupt.
#[cfg(debug_assertions)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::panic,
    clippy::indexing_slicing
)]
// INVARIANT: h is a hart index below MAX_HARTS; the panic is the detector's report
// KT13: audited, a frame over 1 KiB on a path that ends in a panic (the report holds a thread record)
#[allow(clippy::large_stack_frames)]
fn starvation_watchdog(h: usize, now: u64) {
    static LAST: [core::sync::atomic::AtomicU64; crate::percpu::MAX_HARTS] =
        [const { core::sync::atomic::AtomicU64::new(0) }; crate::percpu::MAX_HARTS];
    static REPEATS: [core::sync::atomic::AtomicU32; crate::percpu::MAX_HARTS] =
        [const { core::sync::atomic::AtomicU32::new(0) }; crate::percpu::MAX_HARTS];
    if now.saturating_sub(WD_AT[h % crate::percpu::MAX_HARTS].load(Ordering::Relaxed))
        >= WATCHDOG_PERIOD
    {
        WD_AT[h % crate::percpu::MAX_HARTS].store(now, Ordering::Relaxed);
        let found = sched().starved(now, 500 * MS);
        let tid = found.map_or(0, |f| f.0);
        let before = LAST[h % crate::percpu::MAX_HARTS].swap(tid, Ordering::Relaxed);
        // (the same thread in three samples in a row: on a loaded host the emulated machine is slow, and a thread queued behind a few
        // thousand others while a hart is between two pulls would trip a shorter rule)
        let repeats = if before == tid && tid != 0 {
            REPEATS[h % crate::percpu::MAX_HARTS].fetch_add(1, Ordering::Relaxed) + 1
        } else {
            REPEATS[h % crate::percpu::MAX_HARTS].store(0, Ordering::Relaxed);
            0
        };
        if let Some((t, on, idle)) = found {
            if repeats >= 2 {
                let sc = sched();
                let harts: alloc::vec::Vec<_> = (0..crate::smp::started())
                    .map(|i| {
                        (
                            sc.current_on(i),
                            sc.queued_on(i),
                            sc.resched_pending(i),
                            sc.in_transit(i),
                            sc.next_event_on(i),
                        )
                    })
                    .collect();
                panic!("starved thread {t}: queued on hart {on} for more than 500 ms while hart {idle} idles (per hart: current, queued, marked, in transit, next event: {harts:?}; the thread: {:?})", sc.record(t));
            }
        }
    }
}

#[cfg(debug_assertions)]
const WATCHDOG_PERIOD: u64 = 100 * MS;
#[cfg(debug_assertions)]
static WD_AT: [AtomicU64; crate::percpu::MAX_HARTS] =
    [const { AtomicU64::new(0) }; crate::percpu::MAX_HARTS];

/// Arm this hart's one-shot timer (L13: there is no periodic tick) for the earliest of its scheduler's next event (a sleeper's hard
/// expiry, a slice end when an equal peer waits) and, in debug builds while the hart has queued work, the next watchdog sample. Called
/// wherever the answer can change: after a timer interrupt, at the end of a switch, after a wake or enqueue here, before idling.
/// Interrupts off.
pub fn program_timer() -> Option<u64> {
    let h = hart();
    #[allow(unused_mut)] // (`ev` is changed only in debug builds)
    let mut ev = sched().next_event_on(h);
    #[cfg(debug_assertions)]
    if sched().queued_on(h) > 0 {
        let at = WD_AT
            .get(h % crate::percpu::MAX_HARTS)
            .map_or(0, |a| a.load(Ordering::Relaxed))
            .saturating_add(WATCHDOG_PERIOD);
        ev = Some(ev.map_or(at, |e| e.min(at)));
    }
    crate::timer::program(ev);
    ev
}

/// The timer interrupt's scheduler work **without the big kernel lock** (SCHED-SMP-SPEC task 12, 13): read the clock, expire the due
/// timers, account the running thread's slice, make the designation right, arm the next event. Returns whether the hart must now switch
/// threads (a rotation, a timer wake that outranks the running thread, a mark served): the caller then takes the big lock and calls
/// `reschedule`. An interrupt that changes nothing needs no lock but the scheduler's own.
pub fn timer_interrupt() -> bool {
    let h = hart();
    let now = now_ns();
    #[cfg(debug_assertions)]
    starvation_watchdog(h, now);
    let (pokes, switch) = {
        let sc = sched();
        sc.tick_on(h, now);
        let switch = sc.current_on(h) != running_here();
        if !switch {
            sc.switched(h); // the designation came back to the running thread: nothing is in transit
        }
        (sc.take_pokes(), switch)
    };
    send_pokes(pokes, h);
    program_timer();
    switch
}

/// Ticks (1 ms) to the scheduler's nanoseconds; "never" (`u64::MAX`) stays never.
fn ticks_to_ns(t: u64) -> u64 {
    t.saturating_mul(MS)
}

/// Nanoseconds since boot (the clock user mode sees; AB2).
pub fn now_ns() -> u64 {
    crate::timer::now_ns()
}

/// Milliseconds since boot (what a 1 ms tick count used to be: the clock rounded down).
pub fn ticks() -> u64 {
    now_ns() / MS
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

/// `yield_now` for a trap from user mode without the big lock (L12b/L12c): the scheduler's own locks make the designation; the
/// big lock is needed only to switch. Returns whether the hart must now switch (the caller then takes the lock and calls
/// `switch_if_designated`). Interrupts off.
#[allow(clippy::expect_used)] // INVARIANT: the caller is a running thread, which the scheduler always has on its hart
pub fn yield_fast() -> bool {
    let h = hart();
    let (pokes, switch) = {
        let sc = sched();
        sc.yield_on(h).expect("running thread");
        let switch = sc.current_on(h) != running_here();
        if !switch {
            sc.switched(h); // nothing in transit when the thread stays
        }
        (sc.take_pokes(), switch)
    };
    send_pokes(pokes, h);
    program_timer();
    switch
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
    // (MM8) a page the call made visible or replaced may be shown stale through other mappings until the call ends; a thread that sleeps
    // inside it would leave them so for as long as it sleeps
    crate::user::cow_sync();
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

/// Sleep for `ns` nanoseconds (or until woken): the same as `sleep_ticks`, at the clock's own resolution.
#[allow(clippy::expect_used)] // INVARIANT: the caller is a running thread, which the scheduler always has on its hart
pub fn sleep_ns(ns: u64) {
    let deadline = now_ns().saturating_add(ns);
    loop {
        let done = irq::without(|| {
            if now_ns() >= deadline {
                return true;
            }
            if sched()
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

/// Sleep for `n` ticks (or until woken).
#[allow(clippy::arithmetic_side_effects, clippy::expect_used)] // INVARIANT: ticks() + n: ticks far below u64::MAX and n is a kernel-chosen duration; the caller is a running thread
pub fn sleep_ticks(n: u64) {
    let deadline = now_ns().saturating_add(ticks_to_ns(n));
    // A loop: a stale entry on a wait queue can wake a sleeper early, and the sleep must not
    // end before its deadline.
    loop {
        let done = irq::without(|| {
            if now_ns() >= deadline {
                return true;
            }
            if sched()
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
        // negative control (L11d): the stack is freed while this thread still runs on it. The bug only bites if another thread takes the
        // freed slot before this one has switched away, which is a race; the control makes it certain: the slot is taken at once and its
        // new owner writes to it (soak, 2026-11-21: the race alone no longer lost often enough for `teardown_with_threads_on_other_harts`
        // to notice)
        #[cfg(feature = "fault-early-reap")]
        {
            // (`reap_if_dead` itself refuses a thread that is in transit, which is the guard this control removes: reap directly)
            let me = running_here();
            if kernel().threads.get(&me).is_some_and(|t| t.detached) && sched().reap(me).is_ok() {
                if let Some(t) = kernel().threads.remove(&me) {
                    note_stack_use(me, &t);
                }
                if let Some(mut stolen) = crate::kstack::alloc() {
                    stolen.fill(0xdead_dead_dead_dead);
                }
            }
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
                // (an exited thread whose switch is not yet complete, G4, is still on its stack: look again)
                Some(State::Exited) if sched().is_in_transit(tid) => None,
                Some(State::Exited) => {
                    sched().reap(tid).expect("exited thread");
                    if let Some(t) = k.threads.remove(&tid) {
                        note_stack_use(tid, &t);
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

/// Sleep until `ready()` holds or `deadline` (absolute nanoseconds on `now_ns`'s clock), **not queued anywhere**: only
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
            if now_ns() >= deadline {
                return Some(false);
            }
            // (prepared before registering: the order of 3.7 — under the big lock nothing can interleave, but the
            // protocol is the one the later locks need)
            let _ = sched().prepare_block_on(hart());
            register();
            count_sleep(&mut slept);
            if sched()
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
        // 6: the `time` counter (its rate is selector 105), so a benchmark need not depend on user-mode counter access
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
        // 100 and 101: the lowest byte address of RAM and the address after the highest (from the device tree, PL4); 102 and 103: the
        // frames of RAM the tree describes and the frames the allocator manages; 104: where the kernel image starts
        // 110 and 111: executable pages mapped by faults, and the times the instruction caches were synchronised for them (PL7)
        110 => crate::smp::icache_pages(),
        111 => crate::smp::icache_syncs(),
        // 109: the kernel heap's leak oracle (`heap::leak_oracle`, wrapping): taken before and after something, `before - after` is the heap
        // bytes it kept (MM5's acceptance test: what a confined program holds against what its job was charged)
        109 => heap_free() as u64,
        100 => crate::kmem::stats().ram_first << 12,
        101 => crate::kmem::stats().ram_end << 12,
        102 => crate::kmem::stats().memory_frames,
        103 => crate::kmem::stats().managed_frames,
        104 => crate::kmem::stats().image_first << 12,
        // 105: the frequency of the `time` counter (selector 6), in hertz, from the device tree
        105 => crate::timer::timebase(),
        // 107: the first eight bytes the entropy pool produced after it was keyed (test builds; 0 in release), different at every boot; 108: 1 if
        // the pool had a real seed (rng-seed, kaslr-seed or Zkr), 0 if not (then the kernel does not randomise addresses)
        107 => crate::entropy::first_output(),
        108 => u64::from(crate::entropy::seeded()),
        // 106: calls the kernel made to the SBI IPI extension (0: inter-hart interrupts are IMSIC messages, PL6)
        106 => crate::arch::sbi::ipi_calls() as u64,
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
    crate::arch::lazy_units_are_off()
}

/// The leak tests' oracle: equal before and after when nothing was kept (see `heap::leak_oracle`).
pub fn heap_free() -> usize {
    heap::leak_oracle()
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

/// The kernel stack budget (KT13): a stack is 16 KiB, and no thread may go deeper than half of it in a build without optimisation (the
/// debug build the suite mostly runs), so the unmeasured worst case (an interrupt on top of the deepest call chain, a longer path than
/// the suite takes) has as much room again. **Optimised builds are held to 12 KiB for now** (T173): there, inlining makes the path that
/// reaps a dead thread on the next thread's fresh stack (`reap_prev` in `thread_bootstrap`: the stack's pages, the page tables, the run
/// queues) the deepest in the system, 9.4 KiB, and shrinking it is its own item; the budget drops to 8 KiB when T173 is done.
#[cfg(not(kernel_optimized))]
pub const STACK_BUDGET: usize = 8 * 1024;
#[cfg(kernel_optimized)]
pub const STACK_BUDGET: usize = 12 * 1024;

/// The first thread seen over the budget: its stack use, or 0 while none is (the thread's id is in `OVER_BUDGET_TID`; atomics only, so the
/// check at a thread's exit is lock-free).
static OVER_BUDGET: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// That thread's id (a u64 since AB3; written with the name, before `OVER_BUDGET`).
static OVER_BUDGET_TID: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// The first 16 bytes of that thread's name, as two words (written before `OVER_BUDGET`, read after it).
static OVER_BUDGET_NAME: [core::sync::atomic::AtomicU64; 2] = [
    core::sync::atomic::AtomicU64::new(0),
    core::sync::atomic::AtomicU64::new(0),
];

/// What a stack has used: the bytes from the top down to the lowest word that is not zero (stacks start zeroed).
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: first < the stack's words (it is the index of a changed word)
fn stack_used(stack: &[u64]) -> usize {
    stack
        .iter()
        .skip(GUARD_WORDS)
        .position(|&w| w != 0)
        .map_or(0, |first| STACK_BYTES - (GUARD_WORDS + first) * 8)
}

/// Does `used` bytes of stack break the budget?
pub fn over_budget(used: usize) -> bool {
    used > STACK_BUDGET
}

/// The use of the calling thread's own stack so far (a test of the budget check reads it).
pub fn current_stack_use() -> usize {
    irq::without(|| {
        let me = current();
        let k = kernel();
        k.threads
            .get(&me)
            .and_then(|t| t.stack.as_ref())
            .map_or(0, |s| stack_used(s))
    })
}

/// Exempt the calling thread from the budget check when it ends (the test that recurses on purpose).
pub fn exempt_from_stack_budget() {
    EXEMPT.store(
        current().saturating_add(1),
        core::sync::atomic::Ordering::SeqCst,
    );
}
static EXEMPT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

#[allow(clippy::arithmetic_side_effects)] // INVARIANT: i is below 16 (take(16)), so i % 8 * 8 is below 64 and the shift cannot overflow; the byte is shifted into its own lane
fn note_stack_use(tid: Tid, t: &KThread) {
    if let Some(stack) = t.stack.as_ref() {
        let used = stack_used(stack);
        STACK_HIGH_WATER.fetch_max(used, core::sync::atomic::Ordering::Relaxed);
        if over_budget(used)
            && EXEMPT.load(core::sync::atomic::Ordering::SeqCst) != tid.saturating_add(1)
        {
            let mut words = [0u64; 2];
            for (i, b) in t.name.bytes().take(16).enumerate() {
                if let Some(w) = words.get_mut(i / 8) {
                    *w |= u64::from(b) << ((i % 8) * 8);
                }
            }
            for (slot, w) in OVER_BUDGET_NAME.iter().zip(words) {
                slot.store(w, core::sync::atomic::Ordering::SeqCst);
            }
            if OVER_BUDGET.load(core::sync::atomic::Ordering::SeqCst) == 0 {
                OVER_BUDGET_TID.store(tid, core::sync::atomic::Ordering::SeqCst);
                // (a later offender does not overwrite the first's id)
            }
            let _ = OVER_BUDGET.compare_exchange(
                0,
                used as u64,
                core::sync::atomic::Ordering::SeqCst,
                core::sync::atomic::Ordering::SeqCst,
            );
        }
    }
}

/// The deepest kernel stack use seen so far among threads that have ended.
pub fn stack_high_water() -> usize {
    STACK_HIGH_WATER.load(core::sync::atomic::Ordering::Relaxed)
}

/// The first thread that ended over the budget: the start of its name, its id and its use; `None` while every ended thread was
/// inside it.
pub fn stack_budget_violation() -> Option<(alloc::string::String, Tid, usize)> {
    let v = OVER_BUDGET.load(core::sync::atomic::Ordering::SeqCst);
    if v == 0 {
        return None;
    }
    let bytes: Vec<u8> = OVER_BUDGET_NAME
        .iter()
        .flat_map(|w| w.load(core::sync::atomic::Ordering::SeqCst).to_le_bytes())
        .take_while(|&b| b != 0)
        .collect();
    Some((
        alloc::string::String::from_utf8_lossy(&bytes).into_owned(),
        OVER_BUDGET_TID.load(core::sync::atomic::Ordering::SeqCst),
        v as usize,
    ))
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
