# Review: L11b-1, the kernel on per-hart run queues (`Sched2`), branch `l11-sched`

Date: 2026-11-16. Reviewer: Fable (fresh context, independent of the author). Scope: the working tree of the worktree
against `origin/main` (the slice is uncommitted): `kcore/src/sched_smp.rs`, `kcore/src/sched2.rs` (the `pub(crate)`
helpers, `RoundRobin`), `kcore/tests/sched2_smp_model.rs`, `kernel/src/thread.rs` (and the `fault-log` lines in
`kernel/src/user.rs`, which are diagnostics only). Read against SCHED-SMP-SPEC 3.2, 3.5, 3.7, 3.9, 4.1, 4.4,
`INVARIANTS.md` (C4, PK1, PK2, ID1, GP1, AF1) and `SYSCALLS.md` (`thread_set_sched`).

Method: reading, plus three things run outside the repository (a copy of `abi/` and `kcore/` in a scratch directory; no
source file in the worktree was changed):

* a scratch program driving `Sched2` through the scenarios below (the numbers quoted in the findings are its output);
* ten hand mutations of the copy, each run against `sched2_smp_model.rs` (finding 3);
* a second random walk written for this review, driving `Sched2` **the way the kernel does**: harts 1.. start offline
  and join later (`set_clock`, `set_online`, `reschedule_on`), threads are pinned to harts that have not joined, only
  hart 0 moves the clock and each hart ticks alone (lagging clocks), wakes hit any thread (blocked or not), marked
  harts are served in random order with a bound, and the poke contract is checked after every operation. 1 to 8
  harts, 150 seeds, 500 steps each (600 000 operations) with `FixedPrio`.

Nothing was booted in QEMU for this review. The kernel findings are from the code.

Summary: for the default (fixed-priority) kernel I found **no lost wakeup, no stuck thread and no record-migration
bug**. The second walk passes: GP1 holds on the online harts at every quiescent point, marks die out within `harts`
reschedules, no thread is ever homed or run on an offline hart unless pinned there, a pinned thread never moves,
`thread_count` stays consistent, and every mark that is turned on has its poke bit. The kernel glue keeps the rule
"only hart h changes `cur[h]`", and every scheduler call that can change `cur[h]` or set a mark is followed by
`reschedule()` in the same locked region. What is wrong is around that core: the round-robin class does not converge
under push and pull (an IPI storm), every wake pokes every lower hart, and the model test cannot see either, nor a
dropped poke.

No blocker for the fixed-priority kernel. Important: 1, 2, 3, 4. Minor: 5 to 13.

---

## Important

### 1. With the round-robin class, pull and push never converge: an IPI ping-pong, and GP1 cannot hold (important)

`kcore/src/sched_smp.rs:207-231` (`pull`), `234-257` (`settle`), `260-275` (`push_queue`);
`kcore/src/sched2.rs:333-335` (`RoundRobin::preempts` is always false).

`pull` and `push_queue` decide by priority alone and assume that a thread which outranks `cur[h]` will run once it is
on `h`. That is true for `FixedPrio` and `RtClass`. It is false for `RoundRobin`, which reports priorities (`top`,
`top_where`) but never preempts. Scenario, two harts, `Sched2<RoundRobin>`:

1. Hart 0 runs A (normal, priority 5), hart 1 runs B (priority 7). Both are free.
2. T (priority 10, free) becomes runnable. `push_marks` marks the hart that does not hold it.
3. That hart, say 1, runs `settle(1)`: `pull` takes T from hart 0 (10 > max(7, nothing queued)). `wants_resched` is
   false, so T just sits in hart 1's queue. `push_queue(1)` then sees hart 0 running 5 < 10 and marks hart 0.
4. Hart 0 runs `settle(0)`: pulls T back (10 > 5), does not run it, marks hart 1. Go to 3.

Measured in the scratch program: after the spawn of T, 100 000 `reschedule_on` calls and 100 001 pulls with a mark
still pending, and `audit(true)` reports `GP1: hart 0 runs priority 5 while thread 5 (priority 10) waits`. In the
kernel each round is a real IPI and a BKL acquisition on both harts. It ends only when a hart's slice happens to end
while T is in that hart's queue (T is at the back each time, because `attach(.., Enq::Migrated, ..)` pushes to the
back), so each storm lasts on the order of one to a few slices and repeats for every such thread. Reachable in the
`sched-round-robin` kernel at 2 or more harts as soon as free normal-band threads have different priorities (user
threads are `spawn_free`; `thread_set_sched` and `job_set_sched` change priorities). That kernel is booted at 4 harts
in the soak tier.

Nothing tests this: `sched2_smp_model.rs` instantiates only `FixedPrio`; `sched2::RoundRobin` is not constructed by
any test in `kcore/tests/`.

Suggested fix: placement, push and pull are policy of the class, as 4.3 has it (`select_hart` is a class hook; 4.4
says `GlobalPrio` is "used by the RT class and by the normal-band stub"). Smallest change: a hart pulls or is marked
for a queued thread only if that thread would actually displace what runs there, i.e. ask the class
(`would_run_instead(cur, candidate)`: priority comparison for `FixedPrio`/`RtClass`; for `RoundRobin` true only when
the hart is idle or the candidate is in the RT band). State GP1 per class in INVARIANTS (for round robin it is work
conservation: no hart idles while a thread it may run is queued). Run the model walk over both classes and make
`drain` fail when marks do not die out (finding 3).

### 2. Every wake, spawn and preemption marks every lower hart: up to `harts - 1` IPIs for one thread, and placement is undone by whichever hart gets the lock first (important)

`kcore/src/sched_smp.rs:186-204` (`push_marks`), called from `after_enqueue` (345-351), `settle` (250), `yield_on`
(410) and `set_priority_on` (436).

`select_hart` already chose the one hart where the thread should run (rule 3 or 4) or decided it queues (rule 5, in
which case no hart is lower and nothing needs a mark). `push_marks` then marks **every** other online hart whose
current thread is lower. Measured, 8 harts, hart 0 busy, seven idle: one `spawn_on` of a free thread puts it on hart
1 and returns pokes `0b11111110`; a later `wake_on` of the same thread again returns `0b11111110`. Seven IPIs, seven
BKL acquisitions, for one thread. And because every marked hart pulls, the thread runs on whichever hart takes the
lock first, not on the hart rule 3 chose for its warm cache (in the scratch run hart 7 looked first and pulled it off
hart 1). Over the review's random walk the average is 0.33 pokes per scheduler operation at 4 harts and 0.66 at 8.

This is not a correctness bug (the marks die out, GP1 holds), but it is on the hottest path of the kernel, it grows
with the hart count, it is serialised by the BKL, and it contradicts 3.9 ("owes exactly one IPI") in spirit and rule
3 of 4.4 in effect. It will also distort the benchmarks of SCHED-SMP-SPEC 6.1.

Suggested fix: one queued thread needs one hart. In `after_enqueue`, mark only the target (when it is not the caller
and its current thread is lower). When a thread is left queued behind something higher (a preempted or rotated
thread, a pinned neighbour), mark only the best candidate: the online hart with the lowest current priority that may
run it, if that is lower. `push_queue` should likewise mark one hart per queued candidate, not every hart for the
same best thread. Add an assertion to the model: at most one new mark per thread made runnable.

### 3. The model test misses whole classes of bugs; six of ten mutations survive (important)

`kcore/tests/sched2_smp_model.rs`. Mutations run on a scratch copy (release build of the same test):

| Mutation | Result |
|---|---|
| M1 `mark` sets `pending` but not the poke bit (the T65 class of bug) | **passes** |
| M2 a wake of a prepared (not blocked) thread does not notify it (`wake_quiet`) | **passes** |
| M3 `select_hart` ignores `online` | **passes** |
| M4 `detach_woken` leaves the timer on the old hart | caught |
| M5 `set_priority_on` does not mark the home hart | caught |
| M6 `settle` without `push_queue` | caught |
| M7 `attach` does not update `last_hart` | **passes** |
| M8 `push_marks` does nothing | caught |
| M9 `set_clock` does nothing | **passes** |
| M10 `push_queue` ignores `pending` and `online` | **passes** |

Why, concretely:

* **The poke contract is never observed** (lines 66-80). `drain` serves harts by reading `resched_pending` and then
  throws `take_pokes()` away. Nothing checks that a mark that goes from clear to set has its bit in the poke set, that
  the caller never pokes itself, or that an offline hart is not marked. In the kernel the poke bit *is* the IPI; a
  mark without it is a hart that is never told (and, since `push_queue` skips `pending` harts, never marked again).
* **`drain` gives up silently after 1000 rounds** and the check then runs anyway. A mark storm (finding 1) can only
  show up through GP1, not as "did not converge". It also serves harts in ascending index order every time, so only
  one of the possible pull races is explored.
* **Only `FixedPrio`** is driven. `sched2::RoundRobin` has no test at all.
* **No hart is ever offline**: `set_online`, `clear_pending`, `set_clock`, a pinned spawn onto a hart that has not
  joined, the join itself, and `thread_count` with offline harts are untested, though that is exactly how the kernel
  boots.
* **Clocks never lag**: the tick step moves every hart to the same time (lines 290-294). The kernel's harts each tick
  alone from a clock only hart 0 moves; a sleeper's deadline against its home hart's lagging clock, and a timer wake
  followed by a pull to a hart whose clock is still behind the deadline, are untested.
* **Only blocked threads are woken** (236-248). The prepared/notified path through `wake_on`, `wake_counted_on` and
  all of `Waiters2` (the stale-entry rule, a wait that a notification lets through) have no test. `Waiters2` replaced
  `sched::Waiters` under the reaper, `join` and every kernel wait queue.
* **Cost is not measured**: nothing bounds pokes or pulls per operation (finding 2), nor checks that `last_hart`
  follows a thread (M7).

Suggested fix: adopt the kernel-style walk used for this review (it is short and passes on the code as it stands, so
it pins what already works): offline harts and joins, a per-hart tick, wakes of any thread, random service order
with a hard bound of `harts` rounds, and after every operation "each newly set mark has its poke bit; no self poke;
no mark on an offline hart". Run it over `FixedPrio` and `RoundRobin` with the class's own form of GP1. Add a
`Waiters2` test against a FIFO reference. Then repeat M1, M2, M3, M9 and M10 and see them fail.

### 4. The invariants' evidence now points at a scheduler the kernel no longer runs, and the stand-ins of this step are not recorded (important)

`INVARIANTS.md:44` (C4), `:166` (PK2), `:168` (ID1), `:169-170` (GP1, AF1); `kcore/tests/idle_concurrency.rs:107`;
`kcore/tests/sched_smp_model.rs`; `TIDY-UP.md` (T85, R23).

With this slice the kernel's scheduler is `Sched2`. The "checked by" column for C4 and PK2 still names
`Sched::audit` and `sched_smp_model.rs`, which drive `kcore::sched::Sched`; the shuttle model that carries ID1 and the
T65 fix (R23, three negative controls) is built on `Sched::with_harts`. All of that is now evidence about dead code.
ID1 in particular (no runnable thread stranded while its hart idles with no IPI pending) has **no** check over the
code the kernel runs, and finding 3 shows the new model would not notice a dropped poke. Task 11's acceptance lists
"the idle model updated"; it must not be read as already covered.

The slice also adds stand-ins that CLAUDE.md wants in `TIDY-UP.md` in the same PR, and the working tree does not touch
that file: the kernel uses `Sched2` as one value under the BKL (4.1 says the kernel holds per-hart `HartRq` and does
not use `Sched`); `loc`, `pending` and `pokes` are global; marks without `need_resched` (X7: a wake still switches
through `reschedule()`); no `finish_switch`/dead list; `set_priority_on` can mark an offline hart (finding 9); the
`rr` flag is not passed (finding 10); T85 item (1) still says task 11 moves the park steps under `RQ`.

Suggested fix: port `idle_concurrency.rs` to `Sched2` (or add the Sched2 variant beside it) before the old core is
called retired; update the INVARIANTS rows to name the tests that really run the kernel's code; add the TIDY-UP
entries.

---

## Minor

### 5. Priority 0 panics inside `Sched2` (minor)

`kcore/src/sched_smp.rs:305` and `:338`; `kcore/src/sched2.rs:603`, `:1051-1066`. `spawn_on` accepts
`prio == 0`, creates the record and calls `enqueue_runnable`, which reaches `unreachable!("the idle thread is never
queued")`. Confirmed in the scratch program. `set_priority_on(.., 0)` on a runnable thread does the same through
`set_priority_quiet`. The kernel does not get there today (`thread_set_sched` requires 1 to 63, the clamps use
`max(1)`, and the idle thread is no longer spawned through `spawn_with_affinity`), but the old core took priority 0
and the model never tries it. Fix: `Error::BadPriority` for 0 in both, and one test line.

### 6. A pulled thread loses its place and gets a fresh slice (minor)

`kcore/src/sched_smp.rs:225` (`Enq::Migrated`), `kcore/src/sched2.rs:204-207`. `Enq::Migrated` falls into the default
arm of `enqueue_common`: back of the level, `slice_left = TIMESLICE`. Measured: a thread preempted with 1 ms of its
slice left and then pulled by another hart arrives with 10 ms. A thread that is preempted and pulled often therefore
never reaches a slice end, which favours it over equal-priority peers on the destination hart; and a preempted RT
thread that is pulled goes behind its equals instead of in front (4.5: "a preempted thread returns to the front").
Today a pull requires strictly higher priority than everything on the destination, so position does not matter yet,
but the slice does. Fix: `Migrated` keeps `slice_left` (as `Preempted` does); decide front or back per the reason the
thread was queued.

### 7. Scans on the hot path that grow with the queue, not with the hart count (minor)

* `pull` (`sched_smp.rs:216-219`) and `push_queue` (`:266-270`) call `queued_top_where` with a predicate that looks
  the record up in a `BTreeMap` for each candidate. Kernel threads are pinned to hart 0, so every other hart's pull
  walks **all** of hart 0's queued pinned threads (each with a map lookup) before finding none it may take, and hart
  0's `push_queue` does the same once per other hart. That is O(harts x queued-not-allowed x log n) per `settle`, and
  `settle` runs on every tick of every hart, every block, wake, yield and exit. The comment at `:254-255` ("O(harts),
  not O(threads)") is not what the code does. Suggest keeping, per `HartRq`, the best queued priority among **free**
  threads (or a second occupancy mask for them), so the common "nothing here for you" answer is one compare.
* `detach_queued` (`sched2.rs:991-997`) scans both classes for `is_queued` and then again in `dequeue`
  (`PrioQueues::contains` and `remove` each walk all 64 levels).
* `RoundRobin::top` and `top_where` are O(queue) and are called from `better_waits` on an idle hart, from `pull` and
  from `push_queue`.
* `class_enqueue`/`class_dequeue` remove and re-insert the record in the `BTreeMap`, and `attach`/`detach` move it
  between maps: node allocation in the wake path and in the timer interrupt (X8; allowed while transitional, worth a
  TIDY-UP line so it is not forgotten when `RQ` locks arrive).

### 8. Equal-priority threads no longer share harts evenly (minor; a behaviour change to record)

Three free hogs of priority 10 on two harts: T1 and T3 rotate on hart 0, T2 has hart 1 to itself. Measured over
3000 ms in the scratch program: 1500, 3000 and 1500 ms, no pull. Under the shared queue each got two thirds. Pull needs a strictly higher priority (the test
`equal_priority_threads_are_not_pulled_back_and_forth` pins this on purpose), and nothing balances equals. No thread
starves, and GP1 does not cover it, but 4.4 claims that "today's observable behaviour ... survives per-hart queues".
Record it (TIDY-UP, with the fair class or a rotation-time pull of an equal as the proper fix) or the claim is wrong.

### 9. `set_priority_on` marks a hart that is not in the scheduler; the kernel's "hart is up" test is the wrong flag (minor)

`kcore/src/sched_smp.rs:439-441`; `kernel/src/thread.rs:445-451`. `mark(home)` does not look at `online`. A priority
change of a thread pinned to a hart that has not joined sets `pending[home]` and a poke bit (confirmed: `pend1=true
pokes=0x2`), against 3.9 ("a hart that is not yet in the scheduler ... is never marked"). `send_pokes` then tests
`percpu.online`, which is set before `join_scheduler`, so it sends the IPI (the trap drops it: `scheduler_ready()` is
false) and never reaches `clear_pending`; that branch is now dead. It is harmless only because `join_scheduler`
ends with `reschedule_this_hart()` and `settle` clears the mark. Fix: `mark` refuses an offline hart (the join
looks anyway), `send_pokes` needs no special case, delete `clear_pending`.

### 10. The round-robin flag still does not reach the scheduler, and SYSCALLS says the opposite of what happens (minor)

`kernel/src/thread.rs:352` and `:176-179` pass `SchedAttr { rr: false }` always; `thread_set_sched` stores the flag
in `sched_flags` only. `SYSCALLS.md:564` says "today every level rotates, so the flag is stored and reported only".
With `RtClass` that is false for priorities 32 to 63: an RT thread with `rr` false never rotates, so two equal RT
user threads on one hart run strictly one after the other however the flag is set. ROADMAP task 6 deferred the flag
"until per-hart classes"; they are here. Either pass it (spawn and a `set_attr` on `Sched2`) or correct SYSCALLS and
add the TIDY-UP entry.

### 11. The "round-robin kernel" is no longer round robin for anything at priority 32 or above (minor)

`Sched2<RoundRobin>` still hosts `RtClass` with strict precedence. The boot/test thread runs at `MAIN_PRIO` 40 and
the reaper at 50, so in that build they preempt every normal thread and are FIFO among themselves; the old
`sched::RoundRobin` rotated everything. The build proves less than OD32 says, `class_name()` (`thread.rs:849-856`)
returns hard-coded strings instead of the class's name, and the comments are stale (`smp_tests.rs` "priorities mean
nothing to the round-robin class"; `thread.rs:1-2` "`kcore::sched`"; `thread.rs:157` "thread 1": the boot thread is
now tid `harts + 1`, tids 1 to `harts` are the idle threads; I found no code that depends on the old numbering).

### 12. Timers on a secondary hart fire up to one tick later than before; a timer wake skips placement (minor)

`kernel/src/thread.rs:465-477`, `kcore/src/sched2.rs:840-874`. A sleeper's timer lives on its home hart and fires on
that hart's own tick, which reads `TICKS` (moved only by hart 0). If the hart's tick comes just before hart 0's, the
sleeper wakes one tick later than under the old code (hart 0 fired every timer). Never early: a hart's clock is
never ahead of `TICKS`, and the kernel checks `ticks() >= deadline` first. Tests with tight windows run pinned to
hart 0, so they do not see it. Separately, `advance` queues a timer-woken thread on its home hart without
`select_hart`; if that hart is busy, `push_queue` marks the others and one pulls it: correct, at the cost of an IPI
and a migration that a placed wake would not need.

### 13. The model test is slow in a debug build (minor; test cost)

`cargo test -p kcore --test sched2_smp_model` took 278 s and then 354 s (two runs, about 350 s of CPU, one thread)
in a debug build of the scratch copy on this machine, and 10 s in release. The machine was busy and its toolchain is
`x86_64-apple-darwin`, so the absolute number may be inflated, but the ratio is not. `kcore` has no `opt-level`
override in the workspace's dev profile, and `make quick` and `make os-test` (part of `make smoke`, the gate) run
`cargo test -p kcore` in debug. The file came in with L11a, so the gate already pays for it; measure it there. The
cost is `audit(true)` after every step: harts x threads x a queue scan (`is_queued` walks 64 levels in two classes),
with a thread population that mostly grows. Bound the population in the walk, give `audit` one pass that collects
the queued set, or give `kcore` `opt-level = 2` under test, before more walks are added (finding 3 asks for more).

---

## Checked and found fine

* **Only hart h changes `cur[h]`.** Every `Sched2` entry point settles only the caller's hart; a remote hart is
  marked. In `thread.rs` every call that can change `cur[h]` or set a mark (`spawn`, `wake`, `wake_many`, `wake_one`,
  `wake_all`, `block`, `sleep`, `yield`, `exit`, `join`, `set_priority(ies)`, the tick, the IPI, the join) is followed
  by `reschedule()` inside the same `irq::without` region, and `reschedule()` sends the pokes before it compares
  `current_on(h)` with `running[h]`. No path leaves the region with the two different.
* **A thread is never run while its context is still being saved.** The BKL is held from the scheduler call through
  `context_switch` and released by the incoming thread, so a hart that pulls a just-preempted thread, or a joiner or
  the reaper freeing an exited thread's stack, runs after the switch away is complete.
* **Lost wakeups, fixed-priority class.** Every event that lowers `cur[h]` (block, sleep, exit, yield, slice end, a
  priority drop, local or remote) ends in `settle(h)` or a mark of `h`; every event that leaves a thread queued marks
  a lower hart that may run it or ends in `push_queue`. "Best queued thread per other hart" in `push_queue` is
  enough, because each marked hart pulls for itself from every queue. Confirmed by the second walk (GP1 after every
  operation, random service order).
* **Harts that have not joined.** A free thread is never placed on one; one pinned there waits in its queue and
  `join_scheduler` (`set_clock`, `set_online`, `reschedule_this_hart`) picks it up. The trap handler drops IPIs and
  scheduler ticks until `in_sched`, so an unjoined hart never calls `settle`, and `running[h]` is set to the hart's
  idle tid before its first switch.
* **Record migration.** `detach_woken` cancels the timer on the home hart, clears `deadline` and `park`, and `loc`
  follows; `detach_queued` carries the park word (a prepared thread that is preempted and pulled is still notified
  by a later wake). A timer exists only for a Blocked thread, and a Blocked thread never moves, so no stale timer can
  fire for a thread that left.
* **Time.** `ticks_to_ns` saturates, so "never" stays never; `sleep_until` cannot report "passed" while the kernel
  saw `ticks() < deadline`; a late-joining hart's clock is set before its first tick, so its first `advance` is one
  tick, not the whole uptime.
* **Pinned and idle threads.** Placement returns the pinned hart, pull filters with `allowed`, `audit` checks AF1; the
  walk never saw a pinned thread anywhere else. Hart 0's idle thread gets a stack and trampoline for the id the
  scheduler made; secondaries adopt their boot context; `thread_count` equals `sched_thread_count` at init, after
  each join, and with threads pinned to offline harts.
* **Old assumptions.** `current_on` no longer returns `Option` and its users were updated; nothing in
  `kernel/src`, `rt` or `init` depends on tids starting at 1; the idle thread is never the subject of `block`,
  `yield` or `exit` (they would now panic with `NoCurrent`; `heap::service` in the idle loop does none of them).
* **`Waiters2`** has the same rule as `sched::Waiters` (a stale entry whose thread is blocked elsewhere takes the
  wake; every kernel waiter re-checks in a loop). Not new, but see finding 3: it is untested.
