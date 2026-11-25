---
id: SIG2
kind: slice
title: Signals phase 2: process suspend and resume, job control, CPU-time itimers, queued real-time signals
area: signals
horizon: long
---

- [ ] **Signals phase 2 (T149, OD122; S11)**: kernel suspend and resume of a process, then job control (Ctrl-Z, `fg`, `bg`, `jobs`, SIGSTOP, SIGTSTP, SIGCONT, SIGTTIN, SIGTTOU, `WUNTRACED`), CPU-time itimers (`ITIMER_PROF`, `ITIMER_VIRTUAL`) and queued real-time signals. Acceptance: a host-oracle C program (`libc/tests/c/signals.c`) and a shell test that stops a spinning program with Ctrl-Z and resumes it with `fg`, at `-smp 2` and `-smp 4`. Phase 1 (default actions, SIGPIPE, SIGHUP, the quit key) is built.

From the old work plan (retired 2026-11-25; git history)'s long-range list:

> a kernel `process_suspend`/`process_resume` (today `thread_suspend` and `thread_resume` are reserved and answer NOT_SUPPORTED; a process-level pair is what the shell can use, since it holds process handles) with a stopped state in the scheduler, a fresh-context review and the `soak` label; then, on top of it, Ctrl-Z and the real SIGSTOP, SIGTSTP and SIGCONT, `fg`, `bg` and `jobs` in the shell, SIGTTIN and SIGTTOU for background jobs that use the terminal, SIGCHLD for stops and `SA_NOCLDSTOP`, `waitpid` with `WUNTRACED` and `WCONTINUED`; per-process CPU time in `job_info` (selector 8 reads 0 today) for `ITIMER_PROF` and `ITIMER_VIRTUAL`; queued real-time signals (`sigqueue`, a value in the signal channel's message). Not in it: signals into a loop that never waits and handlers for faults (both need a kernel upcall; a separate, larger item).

## Progress (2026-11-18, later)

Shell job control built (OD124, T152 lists what it leaves): Ctrl-Z, `jobs`, `fg`, `bg`; Q-SH7 closed. The libc half is built too (kill of SIGSTOP and SIGCONT, `WUNTRACED`/`WCONTINUED`, SIGCHLD for stops, default actions; the host-oracle C test and the shell test pass). Still to do: CPU-time itimers (`ITIMER_PROF`, `ITIMER_VIRTUAL`: needs the kernel to measure per-process CPU time) and queued real-time signals (`sigqueue`).

## Progress (2026-11-18)

Kernel half built (OD123): `process_suspend` and `process_resume` (syscalls 57, 58). Next: `libc`/shell job control on top (Ctrl-Z, `fg`, `bg`, `jobs`, SIGSTOP/SIGTSTP/SIGCONT, SIGTTIN/SIGTTOU, SIGCHLD for stops, `WUNTRACED`/`WCONTINUED`), then CPU-time itimers and queued real-time signals.

## Note (2026-11-18, plan 4: the v1 kernel)

Plan 4 (OD141) parks this until the kernel is frozen (FZ1): it is user-space work that plan 3 left open, and it is not ordered now. Nothing in plan 4 depends on it.
