---
id: IOMMU12
kind: slice
title: Close the gaps: virtio-mmio leaves the allowlist, vmo_phys_addr and the old vmo_create_dma become stubs, the hostile-driver matrix, and T42 closes
area: drivers
horizon: long
complexity: M
change_risk: H
epic: iommu-drivers
blocked_by: [IOMMU11]
refs: [T42, OD148, OD162, OD166, IOMMU-SPEC, SYSCALLS, INVARIANTS, FZ1, X-SANDBOX, T273, T274, T275, T276, Q-IOMMU-IRQ]
---

Child 12 of the IOMMU epic (`work/T42.md`), the last feature PR on `epic/iommu-drivers`. Until this merges the system is **not**
isolated: a holder of the root resource can still map a virtio-mmio slot (which the IOMMU does not cover) and learn physical
addresses. This slice removes both and proves OD148's sentence, "a device reaches only the frames it has been mapped", from the
outside.

**Revised 2026-11-25** (Q-IOMMU-IRQ answered B; this was IOMMU9 before the renumbering): the matrix's interrupt rows are MSI
forgeries against the mechanism of IOMMU7, and "MSI-X later" is no longer among the items this slice writes.

## Plan (2026-11-25)

### The acceptance check, written and seen to fail before the code

| Check | Where | What must be seen |
|---|---|---|
| `no_dma_device_outside_the_iommu` | suite, shard `k`, 1, 2 and 4 harts | With the root resource, `vmo_create_physical` over every virtio-mmio slot of the tree, fw-cfg, and the IOMMU's own register page is INVALID_ARGS; `sys_vmo_phys_addr` and the two-argument `sys_vmo_create_dma` each return NOT_SUPPORTED for every argument (library stubs: IOMMU-SPEC 4.8), and their old numbers through `sys_raw` are BAD_SYSCALL; the test fails today on the first line |
| `kcore/tests/devwin.rs` | host | Against the real tree: the windows are exactly the RTC page, the ECAM window and the PCI memory windows; `virtio,mmio` yields none |
| the hostile-driver matrix | suite, one test per row, on a real virtio-pci device **and** on the test device | A driver process holding exactly what a real driver holds (its register BAR, its buffers with the `DMA` right, a domain handle with IO_MAP and IO_UNMAP only, its interrupt handles; a rig spawns it with `blkd`'s real handle set, in a job with the limits `devmgr` sets) tries: a descriptor at a kernel address; at another process's frame; at another device's buffer; at another device's BAR; at an IMSIC page as DMA (a write of a chosen identity into a hart's file); and, as **messages** (the rig, for these rows, is also given the MSI-X table that a real driver never has: the worst case of Q-IOMMU-MSI 6): another device's vector page and identity, the inter-hart identity on every hart, the IOMMU's own fault identity, an ungranted vector of its own, a granted vector with a wrong data word; after its vectors were revoked; at its own buffer one page beyond; a write through a read-only mapping; use after its own unmap; use after its supervisor destroyed the domain; use after its supervisor died. **With its own handles** (no device involved; each is ACCESS_DENIED or the stated error, and changes nothing): map a VMO it holds without the `DMA` right; map read-write a buffer it holds without WRITE; destroy its domain; grant itself a vector; wait for or read its device's faults; `vmo_phys_addr` on its buffer (NOT_SUPPORTED). **Against its job's limits** (OD166 point 4): map until refused (NO_MEMORY at the limit on I/O-mapped pages `devmgr` set, nothing leaked, no other job disturbed); create a child job with no limits of its own and map, or hold vectors, from there (refused at the **parent's** limit: the limits are hierarchical); a check that every driver job `devmgr` starts has finite limits of both kinds (`job_info`). Every device row: memory intact, exactly one fault record with this device's id (none for a quarantined device, whose faults are not reported), no interrupt delivered to anyone else, the system keeps running and the leak oracles balance |
| `dma_survives_a_dead_supervisor` | suite | The test the old plan specified (T42, 2026-11-24, section 4), now against the kernel mechanism: a rig supervisor and `blkd`, a write in flight, kill both, pattern-fill reused frames, wait past the request, check the pattern; then baseline. Negative control: the `fault-iommu-skip-inval` kernel of IOMMU6 must fail it (or the same caveat as there) |
| every negative-control kernel of the epic | by hand, once, listed in `docs/reference/TEST-TOOLS.md` | `fault-iommu-bare`, `fault-iommu-skip-inval`, `fault-iommu-msi-basic` and `fault-msiid-free-at-move` each fail their test (or carry the stated caveat); added to the soak list beside `os-boot-test-smp-neg` (the owner starts soak, OD142) |
| the syscall fuzzer and tables | `ksys/tests/fuzz.rs`, `kfuzz-gen`, `docs/reference/syscall-tests.tsv` | The two retired kernel calls are gone from the generators and the tables (the library stubs are in the conformance program, which sees NOT_SUPPORTED); the generators cover the domain calls and `vmo_create_dma_in` with hostile arguments |

### What changes

1. `kcore/src/devwin.rs:14`: `ALLOWED` loses `virtio,mmio` (Q-IOMMU-MODEL 7). `kcore/src/aiatree.rs`: the mmio slots' interrupt
   sources stop being admitted (`interrupt_create` refuses them). The wired kind of interrupt object keeps one user (the RTC's source) and keeps one suite test
   on it (`interrupt_object_fires_and_masks` re-pointed at the RTC alarm, with its two negative-control kernels), so IOMMU4's
   route code for wired sources does not go untested once the mmio devices are gone.
2. **The two calls the kernel no longer provides** (Q-IOMMU-ABI 1, OD162; OD166 point 9; IOMMU-SPEC 4.8). `vmo_phys_addr`: the
   kernel call goes (`abi/src/lib.rs:403`, the number retired and never reused; `ksys/src/vmo.rs:170`, `kcore/src/aspace.rs:776`,
   `kfuzz-gen`). The two-argument `vmo_create_dma`: the kernel call goes the same way, and with it the contiguous DMA kind,
   `MAX_DMA_PAGES` and the aligned-block `NO_MEMORY` (`abi/src/lib.rs:311`, `kcore/src/aspace.rs`), since `vmo_create_dma_in`
   (IOMMU6) is now the only way to make a DMA VMO. **The system library is additive** (KERNEL-V1-SPEC 3.1): `sys_vmo_phys_addr`
   and `sys_vmo_create_dma` **stay** in `libsys` and in `libsys/symbols-v1.txt`, with their signatures, as stubs that return
   NOT_SUPPORTED without entering the kernel; `rt`'s wrappers (`rt/src/lib.rs:500`) go or return the same. The `DMA` right's
   comment and meaning (`abi/src/lib.rs:115`). This holds before or after FZ1.
3. The suite: the mmio tests are deleted or replaced by their PCI versions from IOMMU8 (`interrupt_object_fires_and_masks`,
   `device_interrupts_arrive_where_asked`, `mmio_reads_the_virtio_magic`, the address checks in `device_caps_need_the_resource`
   near `:13420`, `dma_runs_are_allocated_and_freed_repeatedly` at `:15253` which reads addresses to check contiguity: rewritten
   onto `vmo_create_dma_in`, keeping the allocation and leak check, with sizes above 64 pages, and dropping the address); `scripts/os_boot_test.py`'s assertions that name the mmio interrupt path
   (`:1336` to `:1347`).
4. Reference documents in place: SYSCALLS.md (the call gone from the text and from the table at `:1032`, `vmo_create_physical`'s
   window list), INVARIANTS.md (D2 becomes "no physical address is learnable from user mode"; D1's window list; NET2; the IO rows
   marked as checked by the tests above), `docs/reference/TEST-TOOLS.md`.
5. Comments that state the old rule as current: `blkd.rs:7` to `:9`, `devmgr.rs:7` to `:10`, `netsup.rs:2`, `kcore/src/devwin.rs:4`.
6. Work items, in the epic's closing commit after a green smoke: `work.py done` for IOMMU8 to IOMMU12 and **T42**; T262's parts
   T41 (what is left: aliasing of physical VMOs, their pinning to the root job) and T84 struck or narrowed in place, and its T59,
   T183 and T184 parts checked against what IOMMU4, IOMMU10 and IOMMU11 did (each already has a dated note); a dated note
   in X-BUS, X-DRIVER-MODEL and X-SANDBOX saying the precondition "an IOMMU" is **met** (each has a note of 2026-11-25 saying
   it is planned); the boxes of T42's "state and order" list.
7. Items for what the epic deliberately left. **Already recorded on 2026-11-25, to be checked against what was built and not
   filed again:** **T273** (superpage I/O mappings), **T274** (an interrupt-driven fence in place of the poll), **T275** (a
   batch form of map and unmap under one fence), **T276** (grouping of functions that share a requester id behind a bridge).
   **To write in this PR** (each `kind: tidy` or a slice): what MSI-X left (multi-queue devices and per-queue placement policy,
   the older MSI capability); a second machine type or real hardware for the IOMMU (QEMU's device is the only one tested);
   anything a review deferred. An IOMMU without MRIF gets no item: the fallback is recorded in OD165 as not built.

### Hazards

- **Order inside the PR**: the allowlist change breaks every boot that still has an mmio device on its command line: remove
  those first (IOMMU11 did), then the allowlist, then the call.
- **The home computer** uses virtio-mmio under its own kernel (`home/kernel/`) and its own command lines: nothing of it is
  touched; `make home-boot-test` is the proof and runs in the epic gate.
- **A spec that still says otherwise**: agents do not edit specs. If S6-SPEC, NET-SPEC or KERNEL-V1-SPEC disagree with the code
  after IOMMU1's amendments, say so in the PR and record an item; do not patch the spec.
- **The MSI-forgery rows** repeat IOMMU7's checks from a driver's seat with real drivers running beside the rig: the point here is
  the bystanders (no other driver's interrupt count moves, no request of theirs is disturbed). The stale-entry row cannot fail on
  QEMU (it caches no MSI entry: IOMMU7's limits); it is kept as a regression test and said to be that. The wrong-data-word row gives **no** fault record by design (nothing is
  delivered and the stray-identity counter rises): the matrix's "one fault record" column says so for that row.
- **"Exactly one fault record"** races with the rate limiter and with a device that retries: assert "at least one, naming only
  this device, and none naming another".
- The two symbols stay in `libsys` as stubs, so nothing built against the v1 symbols stops linking; a caller now gets
  NOT_SUPPORTED at run time. Grep shows no user outside `init/` and `rt/` today; re-check at the time.

### Acceptance

Everything in the table passes in the epic's smoke, kernel-only boots at 1, 2 and 4 harts for the shard-`k` tests (say plainly
which ran); `make home-boot-test` green in the same gate; the feature gate green. **Fable review** on its own PR (a security
slice: the claim "no DMA device is reachable outside the IOMMU" is attacked with fresh context against IOMMU-SPEC and
INVARIANTS.md D1 to D5 and the IO rows; the reviewer looks for any remaining way to a physical address or to a device window
that can do DMA). Then `epic/iommu-drivers` merges into `main` with a merge commit and T42 is closed.

PR size: medium (about 500 lines removed, 700 of tests added).
