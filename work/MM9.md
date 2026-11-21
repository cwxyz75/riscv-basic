---
id: MM9
kind: slice
title: Pager-backed VMOs: page faults that block, `pager_create`, `pager_create_vmo`, `pager_supply`, `pager_fail`
area: kernel
horizon: long
blocked_by: [EX2]
refs: [KERNEL-V1-SPEC, OD133]
---

**How.** KERNEL-V1-SPEC 4.4. New object kind Pager; `pager_create() -> (pager, channel end)` (the kernel keeps the other end, like the bootstrap channel's `KernelEnd`); `pager_create_vmo(pager, key, bytes) -> vmo` (rights without COMMIT; `vmo_commit` on it requests pages). Pure state in `kcore::pager`: per VMO a map of requested ranges to waiters. Fault path (KERNEL-V1-SPEC 4.5): the family returns `NeedPager { range }`; the thread registers as a waiter under the family lock (park protocol: prepare first), a request message `{key, offset, length, kind = READ}` is queued unless an equal one is pending, all locks are dropped, the thread blocks; on wake it retries the fault from the top. The wait ends on supply, fail, pager destruction, kill, or `thread_interrupt` (a fault retried after an interrupt re-queues). Kernel copies that hit an absent page (`vmo_read`, user copies into pager-backed mappings) take the same path and return the failure status. `pager_supply(pager, vmo, offset, length, source, source_offset)`: the source is an anonymous non-child VMO with those pages committed; the frames move (no copy), the charge moves to the pager VMO's job, waiters wake. `pager_fail(pager, vmo, offset, length, status)`: waiters see exception kind PAGER (or the status, in a kernel copy). Requests are charged to the pager's job; if the channel is full the waiter stays registered and the request is queued when room appears. PRIVATE children of a pager VMO work (the child's read faults request from the parent's pager). Dirty tracking is reserved: request kind 2 and the names `pager_query_dirty`, `pager_writeback_done` appear in `abi` as NOT_SUPPORTED.

**Acceptance, written first.** `kcore/tests/pager_model.rs` (requests deduplicated; every waiter woken exactly once by supply or fail; no waiter left when the pager dies), loom and shuttle on fault against supply against kill with the park protocol (negative control: registering after releasing the family lock loses a wake). QEMU: a pager thread serving a 1 MiB VMO from a pattern, a reader in another process sees the pattern; kill the reader while it waits (nothing leaks: PG1); kill the pager while a reader waits (the reader gets the exception or ends); a slow pager does not stop other processes' faults (a latency probe). The lock-order checker shows no lock held at the block.

**Review.** Fable, soak. Unblocks the user-space half of `mmap` (S11-MMAP).
