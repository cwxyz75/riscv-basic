---
id: KT3
kind: slice
title: Panic-finding lints, overflow checks and `unsafe` documentation as errors in the kernel crates
area: testing
horizon: near
refs: [KERNEL-V1-SPEC, OD139, OD140]
---

**What.** KERNEL-V1-SPEC 7.4: no panic a program can cause. Make the compiler find the candidates.

**How.** In `kernel`, `kcore`, `klock` and `abi`: `#![deny(clippy::undocumented_unsafe_blocks)]` everywhere; `clippy::unwrap_used`, `expect_used`, `indexing_slicing`, `arithmetic_side_effects`, `panic`, `unreachable` denied in the syscall and trap path modules (`kernel/src/user.rs`, `thread.rs`, `trap.rs`, `kmem.rs`, `kstack.rs`, `heap.rs` and all of `kcore` outside `#[cfg(test)]`). Each existing hit is fixed (a checked operation returning a `Status`) or, where the invariant is real and a panic is the right response to a kernel bug, allowed on that line with a one-line reason (`#[allow(...)] // INVARIANT: ...`). Boot-only code (`main.rs`, `boot.rs`, self-tests) is exempt by module. `overflow-checks = true` in the kernel's release profile. The kernel crates join the clippy line of `ci.yml` (today it lists only the pure crates; run it from inside `kernel/` as the Makefile's `lint` does, and make `lint` and CI agree).

**Acceptance, written first.** `make lint` fails on a planted `unwrap()` in `user.rs` (checked by hand once, as a mutation check). The count of `#[allow]` lines with their reasons is printed by a script (`scripts/kernel_allows.py`) and recorded in the PR; every one names an invariant. The suite passes at 1, 2 and 4 harts in test and release builds.

**Review.** Fable (the allowed lines are the review's subject). Soak.

**Size.** Large by line count (expect several hundred sites); do it in PRs by file, `kcore` first.

## Progress (2026-11-18)

Part 1 (kcore) built: the panic-finding lints are `deny` in `kcore` outside tests, and every one of the 141 remaining sites carries `#[allow] // INVARIANT: <why>` (`scripts/kernel_allows.py --list` prints them and fails on one without a reason; `scripts/clippy_hits.py CRATE --by-fn` lists the hits of a crate). A planted `unwrap()` in `frame.rs` fails `make lint` (checked once). The frozen `sched_smp_v0` copy and its differential test were deleted (L11b changed the behaviour it froze). Left: `klock`, `abi`, the kernel modules `user.rs`, `thread.rs`, `trap.rs`, `kmem.rs`, `kstack.rs`, `heap.rs`, `undocumented_unsafe_blocks` in the kernel, `overflow-checks = true` in the kernel's release profile, and the Fable review of the allowed lines.

Part 2 built: the same lints are denied in `klock` and `abi` (crate level) and in the kernel's `user.rs`, `thread.rs`, `trap.rs`, `kmem.rs`, `kstack.rs` and `heap.rs`; `undocumented_unsafe_blocks` is denied in the whole kernel crate (one SAFETY comment moved next to its block); the kernel crates keep `overflow-checks` in the release profile; CI's `kernel (ktest)` job runs `make lint-kernel` and the host job's clippy line gained `klock`. 210 allowed sites in all (kcore 141, klock 4, kernel 65), each with its reason. The kernel-only boot in the release build and `make kquick` pass. Left: the Fable review of the allowed lines (the item's review), then close.

