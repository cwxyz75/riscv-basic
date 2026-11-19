---
id: MM1
kind: slice
title: A resolved page fault flushes one page, not the whole TLB; fault-around
area: kernel
horizon: near
refs: [KERNEL-V1-SPEC]
---

**What.** `kernel/src/user.rs:824` and `kernel/src/trap.rs:438` run `sfence.vma` with no operands after every resolved fault. KERNEL-V1-SPEC 4.5.

**How.** (1) After `handle_fault` succeeds: `sfence.vma <va>, <asid>` for the faulting page (the ASID field as `satp` has it; with `x0` for the kernel's own table). (2) Fault-around in `kcore::aspace::handle_fault`: after mapping the faulting page, for up to 15 following and preceding pages of the same region (stopping at the region's ends and at a 64 KiB aligned window), map those already committed in the VMO; never commit a page for it; each mapped neighbour takes its mapping reference as usual. A neighbour mapping that was absent needs no flush on RISC-V with Svade when the stale entry was invalid: confirm against the privileged specification, and if implementations may cache invalid entries, flush each mapped page.

**Acceptance, written first.** `kcore/tests/aspace_model.rs`: the model allows pre-mapped committed neighbours and still checks M1 and M3 (frame counts to baseline). A counter test: touching 64 committed pages of a fresh mapping in order takes at most 5 faults; touching 64 uncommitted pages takes 64. `##COUNT page_fault` (or the wall-time bench until KT5) before and after. The negative controls of SMP5 still fail.

**Review.** Default, soak. Small; do it early.
