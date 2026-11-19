---
id: EX2
kind: slice
title: Exception channels: faults delivered to a handler, `exception_resume`
area: kernel
horizon: long
blocked_by: [EX1]
refs: [KERNEL-V1-SPEC, OD137]
---

**How.** KERNEL-V1-SPEC 3.6. `process_exception_channel(process)` and `job_exception_channel(job)` (MANAGE): each returns the user end of a new channel whose other end the kernel holds; a second call while the first channel's user end is open is BAD_STATE. On a user fault the kernel cannot resolve, an illegal instruction, a breakpoint, or a pager failure: the thread enters the Exception state (a stopped state: state access allowed), an Exception object is created (charged to the process's job), and a message `{kind, tid, pc, address, cause}` (40 bytes) with two handles (the thread: INSPECT, MANAGE, WAIT; the exception: its own rights) is written to the process's channel; if there is none, its user end is closed, or the write fails, the next level is tried (the process's job, then each ancestor); with none, the process ends as today (`-1000 - scause`). `exception_resume(exception, how)`: RETRY returns the thread to user mode with its (possibly rewritten) state; NEXT tries the next level; KILL ends the process with the usual code. Closing the last handle to the exception object is NEXT. A killed process's pending exceptions are discarded. Kinds: PAGE_FAULT (with the access type), ILLEGAL, BREAKPOINT, MISALIGNED, PAGER, ACCESS (a touch beyond a shrunk VMO). An exception taken by the thread that is itself reading the process's channel is not special: the message waits, nobody reads it, and the supervisor's job-level channel is the remedy; say so in `SYSCALLS.md`.

**Acceptance, written first.** Host model of the delivery state machine (each exception resolved exactly once: retried, passed on or killed; none left when the process dies). QEMU: a handler thread in the same process catches a write to a read-only page, makes it writable with `process_protect`, RETRY succeeds (the SIGSEGV-handler pattern); a handler in the parent process catches a child's illegal instruction, reads the pc, kills it; NEXT reaches the job-level channel; closing the exception handle is NEXT; a process with no channel ends with the same code as before (the eight fault kinds of P3 still pass); killing a process with a pending exception leaks nothing.

**Review.** Fable, soak.
