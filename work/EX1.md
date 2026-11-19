---
id: EX1
kind: slice
title: `thread_suspend`, `thread_resume`, thread state read and write, `thread_spawn` SUSPENDED
area: kernel
horizon: long
blocked_by: [L19, AB3, PL2]
refs: [KERNEL-V1-SPEC, OD137]
---

**How.** KERNEL-V1-SPEC 3.6. A per-thread suspend count (checked where the kill flag is checked: every return to user mode). `thread_suspend(thread)` (MANAGE): increment; if the thread is running user code on another hart, send it a reschedule interrupt; the call returns at once. A thread whose count is non-zero at a return to user mode parks (the mechanism of `park_while_stopped`, per thread) and its object raises SUSPENDED (signal bit 7); a thread blocked in a syscall raises SUSPENDED immediately and, when its wait ends, parks instead of returning. `thread_resume`: decrement; at zero, wake. `thread_spawn` flag SUSPENDED (8): created with count 1. `thread_read_state(thread, kind, buffer, length)` / `thread_write_state`: MANAGE; allowed when SUSPENDED is raised, the thread never started, or it is in an exception; `kind` GENERAL = 32 words (x1..x31 then pc) from the saved user frame; FP = 32 words + fcsr; VECTOR = vtype, vl, vstart, vcsr, then 32 registers of `vlenb` bytes (the buffer length must match; `vlenb` is in the system data page and, before AB7, from `debug_stat`). Writing saves into the thread's frame or state buffer (allocating FP or vector state if absent); refused with BAD_STATE while the thread is inside a blocking call; the pc must be below `abi::user::TOP`. A process suspend (`process_suspend`) is unchanged and independent.

**Acceptance, written first.** Host: the suspend-count state machine in `kcore` against a model (a resume never loses a wake; suspend during a wait; kill while suspended). QEMU: suspend a spinning thread on another hart, read its pc twice (equal), write a register, resume, the thread observes the value; redirect a suspended thread's pc to a second function and back (the signal-injection sequence); a thread started SUSPENDED runs only after resume with registers written beforehand (the `fork` child sequence); write is refused on a running thread and on one blocked in `port_wait`; FP and vector state round-trip. At 1, 2 and 4 harts. INVARIANTS gains EX1.

**Review.** Fable, soak.
