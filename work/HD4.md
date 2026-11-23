---
id: HD4
kind: slice
title: No ambient debug calls; a release kernel with no test-only code
area: security
horizon: long
refs: [KERNEL-V1-SPEC, OD139]
---

**How.** KERNEL-V1-SPEC 3.8. (1) A new object kind DebugResource; the kernel-started process receives one in its bootstrap message; `debug_write(debug, ptr, len)` and `debug_stat(debug, which)` require it (INSPECT for `stat`, WRITE for `write`). `init` passes it to the suite and to the programs that print `##TEST` lines; everything else loses the calls (their console output already goes through `consoled`). This closes the `debug_*` half of T2 and T6. (2) Release content: `selftest.rs`, `user_tests.rs`, `smp_tests.rs`, `sched_tests.rs`, `interactive.rs`'s test aids, every `fault-*` feature, the `EXPECT` step-over hook in `trap.rs`, `ff=`, `aslr=off`, `kaslr=off`, the `chaos` hooks and the test finisher are compiled only under a `test-kernel` feature that the release build does not enable; a build check lists the symbols of the release kernel and fails if any name from a deny-list (`selftest`, `expect_refused`, `fault_`, `chaos`) is present. (3) The release kernel runs the `k` shard (KT1) from a separately built `init` suite, so it is tested without containing tests.

**Acceptance, written first.** A confined program's `debug_write` without the handle is BAD_HANDLE and prints nothing (it can no longer forge a `##TEST` line: extend `s4a`'s forgery test). The symbol check fails on a planted `expect_refused` reference.

**Review.** Fable.

## Progress (2026-11-22)

**Part (2), release content, is built** (PR on `epic/phase-11`): `selftest`, `user_tests`, `smp_tests` and `sched_tests` are compiled only without the `release` feature (the `Report` type moved to `report.rs`, `MAIN_PRIO` to `main.rs`), the `fault-*` features and `chaos` were already features, `ff=` already returns nothing in a release build, and `scripts/release_symbols.py` (run by `make os-release-smoke`) fails on a deny-listed name in the release kernel's symbol table (the debug-profile release-feature kernel: clean; the test kernel: 185 hits). What is not done: the feature is still called `release`, not `test-kernel` (the build without it is the test kernel: the same switch, inverted, left as it is), `aslr=off`/`kaslr=off` do not exist yet (HD2, HD3), the `EXPECT` step-over hook in `trap.rs` is a runtime flag that stays, and part (3), the release kernel running the `k` shard from a separately built suite.
**Part (1), the DebugResource object and `debug_write(debug, ..)`, `debug_stat(debug, ..)`, is open:** it changes both calls' signatures and every program that prints (`rt::println!` is in 12 programs of `init/src`, the netsup/netd family among them), so each needs the handle from its startup message or goes through `consoled`; the design (a `DEBUG_ROOT` call for the kernel-started process like `RESOURCE_ROOT`, a `KObj::Debug` with INSPECT and WRITE, `rt::debug` holding the handle, the launcher passing it to the programs that print `##TEST` lines) is next.


## Progress (2026-11-23)

**Part (3) is built** (PR on `epic/phase-11b`): the `release-suite` feature (`release` content, plus the embedded suite and `user_tests.rs`'s leak and fault checks, the test finisher and `rdinstret` for the suite), `make os-ktest-release-content` (the `k` shard at four harts, `OS_RELEASE_SUITE=1` drops the boot self-tests from the harness's required list), in the epic gate beside `os-ktest-release`. First boot: 226 tests, 0 failures, no fix needed. What remains of HD4 is part (1) (the DebugResource object) and the rename of `release` to `test-kernel`'s inverse; the release kernel proper (`release`) still embeds the suite ELF, since `build.rs` embeds every program; leaving it out of the shipped build is part of that rename.
