---
id: MM2
kind: slice
title: A buddy frame allocator with metadata sized from RAM
area: kernel
horizon: long
blocked_by: [L12]
refs: [KERNEL-V1-SPEC, OD138]
---

**How.** KERNEL-V1-SPEC 4.1. A new pure module `kcore::buddy` (orders 0 to 18; free lists per order; split and merge by buddy address; one `u32` reference count, one order byte and two `u32` links per frame, in a caller-provided slice) and `FrameDb` reimplemented on it with the same interface (`alloc`, `alloc_contiguous(n)` rounding to an order and freeing the tail, `inc`, `dec`, `refs`, `audit`, `free_count`, `is_managed`), so the existing models run unchanged. It manages several ranges (PL4 supplies them; until then one). The metadata slice is carved from the first usable range at boot, before any allocator exists (`kmem::init` computes its size from the frame count); the two heap `Vec`s go.

**Acceptance, written first.** `kcore/tests/buddy_model.rs`: random alloc and free of mixed orders against a bitmap reference (no overlap, no double free, full merge back to the initial free lists at the end), mutation-checked (a merge that skips the buddy's order check). Every existing frame, aspace and device-VMO model passes on the new `FrameDb`. QEMU: the leak suites (`churn_does_not_leak`, `process_teardown_returns_every_frame`), and a DMA test that allocates and frees 256 contiguous pages repeatedly.

**Review.** Fable, soak.
