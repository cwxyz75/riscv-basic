---
id: SC1
kind: slice
title: Sixty-four harts: hart index and hart id apart, per-hart records allocated at boot, 64-bit masks
area: sched
horizon: long
blocked_by: [PL5, L12]
refs: [KERNEL-V1-SPEC, OD126]
---

**How.** KERNEL-V1-SPEC 2.3. `MAX_HARTS = 64`. Harts are enumerated from `/cpus` (any ids, any order; the boot hart is index 0); `percpu` holds index-to-id and the SBI and IMSIC layers alone use ids (`shootdown`'s `1 << (id % 64)` mask arithmetic is replaced by a proper hart-mask builder that handles ids above 63 with the base argument). The static `BootStacks([[u8; 32 KiB]; MAX_HARTS])` and the per-hart arrays become allocations made at boot for the harts found (frames for stacks, with guard pages; `PerHart` records in one boot allocation). Every `[T; MAX_HARTS]` in `kcore::sched_smp` becomes sized at construction. Masks (`online`, `pending`, `would_run`, `ran_on`) are `u64`/`AtomicU64`.

**Acceptance, written first.** The `k` shard at `-smp 8`, `16` and `32` (soak; 16 and 32 overnight if slow under TCG: measure and say). `kcore` scheduler models run at 1 to 64 harts for the cheap properties and 1 to 8 for the random walks. A host test of the hart-mask builder with ids 0, 63, 64, 200.

**Review.** Fable, soak.
