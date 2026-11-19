---
id: MM4
kind: slice
title: A slab kernel allocator with per-hart magazines; `linked_list_allocator` retired
area: kernel
horizon: long
blocked_by: [MM3]
refs: [KERNEL-V1-SPEC, OD138]
---

**How.** KERNEL-V1-SPEC 4.2. Pure core `kcore::slab`: size classes 16, 32, 64, 128, 256, 512, 1024, 2048; one frame per slab with a header (class, free count, free bitmap, list links); per-class partial and empty lists; a request above 2048 bytes is a power-of-two run of frames. The kernel half (`kernel/src/heap.rs`, rewritten): `GlobalAlloc` over it; a magazine of up to 32 free objects per class in `PerHart`, touched with interrupts off and no lock; the shared lists under the `HEAP` lock; frames from the buddy allocator under `FRAME` (which is below `HEAP` in the order, so the allocator may take frames inline: the "grow at safe points" machinery, `heap::service` and the boot/grown split go). Addresses are direct-map addresses. Empty slabs beyond a small reserve return to the frame allocator. Early boot before the frame allocator exists uses a 256 KiB bump region in `.bss` that is never freed. The `linked_list_allocator` dependency is removed from the kernel (OD138).

**Acceptance, written first.** `kcore/tests/slab_model.rs`: random alloc and free of mixed sizes against a reference that tracks every live block (no overlap, right alignment, class reuse, frames return when everything is freed), mutation-checked. Loom or shuttle on the magazine and shared-list handoff. QEMU: T-SPEC section 8's churn test (the heap grows and shrinks; frames return), every cap test still ends in NO_MEMORY, `ten_thousand_threads_leak_nothing`, the benchmarks before and after.

**Review.** Fable, soak.
