---
id: IOMMU9
kind: slice
title: The bus move, part 2: MSI-X in devmgr (table and pending bits, vector allocation and grants, the claim protocol's interrupt handles) and the interrupt-driven hand-driven tests
area: drivers
horizon: long
complexity: H
change_risk: M
epic: iommu-drivers
blocked_by: [IOMMU8]
refs: [T42, OD148, OD164, OD165, OD166, Q-IOMMU-IRQ, Q-IOMMU-MSI, IOMMU-SPEC, IOMMU7, X-BUS, X-DRIVER-MODEL, T262, S6-SPEC]
---

Child 9 of the IOMMU epic (`work/T42.md`). **New on 2026-11-25** (Q-IOMMU-IRQ answered B). Feature PR on `epic/iommu-drivers`,
after IOMMU8. It is the user-space half of MSI-X: `devmgr` turns the kernel's vector grants (IOMMU7) into a programmed MSI-X
table and hands a driver interrupt handles, and the suite proves the whole path with hand-driven devices before any real driver
moves (IOMMU10, IOMMU11). No kernel change.

**From T262:** part **T184**'s "the interrupt is mandatory" is made true for PCI claims here (a claim without its vectors fails;
there is no `Option`), and finished per driver in IOMMU10 and IOMMU11.

## Plan (2026-11-25)

### The acceptance check, written and seen to fail before the code

| Check | Where | What must be seen |
|---|---|---|
| `pcicore` MSI-X programming model | host: `pcicore/tests/` | A fake function (configuration bytes plus a table BAR and a pending-bit BAR as memory) driven through the crate's sequence: with the function masked, entries written, vector control cleared, MSI-X enabled, INTx disabled; and the reverse. At every step the fake asserts what a real device requires (no entry written while it is unmasked and enabled; the table size respected; reserved bits preserved). Random vector counts against the capability's table size; a table size the capability lies about |
| `virtio` vector tests | host: `virtio/tests/` | `msix_config` and each `queue_msix_vector` are written and **read back**; the read-back value `0xffff` (the device refused the vector: VIRTIO 1.x, "no vector") fails the bring-up. Constants from the VIRTIO text, not from the code |
| `devmgr` vector allocation model | host (`init` unit test or a small pure module `proto`-side) | Random claims and drops over devices with different table sizes and a job vector limit: every claim gets exactly the vectors its driver class asks for or fails whole; nothing is leaked on any failure point; a device is never left with MSI-X enabled and a vector ungranted |
| `pci_virtio_blk_interrupts_through_msix` | suite, shard `k`, 1, 2 and 4 harts | The hand-driven block test of IOMMU8, now claimed through `devmgr`'s real path: the reply carries interrupt handles; the request completes by **interrupt** (no poll), the object masks until acknowledged and re-fires (D5 for a vector), delivered on the hart asked for; zero IOMMU faults; `interrupt_object_fires_and_masks`'s two negative-control kernels (`fault-aia-no-unmask`, `fault-aia-no-repend`) are re-pointed or given vector twins and each fails it |
| `pci_two_devices_do_not_share` | suite | Two devices claimed, each with two vectors; requests on both; every interrupt arrives on its own object and none on another's (counts per object equal requests) |
| `pci_driver_cannot_forge_through_devmgr` | suite | A rig process holding exactly what a driver is handed (register BAR, its DMA VMOs, the domain handle with IO_MAP and IO_UNMAP only, interrupt handles) cannot reach the MSI-X table or configuration space (no handle to try with), and cannot grant a vector, destroy the domain or read its faults with the handle it has (ACCESS_DENIED each); it then sets `queue_msix_vector` to an entry `devmgr` did not program: the device's message for that queue faults in the IOMMU (cause 262) or is not sent, and nobody is interrupted |
| `msi_*` tests of IOMMU7 | suite | Ported from the local MSI-X helper onto `pcicore`; the helper is deleted |
| the five-device boot | `scripts/os_boot_test.py`, one boot | Two disks, network, entropy and the test device as PCI functions on one machine, each with its vectors: the thing option A could not do. Listed by `devmgr`, all claimable |

### What is built

1. **`pcicore`**: the MSI-X **sequence** as a caller-driven protocol over the codecs of IOMMU8 (the crate still touches no
   memory): mask the function, write entries, unmask entries, enable, set INTx-disable; and teardown: function mask, disable,
   entries cleared. A reader for the pending bits (diagnostic only).
2. **`virtio`**: `msix_config` and `queue_msix_vector` in the common configuration with the read-back rule; the transport's
   bring-up takes a vector map (configuration change, one per queue or shared) from its caller.
3. **`devmgr`** (`init/src/bin/devmgr.rs`): per claimable function, the vector plan of its class (block: configuration and one
   queue; network: configuration, receive, transmit; entropy: one queue; a table in the source, not a guess from the device);
   at claim, in the order of IOMMU-SPEC 9: after the domain exists and the driver's DMA VMOs are created, grant the vectors on
   the domain with the control handle (IOMMU7's call), map the table BAR privately, program the entries with the pairs the
   kernel returned with the function masked, disable INTx, enable MSI-X, hand the driver its handles (the interrupt handles
   duplicated with wait and acknowledge only, beside the BAR, the buffers and the map-and-unmap domain handle of IOMMU8), and
   **last** enable bus mastering. The driver maps its own buffers; `devmgr` maps none. At drop or driver death (IOMMU-SPEC
   10.1, 10.2): function mask and MSI-X disable, bus mastering off, a function-level reset, then `iodomain_destroy`, which
   revokes every vector and removes every mapping (the kernel's ordering of IOMMU7 step 7 does the safe part); `devmgr` closes
   its interrupt handles. **Grants are not kept across driver lifetimes**: a re-claim makes a new domain and grants afresh
   (OD166 point 2). A device whose table shares a BAR with its registers is claimable too; `devmgr` then
   cannot keep the table from the driver and says so in its listing line (the IOMMU confines it either way).
4. **`proto::devmgr`**, **`proto::startup`** (`proto/tests/wire.rs`): the PCI claim reply and the startup roles carry a list of
   interrupt handles with their meaning (configuration, queue n). The old operations stay until IOMMU12.
5. **Limits**: `devmgr` asks for no more vectors than the class plan, and **sets finite limits of both kinds** (I/O-mapped
   pages and interrupt vectors, `job_set_io_limits`) on every driver job before it creates a domain for it: the limits have no
   default, and the owner accepted that only on this condition (OD166 point 4; IOMMU-SPEC 5.3, 9). The numbers come from the
   class table (the vectors of the plan; the pages of the class's buffers with a small margin). They are hierarchical, so a
   job a driver creates under its own is bound by them. Exhaustion is a failed claim or a failed map with a loud line, never a
   silently shared vector. The allocation model above checks that no claim is made for a job without both limits.
6. **The suite and the script**: the tests above; the device helper in `os_boot_test.py` gains nothing for MSI-X (QEMU's
   defaults give each virtio-pci device a table) but asserts in the `## devmgr: pci` lines that every claimable device reported
   one.

### Order of bring-up and teardown (the spec's section 10; state it in `devmgr`'s header comment)

Up: the driver job's limits, domain, DMA VMOs, vector grants, MSI-X table and enable, the driver's handles, memory decoding and
bus mastering last; the driver then maps and starts. Down: MSI-X function mask and disable, bus mastering off, function-level
reset, domain destroyed (vectors revoked and mappings removed by the kernel), interrupt handles closed. The kernel does not rely
on any of it (IOMMU6 and IOMMU7 order the safe part themselves); it keeps the fault log quiet,
and the zero-unexpected-faults assertion of IOMMU10 will catch a wrong order as a fault line at driver restart.

### Hazards

- **virtio's own vector registers are the driver's**: a driver can point a queue at any table entry or at none. That is inside
  its own device and its own grants; the test above proves it stays there.
- **Lost interrupt at restart**: nothing of the old driver's vectors survives (the domain was destroyed and the function
  reset), so a new driver gets fresh grants, armed, with empty files; what can go wrong is a table entry left from before the
  reset or MSI-X enabled before every entry is written. A stuck restart shows in
  `blk_driver_restart` (IOMMU10) as a request completed only by the safety poll: count those and fail on them.
- **MSI-X table size against the plan**: QEMU's default `vectors` differs per device (IOMMU0 item 13); the plan asks for what the
  class needs and fails loudly if the table is smaller.
- **Record and replay, icount** with MSI-X and the IOMMU: not verified (as IOMMU8); a broken mode is said and recorded, not
  dropped.
- `devmgr` writes device memory a device controls (the table BAR): bounds from the capability are checked against the BAR size
  by `pcicore` before any write.

### Acceptance

The table passes in the epic's smoke (kernel-only boots at 1, 2 and 4 harts for the suite rows; say which ran); the feature gate
is green; the mmio drivers are unchanged and green. Review: default reviewer (user space; the isolation it relies on was reviewed
in IOMMU7).

PR size: large (`pcicore` and `virtio` additions about 400 lines with tests, `devmgr` about 500, protocols, suite and scripts
about 600).
