# Review: `klock` and the park protocol (SCHED-SMP-SPEC task 10, PR #136, branch `klock`)

Independent review of `git diff main...klock` against SCHED-SMP-SPEC 3.2 to 3.4 and 3.7 and INVARIANTS C1, C3, K5,
PK1, PK2. Run: `cargo test -p kcore -p klock` (all green), `park_concurrency` under loom and under shuttle (4 of 4
pass in each), and a scratch crate outside the repo that calls `kcore` and `klock` to reproduce findings 1, 2 and 6.
Not run: the QEMU boot matrix, Miri, the `klock` loom/shuttle models. Note: the local `main` is one commit behind
`origin/main`, so the diff also shows task 6b (`TwoBand`, `two_band.rs`); that part was not reviewed.

## Verdict

Nothing found that is reachable in the kernel as it runs today (one big lock, every wait re-checks its condition and
blocks in one `irq::without` region). No kernel or user-visible path depended on the wake token. The kernel wiring
of the checker and the heap lock is sound. Three things should be fixed before task 11 builds on this, because they
are exactly what the fine-lock work will lean on: the timed block ignores a notification (finding 1), `Guard` has
the wrong auto traits (finding 2), and the randomised models no longer exercise the park word at all (finding 3).

## Findings

1. **Medium. The timed block overwrites `Notified`, so step 7 of 3.7 is not implemented on the path every
   user-visible wait uses.** `Sched::sleep_until_on` calls `park.prepare()` unconditionally
   (`kcore/src/sched.rs:536`), then `block_current_on` tests `take_notified()` (`sched.rs:494`), which can no
   longer be true. `HartRq::sleep_until` does the same (`kcore/src/sched2.rs:654`). So the
   `prepare_block_on` in `wait_deadline_registered` (`kernel/src/thread.rs:709`) has no effect: a wake between it
   and the `sleep_until_on` at `thread.rs:713` is discarded and the thread blocks until its deadline. Futex,
   `object_wait`, port waits and `channel_call` all wait through this function (`kernel/src/user.rs:1744, 1795,
   1855, 1969, 2156`).
   Reproduced on the host: `prepare_block_on(0); wake(t)` gives `Notified`; `sleep_until(1000)` then returns
   `Ok(true)` with the thread `Blocked` and the park word `Prepared`.
   Not reachable today: prepare, register and sleep are one interrupts-off region under the big lock. It becomes a
   lost wakeup (PK1) the day `register` runs under a different lock from the run queue, which is task 11's premise;
   the comment at `thread.rs:707` says the code already follows "the protocol the later locks need", and it does not.
   No test covers prepare, wake, timed block.
   Fix: in both `sleep_until` implementations consume a pending notification (return `Ok(false)`) and prepare only
   from `Idle`; add the scenario to `sched_model.rs`, `sched_smp_model.rs` and a `HartRq` test.

2. **Medium. `klock::Guard` is `Sync` and `Send` whenever `T: Send`, which is unsound.** `Guard` holds
   `&Spin<T, M>`, and `Spin: Sync` needs only `T: Send` (`klock/src/lib.rs:204`), so the auto traits make
   `Guard<Cell<u32>>` `Sync`: two harts can share `&Guard` and both reach `&Cell<u32>` through `Deref`
   (`lib.rs:278`) in safe code. Confirmed by compiling `is_sync::<Guard<'static, Cell<u32>, M>>()`. `Send` is also
   wrong for this lock: a guard dropped on another hart calls `M::released` and `ticket.unlock` with that hart's
   identity (`lib.rs:293-297`), popping the wrong `Held` stack. The only instance today is `Spin<Heaps>` behind
   `with_heaps`, so nothing exploits it, but this is the one pure crate allowed `unsafe` and Miri cannot see a
   missing bound.
   Fix: add `PhantomData<*mut ()>` to `Guard` and `unsafe impl<T: Sync, M: Machine> Sync for Guard<'_, T, M>`; add
   a compile-fail check.

3. **Medium (coverage). The randomised scheduler models no longer exercise the park word.** The token was removed
   from the reference model in `kcore/tests/sched_model.rs` and nothing replaced it: the random operation mix
   (`sched_model.rs:253-290`) has no prepare or cancel, so in every random run the word only goes Idle, Prepared
   (inside `block`), Idle. Before, the same runs covered token set, consume and discard interleaved with
   preemption, yield, tick, exit and priority changes. `Notified` is now covered by four hand-written scenarios
   only, and `HartRq::prepare_block` (`sched2.rs:619`) is called by no test: `sched2_differential.rs` and
   `two_band.rs` never prepare. Finding 1 would have been caught by a model with the park word in it.
   Fix: give the model a park word and add prepare and cancel operations to `sched_model.rs`,
   `sched_smp_model.rs` and `sched2_differential.rs`; extend `Sched::audit` (`sched.rs:775`) with
   "Blocked implies Prepared" and "Notified implies Running or Runnable".

4. **Low to medium. `Waiters` moved from host-tested `kcore` into the kernel, and three of its behaviours are now
   untested; its waker also breaks the protocol it is named after.** The deleted tests
   `stale_wait_queue_entries_do_not_swallow_wakeups`, `a_thread_is_queued_at_most_once` and the stale-entry half
   of the deadline test have no replacement: the boot tests cover FIFO order (`two_waiters_on_one_queue_wake_in_order`)
   and producer/consumer only, and nothing wakes a queued thread by id and then calls `wake_one`. The dedup and
   skip logic at `kernel/src/thread.rs:51, 60-66` is unhosted logic, against the "logic on the host" rule.
   Separately, `wake_one_on` pops and **drops** any waiter that is not yet `Blocked` (`thread.rs:63`). Under 3.7 a
   registered, prepared, still-running waiter must be notified (step c), not skipped. Unreachable under the big
   lock; a lost wake for joiners and the reaper once it is not.
   Fix: put `Waiters` back in `kcore` (generic over `SchedClass`) with the three tests ported; have `wake_on`
   report whether the wake counted and let `wake_one_on` use that instead of the `Blocked` test.

5. **Low. The park model checks less than INVARIANTS says, and its controls accept any panic.**
   `kcore/tests/park_concurrency.rs`:
   * PK2 is listed as checked by "the same models" (`INVARIANTS.md:166`), but the model has no run queue and no
     second hart: nothing asserts "enqueued at most once" or "not enqueued while Running". The spec's third control
     ("enqueue without taking `RQ[home]`") is a PK2 failure; the model's version reads the state without the lock
     and is detected as a PK1 loss.
   * The header says "a condition wake and a timer", but both wakers run the same function (lines 76-95). A timer
     does not look at the registration and notifies under the run-queue lock.
   * Steps 2 to 5 (condition lock, re-check, cancel) and step 9 with a second wait are absent, so cancel racing
     notify and a stale waker from the previous wait are unexplored.
   * The model drives `Park` through a hand-copied protocol, not `Sched::wake_on`, which does something different
     (it cancels a blocked thread's word and never CASes it, `sched.rs:558`).
   * `is_caught` (line 136) is `catch_unwind(..).is_err()`: a loom branch-limit or deadlock panic would pass.
   The three controls do fail for the claimed reason today (under loom each panics on the PK1 assertion at line
   123), and I traced a losing interleaving for each by hand.
   Fix: check that the panic payload contains "PK1"; add an enqueue counter with the two PK2 assertions and make
   control 3 the spec's; add a real timer waker and a second wait round; correct the INVARIANTS wording until then.

6. **Low. `Held` is looser than the table in three places.** (a) `check` allows any number of `RQ` locks in
   ascending order (three pass; 3.3 says two) (`klock/src/lib.rs:140`). (b) `pop` of a lock that is not held is a
   `debug_assert` only, and is silent in every build for `Legacy` and `Uart` (`lib.rs:167-172`), so a double
   release of the big lock goes unseen by the checker. (c) `note_acquire` pushes even when `check` failed
   (`kernel/src/bkl.rs:50-54`), which leaves an unsorted stack while `check` assumes the top is the maximum; it
   matters only with panics disabled in `order_check`, which restores the stack.
   Fix: count same-class entries in `check`; make the non-reentrant case of `pop` an `assert!`; skip the push on a
   violation.

7. **Low. The `Spin` models have no negative control, although the commit message of 533abca says so.**
   `klock/tests/spin_concurrency.rs` has two positive tests. Nothing shows that the read, yield, write body would
   catch a lock that does not exclude.
   Fix: add a control (for example a variant that skips the lock) and assert it is caught.

8. **Low. Paperwork.** No `TIDY-UP.md` entry for the stand-ins this slice leaves: `Sched::wake_on` is not the
   atomic protocol, `Waiters` is kernel-side, `HartRq` has no `cancel_block`. The module comment of `sched.rs`
   keeps a paragraph headed "the remainder of the old text ... no longer applies" (`sched.rs:24-26`); delete it.
   3.4 says the kernel's `lock_order_violations_are_detected` covers every adjacent pair; it covers ten pairs and
   the full table is host-only (`klock/tests/order.rs`), which is the better place, but the spec should say so.
   `Machine::hart` returns `u32` where 3.4 says `usize`.

## Checked and found sound

* **`Held::check` for every pair.** The stack stays sorted because a push happens only above the top and a removal
  keeps order, so comparing with the top is comparing with everything held. Reentrant takes: `Legacy` under a
  later class is refused, `Uart` over `Uart` is allowed, neither is pushed twice, and the big lock pops only at
  depth 1 (`bkl.rs:177-179`). Out-of-order release keeps the remaining entries and the rule. Depth nine panics.
* **Checker wiring.** Every `HeldCell::with` runs with interrupts off: `bkl::lock` and `try_lock` assert it, the
  console and `Spin` disable first, `violates` has no caller outside `bkl.rs`. The closures take no lock.
* **The big lock following the thread.** The hart holds `Legacy` across every switch and only the depth moves
  (`thread.rs:442-447`), so the per-hart stack stays `[Legacy]`; a new thread releases at depth 1
  (`thread.rs:321`); no switch happens with `Heap` or `Uart` held. Same behaviour as the old bit mask.
* **Heap lock contexts.** `percpu::bind` runs before `heap::init` (`main.rs:79, 111`). An allocation from an
  interrupt handler cannot nest because the lock is taken with interrupts off. The panic handler
  (`main.rs:236-241`) breaks the console lock and prints without allocating; `Uart` is the last class and
  reentrant, so the checker cannot fire again on that path. A recursive take is caught by the checker first and
  by the `held_by` assert second, never by the ticket's reentrant branch (which would hand out two `&mut`).
* **`Spin`.** `lock` order (interrupts, checker, ticket) and `Guard::drop` order (checker, ticket, interrupts) are
  right; a failed `try_lock` restores interrupts and tells the checker nothing. `Sync`/`Send` for `Spin` itself
  with `T: Send` are correct. Hart identities in the loom and shuttle tests are per model thread and reset per
  iteration.
* **No dependence on the token.** Every kernel wait re-checks an authoritative condition under the big lock before
  blocking: `wait_deadline_registered`, `wait_until`, `join`, `sleep_ticks` (loops on the clock). Kill wake-ups
  (`wake_threads_of`, `user.rs:1632`), `futex_wake`, the object commit (`user.rs:1497`) and `wake_many` either find
  the thread blocked or find it about to re-check (`killed_now()` is in each `ready`). The change removes spurious
  early returns; it cannot lose one.
* **Park word after each transition in `Sched`.** Wake of a blocked thread, timer expiry and exit all leave `Idle`;
  a passed deadline leaves `Idle`; a thread preempted or yielding while `Prepared` keeps it and a later wake
  notifies it (probe run). `Park::take_notified` being a load then a store is safe: once `Notified`, only the
  waiter changes the word.
* **Negative controls 1 to 3** each have a real losing interleaving and fail on the PK1 assertion.
