---
id: IOMMU8
kind: slice
title: The bus move, part 1: PCI enumeration in devmgr (pcicore, with MSI-X capability parsing), a shared virtio-pci transport, one hand-driven device through the IOMMU
area: drivers
horizon: long
complexity: H
change_risk: M
epic: iommu-drivers
blocked_by: [IOMMU7]
refs: [T42, OD148, OD164, OD166, IOMMU-SPEC, X-BUS, X-DRIVER-MODEL, T262, T276, S6-SPEC, Q-IOMMU-IRQ, Q-IOMMU-MSI]
---

Child 8 of the IOMMU epic (`work/T42.md`), and OD148's step (d). First feature PR on `epic/iommu-drivers` (cut from `main` after
`epic/iommu-kernel` merged). It builds what every converted driver needs and proves it with the suite's own hand-driven virtio
tests; `blkd`, `netdrv` and `rngd` are untouched and keep running on virtio-mmio beside it. This and IOMMU9 are the committed
part of topic **X-BUS** (one ECAM host, bus 0, MSI-X, no bridges, no hotplug, no USB); X-BUS keeps the rest.

**Revised 2026-11-25** (Q-IOMMU-IRQ answered B; this was IOMMU6 before the renumbering). INTx is gone from the plan: the PCI
host's wired sources are never admitted, and this slice makes no kernel change at all. The slice was split so each half is one
PR: **this one** is enumeration, the two pure crates (with the MSI-X capability **parsed** and its table and pending-bit BARs
located) and DMA through the IOMMU with the hand-driven device **polled**; **IOMMU9** programs MSI-X, allocates vectors and brings
the interrupt-driven tests.

## Plan (2026-11-25)

### The acceptance check, written and seen to fail before the code

| Check | Where | What must be seen |
|---|---|---|
| `pcicore` vectors and model | host: `pcicore/tests/` | Configuration-space images captured from QEMU 11.1.3 (the header and capability bytes of `virtio-blk-pci`, `virtio-net-pci`, `virtio-rng-pci` and `iommu-testdev` with `disable-legacy=on`; taken once with the monitor or a probe and committed as fixtures) parse to the vendor, device, class, BAR kinds and sizes, the **MSI-X capability (table size, table BAR and offset, pending-bit BAR and offset)** and the virtio capability list that `info pci` printed for the same machine (the independent listing). BAR assignment over random device sets: no overlap, natural alignment, inside the host's window, 64-bit BARs handled; a hostile image (a capability loop, a pointer outside the space, a BAR size of zero or everything, an MSI-X table that runs past its BAR or overlaps the pending bits or a virtio structure) ends in an error, never a hang or a panic |
| `virtio` transport tests | host: `virtio/tests/` | The common-configuration layout, feature negotiation (a device that offers `VERSION_1` and `ACCESS_PLATFORM` gets both accepted or the bring-up fails), the split-queue layout arithmetic, checked against the offsets and sizes in the VIRTIO 1.x text (spec-derived constants in the test, not imported from the code) |
| `pci_devices_are_found_and_claimed` | suite | `devmgr` lists the PCI devices of the machine; the list equals what the script put on the command line (`os_boot_test.py` passes the expected slots in the kernel arguments or checks a structured `## devmgr: pci` line per device) |
| `pci_virtio_blk_reads_through_the_iommu` | suite, at 1, 2 and 4 harts in shard `k` | The hand-driven test that today is `interrupt_object_fires_and_masks` (`suite.rs:13586`, a queue of 8 in a DMA VMO at `vmo_phys_addr`) runs against a **virtio-blk-pci** device: domain created with the control handle, the buffer (a `vmo_create_dma_in` VMO) mapped through a duplicate with IO_MAP and IO_UNMAP only, as a driver would, the queue programmed with the I/O addresses `iodomain_map` returned, sector 0's pattern arrives (the test polls the used ring: no interrupt in this slice; MSI-X stays disabled and INTx is disabled in the command register), and the IOMMU fault counter did **not** move |
| `pci_virtio_outside_its_mapping_faults` | suite | The same device given one descriptor whose address is outside its mapping: the request fails or the device stops, the target memory is intact, one fault record names this device. This is the negative test Q-IOMMU-MODEL 8 asks to keep for each machine (it proves the device was started with `iommu_platform=on`) |
| `a_child_has_no_device_authority`, `device_caps_need_the_resource` | suite | Extended: no ECAM, no BAR, no domain without the root resource |

### What is built

1. **`pcicore/`** (new pure crate, `no_std`, `forbid(unsafe_code)`): ECAM address arithmetic, header type 0 parsing, the capability
   walk with a step bound, BAR sizing as a two-phase protocol the caller drives (the crate never touches memory: it is given
   bytes and returns what to write), a BAR allocator over the host's memory windows, the requester id, the **function-level
   reset** as a caller-driven sequence (which capability offers it and the wait: IOMMU0 item 16; `devmgr` issues one when a
   driver dies, IOMMU-SPEC 10.2), and the **MSI-X capability**: its fields, the table and
   pending-bit locations checked against the BARs, the 16-byte table entry (address low and high, data, vector control) and the
   message-control bits (enable, function mask) as codecs that IOMMU9 drives. No INTx routing: `interrupt-map` is not read.
2. **`virtio/`** (new pure crate, same rules): the modern virtio-pci capability structures (common, notify, ISR, device
   configuration), the common-configuration register layout, the split virtqueue layout and ring arithmetic, and a small
   `Transport` trait with one implementation over a `Regs`-like trait, so the three drivers stop carrying three copies of the
   register code (X-DRIVER-MODEL: "no shared virtio-queue crate"). The mmio register layout is **not** added to it (those
   drivers are deleted in IOMMU12).
3. **`devmgr`** (`init/src/bin/devmgr.rs`; today it probes the eight virtio-mmio slots, `:106`, `:556`): reads the PCI host from
   the device tree (`resource_dtb`), maps ECAM, enumerates bus 0, assigns BARs (nobody else does: every BAR reads "not mapped" at
   start), keeps a table of functions. It never hands out configuration space. The existing mmio table stays for the unconverted
   drivers. A function is claimable only if it is a virtio device that offers `VIRTIO_F_ACCESS_PLATFORM` (Q-IOMMU-MODEL 8), or
   the test device.
4. **Kernel: nothing.** The PCI host's INTx sources (32 to 35 on `virt`) are **not** admitted (`kcore/tests/aiatree.rs` gains
   the assertion against the dumped tree that no source of the host's `interrupt-map` is in the admitted list), and `devmgr`
   sets the interrupt-disable bit in every function's command register.
5. **`proto::devmgr`** (`proto/src/devmgr.rs`, `proto/tests/wire.rs`): the claim reply's new shape for a PCI device, which is
   what IOMMU-SPEC 9 says a driver is handed: the BAR VMO and the offsets of the four virtio structures, **a handle to the
   device's domain with IO_MAP and IO_UNMAP only**, and the device's DMA VMOs with the `DMA` right (created by `devmgr` with
   `vmo_create_dma_in`, charged to the driver's job). No I/O address travels in the reply: the driver maps its buffers itself
   and learns the addresses from `iodomain_map` (Q-IOMMU-MODEL 3, OD164). The interrupt handles are added by IOMMU9. Added
   beside the old operations, which stay until IOMMU12.
6. **The suite** (`init/src/bin/suite.rs`): the hand-driven tests above, on the shared `virtio` crate; the mmio versions
   (`mmio_reads_the_virtio_magic` and friends) stay until IOMMU12.
7. **The test machine** (`scripts/os_boot_test.py`): one more disk as `virtio-blk-pci,iommu_platform=on,disable-legacy=on` with an
   explicit `addr=` (PCI slots are stable, unlike the mmio order the comment at `:1085` works around). With MSI-X there is no
   limit of four devices: the test device, two disks, network, entropy and later the console can share one boot. The device set
   per boot is still one table in the script.

### Order of device bring-up (the spec's sections 9 and 10; state it in `devmgr`'s header comment)

The claim, in the spec's order: refuse a device without MSI-X or without `ACCESS_PLATFORM`; `iodomain_create` naming the
driver's job (on which finite limits on I/O-mapped pages and vectors were set); create the driver's DMA VMOs; (vectors: IOMMU9);
hand the driver its BAR VMO, its map-and-unmap duplicate of the domain handle and its buffers; **bus mastering last**. The
driver then maps its buffers and starts the device. `devmgr` keeps the control handle and maps nothing.

Teardown when a driver exits or is killed (IOMMU-SPEC 10.1, 10.2): bus-master enable cleared and a **function-level reset**
(wait for it; the old plan's hole (c) was `reset` giving up silently, `devmgr.rs:78`: make it loud), then **the domain is
destroyed** (`iodomain_destroy`), which removes every mapping the driver made. Nothing is kept for the next driver: a re-claim
makes a new domain and the new driver maps afresh (10.4, OD166 point 2). None of this is a safety property any more (the
kernel's ordering in IOMMU6 is); it is what keeps the device working and the fault log quiet.

### Hazards

- **Functions that share a requester id** (behind a bridge) would share one domain; nothing here handles it, bus 0 has none,
  and the grouping is tidy item T276.
- **A device without MSI-X** is listed and not claimable (Q-IOMMU-MSI 5); `devmgr` says so in its `## devmgr: pci` line. QEMU's
  virtio-pci devices have it unless started with `vectors=0`: the script's one device helper never passes that.
- **QEMU's virtio bypass**: a command line that forgets `iommu_platform=on` gives a device that works without any mapping. Only
  the negative test catches it. The script adds the option in one helper so no device line can omit it.
- **A 64-bit BAR assigned above 4 GiB** needs the 64-bit window (at 16 GiB on `virt`); assign everything in the 32-bit window
  (1 GiB at `0x4000_0000`) and refuse what does not fit.
- **Record and replay, and icount** (`OS_RECORD`, `OS_ICOUNT`): not verified with PCI devices and the IOMMU on QEMU 11.1.3. If a
  mode breaks, say so and record a tidy item; do not drop the mode silently.
- `devmgr` now parses bytes a device controls (configuration space): hostile-input rules, the fuzz-style host test above.
- Two new crates: each gets its workspace entry, its place in `make os-test` and `make os-mutants`; they are ours, so no
  decision-log entry for a dependency is needed.

### Acceptance

The table passes in the epic's smoke (kernel-only boots at 1, 2 and 4 harts for the two `pci_virtio_*` tests); the feature gate is
green; the mmio drivers are unchanged and green. Review: default reviewer (`pcicore`, `virtio` and `devmgr`: no kernel change).

PR size: very large (two crates of about 800 to 900 lines each with tests, `devmgr` about 500, suite and scripts about 500). It is
one PR because none of the parts can be seen working in the machine without the others; the interrupt half is IOMMU9.
