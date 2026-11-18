# Review: scheduler task 8, priority authority (PR #132, branch sched-t8 at 1d2be27)

Independent security review, 2026-11-14. Scope: `git diff main...sched-t8` against SCHED-SMP-SPEC 4.9, INVARIANTS
PR1/PR2, SYSCALLS `job_set_sched` / `thread_set_sched` / `thread_get_sched`. Read-only. Run:
`cargo test -p kcore --test job_sched` (2 passed). Nothing was booted: every kernel finding below is from reading the
code, not from a QEMU reproduction.

## Confirmed bugs

### 1. High: a thread that has been created but has not yet run escapes the clamp (PR1 broken)

`kernel/src/user.rs:2982-2997` (`job_set_sched`) clamps by walking `s.thread_pid`. A thread enters `thread_pid` only
when it first runs (`user_thread_main`, `user.rs:397`); until then it exists only in `s.starts` and in the scheduler,
already carrying the priority computed at `user.rs:2612` / `2712`.

* Input A (no attacker needed): supervisor does `process_start(p)` then `job_set_sched(job, 5, ..)`. The first thread
  was spawned at 10, has not run (the supervisor did not block), is not in `thread_pid`, and keeps 10 forever.
* Input B (confined program): keep calling `thread_create`; each new thread sits runnable at the creator's priority
  (equal priority does not preempt). When the supervisor lowers the job, every pending thread keeps the old priority.
* Impact: base priority above `max_prio` indefinitely; "lowering clamps every live thread at once" is false.
* Fix: re-clamp at registration in `user_thread_main` (under the BKL, against the job's current `max_prio`), or record
  the tid in `ThreadStart` / the thread object at spawn and have the clamp walk `starts` too. The suite test only
  lowers after the child has reported in, so it cannot see this; add a case that lowers right after `process_start`.

### 2. High (policy hole opened by this PR): every confined program can now take priority 63

`kcore/src/job.rs:225` gives a new job its parent's limits and nothing outside `init/src/bin/suite.rs` calls
`job_set_sched`, so every job has the root's 63. Before this PR no program could change its priority; after it,
`thread_set_sched(0, 63, 0)` succeeds for any program the shell launches. One spinner per hart then outranks the
reaper (`REAPER_PRIO` 50, `kernel/src/thread.rs:816`), the serial bridge (20) and every server (10): total denial of
service, and exited threads are never reaped.

* ROADMAP admits the launcher policy is not done but says "programs still run at 10", which is no longer a bound.
  TIDY-UP T73 says "the launcher grants none", which is untrue on this branch. No TIDY-UP entry records the shortcut
  (CLAUDE.md requires one in the same PR).
* Fix (pick one, in this PR): the launcher sets `max_prio` 20 / budget 0 on program jobs and init 31 on the session
  (4.9 policy); or non-root jobs start at `min(parent, 31)` with `rt_budget` 0 until the policy lands. At minimum a
  TIDY-UP entry and a corrected T73.

### 3. Medium: the clamp loop can context-switch part-way, so it is not atomic

`user.rs:2995-2997` calls `thread::set_priority` per thread, and that calls `reschedule()` every time
(`kernel/src/thread.rs:528-533`). If the caller is itself in the subtree (a holder of SET_LIMITS on its own or an
ancestor job, for example init on the root), clamping the caller makes any runnable thread above the new maximum
preempt it mid-loop. Threads with a larger tid (the map iterates in tid order) stay unclamped while running on other
harts; with a mid-priority spinner present the caller may never resume to finish. A pending reschedule IPI gives the
same mid-loop switch for any caller, transiently.

* Fix: apply all changes with `sched.set_priority_on` in one critical section and call `reschedule()` once after the
  loop (a `thread::set_priorities(&[(Tid, Prio)])`).

### 4. Medium-low: `thread_set_sched` / `thread_get_sched` on a fresh handle answer BAD_STATE

`thread_target` (`user.rs:2900-2909`) finds the tid through `p.thread_objs`, filled only when the thread first runs
(`user.rs:403`). On one hart `thread_create` followed at once by `thread_set_sched(handle, ..)` always fails, because
the new thread has the creator's priority and has not run. SYSCALLS documents "has not started", but it defeats the
normal create-then-set pattern, and SCHED-SMP-SPEC lists no BAD_STATE for `thread_get_sched`. Same root cause as 1;
storing the tid at spawn fixes both.

### 5. Low: `max_prio` 0 violates PR1 as written

`clamp_prio` (`user.rs:2871-2874`) and the clamp (`user.rs:2992`) floor at 1, so a job with `max_prio` 0 holds threads
with base 1. Either refuse 0 in `job_set_sched` (range 1 to 63, spec and SYSCALLS changed) or state the floor in PR1.

### 6. Low: error order is inconsistent, and the root's budget is unbounded

* `job_set_sched` (`user.rs:2956`): `max_prio > 63` and `cpu_weight > 65536` answer INVALID_ARGS before the handle is
  looked at, but `cpu_weight == 0` is found in `kcore` after BAD_HANDLE / WRONG_TYPE / ACCESS_DENIED. So a handle
  without SET_LIMITS gets INVALID_ARGS for weight 65537 and ACCESS_DENIED for weight 0. `thread_set_sched` also
  checks arguments before the handle, while SYSCALLS lists BAD_HANDLE first. Decide one order and document it
  (the process calls use an explicit "order:" line).
* The root has no parent, so `job_set_sched(root, 63, u64::MAX, ..)` stores a budget above the 100 ms window
  (`job.rs:311-315`). Cap `rt_budget` at the window for every job.

## Checked and found sound

* Truncation: `wide(0)` rejects handles above u32 before any cast; `a1 as u8`, `a2 as u8` and `a3 as u32` are all
  cast after range checks.
* Rights: a duplicate without MANAGE / INSPECT / SET_LIMITS is refused (`thread_of`, `job_of`). A handle to a thread
  of another job's process is bounded by the *target's* job. Thread 0 is bounded the same way.
* A thread ending between lookup and set: one BKL critical section, and tids are never recycled
  (`kcore/src/sched.rs:328`), so no stale tid can be hit.
* A process never changes job. A job created under a lowered parent inherits the lowered values. Raising a child is
  checked against the parent's current values.
* `kcore::job::set_sched` with recycled ids: a freed slot is `None`, `is_within` on it is false, and a live record
  cannot have a freed ancestor (use counts). Correct by reading; note the random walk in `kcore/tests/job_sched.rs`
  never releases a job, so recycling is only covered by the one hand-written case.
* `job_info` selector 6 needs INSPECT and returns only that job's two values.
* No user thread can reach priority 0.

## Suspicions and forward notes

* `creator_prio` and the clamp read `sched.priority()`, the single priority field. When donation lands (task 22) these
  must read the **base**, or a donated creator passes its boost to a child (still clamped to the job, so bounded) and
  the clamp would mis-compare.
* `thread_get_sched` returns the global, monotonically rising kernel tid: a small cross-job information channel
  (system-wide thread creation rate). Specified, but worth an owner decision.
* The clamp builds a `Vec` over all user threads under the BKL; the "chunked (3.12)" of 4.9 is not implemented.
