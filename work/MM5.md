---
id: MM5
kind: slice
title: All kernel memory charged to the job that causes it
area: kernel
horizon: long
blocked_by: [MM4, L16]
refs: [KERNEL-V1-SPEC, OD135]
---

**How.** KERNEL-V1-SPEC 4.7. `kcore::job` gains a level `KernelBytes`. Charged at creation and credited at destruction: kernel stacks (already pages: restated in bytes), page-table frames (exactly: `PageTable::map` reports frames it allocates; the estimate `length / 4096 / 512 + 4` and `table_charge` go), object bodies and their queued data by slab class size, handle-table slots as the table grows, wait registrations and port bindings, FP and vector state, pager requests. Mechanism: the syscall layer sets a "charging job" for the current operation and the allocator-facing constructors take it explicitly (`create_charged` generalised); nothing is charged implicitly by the allocator. The default `KernelBytes` limit of a job is 1/8 of its page limit unless set. Closes T1, T57, T81.

**Acceptance, written first.** `kcore/tests/job_kmem.rs`: random create and destroy of every chargeable thing against a recomputed sum (J5's method). QEMU: a confined program that makes objects, threads, mappings and handles until refused never pushes the root job's uncharged kernel memory up by more than a fixed slack (64 KiB), and killing its job returns the ledger and the allocator's in-use bytes to baseline.

**Review.** Fable, soak.
