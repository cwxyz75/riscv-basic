---
id: KT8
kind: slice
title: Syscall fuzzing: on the host against the syscall layer, and in the guest
area: testing
horizon: long
refs: [KERNEL-V1-SPEC, OD139, OD140]
---

**How.** One generator crate `kfuzz-gen` (pure, `no_std`, seeded, no new dependency): it produces sequences of calls from a model of plausible state (live handles by kind, mapped ranges, threads), mixing valid calls, near-valid calls (wrong kind, missing right, off-by-one sizes, stale handles) and garbage, about 70/25/5. (1) Host: a test binary drives the syscall layer behind TS3's `Machine` with N sequences per seed, under Miri for a small N and natively for a large one; after every call the audits run (`Registry` reachability, `FrameDb::audit`, `Jobs` ledger equals the sum of holders, `Sched2::audit`), and no call may panic. (2) Guest: a program `kfuzz` in the boot image runs the same generator in child processes inside a small job, with the kernel's audits exposed through `debug_stat`; the parent checks after each batch that the audits pass and that killing the job returns frames, objects and ledger to baseline. Targets: `os-fuzz-host SEEDS=...` and `os-fuzz-guest SEEDS=...`, overnight.

**Acceptance, written first.** A planted defect is found by each within the overnight budget: an unchecked index in a handler (host: a panic; guest: `##PANIC`), and a missing credit on an error path (both: the ledger audit). A failing seed replays to the same failure.

**Progress (2026-11-20).** Part (1), the host half, is built: `kfuzz-gen`, `ksys/tests/fuzz.rs`, `make os-fuzz-host`, with both planted defects of the acceptance found on the host and replayed (TESTING.md, "Fuzzing the syscall layer on the host"). Not built: part (2), the guest `kfuzz` program in the boot image and `os-fuzz-guest`, and with it the guest halves of the acceptance; the calls that still live in `kernel/src/user.rs` (process, thread, wait, futex, `channel_call`) are not reached by the host fuzzer until TS3c moves them. **Review.** Fable (the generator's model decides what is reachable; a reviewer should attack what it cannot generate).

**Progress (2026-11-24).** Part (2) is built: `init/src/bin/kfuzz.rs` (the generator driving the real syscalls from a child process in a limited job; a duplicate of the job handle comes over the bootstrap channel), the kernel-side image op `F`, and the suite test `the_guest_fuzzer_leaves_nothing_behind` (seeds 1 to 4, job counters and kernel heap back to baseline after the job is killed). Both planted defects of the acceptance were found in the guest and replay by seed: an assert on event-signal masks in `ksys` (`##PANIC`), and an object charged twice in `event_create_in` (the job's object count stays up after the kill). Not built: an `os-fuzz-guest SEEDS=` target with a seed range from outside (the range is a constant in the suite); the process, thread, wait, futex and `channel_call` calls are not in the generator. Left open for those.
