# Design D1: targeted wake, ports as registrations, exclusive wakes, retiring the global queue

Independent design pass, read-only, 2026-11-13. Nothing was built, run or booted. "Read" means I
read the cited lines. Same-family caveat (T24) applies. Confidence is marked per item.

## (a) Summary recommendation

Keep signals derived and keep the single BKL (no new lock). Add two pure pieces to `kcore`:
(1) a **dirty log inside `Registry`** (`touch(id)`; `release`, `sweep` and `peer_destroyed` push
ids themselves), so every mutation records which objects' signal words may have changed, and
(2) a **`kcore::wait::WaitTable`** holding weak registrations keyed by `ObjId`: thread waiters
(`Signals{mask}` and `Call{txid}`) and port bindings. One pure function, `wait::commit`, drains
the dirty log, evaluates each registration on the dirty objects against the freshly derived signal
word, maintains each port's **exact ready set**, and returns the list of thread ids to wake. The
kernel calls it after every syscall, IRQ and teardown step (always before the BKL is released or a
thread blocks) and wakes exactly those threads by id. Ports become wait queues with FIFO wake-one
under an in-flight rule; bindings stay level-triggered and still hold no reference. Futex, kill,
join and kernel waits move to wake-by-thread-id, which retires OD43's queue and the hand-kept list
of "syscalls that can change a signal". The old behaviour (wake all, each re-checks) becomes the
**reference model** in the tests. Size: a milestone-sized slice, 6 to 7 PRs.

## (b) The four questions

### Q1. Targeted wake

**Recommendation.** Dirty log in the registry + per-object registrations + waker-side evaluation.

- Mutations call `reg.touch(obj)`. Rule: *any mutation of a channel or socket end touches that end
  and its peer* (WRITABLE and WRITE_CLOSED are derived from the peer: `ipc.rs:630-634, 644-650`).
  `event_signal`, `interrupt_fire/ack`, `process/thread_mark_terminated`, `socket_shutdown`,
  port bind/unbind touch their object. `Registry::release` and `sweep` push the destroyed id and
  the surviving peer themselves (`object.rs:221-250, 279-333`), so destruction cascades, table
  drains and the cycle sweep need no caller cooperation.
- Removing a handle-table entry (`close`, `replace`, `remove_for_transfer`, `drain`;
  `handle.rs:125,165,177,206`) does a **forced** touch of its object: all thread waiters on that
  object wake unconditionally and re-validate their own handle (BAD_HANDLE semantics of
  `SYSCALLS.md:251`). Spurious wakes are confined to waiters on that one object.
- The waker's evaluation is only a **filter**; the woken thread still re-checks its own predicate
  in its own context and sleeps again if it is false. Safety property (filter soundness): if a
  registered waiter's own predicate is true at a commit point, it is woken.
- Per kind: `object_wait` registers `Signals{obj, mask}`; `channel_call` registers
  `Call{obj, txid}` (must be exact, not READABLE: an unrelated queued message would otherwise make
  it permanently "ready", and N callers sharing an endpoint would be an N-squared herd);
  `port_wait` queues on the port (Q2/Q3); TERMINATED is an ordinary touch; deadlines stay the
  scheduler's per-thread timers (`sched.rs:496-505, 634-651`), the timed-out waiter deregisters
  itself; kill wakes the victim's threads by id and removes their registrations eagerly.
- Signals stay derived: `object_signals` remains a pure function of registry state; nothing is
  cached as a second source of truth.

**Why a dirty log rather than "each op returns the changed set"** (the review's suggestion):
destruction happens deep inside `release`/`sweep`/`HandleTable::close`, where a return value would
have to thread through every signature; a log cannot be dropped by an intermediate caller, and it
lets one assertion ("the log is empty whenever the BKL is released or a thread switches out")
police every path. Both variants need the same completeness test (below); the log is less churn.

**Alternatives.**
- *Stored signal words with change counters*: same touch discipline, plus a cached copy that can go
  stale. Buys edge detection, which the per-binding ready flag already gives. Not now.
- *Per-object `WaitQueue` in the scheduler, waiters re-evaluate*: simplest, but no mask filter, and
  a thread can sit on one queue only, so ports still need registrations. Rejected as the mechanism.
- *Hashed table keyed by object id*: this is the storage I recommend (an ordered map keyed by
  `(ObjId, waiter)`; `ObjId` carries the generation, so slot reuse cannot alias). A list in the
  registry slot would be O(1) but couples the generic registry to waiting.

**Evidence.** `user.rs:692-776` (the syscall-number list), `1299-1310` (notify),
`1496-1529`, `1599-1634`, `1646-1732`, `1910-1923` (the five predicates on `signal_queue`),
`thread.rs:590-613`, `ipc.rs:598-668`. Thread caps today are 8 per process and 128 in total
(`SYSCALLS.md:92-98`), so the cost is bounded now and becomes a defect when OD73 raises them.

**Confidence**: high on the mechanism; medium that the touch rules above are complete (that is
what the completeness test is for).

### Q2. Ports as registrations

**Recommendation.** Yes. A binding becomes a registration `Port{port, key, mask}` on the object;
the port holds `bindings` (by key), an **exact ready set** in FIFO order, a FIFO of waiting threads
and an in-flight count.

- OD48 "holds no reference": **keep**. Registrations are weak ids, never roots, never in `held()`,
  so refcounts, H4/H6 and the sweep are untouched. "Queues nothing": **amend** to "queues at most
  one readiness entry per binding" (bounded by bindings, already charged; still nothing to overflow
  or lose).
- Level-triggered: **keep, exactly**. Invariant P1: at every commit point
  `ready == { b : alive(b.obj) and signals(b.obj) & b.mask != 0 }`. Commit maintains it eagerly on
  both rising and falling edges; `port_bind` evaluates the new binding at once. Port READABLE stays
  derived (`ready` non-empty). When a port's ready set changes emptiness the port is touched, so
  `object_wait` on a port works; ports still cannot be bound to ports, so commit's fixpoint has
  depth 2.
- Fairness: poll takes the oldest ready entry and moves a still-ready persistent binding to the
  tail. "None starves" (`SYSCALLS.md:636-637`) stays true: a continuously ready binding is reported
  within `|ready|` polls. The exact order changes (readiness order instead of binding index order).
- The 256 cap: no longer a latency bound. Bound bindings by the job's IPC quota only (OD59), and
  raise `BINDING_COST` (32, `ipc.rs:29`) to the honest size of a binding plus its registration.
- Object dies: the registry logs the dead id; commit removes the binding and credits it (replaces
  the lazy `prune_port`, `ipc.rs:882-906`). Port dies: its registrations must be removed **in the
  same commit**, not lazily (a bind/close-port loop would otherwise grow an object's list for
  free); keep all binding state in `WaitTable` keyed by port id so the dead id is enough.
- D17: record the handle the binding was made through (process id + handle value); a forced touch
  from close/replace/transfer drops it. This restores "dropping WAIT ends the watch". Visible
  semantics change; see (f).
- Cost: bind, unbind and poll O(log n); commit O(registrations on the objects actually touched);
  no scan anywhere (today: O(bindings) per poll and per READABLE evaluation, `ipc.rs:654-660,
  998-1005`).

**Alternatives.** *Lazy ready list (epoll style, re-validate at poll)*: cheaper falling edges, but
READABLE on the port becomes approximate and the strong invariant P1 (which doubles as the lost-wake
detector) is lost. *Packet queue (Zircon ports)*: needs queue memory, overflow policy and
edge semantics; larger change and a different contract. Not recommended now.

**Confidence**: high on structure; medium on quota/cost constants.

### Q3. Exclusive wakes

**Recommendation.**
- `object_wait`: wake **all** waiters whose mask matches. It consumes nothing, so exclusivity has no
  meaning; no option.
- Futex: unchanged (count, FIFO, F3).
- Port: **always wake-one**, FIFO (longest waiter), never wake-all. Rule PW: whenever commit ends
  with `inflight < |ready|` and a waiter is queued, pop the head waiter, mark it Woken, `inflight++`,
  wake it; repeat. A woken waiter, on re-entering under the BKL, does `inflight--` and **polls before
  it looks at its deadline** (today's loop already does, `user.rs:1613-1629`). If it gets an event it
  returns; if not, it re-queues at the tail and sleeps (never returns to user empty-handed before
  the deadline). A waiter woken by its timer is still queued and still eligible; it removes itself
  when it runs. A killed waiter is removed eagerly by the kill path; if it was Woken, `inflight--`
  and PW is re-run, so the baton passes. A timed-out winner polls first, so it either takes the
  event or (ready empty) nothing was lost.
- Choice is **per binding, not per waiter**. Default: persistent level-triggered (today's contract,
  right for one thread per port, which is OD73 item 5's model). Opt-in `ONESHOT`: delivery disarms
  the binding (it leaves the ready set) until `port_rearm(port, key)`, which re-evaluates the level
  at once, so nothing is missed. This is the only mode in which several threads on one port get no
  duplicates; with persistent bindings a still-ready key can be reported to a second waiter (as
  today, but serialised instead of a herd). Document that.

**Alternatives.** *Edge-triggered bindings*: user code must drain to empty or it hangs; the classic
bug source; rejected. *Wake one per rising edge only, no in-flight rule*: a ready port can keep
waiters asleep if the winner dies or never returns; rejected. *Priority-ordered winner*: plausible
for the real-time band; see (f).

**Confidence**: high for the PW rule under the BKL; medium on whether ONESHOT should ship in this
slice or with the S8 service loop (D7).

### Q4. Other users of the global queue

Everything moves to "registration in a pure table + wake by thread id". New kernel primitives:
`thread::wait_registered(ready, deadline)` (today's `wait_deadline` without a queue) and
`thread::wake_blocked(tid)` (wake only if `Blocked`, no token, as `WaitQueue::wake_one_on` already
does, `sched.rs:861-869`).

- `futex_wake`: `Futexes::wake` already returns the sleepers (`futex.rs:37-52`); wake those ids.
  Removes T49's cross-process cost.
- Kill (`request_kill` sets a flag and kicks harts but wakes nobody itself, `user.rs:1437-1463`;
  `leave(whole)`, `user.rs:550-585`; `job_kill`; kill-on-close from the graveyard): one function
  marks the process killed, removes each of its threads' registrations and wakes them.
- IRQ path (`trap.rs:320-327`): `interrupt_fire` touches; the handler commits.
- `KernelEnd::wait_readable`: a `Signals` registration with a kernel thread id.
- Thread `join`: the `exits` queue is a second wake-all (`thread.rs:534, 549-576`). A thread is
  joined once, so store one joiner id in the thread record. User-level join is `object_wait` on
  TERMINATED, already covered.
- Timed sleeps use only the scheduler timer; no change. The reaper keeps its private one-waiter
  queue. `WaitQueue` survives as a kernel-internal type.

Then delete `signal_queue`, `wake_signal_waiters` and the syscall list; `notify()` becomes
"cleanup, reap, commit". **Confidence**: high.

## (c) Protocol specification

State (all under the BKL; **no new lock**, lock order unchanged):
- `Registry.dirty: Vec<(ObjId, forced)>`.
- `WaitTable`: `regs: (ObjId, Waiter) -> Kind`; `thread_reg: Tid -> registration or Woken(port)`;
  per port: `bindings`, `ready` (FIFO), `waiters` (FIFO), `inflight`.
- Thread wait states: Idle, Registered (blocked or about to block), Woken (removed by a waker).

Operations:
1. **Wait** (one BKL region): evaluate own predicate; if true or killed or past deadline, return.
   Else `debug_assert!(dirty empty)`, register, `sleep_until`, switch. On resume: if still
   Registered, deregister (timer or forced wake); if Woken(port), `inflight--`; loop to the top.
2. **Mutate**: kcore operations touch as in Q1.
3. **Commit** (pure; before every BKL release, block or switch): loop until the log is empty; per
   distinct dirty object compute `alive` and the signal word once; for thread registrations wake if
   `!alive || forced || matches` (remove the registration); for port registrations update the
   binding's ready flag (remove the binding if `!alive`, or if forced and made through that
   handle), touch the port if its emptiness changed; then apply PW to every port whose ready set or
   waiter list changed. Output: thread ids.
4. **Wake**: `wake_blocked` each id, then one `reschedule()` (one batch of pokes).
5. **Kill / thread exit**: `forget_thread(tid)` (removes a registration, or `inflight--` if Woken),
   then commit. `leave()` calls it as a backstop and asserts it found nothing.

Invariants to add (each with a check):
- W1 The dirty log is empty whenever the BKL is released or the running thread changes
  (assert in `bkl::unlock` at depth 1 and in `reschedule`).
- W2 A thread has at most one registration and none while in user mode.
- W3 Every registration names a live object and a live port (`WaitTable::audit`).
- W4 Filter soundness: after commit no Registered thread's own predicate is true.
- P1 exact ready set. PW: `ready` non-empty implies no queued waiter or `inflight >= 1`.

Lost-wakeup argument. Check+register+block is one BKL region, and the BKL stays with the hart
across the switch, so no waker on any hart runs between the check and the Blocked state, and none
can run the thread before its context is saved. Every mutation that can make a predicate true
touches the object (completeness, tested) and W1 forces a commit before any other thread can
observe the state; commit wakes by W4. A wake that finds the thread already runnable (timer) is
harmless because every resumption re-checks before the deadline test. For ports, PW plus
"poll before deadline" plus eager kill removal give the baton argument in Q3. The IPI/idle path is
unchanged: `wake_on` still marks the hart pending and pokes it.

What this argument does **not** cover: it leans entirely on the BKL. If the scheduler is later
carved out (D2), register and block stop being atomic and the token rule (`sched.rs:473-528`) does
not rescue deadline waits, because `sleep_until` discards tokens. That needs a
prepare/commit/cancel wait; write the kcore API in that shape now.

## (d) Migration plan (each step green and bootable)

0. **Acceptance first.** Kernel wake counters in the `##` output (wakes, re-sleeps); QEMU test
   "bystanders are not woken" (N blocked threads on unrelated objects, a ping-pong pair, assert
   bystander wakes == 0; deterministic, not timing-based) recorded as a failing baseline; the OD73
   `channel_call` round-trip benchmark with 0 and N bystanders. Add the T38 shuttle model of
   idle/wake/IPI **now**: the wake storm currently hides any lost poke.
1. **First code step: the dirty log, pure, no behaviour change.** `Registry::touch`, the touch
   rules, and the completeness property test (snapshot every object's signal word and liveness
   before and after each random op; anything that changed must be in the log). The kernel drains
   and discards the log.
2. Wake by thread id: `wake_blocked`, futex wake, one kill function. Futex waiters leave the
   global queue. Extend the futex loom model with kill and timeout.
3. `kcore::wait` with thread waiters; `object_wait`, `channel_call`, `KernelEnd`, IRQ move over.
   Ports still use the global queue, which is now woken only when a port could be affected.
4. Ports as registrations: exact ready set, FIFO, PW; remove the cap; per-handle binding lifetime.
5. Retire: delete the queue and the syscall list, per-thread joiner, documents.
6. ONESHOT bindings and `port_rearm` (here or with the S8 service loop).

Tests.
- Model (kcore): random walk over all ipc ops, handle ops, sweep, kill, timeouts, with OD43
  semantics as the oracle: the set the old code would release must be a subset of the set woken
  (W4); spurious wakes counted and asserted zero outside forced touches. Port model: P1 against the
  current stateless `binding_ready` scan after every step; fairness bound; PW.
- Mutation checks: drop one touch; wake-one without the baton; deadline test before poll; lazy
  dead-port cleanup. Each must fail a test.
- loom (2 waiters, waker, timer, killer over a mutex standing for the BKL, real `kcore::wait` and
  `kcore::sched`) and shuttle (wider); negative control with check and register in separate
  regions.
- QEMU: existing `wait_*`, `channel_call_*`, `futex_*`, kill suites at -smp 1, 2, 4 and icount; new:
  bystander test, close/replace/transfer of a waited handle, kill of a port winner, many waiters on
  one port, a binding count far above 256, a `fault-skip-touch` negative-control kernel, and a debug
  `wait-audit` feature that evaluates every blocked waiter's real predicate after each commit and
  panics if one is true.

Documents. New OD (OD74) supersedes OD43's queue; OD48 amended (weak registrations, bounded
readiness entries, cap, order); OD49/OD59 note on `BINDING_COST`; SYSCALLS.md execution model
(`191-203`), `object_wait`, ports section, limits, later `port_rearm`; INVARIANTS: C3b rewritten,
W1-W4, P1, PW, F1/F2 reworded; TIDY-UP: T12 and the futex half of T49 resolved, T38 partly, new
entries for FIFO-not-priority and one event per `port_wait`.

Size (honest estimate, not measured): about 1,000 to 1,300 lines of kernel and kcore code touched
(`wait.rs` 350-450 new, `ipc.rs` ports rewritten, about 250 in `user.rs`), 1,200 to 1,800 lines of
tests, 6 to 7 PRs. Comparable to S5c or S7b, not a tidy-up.

## (e) Risks and what I could not verify

- Touch completeness is the single point of failure; a missed touch is now a hang, where today it
  is masked. Mitigation: the completeness test, P1, `wait-audit`.
- Bugs currently hidden by the storm will surface: a lost reschedule poke, or a kill that wakes
  nobody (`end_process` calls `request_kill` with no notify, `user.rs:1433-1435`; I did not trace
  its callers). `finish()` polls every tick (`user.rs:472-479`) and is unaffected.
- Registrations allocate map nodes under the BKL; a heap failure panics, as for every other map in
  the kernel. Bounded by thread count and charged bindings, but OD73's higher thread limits need
  per-thread kernel memory charged to jobs.
- Not read: `dispatch()` in full, `S5-SPEC.md`, the kcore tests' harness shape, userland users of
  ports (whether any relies on report order), `rt`. Not verified: the cost today (no measurement),
  and that no test pins binding-index order.
- A commit in the IRQ path lengthens interrupt-off time by O(registrations on one object).

## (f) Questions for the product owner

1. Binding lifetime (D17): should closing, narrowing or sending the handle a binding was made
   through end the binding? I recommend yes; it changes `port_bind`'s contract.
2. ONESHOT bindings: in this slice, or with the S8 service loop? Until they exist, several threads
   on one port still see duplicates.
3. Port winner: FIFO (recommended, matches futex) or highest priority first for the real-time band?
4. Binding limit: quota only, or a fixed higher cap?
5. Is a changed report order from `port_wait` acceptable (the no-starvation promise is kept)?
6. Sequencing: this slice plus the T38 model before S8, as the review proposed? I recommend yes.
