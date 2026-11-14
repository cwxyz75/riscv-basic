# W: targeted wake and ports as registrations (specification, v1)

Status: v1, written 2026-11-14 from the Fable design recommendation (`reviews/2026-11-14-wake-path-design.md`,
finding D1 of `reviews/2026-11-14-design-review.md`) and the owner's decisions recorded in `ROADMAP.md` (slice W).
Nothing in sections 3 and 4 is built yet; step 0 (counters, the baseline, this document) is. This file is the written
acceptance contract for W (CLAUDE.md: spec, then acceptance checks, then code, then independent review).

## 1. The problem, measured

One global wait queue (OD43). After almost every syscall `notify()` wakes every blocked thread; each re-checks its own
condition under the big kernel lock and usually sleeps again. Step 0 added kernel counters (`debug_stat`, SYSCALLS.md)
and the suite test `wake_baseline_bystanders_and_counters`, which runs 60 ping-pong round trips between two threads with
and without four *bystander* threads blocked on unrelated events. Baseline, QEMU one hart, debug kernel:

| run | guest ticks | blocks | spurious wakes | threads woken |
|---|---|---|---|---|
| ping-pong alone | 9 | 361 | 241 | 360 |
| plus 4 bystanders | 10 | 601 | 481 | 600 |

Four spurious wakes per round trip with nobody else around, and exactly four bystanders times 60 round trips
(240) more with them. The cost is O(blocked threads) per syscall: harmless at a dozen threads, quadratic at the thread
counts the cheap-threads direction (OD73) is for. When this slice lands the same test must show no extra spurious
wakes for the bystanders (`TARGETED_WAKE = true` in the suite flips its assertion).

## 2. Decisions (owner, 2026-11-14)

Signals stay derived; the single big kernel lock stays and **no new lock** is added (lock order unchanged); ports become
registrations with level-triggered semantics kept and a first-come ready list (report order changes from rotation to
oldest-ready-first, accepted); a port wakes one waiter, first in first out; opt-in one-shot bindings with `port_rearm` in
this slice; closing, narrowing or sending the handle a binding was made through ends the binding; the 256-binding cap goes
in favour of a per-job quota; the global wait queue is retired (futex, kill, IRQ, kernel reads and thread join all wake
by thread id).

## 3. The protocol

State (all under the big kernel lock):

* `Registry.dirty`: a log of objects whose signal word or liveness may have changed, with a *forced* flag (handle removed).
* `kcore::wait::WaitTable`: registrations `(object, waiter) -> kind` (a thread waiter with its mask, or a port binding),
  per-thread wait state (Idle, Registered, Woken(port)), and per port: its bindings, an exact FIFO `ready` set, a FIFO of
  waiting threads and an `inflight` count.

Operations:

1. **Wait** (one BKL region): evaluate the predicate; if true, killed or past the deadline, return. Otherwise assert the
   dirty log is empty, register, `sleep_until`, switch. On resume: if still Registered, deregister (timer or forced wake);
   if Woken(port), `inflight -= 1`; loop.
2. **Mutate**: every mutating `kcore::ipc` operation touches the objects whose signals may change: any change to a channel
   or socket end touches that end and its peer (WRITABLE and WRITE_CLOSED derive from the peer); `release`, `sweep` and
   `peer_destroyed` push ids themselves. Handle removal (close, replace, transfer, drain) is a *forced* touch: waiters on
   that object wake unconditionally and re-validate their handle, preserving BAD_HANDLE.
3. **Commit** (pure; before every BKL release, block or switch): drain the log; per distinct dirty object compute liveness
   and the signal word once; wake a thread registration if the object died, the touch was forced, or its mask matches
   (the registration is removed); update the ready flag of a port binding (remove the binding if the object died, or the
   touch was forced and it was made through that handle); then apply **PW** to every port whose ready set or waiter list
   changed. Output: the thread ids to wake.
4. **Wake**: wake each id, then one `reschedule()` (one batch of IPIs).
5. **Kill and thread exit**: `forget_thread(tid)` removes its registration (or decrements `inflight` if Woken), then commit.
   `leave()` calls it as a backstop and asserts it found nothing.
6. **Port wake (PW)**: while `inflight < |ready|` and a waiter is queued, wake the head waiter and increment `inflight`. A
   woken waiter decrements `inflight` and polls before testing its deadline; finding nothing, it re-queues and sleeps (it
   never returns to user empty-handed before its deadline). A killed winner is removed eagerly and PW re-runs (the baton
   passes). `channel_call` registers for its exact reply (`Call{txid}`), not for READABLE, so an unrelated queued message
   cannot keep it permanently ready.
7. **Bindings**: persistent and level-triggered by default (today's contract); `ONESHOT` disarms a binding on delivery
   until `port_rearm`, which re-evaluates the level. The binding count is limited by the job quota (`BINDING_COST`
   charged honestly), not a fixed 256. A binding records the handle it was made through (finding D17).

## 4. Invariants (each gets a check)

* **W1** The dirty log is empty whenever the BKL is released or the running thread changes (asserted in `bkl::unlock` at
  depth 1 and in `reschedule`).
* **W2** A thread has at most one registration, and none while in user mode.
* **W3** Every registration names a live object and a live port (`WaitTable::audit`).
* **W4** Filter soundness: after a commit no Registered thread's own predicate is true.
* **P1** A port's ready set is exactly the bindings whose `signals & mask != 0` at every commit (checked against today's
  stateless scan, which is kept as the test oracle).
* **PW** A non-empty ready set implies no queued waiter or `inflight >= 1`.

Lost-wakeup argument: check, register and block are one BKL region and the BKL stays with the hart across the switch, so no
waker runs between the check and the Blocked state. Every mutation that can make a predicate true touches the object
(completeness is tested), W1 forces a commit before anyone else can observe the state, and the commit wakes by W4. The
argument leans on the BKL; if the scheduler is later carved out (slice L) register-and-block stops being atomic, so the
kcore API is written as prepare / commit / cancel now.

## 5. Acceptance (written first)

Step 0 (done): counters and the bystander baseline (section 1); the baseline test asserts that the defect is visible
(`busy spurious > quiet spurious`) and that the counters count, and is flipped by `TARGETED_WAKE`. Still to write with
step 2: the shuttle model of the idle/wake/IPI protocol (T38; the storm currently hides any lost reschedule poke) and the
`channel_call` round-trip benchmark with 0 and N bystanders.

Later steps: a completeness property test of the touch rules (snapshot every object's signal word and liveness before and
after each random op: anything that changed must be in the log); model tests of `kcore::wait` against OD43's
wake-everyone behaviour as the oracle (the set the old code would release must be a subset of the set woken; spurious wakes
asserted zero outside forced touches); a port model (P1 after every step, fairness bound, PW); mutation checks (drop one
touch; wake-one without the baton; deadline test before poll; lazy dead-port cleanup), each must fail a test; loom (two
waiters, a waker, a timer and a killer over a mutex standing for the BKL) and shuttle (wider) with a negative control that
checks and registers in separate regions; QEMU: the existing `wait_*`, `channel_call_*`, `futex_*` and kill suites at one,
two and four harts, plus new tests for close, replace and transfer of a waited handle, kill of a port winner, many waiters
on one port, a binding count far above 256, a `fault-skip-touch` negative-control kernel and a debug `wait-audit` feature
that evaluates every blocked waiter's real predicate after each commit and panics if one is true.

## 6. Migration (each step green and bootable)

0. Counters, baseline, this document. 1. The dirty log and touch rules in `kcore`, no behaviour change, with the
completeness property test. 2. Wake by thread id for futex and kill. 3. `kcore::wait` with thread waiters (`object_wait`,
`channel_call`, kernel reads, IRQ); ports still use the global queue, woken only when a port could be affected. 4. Ports as
registrations, quotas, per-handle binding lifetime. 5. Retire the queue and the hand-kept syscall list; documents
(SYSCALLS execution model, `object_wait`, ports; INVARIANTS C3b rewritten, W1-W4, P1, PW; T12 and the futex half of T49
resolved). 6. One-shot bindings and `port_rearm`.

## 7. Risks

Touch completeness is the single point of failure: a missed touch becomes a hang where today the storm masks it
(mitigations: the completeness test, P1, `wait-audit`). Bugs the storm hides will surface: a lost reschedule poke, a kill
that wakes nobody (`end_process` calls `request_kill` without a notify). Report order of `port_wait` changes (accepted).
Estimated size (not measured): about 1,000 to 1,300 lines of code and 1,200 to 1,800 lines of tests, 6 to 7 PRs.
