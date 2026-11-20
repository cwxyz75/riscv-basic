---
id: MM2
kind: slice
title: A buddy frame allocator with metadata sized from RAM
area: kernel
horizon: long
refs: [KERNEL-V1-SPEC, OD138]
---

**How.** KERNEL-V1-SPEC 4.1. A new pure module `kcore::buddy` (orders 0 to 18; free lists per order; split and merge by buddy address; one `u32` reference count, one order byte and two `u32` links per frame, in a caller-provided slice) and `FrameDb` reimplemented on it with the same interface (`alloc`, `alloc_contiguous(n)` rounding to an order and freeing the tail, `inc`, `dec`, `refs`, `audit`, `free_count`, `is_managed`), so the existing models run unchanged. It manages several ranges (PL4 supplies them; until then one). The metadata slice is carved from the first usable range at boot, before any allocator exists (`kmem::init` computes its size from the frame count); the two heap `Vec`s go.

**Acceptance, written first.** `kcore/tests/buddy_model.rs`: random alloc and free of mixed orders against a bitmap reference (no overlap, no double free, full merge back to the initial free lists at the end), mutation-checked (a merge that skips the buddy's order check). Every existing frame, aspace and device-VMO model passes on the new `FrameDb`. QEMU: the leak suites (`churn_does_not_leak`, `process_teardown_returns_every_frame`), and a DMA test that allocates and frees 256 contiguous pages repeatedly.

**Review.** Fable, soak.

## Progress (2026-11-20)

Built on `epic/phase-4`: `kcore::buddy` (orders 0 to 18, free lists with links in the per-frame `Meta`, split and merge by buddy address, alignment by physical frame number so MM3 can map big blocks as huge pages, `audit`, `free_blocks`); `FrameDb` reimplemented on it with the same interface plus `with_ranges` (several ranges over one metadata slice; PL4 supplies them) and `meta_len`; `kmem::init` carves the metadata (16 bytes a frame) from the first frames after the kernel image, outside the managed range, and the two heap `Vec`s are gone. `alloc_contiguous(n)` rounds up to an order and gives the tail back (the old first-fit scan could serve a run the buddy cannot: a run of 3 needs a free order-2 block). `kcore/tests/buddy_model.rs` runs random mixed-order allocation and freeing against a bitmap (no overlap, aligned, audit every step, full merge back to the initial free lists) and `FrameDb` over two ranges against a reference-count map; one mutation check (a merge that skips the buddy's order check: both models fail). Every existing frame, aspace and device-VMO model passes unchanged. QEMU: the new suite test `dma_runs_are_allocated_and_freed_repeatedly` (256-page runs, eight live, odd 37-page runs), and the leak checks at the end of the suite; `device_caps_need_the_resource` had asserted that a DMA address lies in the first 128 MiB of RAM, an artefact of the old allocator, and now accepts all of RAM. Verified: host tests of `kcore`; `make kquick` (236 tests at four harts, exit 0). Not run: smoke, the instruction counts (a page fault's allocation now takes a different path: the epic gate's `os-count` will say), soak. Per-hart caches of free frames (SCHED-SMP-SPEC 3.10) are not part of this item.
