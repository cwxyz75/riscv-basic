---
id: AB8
kind: slice
title: Entry enforcement: an `ecall` from outside the system library is an exception; shuffled syscall numbers in soak
area: security
horizon: long
blocked_by: [AB7]
refs: [KERNEL-V1-SPEC, OD128]
---

**How.** In the user trap path, before dispatch: if `sepc` is not inside the process's recorded library text range, treat the trap as an illegal instruction (exception channel, else the process ends with the illegal-instruction code). The check is two compares on the hot path. The kernel-started first process is not exempt. A Cargo feature `abi-shuffle` permutes `abi`'s syscall numbers from a build-time seed (the same generated table feeds the kernel and `libsys`); `make os-boot-test-shuffle` runs the whole suite on it, as an overnight target. INVARIANTS gains ABI1.

**Acceptance, written first.** A test program with a hand-assembled `ecall` in its own text is ended (or its handler sees an illegal-instruction exception at that pc), and the kernel object counts are unchanged. Negative control `fault-skip-entry-check` makes that test fail. The suite passes with shuffled numbers for three different seeds.

**Review.** Fable.
