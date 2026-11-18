//! The wake / IPI / idle protocol of the kernel (work item T38, T65; SCHED-SMP-SPEC section 3.9, task 4): a model
//! of two harts and a waker over the real `kcore::sched_smp::Sched2` (the scheduler the kernel runs since task 11), explored by random schedules (shuttle). The
//! kernel's glue (`send_pokes`, the IPI handler, `join_scheduler`, the idle `wfi`) cannot run on the host, so
//! it is written out here once, with the protocol the spec requires and, as **negative controls**, the three
//! ways it can go wrong; each wrong variant must be caught (a runnable thread stranded while its hart idles with
//! no IPI pending, the invariant ID1) and the right one must pass every explored schedule.
//!
//! Run by `make os-test` with `RUSTFLAGS="--cfg shuttle"`; without the cfg this file is empty.
#![cfg(shuttle)]

use kcore::sched2::{FixedPrio, SchedAttr};
use kcore::sched_smp::Sched2;
use shuttle::sync::atomic::{AtomicBool, Ordering};
use shuttle::sync::{Arc, Mutex};
use shuttle::thread;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Variant {
    /// The protocol as specified: a hart that joins late reschedules itself, a hart that is not in the scheduler is never
    /// marked (nothing is owed to it), the IPI handler makes its hart's designation right.
    Right,
    /// Negative control 1: `join_scheduler` does not end with a reschedule (the T65 bug): an IPI that arrived
    /// before the hart was ready is lost.
    NoJoinReschedule,
    /// Negative control 2: the sender drops every poke (a lost reschedule IPI).
    DropPokes,
    /// Negative control 3: the IPI handler does not look at the run queues.
    HandlerIgnoresIpi,
}

struct World {
    sched: Mutex<Sched2<FixedPrio>>,
    ipi: [AtomicBool; 2],
    online: [AtomicBool; 2],
    in_sched: [AtomicBool; 2],
    ran: AtomicBool,
    stop: AtomicBool,
    v: Variant,
}

/// What `send_pokes` does in the kernel: an IPI to every poked hart that is up.
fn send_pokes(w: &World, mut pokes: u32, me: usize) {
    pokes &= !(1 << me);
    while pokes != 0 {
        let i = pokes.trailing_zeros() as usize;
        pokes &= pokes - 1;
        if w.v == Variant::DropPokes {
            continue;
        }
        if w.online[i].load(Ordering::Acquire) {
            w.ipi[i].store(true, Ordering::SeqCst);
        }
    }
}

/// A hart's trap-handler work: make its designation right, switch to what it picks, "run" it.
fn look(w: &World, h: usize, idle: u32) {
    let mut s = w.sched.lock().unwrap();
    s.reschedule_on(h);
    let pokes = s.take_pokes();
    send_pokes(w, pokes, h);
    loop {
        let c = s.current_on(h);
        if c == idle {
            break;
        }
        // the thread runs to completion at once (what matters is that it was picked)
        w.ran.store(true, Ordering::SeqCst);
        s.exit_on(h).unwrap();
        s.reschedule_on(h);
    }
}

fn hart_main(w: Arc<World>, h: usize) {
    // join: the boot context becomes the hart's idle thread, pinned to it
    let idle;
    {
        let mut s = w.sched.lock().unwrap();
        idle = s.idle(h);
        // "online" (it may receive IPIs) comes before "in the scheduler" (its handler acts on them, and the scheduler
        // marks it)
        w.online[h].store(true, Ordering::Release);
        thread::yield_now();
        w.in_sched[h].store(true, Ordering::Release);
        s.set_online(h, true);
        if w.v != Variant::NoJoinReschedule {
            drop(s);
            look(&w, h, idle); // the fix: end the join with a reschedule
        }
    }
    // the idle loop: `wfi` returns when an interrupt (here the IPI flag) is pending
    loop {
        if w.stop.load(Ordering::SeqCst) {
            return;
        }
        if w.ipi[h].swap(false, Ordering::SeqCst) {
            // the IPI handler (it acts only on a hart that is part of the scheduler, as trap.rs does)
            if w.in_sched[h].load(Ordering::Acquire) && w.v != Variant::HandlerIgnoresIpi {
                look(&w, h, idle);
            }
        } else {
            thread::yield_now();
        }
    }
}

fn model(v: Variant) {
    let w = Arc::new(World {
        sched: Mutex::new({
            // the boot hart is part of the scheduler from the start; the second joins in `hart_main`
            let mut s = Sched2::new(2, FixedPrio::new);
            s.set_online(1, false);
            s
        }),
        ipi: [AtomicBool::new(false), AtomicBool::new(false)],
        online: [AtomicBool::new(false), AtomicBool::new(false)],
        in_sched: [AtomicBool::new(false), AtomicBool::new(false)],
        ran: AtomicBool::new(false),
        stop: AtomicBool::new(false),
        v,
    });
    // the boot hart is up first (it has to be: it creates the scheduler); the second joins concurrently
    let h0 = {
        let w = w.clone();
        thread::spawn(move || hart_main(w, 0))
    };
    let h1 = {
        let w = w.clone();
        thread::spawn(move || hart_main(w, 1))
    };
    // a thread pinned to hart 1 is created by hart 0's code (so only hart 1 can run it) at a random moment
    // relative to hart 1 coming up
    thread::yield_now();
    {
        let mut s = w.sched.lock().unwrap();
        s.spawn_on(
            0,
            SchedAttr {
                prio: 10,
                rr: false,
            },
            Some(1),
        )
        .unwrap();
        let pokes = s.take_pokes();
        send_pokes(&w, pokes, 0);
    }
    // let the harts settle: ID1 says the thread runs without any further event (no tick is modelled)
    for _ in 0..200 {
        if w.ran.load(Ordering::SeqCst) {
            break;
        }
        thread::yield_now();
    }
    let ran = w.ran.load(Ordering::SeqCst);
    w.stop.store(true, Ordering::SeqCst);
    h0.join().unwrap();
    h1.join().unwrap();
    assert!(
        ran,
        "ID1: a runnable thread was left queued while its hart idled with no IPI pending ({v:?})"
    );
}

#[test]
fn the_protocol_never_strands_a_runnable_thread() {
    shuttle::check_random(|| model(Variant::Right), 3000);
}

#[test]
#[should_panic(expected = "ID1")]
fn control_join_without_a_final_reschedule_strands_a_thread() {
    shuttle::check_random(|| model(Variant::NoJoinReschedule), 3000);
}

#[test]
#[should_panic(expected = "ID1")]
fn control_dropped_pokes_strand_a_thread() {
    shuttle::check_random(|| model(Variant::DropPokes), 3000);
}

#[test]
#[should_panic(expected = "ID1")]
fn control_an_ipi_handler_that_ignores_the_ipi_strands_a_thread() {
    shuttle::check_random(|| model(Variant::HandlerIgnoresIpi), 3000);
}
