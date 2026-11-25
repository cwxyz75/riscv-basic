---
id: IOMMU10
kind: slice
title: The bus move, part 3: the block driver (blkd, devmgr's block claims, the suite's rig) on virtio-pci behind the IOMMU
area: drivers
horizon: long
complexity: M
change_risk: M
epic: iommu-drivers
blocked_by: [IOMMU9]
refs: [T42, OD148, OD164, OD166, IOMMU-SPEC, S6-SPEC, T262, Q-IOMMU-IRQ]
---

Child 10 of the IOMMU epic (`work/T42.md`), and the first half of OD148's step (e) ("block driver, then network"). Feature PR on
`epic/iommu-drivers`. After it every disk of the OS track is a `virtio-blk-pci` device whose DMA is confined to the block
driver's buffer.

**Revised 2026-11-25** (Q-IOMMU-IRQ answered B; this was IOMMU7 before the renumbering): the disk's interrupts are MSI-X vectors
granted through `devmgr` (IOMMU9), not an INTx line; it follows IOMMU9.

**The model this slice implements** (IOMMU-SPEC 4.1, 9, 10; OD164, OD166 point 2). `devmgr` is the supervisor: it creates the
disk's domain and keeps the control handle, creates the driver's buffer, grants the vectors, and maps nothing. `blkd` is the
driver: it holds a domain handle with the map and unmap rights only and its buffer with the `DMA` right, maps the buffer itself
and learns the I/O addresses from the call. **A killed driver's domain is destroyed, and a new one is created at the
re-claim**: when `blkd` dies, `devmgr` clears bus-master enable, issues a function-level reset and destroys the domain; for the
next `blkd` it runs the claim again (a new domain, new grants) and the new driver maps afresh. Nothing is mapped once and kept.

## Plan (2026-11-25)

### DMA users converted here (all found by `grep` on 2026-11-25)

| User | Today | After |
|---|---|---|
| `init/src/bin/blkd.rs` | maps `BlkMmio` and a 4-page `BlkDma`; `vmo_phys_addr` at `:329`; mmio registers at `:32` to `:96`; zeroes the queue at start (`:131` to `:140`) | the `virtio` crate's transport over its BAR; from the startup message its buffer **with the `DMA` right**, **a domain handle with IO_MAP and IO_UNMAP only** and **two interrupt handles** (configuration, queue); it maps the buffer with `iodomain_map` and programs the queue with the I/O addresses returned; no control right on the domain, no MSI-X table, no configuration space, no physical address (Q-IOMMU-MODEL 3, Q-IOMMU-MSI 6) |
| `init/src/bin/devmgr.rs`, block part | `vmo_create_dma(res, 4)` at `:522`, duplicates with `Rights::SAME` to the driver (`:193`), `reset` then reuse (`:77`, `:134`) | per claim of a disk: finite I/O limits on the driver's job, a domain (control handle kept), the buffer from `vmo_create_dma_in` charged to the driver's job, the vector grants; the driver gets the buffer with the `DMA` right and the map-and-unmap duplicate. `devmgr` maps nothing. When the driver exits or dies: bus mastering off, function-level reset, **the domain destroyed**; the next driver gets a new domain (IOMMU-SPEC 10.2, 10.4). The buffer VMO may be kept or made anew: nothing depends on it |
| `init/src/bin/suite.rs`: `BlkRig` (`:10697` to `:10790`), `blk_round_trip`, `blk_driver_restart` (`:11081`) | its own DMA VMO and mmio slot | the rig claims a PCI disk the same way `devmgr` does |
| everything above `blkd` (`volmgr`, `fsd`, doofs, `/store`) | the block protocol (`proto::blk`) | **unchanged**: no protocol change |

### The acceptance check, written and seen to fail before the code

| Check | Where | What must be seen |
|---|---|---|
| `blk_round_trip`, `blk_driver_restart`, the filesystem and volume tests | suite, full boots and shard `k` at 1, 2 and 4 harts for the rig tests | Green on PCI disks, with the image oracle of `os_boot_test.py` (`:125`: after QEMU exits the script reads the disk image itself) unchanged, and **zero** IOMMU faults counted over the boot (a new end-of-run assertion in the script: the fault counter is printed in the summary line and must be 0 in every boot that has no deliberate fault test) |
| `blk_driver_killed_mid_write` | suite | `blkd` is killed with a write in flight: until `devmgr` acts, the mappings stay with the domain and hold the buffer (nothing is freed, the device reaches what it reached). `devmgr` then clears bus mastering, resets the function and **destroys the domain** (the live-domain counter drops, the I/O-mapped pages and mapping records of the dead driver's job return to zero), and restarts it: a **new** domain (the counter of domains created rose), the new `blkd` maps its buffer itself, the next write lands. No unexpected fault, no lost frame. A rig variant closes every handle to the buffer while it is mapped and a write in flight still lands (the mapping holds the VMO) |
| `blk_driver_cannot_reach_outside` | suite | A rig "driver" that programs a descriptor with an address outside its buffer (another process's page, kernel memory, the buffer's own physical address): the device fails the request, one fault names this disk, the target is intact. With the real `blkd` binary unchanged; the hostile part is the rig |
| `blk_driver_restart`, interrupt side | suite | After a restart the new `blkd` completes its first request by interrupt, not by a poll (count of poll-completed requests is 0), on vectors **granted afresh** on the new domain; the old driver's vectors were revoked with the old domain and the interrupt-identity count is back where it was |
| the no-disk boot | `make os-boot-test-nodisk` | Still boots; `devmgr` finds no disk on either bus |

### What changes

`blkd.rs`, `devmgr.rs` (block table: `Block` at `:57`, `open` at `:179`, the start-up scan at `:518` to `:560`), `suite.rs` (the
rig), `proto/src/startup.rs` (roles: `BlkMmio` becomes the BAR, `BlkDma` keeps the `DMA` right, a new role carries the
map-and-unmap domain handle; no address travels in the startup arguments), `proto/src/devmgr.rs` (`OP_OPEN_BLOCK` keeps its number and meaning: the index is now the PCI disk order by slot),
`scripts/os_boot_test.py` (`:1083` to `:1091`: the two scratch disks become `virtio-blk-pci,addr=...,iommu_platform=on,
disable-legacy=on`; the comment about mmio slot order goes), the Makefile's OS-track disk options, S6-related reference text in
SYSCALLS.md if it names the mmio slot. The virtio-mmio block path in `devmgr` is deleted in this PR (no disk uses it); the mmio
scan stays for network and entropy until IOMMU11.

T262's **T184** part (the optional interrupt: `Block.irq: Option<u32>`, `blkd` exiting 3 without one) is finished for block here:
a PCI disk always has its MSI-X vectors or the claim fails (IOMMU9 made the claim reply carry them without an `Option`); `blkd`'s
`BlkIrq` role becomes the queue vector's handle, and its exit code 3 path goes.

The three holes of the old supervisor policy (T42's 2026-11-24 table): (a) a dead `devmgr` losing the frames to a live device is
closed by the kernel (IOMMU6) and shown by `blk_driver_killed_mid_write`; (b) `devmgr` waiting 2,000 ticks for a killed `blkd`
and resetting whatever the wait returned (`:603`) and (c) the silent reset timeout (`:78`) are fixed here as plain bugs.

### Hazards

- A boot with both buses in use (PCI disks, mmio network) is the state of the epic between this PR and IOMMU11: `devmgr` carries
  both tables for one PR. Keep the mmio code untouched rather than half-shared.
- Disk order: tests assume "device 0 is the 16 MiB scratch disk". With explicit `addr=` the order is by slot; set it once in the
  script and in the suite's constants.
- Throughput: every request now passes QEMU's IOMMU translation (cached). The block benchmarks are soak-tier (not run here);
  note the expected cost in the PR so a later bench run is not read as a regression of something else.
- The driver must not be given anything it could turn into a physical address or into control of its device's domain: check
  its startup handles in `a_child_has_no_device_authority` style (the domain handle has IO_MAP and IO_UNMAP and no other right;
  no ECAM, no MSI-X table, no root resource). The `DMA` right on its buffer is expected: it only allows mapping for the device.
- A restart that reuses the old I/O addresses by habit: the new domain's allocator starts empty, so the addresses happen to be
  the same on QEMU; the driver must program what `iodomain_map` returned, never a remembered value.

### Acceptance

The table passes in the epic's smoke; zero unexpected IOMMU faults in every boot; the feature gate green. Review: default
reviewer (user-space; the kernel is unchanged).

PR size: medium to large (about 900 lines changed, mostly `blkd`, `devmgr` and the rig).
