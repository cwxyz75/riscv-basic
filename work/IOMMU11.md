---
id: IOMMU11
kind: slice
title: The bus move, part 4: the network driver and its pools, and the entropy device, on virtio-pci behind the IOMMU
area: drivers
horizon: long
complexity: H
change_risk: M
epic: iommu-drivers
blocked_by: [IOMMU10]
refs: [T42, OD148, OD164, OD166, IOMMU-SPEC, NET-SPEC, NET-DESIGN, T262, Q-IOMMU-IRQ]
---

Child 11 of the IOMMU epic (`work/T42.md`), and the second half of OD148's step (e). Feature PR on `epic/iommu-drivers`. After it
no driver of the OS track uses virtio-mmio or `vmo_phys_addr`.

**Revised 2026-11-25** (Q-IOMMU-IRQ answered B; this was IOMMU8 before the renumbering): the network and entropy devices'
interrupts are MSI-X vectors (network: configuration, receive, transmit; entropy: one), and the four-line limit on a boot's
device set is gone.

**The model this slice implements** (IOMMU-SPEC 4.1, 9, 10; NET-SPEC and NET-DESIGN, amendments of 2026-11-25; OD164, OD166
point 2). The **driver maps**: `netdrv` (and `rngd`) hold a domain handle with the map and unmap rights only and their DMA VMOs
with the `DMA` right, and map them into their device's domain themselves. The **supervisor controls**: `netsup` holds the
network device's control handle (destroy, faults) and the vector handles it was given, `devmgr` the entropy device's. Narrowing
is done with handle rights: `netsup` gives `netdrv` the transmit pool **without WRITE**, so the driver can only map it
read-only for the device (IO4). **A killed driver's domain is destroyed and made anew**: nothing is mapped once and kept
across driver lifetimes.

## Plan (2026-11-25)

### DMA users converted here

| User | Today | After |
|---|---|---|
| `init/src/bin/devmgr.rs`, network claim (`claim_net`, `:369`) | creates three DMA VMOs (64 + 64 + 2 pages, `:51` to `:53`) charged to **its own** job, hands them to `netsup` with `Rights::SAME` (`:393`) | runs the claim of IOMMU-SPEC 9 for the claimant: finite I/O limits on the job that pays, the domain, the three VMOs from `vmo_create_dma_in` **charged to the claimant's job** (T262's **T84** part), the vector grants; the claim reply carries the BAR VMO, the three VMOs with the `DMA` right, the domain's control handle and one interrupt handle per vector (NET-SPEC 4.3 as amended). A second operation re-claims the device after a driver's death (below) |
| `init/src/bin/netsup.rs` | holds the pools across driver and stack restarts; gives `netdrv` the pools with `DMA | TRANSFER` only (`:131`, `:136`) and the queue VMO with `SAME` (`:139`); gives `netd` the pools without the `DMA` right, receive read-only | **maps nothing**. It keeps the control handle and gives `netdrv` a duplicate with IO_MAP and IO_UNMAP only, the receive pool and the queue VMO with `DMA`, READ and WRITE, and the **transmit pool with `DMA` and READ but no WRITE** (so it can only be mapped read-only for the device: a device can no longer be made to overwrite frames the stack is sending), and duplicates of the interrupt handles; `netd`'s handles are as today |
| `init/src/bin/netdrv.rs` | `vmo_phys_addr` three times (`:461` to `:463`); mmio registers (`:36` to `:88`, MAC from the config space at `:414`); never maps a pool | the `virtio` crate's transport; **maps the three VMOs into the device's domain itself** (`iodomain_map`: receive pool and queue read-write, transmit pool read-only) and programs the queues with the I/O addresses returned; still never maps a pool into its own address space |
| `init/src/bin/netd.rs` | maps the pools; never has the `DMA` right (NET2) | **unchanged** |
| `init/src/bin/rngd.rs` | claims the entropy device (`OP_CLAIM_RNG`), 2 DMA pages, `vmo_phys_addr` at `:335`, mmio registers | PCI claim, the `virtio` transport; it maps the page the device fills and the queue page into its device's domain itself, read-write for the device, with its map-and-unmap handle |
| `devmgr`'s entropy claim (`claim_rng`, `:431`) and `drop_claims_of` (`:469`) | resets, then closes the DMA | reset, bus mastering off, destroy the domain, close |

### The acceptance check, written and seen to fail before the code

| Check | Where | What must be seen |
|---|---|---|
| the network suite (`net_tests::*`, including `net_children_restart` and `net_streams_survive_a_stack_restart`), the DHCP, DNS and HTTP checks of `os_boot_test.py`, the lossy-relay boot | suite and script, the boots that have a network | Green on `virtio-net-pci`, with the packet capture oracle (`filter-dump`, `os_boot_test.py:1016`) unchanged and zero unexpected IOMMU faults |
| `net_device_cannot_write_the_transmit_pool` | suite (`stackless` mode, where the self-tests act as the stack) | A rig driver holding what `netdrv` holds first tries to map the transmit pool read-write: ACCESS_DENIED (its handle has no WRITE). It then posts a **transmit** slot's I/O address as a receive buffer: the device's write faults (the pool is device-read-only), the slot's bytes are intact, one fault names the network device and its record is readable by `netsup`'s control handle, not by the driver's |
| NET2, strengthened | suite (the loopback test that checks it today) | `netd` and now also `netdrv` cannot learn a physical address: `vmo_phys_addr` is refused for both; `iodomain_map` with `netd`'s pool handles (no `DMA` right) is ACCESS_DENIED |
| `net_children_restart`, the device side | suite | After `netdrv` is killed: the old domain is destroyed (live-domain counter, the dead driver's I/O-mapped pages at zero), a new one exists, the new driver mapped the same pools afresh, traffic flows by interrupt on vectors granted afresh; zero unexpected faults |
| `rng_seeds_through_the_iommu` | suite, `scripts/entropy_boot_test.py` | `rngd` is seeded from the PCI device; the entropy boot test passes |
| the charge | suite | After a network claim the 130 pages are on the claimant's job, not `devmgr`'s (the job statistics), and return when the claim is dropped |

### What changes

`netdrv.rs`, `netsup.rs` (`:50`, `:128` to `:139`, `:277`), `rngd.rs`, `devmgr.rs` (the network and entropy claims; **the whole
virtio-mmio table and `probe` are deleted**: `:36` to `:50`, `:106`, `:556`), `proto/src/devmgr.rs` (`OP_CLAIM_NET`,
`OP_CLAIM_RNG` reply shapes, and the re-claim operation; `NET_FLAG_IRQ` and the `Option` interrupt go: the rest of T262's **T184** part), `proto/src/startup.rs`
(roles), `proto/tests/wire.rs`, `scripts/os_boot_test.py` (`:1010` to `:1015`: `virtio-net-pci,netdev=n0,mac=...` and
`virtio-rng-pci`, both with `iommu_platform=on,disable-legacy=on,addr=...`; `-global virtio-mmio.force-legacy=false` goes),
`scripts/entropy_boot_test.py`, NET reference text in SYSCALLS.md and INVARIANTS.md (NET2 reworded in place). The real-time clock
(`OP_CLAIM_RTC`, `google,goldfish-rtc`) is not a DMA device and stays on its MMIO page.

### Hazards

- **`virtio-net-pci` defaults**: the PCI device offers many offloads (`csum`, `gso`, `mrg_rxbuf`, the control queue: all default
  on in `-device virtio-net-pci,help`). `netdrv` negotiates only `VERSION_1` and `MAC` today and must now accept
  `ACCESS_PLATFORM` and still refuse the rest; `mrg_rxbuf` changes the header size if accepted by accident. The capture oracle
  catches a wrong header.
- **Restart paths** are the network's hard part (generation counters, re-attach, NET-SPEC 5.6). A driver restart is a
  **re-claim** (IOMMU-SPEC 10.2, 10.4): when `netdrv` dies its mappings stay with the domain until the supervisor acts;
  `netsup` then asks `devmgr` (which alone holds configuration space and the root resource) to clear bus mastering and reset
  the function, the old domain is destroyed, and `devmgr` creates a new domain and new grants and returns the new handles;
  the new `netdrv` maps the same three VMOs afresh. The pools themselves (the VMOs, which `netsup` holds, and their contents)
  survive; the I/O addresses need not be the same, and no old descriptor is in flight because the function was reset before
  the old domain went. **The shape of the re-claim operation in `proto::devmgr`** (a new operation on the claimant's channel,
  or drop and claim again) is not fixed by the spec: settle it in this PR and say so, or ask. `net_children_restart` under
  smoke is the check; any intermittent failure is a defect with a work item (OD140, OD160), not a rerun.
- **The safety poll** (NET8) stays; the interrupt is no longer optional. A network test that passes only because of the poll
  would hide a broken vector: the driver counts poll-found work and the suite asserts it is 0 in a quiet run.
- **Vectors and restarts**: grants are not kept across driver lifetimes either (OD166 point 2): the old domain's destruction
  revokes them in the kernel's order (IOMMU7), and the re-claim grants afresh on the new domain; `netsup` hands the new
  `netdrv` duplicates of the new interrupt handles. Separate receive and
  transmit vectors are what a multi-queue device would scale with; `netdrv` waits on both. The per-boot device sets stay one
  table in the script; nothing limits them to four.
- `netsup` in `stackless` mode hands the stack-side handles to the suite: the rig driver of the new negative test needs the
  device-side ones; add a test-only mode rather than weakening what the real `netdrv` receives.

### Acceptance

The table passes in the epic's smoke (the network boots are full boots, not shard `k`; say which hart counts ran); the feature gate
green; zero unexpected faults. Review: default reviewer.

PR size: large (about 1,200 lines changed across `netdrv`, `netsup`, `rngd`, `devmgr`, protocols and scripts).
