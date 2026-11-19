---
id: KT4
kind: slice
title: Docs-check gates: atomics only in the audited layer (C5), and every syscall error names a test
area: testing
horizon: near
refs: [KERNEL-V1-SPEC, INVARIANTS]
---

**How.** (1) C5: `scripts/check_docs.py` (or a sibling run by `docs-check`) greps the kernel crates for `core::sync::atomic` and `AtomicU*` outside an allow-list of files (`kcore/src/sync.rs`, `klock`, `kernel/src/percpu.rs`, `latency.rs`, counters modules); the allow-list lives in the script with a reason per file. INVARIANTS C5's "TODO" is replaced by the check. (2) `SYSCALLS.md`: each call's entry lists its errors; a table `docs/reference/syscall-tests.tsv` maps `call, status, test name`; the check fails when an error in the reference has no row, or a row names a test that does not exist in `init/src/bin/suite.rs` or the host tests.

**Acceptance, written first.** Both checks fail on a planted violation (a new `AtomicU64` in `user.rs`; a deleted row), shown once by hand.

**Review.** Default. Expect (2) to find errors with no test: each gets a test in the same PR or a tidy item.
