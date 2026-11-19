---
id: AB9
kind: slice
title: The contract written down: `abi-conformance`, `SYSCALLS.md` as the reference of the system library, the versioning rule
area: kernel
horizon: long
blocked_by: [AB8]
refs: [KERNEL-V1-SPEC, OD128]
---

**How.** (1) A program `abi-conformance`, built against `libsys` only (no `rt`: its own 100-line startup), that exercises every `sys_` function, every status each can return according to the reference, and every limit, printing `##TEST` lines. It runs in the `k` shard. (2) `docs/reference/SYSCALLS.md` is rewritten around the functions: the "Syscall numbers and registers" table goes (numbers are private), each entry keeps its preconditions, rights and errors, and the stale early sections are removed. `syscall-tests.tsv` (KT4) is regenerated against it. (3) The versioning rule is added to the reference: `sys_abi_version()`, additive only, a removed capability stays as a NOT_SUPPORTED stub, features are probed by calling, not by comparing versions. A host test fails when a symbol present in the previous release's symbol list (`libsys/symbols-v1.txt`, checked in) is missing or its argument count changed.

**Acceptance.** `abi-conformance` passes; removing a symbol fails the host test; the docs check finds every error in the reference in `syscall-tests.tsv`.

**Review.** Default. Takes up X-ABI-STABILITY for the kernel.
