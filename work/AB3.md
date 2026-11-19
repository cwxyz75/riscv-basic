---
id: AB3
kind: slice
title: Identifiers that are never reused become 64 bits; no counter wraps (CNT1); counter fast-forward tests
area: kernel
horizon: long
blocked_by: [AB1]
refs: [KERNEL-V1-SPEC, OD130]
---

**What.** KERNEL-V1-SPEC 3.3. Today: thread ids are `AtomicU32::fetch_add` (wraps, `kcore/src/sched_smp.rs:219`); process ids are `u32` `+= 1` (`kernel/src/user.rs:302`, `:2502`: panic in debug, wrap in release); registry generations use `wrapping_add` (`kcore/src/object.rs:336`, `:414`); the koid is `generation << 32 | index`.

**How.** `Tid` is `u64` throughout `kcore` and the kernel (`thread_spawn` and `thread_get_sched` return it in a full register; the start state's `a1`). The process id is the process object's koid. The koid is a `u64` counter from 1 kept by the registry and stored in the slot at creation; `ObjId` keeps `index` and a `u64` generation. FP and vector state tokens: confirm 64-bit. Every remaining counter in the kernel crates is listed in the PR (name, width, what happens at the limit); none uses `wrapping_*` or an unchecked `+= 1` on a width below 64 bits. Test builds accept `ff=<name>:<value>,...` on the kernel command line for `tid`, `koid`, `objgen`, `handlegen`, `fptoken`.

**Acceptance, written first.** Boots of the `k` shard with `ff=tid:0xffff_ffff_ffff_fff0`, `ff=koid:...` and `ff=objgen:...` pass, and tests added for the purpose create 64 threads and objects across the old 32-bit boundary (`ff=tid:0xffff_fff0`) and check that ids stay distinct and joins work. A host test per counter. INVARIANTS gains CNT1 with these checks.

**Review.** Fable (the width change touches the scheduler's atomics and the thread table).
