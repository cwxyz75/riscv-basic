---
id: KT5
kind: slice
title: Instruction-count checks of the main syscall paths
area: testing
horizon: long
refs: [KERNEL-V1-SPEC, OD140]
---

**What.** Wall-time benchmarks on QEMU move with the host (T75) and gate only at 3x. Under icount at one hart the guest instruction count of a path is deterministic.

**How.** A suite benchmark kind `##COUNT <name> <instructions>`: the test reads the `instret` counter (enabled for user mode through `scounteren` in test builds only) around N iterations of: `handle_duplicate`+`handle_close`, `channel_write`+`channel_read` of 64 bytes, a `channel_call` round trip between two threads, `event_signal`+`object_wait`, a page fault on a fresh page, `thread_spawn`+join, `futex_wake` with no waiter. `make os-count` boots the `k` shard with `-smp 1 -icount shift=0`; `bench/counts.tsv` holds the recorded values; `scripts/bench_compare.py` fails on a rise of more than 5% and prints falls. It joins `os-ktest`'s job if it fits the budget, else its own gate job.

**Acceptance, written first.** Two consecutive runs give identical counts. A planted extra loop of 1000 instructions in `channel_write` fails the comparison.

**Review.** Default.
