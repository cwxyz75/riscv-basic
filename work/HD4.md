---
id: HD4
kind: slice
title: No ambient debug calls; a release kernel with no test-only code
area: security
horizon: long
blocked_by: [KT1]
refs: [KERNEL-V1-SPEC, OD139]
---

**How.** KERNEL-V1-SPEC 3.8. (1) A new object kind DebugResource; the kernel-started process receives one in its bootstrap message; `debug_write(debug, ptr, len)` and `debug_stat(debug, which)` require it (INSPECT for `stat`, WRITE for `write`). `init` passes it to the suite and to the programs that print `##TEST` lines; everything else loses the calls (their console output already goes through `consoled`). This closes the `debug_*` half of T2 and T6. (2) Release content: `selftest.rs`, `user_tests.rs`, `smp_tests.rs`, `sched_tests.rs`, `interactive.rs`'s test aids, every `fault-*` feature, the `EXPECT` step-over hook in `trap.rs`, `ff=`, `aslr=off`, `kaslr=off`, the `chaos` hooks and the test finisher are compiled only under a `test-kernel` feature that the release build does not enable; a build check lists the symbols of the release kernel and fails if any name from a deny-list (`selftest`, `expect_refused`, `fault_`, `chaos`) is present. (3) The release kernel runs the `k` shard (KT1) from a separately built `init` suite, so it is tested without containing tests.

**Acceptance, written first.** A confined program's `debug_write` without the handle is BAD_HANDLE and prints nothing (it can no longer forge a `##TEST` line: extend `s4a`'s forgery test). The symbol check fails on a planted `expect_refused` reference.

**Review.** Fable.
