---
id: IOMMU3
kind: slice
title: kcore device domains: the map, hold, unmap-before-free, fault-ring and charging rules as a host model
area: drivers
horizon: long
complexity: H
change_risk: H
epic: iommu-kernel
blocked_by: [IOMMU2]
refs: [T42, OD148, OD164, OD166, IOMMU-SPEC, INVARIANTS, T262, T177]
---

Child 3 of the IOMMU epic (`work/T42.md`), and OD148's step (b): "a host model of the map, unmap-before-free and flush rules in
`kcore`, with the model and loom tests as the oracle". Second feature PR on `epic/iommu-kernel`. Nothing here runs in the kernel
yet: the store is built and tested on the host, and IOMMU6 wires it to syscalls.

**Note (2026-11-25, Q-IOMMU-IRQ answered B):** this slice stays the memory half. The vector half of a domain (grants, masking,
moves, identity lifetime) is modelled in IOMMU7 on top of this store; two things are prepared here so that slice only adds:
`create` writes the context with an **empty MSI page table** (one more frame, charged as kernel bytes) and the I/O address
allocator reserves the synthetic interrupt-file window (IOMMU7 design step 2), and `destroy`'s phases leave a place between "fence
complete" and "tables freed" where IOMMU7 retires identities. The spec fixes second-stage tables (IOMMU-SPEC 3.2), so the
invalidation is `IOTINVAL.GVMA` with the domain's GSCID, and a GSCID is one more thing the store allocates and frees after the
fence.

**Who calls what** (the rights split, Q-IOMMU-MODEL 3, OD164): in the running system `map` and `unmap` are called by the
**driver** through its own handle to its device's domain, and `create`, `destroy` and the fault read by the **supervisor**. The
store knows no rights (IOMMU6 checks them), but its model must cover what the split makes possible: an owner that maps and
dies while another owner still holds the domain (the mappings stay with the domain and keep their VMOs: IOMMU-SPEC 10.2
step 1), and a `destroy` by one owner racing a `map` or `unmap` by another.

## Plan (2026-11-25)

### What is built (`kcore/src/iodomain.rs`, plus changes in `kcore/src/aspace.rs`, `job.rs`, `limits.rs`)

1. **`IoDomains`**, the store (the state the new lock class protects): a table of domains keyed by device id (one live per id:
   the exclusive-claim record T262's T41 part asks for, for DMA), each with its I/O page table (`iommucore::iopt`), its I/O address
   allocator (`iommucore::iova`), its mappings, its **fault ring** (16 records and a dropped count, allocated with the domain:
   IOMMU-SPEC 7.4), its job, and its state (`Attached`, `Detaching`, `Quarantined`). **A mapping is a record of its own**
   `(iova, pages, vmo id, offset, access, the hold, the fence ticket, the links)`, allocated at map time and charged to the
   domain's job as kernel bytes; there is no table of 64 and no fixed number of mappings per domain (IOMMU-SPEC 4.3, OD166
   point 5). The records are linked into the domain intrusively (by I/O address for lookup, and into the pending and leaked
   lists), so linking, unlinking and walking them allocate nothing. The directory table (`iommucore::ddt`) and the command queue state
   (`iommucore::cmdq`) are singletons in the store.
2. **The operations**, each split into the phases the kernel will run under different locks, so the phase boundaries are what
   the concurrency tests explore:
   - `create(device_id, job)`: charge the job (kernel bytes for the object body with its fault ring, the root table, the MSI
     page table and the allocator's storage), write a `translate` context, queue the directory invalidation and a fence.
   - `map`: phase A under the VMO store (`Vmos::retain_for_io(vmo)`: only a `VmoKind::Dma` VMO, takes a hold like a region's and
     returns the frame list, which cannot change because the kind is fixed); then, with no store held, the job is charged (the
     I/O-mapped pages and the record's kernel bytes) and **the mapping's record is allocated**; phase B under the domain store
     (reserve an I/O range, link the record, prepay and write the leaves; on failure everything is rolled back, the record freed
     and the hold dropped). Returns the I/O address.
   - `unmap`: phase A under the domain store (clear the leaves, mark the range **pending**, queue `IOTINVAL` for the device and
     range and an `IOFENCE` with PR and PW, return a ticket); phase B is the wait (the kernel polls the queue head; the model's
     simulated device advances); phase C under the domain store (`complete(ticket)`: the range becomes free and the record is
     unlinked) and then, with the domain store released, under the VMO store (`Vmos::release`: the hold goes, the frames may be
     freed), and the record is freed (freeing cannot fail). Every phase uses only what the record owns: unmap allocates nothing.
     A fence that does not complete: `abandon(ticket)` leaves the range pending for ever, moves the record with its hold to a
     **leaked** list that is counted, and never calls `release`.
   - `destroy` (the `iodomain_destroy` call, or the last handle went): deny context, directory invalidation, translation invalidation for the whole domain, fence,
     wait, then every table frame back, every hold dropped, the job credited.
   - `fault(record)`: decode (`iommucore::faultq`), find the domain, count, store the record in the domain's ring (when the ring
     is full the record is not stored and the dropped count rises: the sixteen oldest unread are kept), and say whether the
     console line is due (rate limit). `fault_read(domain)` returns the oldest record and the dropped count and resets the
     count. Neither allocates.
3. **`Vmos` changes**: `retain_for_io` and its release reuse the reference count of M8 (a device mapping is a third kind of
   holder, next to handles and regions); `release` keeps its code. The DMA case in `release` is made explicit **before** the size
   test (today a DMA run never takes the batched `stash_dying` path only because `MAX_DMA_PAGES` equals `RANGE_STEP`, 64:
   `abi/src/lib.rs:311`, `kcore/src/aspace.rs:220` and `:1580`; a coincidence the old T42 plan found). It stops being true in
   this slice: the new kind below has no 64-page limit, so the release of a large DMA VMO is tested here.
4. **Charging and the two job limits** (the spec's section 5.3, Q-IOMMU-ABI 4, Q-IOMMU-MSI 4; T262's T41 and T84 parts at the
   model level). **This slice adds both new job-limit fields** to `job.rs` and `limits.rs`, each with its usage counter:
   **I/O-mapped pages** and **interrupt vectors** (the store charges the first here; IOMMU7 charges the second and adds no
   field). Both are **hierarchical**: usage is counted on the job and every ancestor, all or nothing (J1), the smallest limit
   set on the job and its ancestors applies, so a child inherits its parent's cap, and there is no default (OD166 point 4).
   Credited exactly once at `complete` or `destroy` (J2). The system DMA backstop stays.
5. **`Vmos::create_dma_in(pages, job)`**, the model of the new call `vmo_create_dma_in` (IOMMU-SPEC 4.7, OD162 point 3, OD166
   point 9): a DMA VMO charged to the named job, pinned, committed, zeroed and fixed, with frames from the ordinary allocator
   (**not contiguous**), no per-object page limit and no failure for want of an aligned block. The existing contiguous
   `create_dma` is left as it is for the old call, which the virtio-mmio drivers use with `vmo_phys_addr` until IOMMU12
   removes both.

### Acceptance checks (written first; all on the host)

| Test | What it proves | Oracle |
|---|---|---|
| `kcore/tests/iodomain_model.rs` | Random create, map, unmap, wait, complete, abandon, destroy, VMO handle close and "process death" (drop every handle of an owner) in any order. After every step: the simulated device (`iommucore::sim`) can reach a frame **iff** the reference says it is mapped or pending; **no frame is in the free lists while the simulated device, including its translation cache, can reach it**; frame, table-frame, job-page, job-kernel-byte and I/O-mapped-page counts are exact; baseline at the end (M3) | A reference model (`BTreeMap`s) written from IOMMU-SPEC, and the simulated device's own walker and cache |
| `kcore/tests/iodomain_concurrency.rs` (shuttle; loom if the state fits) | Two harts: unmap against map of the same VMO, unmap against destroy, destroy against a fault interrupt, VMO last-handle close against map, with the phase boundaries as yield points. Property: the frame-free event is always after the fence-complete event of the invalidation that covered it | Schedule exploration |
| `kcore/tests/iodomain_noalloc.rs` | `unmap`, `complete`, `abandon`, `destroy`, `fault` and `fault_read` never allocate (a counting allocator, as `pager_noalloc.rs`), with hundreds of mappings live; `map` and `create` fail cleanly at every allocation point, **the mapping record's among them** (as the `allocfail` tests), and leave nothing behind | The allocator |
| `kcore/tests/iodomain_model.rs`, mappings and faults | A domain takes far more than 64 mappings (bounded only by the job's limit on I/O-mapped pages and by the window) and returns every record; after more than 16 faults `fault_read` yields the 16 oldest in order and then the right dropped count; a quarantined device's faults reach no ring | The reference model |
| `kcore/tests/device_vmo.rs` (extended) | D4 with a device mapping as a holder: a DMA VMO closed while mapped for a device is freed at unmap completion, not before | The existing random model |
| `kcore/tests/job_*.rs` (extended) | Both new limits (I/O-mapped pages, interrupt vectors) and the named-job DMA charge obey J1 and J2. **Hierarchical inheritance, for each limit**: a limit set on a parent refuses a child (and a grandchild) that has none of its own, at the parent's number, with nothing charged on any job of the chain; a child's smaller limit binds the child only; with no limit anywhere the charge is unlimited | The existing ledger checks |

Mutation checks, each seen to fail a named test (recorded in the PR; `cargo mutants -p kcore` restricted to `iodomain.rs` once):
release before the fence; `complete` without a fence; the pending range reusable; a hold not taken at map; a hold dropped twice;
`abandon` that frees; destroy that skips the directory invalidation; destroy that leaves a `translate` context; a second domain for
a live device id accepted; access wider than asked; the credit skipped; the credit doubled; the mapping record freed before its
fence; the record's kernel bytes not charged; a full fault ring overwriting its oldest record; the dropped count not reset by a
read; a parent's limit not applied to a child.

### Acceptance

The feature gate green with these tests in it; each mutation above caught; no kernel or `ksys` change. A **Fable review** (kernel
and security slice: its own PR) attacks the model against the spec's sections 5, 6 and 12 and INVARIANTS.md M3, M8, D4, J1, J2.

### Hazards

- The model and the code come from one context: the simulated device is the check that does not share the bookkeeping (it walks
  real tables and keeps its own cache), and IOMMU6's QEMU negative control is the check that does not share the simulator.
- `VmoKind::Dma` frames are fixed today. Contiguity is dropped for the new kind (OD162 point 3); "fixed" must still hold (no
  decommit, resize or copy-on-write child: `aspace.rs:168`, `:1980`), or a device mapping would outlive a frame.
- J2 and the debug assertion in `Jobs::release` (a job freed with usage is a kernel panic): the domain holds a use of its job from
  create to destroy, like a reservation.
- The leaked list is deliberate memory loss; it must be visible (a counter) and must fail the leak oracles of the suite unless a
  fence really timed out.

PR size: large (about 1,500 lines of `kcore` and as much test). Review: Fable.
