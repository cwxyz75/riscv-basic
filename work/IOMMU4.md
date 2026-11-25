---
id: IOMMU4
kind: slice
title: Interrupt identities: a per-hart message-id allocator, more than 64 ids per hart file, routes instead of source + 1, kernel-owned sources
area: drivers
horizon: long
complexity: H
change_risk: H
epic: iommu-kernel
refs: [T42, OD148, Q-IOMMU-IRQ, Q-IOMMU-MSI, IOMMU-SPEC, INVARIANTS, SYSCALLS, T262, T14, KERNEL-V1-SPEC]
---

Child 4 of the IOMMU epic (`work/T42.md`). **New on 2026-11-25** (Q-IOMMU-IRQ answered B: MSI-X in the epic). It is the kernel's
interrupt-controller half of MSI-X and needs no IOMMU: it changes how a message identity on a hart's IMSIC file is chosen, opened
and routed, for the wired sources that exist today, so that IOMMU5 can hang the IOMMU's own interrupts on it and IOMMU7 the MSI
vectors. Feature PR on `epic/iommu-kernel`; it may run beside IOMMU2 and IOMMU3.

**It absorbs, from T262:** part **T59**'s "the message id equals the source number" and "a message still in flight to the old hart
when an object is destroyed and re-created"; part **T183**'s points 1, 2, 4 and 5 (a hart the IMSIC does not list is dropped
quietly, sources above 63 are dropped without a line, a missing `irq_objs` slot is ignored, the per-source allocations). Left in
T262: T183's points 3 and 6 (IMSIC groups, plain `virt`), T59's level-only sources and the unproven real APLIC. T262 carries a
dated note for each.

## Plan (2026-11-25)

### What is true today (read in the code, 2026-11-25)

- `kernel/src/arch/riscv64/aia.rs`: `eiid(source) = source + 1` (`:78`); `init_hart` opens ids with one `eie0` write, so only ids
  below 64 (`:186`); `claim` returns a 64-bit mask and **drops any id of 64 or more** (`:284`); `deliver` walks ids 2 to 63 and
  maps back with `id - 1` (`:309`); `aim_and_unmask` returns silently for a hart the IMSIC does not list (`:231`);
  `retarget_from` rewrites APLIC targets for an offline hart (`:259`, called from `kernel/src/thread.rs:542`).
- `kcore/src/aiatree.rs`: admits only sources 1 to 62 (`:138`), never reads `riscv,num-ids`, and refuses an IMSIC with guest
  files (`:104`).
- Interrupt objects are a table indexed by **source** (`ksys/src/state.rs:118`, `kernel/src/user/trap.rs:1364` to `:1393`,
  `kernel/src/user/wake.rs:8`).
- The machine (dumped tree, QEMU 11.1.3, `-smp 4`): the supervisor IMSIC has `riscv,num-ids = <0xff>` (identities 1 to 255 in each
  hart's file) and one page per hart at `0x2800_0000`; the APLIC has 96 sources.

### What is built

1. **`kcore/src/msiid.rs`** (pure, host-tested): the identity allocator and route table. Per hart: which identities exist (from
   `riscv,num-ids`, now read by `aiatree`), which are taken, and for each taken one a **route**: `Ipi`, `Kernel(k)` (a source the
   kernel owns), `Wired(source)` (an APLIC source behind an interrupt object), `Vector(v)` (an MSI vector, used from IOMMU7), or
   `Retiring(route)`. Operations: `alloc(hart) -> id`, `alloc_any() -> (hart, id)` (the hart with the fewest routes: the default
   placement), `move_to(hart)` (takes the new identity and marks the old one retiring), `retired(hart, id)` (the old hart
   confirmed its file holds nothing for it: only then is the identity free), `free`. Identity 0 is never given, identity 1 is the
   inter-hart interrupt on every hart. Allocation is next-fit, not lowest-fit, so reuse is delayed and the whole identity space
   is exercised. Sized at boot from the hart count and `num-ids`; no allocation after boot (the T177 rule
   for anything an interrupt path or a teardown path touches).
2. **Per hart, not global** (Q-IOMMU-MSI 2, OD165): each hart's file has its own identity space.
3. **The IMSIC driver** (`aia.rs`): `init_hart` opens nothing but the inter-hart identity; an identity is opened (`eie` bit set)
   on its hart when its route is installed and closed when it is retired, with the `eie` registers for all identities up to
   `num-ids` (on RV64 the even-numbered ones, 64 identities each). Opening an identity on another hart is that hart's own CSR
   write: done by the target hart when it is told (the existing inter-hart message with a new reason), never assumed. `claim`
   yields identities one at a time to a callback instead of folding them into a mask; an identity with no route is counted
   (`debug_stat`, new selector) and printed once.
4. **Wired sources through routes**: `interrupt_create` takes an identity for the source and writes it into the APLIC target word
   (hart index, identity), so `source + 1` is gone and a source number above 62 is as good as any (the limit becomes
   `riscv,num-sources`). `interrupt_ack`'s re-aim and `retarget_from` become `move_to` plus the target write. A hart the IMSIC
   does not list: `interrupt_create` fails with an error (SYSCALLS.md in place), not a silent success.
5. **The retire step** (what a proper IMSIC driver does: Linux's `irq-riscv-imsic-state.c` reads and clears the old identity's
   pending bit on the old CPU and re-sends it to the new one). After a move, the old hart, at its next claim or on the message
   that told it, reads and clears the pending bit of the retiring identity; if it was set the event is delivered to the route's
   object as usual; then the identity is closed and freed. An identity is **never** handed to another route while a message for
   its old owner can still arrive.
6. **Kernel-owned sources**: a `Kernel(k)` route is installed at boot for a source in a list that `irq_allowed` never contains;
   `deliver` calls the kernel handler for it. No device uses it in this PR (the host model covers the route kind); IOMMU5 adds
   the IOMMU's fault and command sources. (T14's plan no longer builds this hook: OD146.)
7. **Reference documents in place**: SYSCALLS.md (`interrupt_create`: the identity is the kernel's choice, the new error);
   INVARIANTS.md (the new row IO12 of IOMMU-SPEC 12: an identity has at most one live route, and is reused only after its hart
   retired it; D5 unchanged);
   KERNEL-V1-SPEC is not touched (IOMMU1 amended 2.4).

### Acceptance checks (written first)

| Check | Where | What must be seen |
|---|---|---|
| `kcore/tests/msiid_model.rs` | host | Random alloc, move, retire, free, hart offline against a reference map: no identity has two live routes; a retiring identity is never returned by `alloc`; every identity comes back; exhaustion on one hart spills to another or fails cleanly; identities 0 and 1 are never given. Mutations each seen to fail: free at move instead of at retire; `alloc` returning 1; the `num-ids` bound off by one |
| `kcore/tests/aiatree.rs` (extended) | host | Against the dumped tree: `num-ids` 255, sources up to `num-sources` admitted when an admitted device names them, including a fixture with a source above 63 |
| `interrupt_identities_above_63_work` | suite, shard `k`, 1, 2 and 4 harts | Enough interrupt objects are created (on the sources the machine has, re-created in a loop so the allocator walks the identity space) that an object is routed through an identity of 64 or more; it fires, masks and re-fires after acknowledgement (D5). Fails today: `claim` drops the id |
| `interrupt_object_fires_and_masks`, `device_interrupts_arrive_where_asked` | suite (existing) | Unchanged and green, with the misrouted counter (`debug_stat` 5) still 0 and the new no-route counter 0 |
| `interrupt_moves_do_not_lose_or_leak` | suite, 2 and 4 harts | An object acknowledged from alternating harts a few thousand times while its device keeps firing: every request completes, the count of free identities at the end equals the start (a new `debug_stat` selector), no-route counter 0 |
| negative control `fault-msiid-free-at-move` | kernel feature | The identity is freed at the move, not at the retire: `interrupt_moves_do_not_lose_or_leak` or the no-route counter must show it. Run once by hand (TESTING.md rule 9); if QEMU never leaves a message in flight (delivery is synchronous in the emulator), say so in the PR: the host model is then the only proof |

### Invariants, locks, how it goes wrong

- The claim path runs with interrupts off under `OBJ` as today (`kernel/src/arch/riscv64/trap.rs:644`, `:703`); the route table is
  read there and written by create, acknowledge and teardown, all under `OBJ`: no new lock class. When `OBJ` is split (OD149) the
  table needs its own rule; say so in INVARIANTS.md.
- A wrong `eie` register index or a 32-bit assumption shows as "the object never fires" for identities in one band only: the
  suite test walks every band.
- Opening an identity on a hart that is offline or parked: the request is queued for the hart's next start; the route is not
  usable before the hart confirmed.
- This is the trap path of every device interrupt: a regression is a hang of the block or network tests, visible in the first
  boot.

### Acceptance

The table passes in the epic's smoke at 1, 2 and 4 harts (say which ran); the feature gate is green. **Fable review** on its own
PR (trap path, kernel): against IOMMU-SPEC 8, INVARIANTS.md D5, K5 and the new row. A dated note goes under T262's T59 and T183
parts in the same PR (what is now done there).

PR size: medium to large (`msiid` about 400 lines and as much test, `aia.rs` and the object paths about 400).
