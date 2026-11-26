---
id: IOMMU6
kind: slice
title: Device-domain objects and calls with the rights split: a driver maps its VMOs for its device, unmap and flush before any frame is freed
area: drivers
horizon: long
complexity: H
change_risk: H
epic: iommu-kernel
blocked_by: [IOMMU3, IOMMU5]
refs: [T42, OD148, OD162, OD164, OD166, OD168, OD169, IOMMU-SPEC, SYSCALLS, INVARIANTS, T262, T177, T273, T274, T275, FZ1]
---

Child 6 of the IOMMU epic (`work/T42.md`): the kernel half of the mechanism. It wires `kcore::iodomain` (IOMMU3) to the IOMMU
driver (IOMMU5) behind the calls IOMMU-SPEC 4 defines. Feature PR on `epic/iommu-kernel`, followed by IOMMU7. No driver is
converted here: the test device is the only user.

**Revised 2026-11-25** (Q-IOMMU-IRQ answered B; this was IOMMU5 before the renumbering): it is no longer the last PR of the
kernel epic (IOMMU7, MSI translation and vector grants, follows and its smoke merges the epic). What this slice prepares for it:
the domain object's rights include the **grant** right bit from the start; the device context is written in the extended format
with an empty MSI page table and the mask and pattern; the quarantine context has `msiptp` off; `destroy` runs the phases in the
order IOMMU-SPEC 6.3 and 10 give, with the vector step empty until IOMMU7.

**Added 2026-11-26** (Q-IOMMU-WORKBOUND and Q-IOMMU-NONLEAF answered: OD168, OD169, IOMMU-SPEC amendment 2026-11-26).
This slice builds the stepped work of OD168: map, unmap and the destruction's completion take `IOMMU` once per `RANGE_STEP`
pages of work, a map in progress gives way to a destruction, and `vmo_create_dma_in` zeroes in steps outside `VMO`; the
acceptance table gains host checks for each, including a mapping and a domain far larger than one step. It also adds the
page-walk cache of OD169 (assumption A18) to `iommucore::sim`, with a mutation check that a page-wise `inval_for` where a
table frame was freed fails a test.

**The rights split** (Q-IOMMU-MODEL 3, OD164; IOMMU-SPEC 4.1). `iodomain_create` returns the control handle, which the
**supervisor** keeps: IO_MAP, IO_UNMAP, IO_GRANT, IO_DESTROY, DUPLICATE, TRANSFER, WAIT and INSPECT. The **driver** is given a
duplicate with **IO_MAP and IO_UNMAP only**: it maps VMOs it already holds with the `DMA` right into its own device's domain,
learns the I/O addresses from the call, and its job pays; no buffer goes through the supervisor. It cannot destroy the domain,
grant a vector, wait for or read a fault. In this slice the suite plays both roles and every test below says which handle it
uses.

## Plan (2026-11-25)

### The acceptance check, written and seen to fail before the code

| Check | Where | What must be seen |
|---|---|---|
| `iodomain_maps_exactly_what_was_granted` | suite, shard `k`, at 1, 2 and 4 harts | Create a domain for the test device (the control handle); **through a duplicate with IO_MAP and IO_UNMAP only** (what a driver holds), map two pages of a DMA VMO (made with `vmo_create_dma_in`) read-write and one page of another read-only, then a hundred single-page mappings more, to show that no table of 64 bounds them. The device (commanded with the **I/O addresses** the kernel returned) writes a pattern that the test then reads through its own mapping, and reads back what the test wrote: **positive DMA through the IOMMU**. A device write to the read-only page, an access one page past each mapping, an access at the frames' physical addresses and at address 0 each fail in the device, leave memory intact and produce one fault record with the right address and direction |
| `iodomain_unmap_stops_the_device` | suite | After `unmap` returns, the same device access faults. Run with the device having just used the translation (so QEMU's translation cache holds it) |
| negative control `fault-iommu-skip-inval` | kernel feature: `unmap` clears the leaf and frees, without the invalidation and fence | `iodomain_unmap_stops_the_device` must **fail** on it (the device still reaches the frame through the cache). If IOMMU0 item 8 finds QEMU's cache does not serve the stale entry, say so plainly in the PR: the host model of IOMMU3 is then the only proof of the ordering, and this control is dropped rather than left as a test that cannot fail |
| `iodomain_outlives_its_makers` | suite | The T42 scenario, without a supervisor's good behaviour. The test is the supervisor (the control handle); a child process is the driver (the map and unmap duplicate, its DMA VMO): it maps, starts a slow device write, and is **killed**. First run, the supervisor lives (IOMMU-SPEC 10.2 step 1): the mappings stay with the domain and hold their VMO, nothing is freed, the device's write lands in the buffer and the page counts have not moved; then the supervisor destroys the domain and everything returns. Second run, the supervisor role is a second child and both are killed (10.3): the domain is destroyed by the last handle, the device is **Quarantined**; allocate and pattern-fill until the freed frames are reused, command a late device write at the old I/O address: it fails in the device, the pattern is intact, and **no fault is reported for it** (a quarantined device's context has fault reporting off: the fault counters and the last-fault record do not move, and there is no domain left to hold a record; assumption A14). Then the frame, job, mapping-record and I/O-page counts are back at baseline (`balanced()`) |
| `iodomain_faults_reach_the_supervisor` | suite | Faults of a device with a live domain: DEVICE_FAULT rises on the control handle; `iodomain_fault_read` returns the records oldest first with cause, I/O address and direction; after more than 16 unread faults the first 16 are returned and then the dropped count is right and resets; the signal clears when the ring is empty (SHOULD_WAIT). Through the driver's duplicate both the wait and the read are ACCESS_DENIED |
| `iodomain_rights_and_errors` | `ksys/tests/iodomain_calls.rs` (host), then `docs/reference/syscall-tests.tsv` regenerated | Every error of every new call in the order SYSCALLS.md gives: no root resource; a device id outside the tree's `iommu-map`; a second domain for a live device id (BAD_STATE, as `interrupt_create`); a VMO without the `DMA` right; a VMO that is not pinned (plain, device, pager-backed, a copy-on-write child); access wider than the handle's; over the job's I/O-page limit; the job's kernel bytes too short for the mapping's record; a full I/O address space. **Each right alone**: a handle without IO_MAP cannot map, without IO_UNMAP cannot unmap, without IO_DESTROY cannot destroy, without INSPECT cannot read faults; a driver's duplicate (IO_MAP and IO_UNMAP) can do the first two and nothing else, and cannot be widened by duplication. `iodomain_destroy` twice is OK; map on a destroyed domain is BAD_STATE. `vmo_create_dma_in`: its errors in order, a VMO of more than 64 pages succeeds, the pages are on the named job. `job_set_io_limits` and the two `job_info` selectors: both limits, and a child job refused at its parent's limit |
| fuzz and allocation failure | `ksys/tests/fuzz.rs`, `ksys/tests/allocfail.rs`, `kfuzz-gen` | The new calls are in the generators; map and create fail cleanly at every allocation point; unmap and destroy never fail on memory |
| leak oracles | suite (`no_process_or_vmo_survives_init`, the ledger check) | Count zero live domains, zero I/O-mapped pages and zero leaked holds at the end of a session |

### What is built

1. **ABI** (`abi/src/lib.rs`, `libsys`, `rt/src/lib.rs`, `kfuzz-gen`; the shuffled-number tables of AB8 all need the new numbers):
   the object type; its rights IO_MAP, IO_UNMAP, IO_GRANT and IO_DESTROY (bits 14 to 17; new right bits rather than `MANAGE`:
   OD159 splits `MANAGE`, T263) and the signal DEVICE_FAULT (bit 6); the calls of IOMMU-SPEC 4 and 7.4: `iodomain_create`,
   `iodomain_map`, `iodomain_unmap`, `iodomain_destroy` (the last handle destroys too) and `iodomain_fault_read`;
   `job_set_io_limits(job, io_pages, vectors)` with **both** limits and the two `job_info` selectors (the fields are IOMMU3's;
   the vector limit is charged from IOMMU7 on); and **the new call `vmo_create_dma_in(resource, pages, job)`** (IOMMU-SPEC 4.7,
   OD166 point 9). The two-argument `vmo_create_dma` is **not** changed in place, and it and `vmo_phys_addr` **stay working**
   until IOMMU12 (the virtio-mmio drivers still need both), where they become stubs.
2. **`ksys`** (`ksys/src/iodomain.rs` or in `vmo.rs`): handle resolution and rights under `OBJ`, then the phases of IOMMU3 in the
   lock order the spec fixed (IOMMU-SPEC 11.2): `VMO` (take the hold, read the frames), `JOBS` (charge), the mapping's record
   allocated with no lock held, then `IOMMU` (reserve, link the record, write leaves), with `FRAME` inside for table frames.
   Unmap: `IOMMU` (clear, queue, fence), the bounded wait **with the lock released** (tickets make that safe; the lock is never
   held across a wait), then `IOMMU` (complete, unlink the record), then `VMO`, `JOBS`, `FRAME` for the release and the record
   freed. Never `IOMMU` then `VMO`.
3. **Kernel** (`kernel/src/user/kobjects.rs`, `lifecycle.rs`, `arch/riscv64/iommu.rs`): the object's destructor runs `destroy`
   from handle close and from `teardown_objects` (`lifecycle.rs:220`); a process's teardown therefore detaches its devices
   **before** `teardown_memory` (`:294`) returns frames, and even that order is not load-bearing because the mapping holds the
   VMO. The command-queue submit and fence wait (poll `cqh`, bounded by a time read from the `time` counter; on expiry
   `abandon`, a counter, and one loud line). The fault path now names the domain, stores the record in the domain's ring of 16
   (or counts it as dropped when the ring is full) and raises DEVICE_FAULT (Q-IOMMU-MODEL 5, OD166 point 8); it allocates
   nothing.
4. **The quarantine state** for a destroyed domain's device (Q-IOMMU-MODEL 6): a kernel-owned empty table shared by all such
   devices, fault reporting off in the context.
5. **Reference documents in place**: SYSCALLS.md (the calls, rights, errors in order, the statistics), INVARIANTS.md (IO3 to IO8
   of the spec; M8 and H6 reworded for the third kind of holder; D4 "freed exactly once, after the last handle, region **and
   device mapping** go"; M3's oracle includes domains; J1 and J2 for the new charges), `docs/reference/syscall-tests.tsv`.
6. T262: strike its **T41** part's "per-job DMA quota" and "exclusive-claim record" (for DMA; the aliasing of physical VMOs stays
   open there) and note that **T84** (who pays for the network device's memory) is now possible and is done in IOMMU11 (T262
   carries a dated note for both since 2026-11-25).

### Invariants, locks, SMP, error paths

- **SMP.** There is one IOMMU and one command queue: no per-hart shootdown and no IPI. What the harts race on is the queue and the
  domain store, both under `IOMMU`. Two harts unmapping at once each wait for their own ticket. A hart taken offline while it
  waits is impossible (the wait is not a sleep); the IOMMU's interrupt sources are moved by IOMMU4's route code.
- **The wait.** No lock is held while a fence is waited for; the bound (100 ms) and what "bounded" means on a real device
  (the specification gives none) are in the spec (IOMMU-SPEC 6.4). On QEMU, if IOMMU0 item 8 confirms commands run synchronously on the tail
  write, the wait is one read.
- **Error paths that must leave nothing**: map failing after the hold was taken (drop it), after some leaves were written (clear
  them: they were never visible to a completed fence, but the device may have used them, so the roll-back is itself an unmap
  with a fence); create failing after the context was written; a handle table full after the domain exists.
- **A killed process mid-call**: every phase is a critical section; a ticket abandoned by a dying thread is completed by the
  destroy path, which fences everything.
- **Frames freed under a live translation** is the failure this slice exists to prevent. Headless diagnosis: the suite's pattern
  check (a late device write lands in a reused frame), the fault counters, and in debug builds an assertion in the frame
  database's `dec` path that a frame with a device-mapping mark is never freed (a mark kept in debug builds only).
- **The old plan's hazards that still apply** (from T42's 2026-11-24 text): a job freed with usage panics (`Jobs::release`); the
  J1 ledger formula counts index leaves; every leak test in the suite assumes free-at-close for DMA VMOs (true again here once
  nothing maps them, but check `device_caps_need_the_resource`, `dma_runs_are_allocated_and_freed_repeatedly`, `BlkRig::drop`).

### Contiguity and the new call (Q-IOMMU-ABI 3, OD162; OD166 point 9)

**This is the PR in which DMA VMOs stop being physically contiguous**, and it does so through the new call:
`vmo_create_dma_in(resource, pages, job)` makes a DMA VMO whose frames come from the ordinary allocator, with **no
`MAX_DMA_PAGES` limit and no `NO_MEMORY` for want of an aligned block**, charged to the named job, pinned, committed, zeroed and
fixed (the `kcore` side is IOMMU3's `create_dma_in`). Everything the epic builds uses this call from here on (the tests of this
slice, IOMMU8 to IOMMU11).

The two-argument `vmo_create_dma` is left exactly as it is in this PR, contiguous and limited to 64 pages, because its only
remaining users are the virtio-mmio drivers, which read one base address with `vmo_phys_addr` and so need contiguous frames;
`vmo_phys_addr` on a VMO made by the new call is NOT_SUPPORTED (it has no single address). Both old calls, `MAX_DMA_PAGES` and
the aligned-block path go in IOMMU12, when no such driver is left: the calls become library stubs returning NOT_SUPPORTED.

IOMMU3's note that `MAX_DMA_PAGES` equals `RANGE_STEP` stops mattering for the new kind: the batched free path is tested with
DMA VMOs of more than 64 pages, and the contiguity-reading test named in IOMMU12 is rewritten, not kept.

### Acceptance

Everything in the table passes in the epic's smoke at 1, 2 and 4 harts; the negative control fails as stated (or its absence is
explained); the feature gate is green; the leak oracles count zero. **Fable review** on its own PR against IOMMU-SPEC 4, 5, 6, 10,
11, 12, SYSCALLS.md and INVARIANTS.md (handle unforgeability, rights monotonicity, lock ordering, memory ownership). The epic is merged after IOMMU7, not here; a smoke may be started by hand on the epic after this PR as a checkpoint (it is the
riskiest memory change of the epic).

PR size: large (about 1,200 lines of kernel, `ksys` and ABI, 1,000 of tests). Shortcuts become tidy items in the PR. Already
recorded, not to be filed again: the polled fence (T274), 4 KiB leaves only (T273), one fence per call (T275).
