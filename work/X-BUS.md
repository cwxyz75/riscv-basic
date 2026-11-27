---
id: X-BUS
kind: topic
title: Bus enumeration
area: drivers
horizon: someday
---

PCIe (config space, BARs, MSI/MSI-X, hotplug) and USB (host controller, hubs, class drivers for storage, keyboard, serial, network). The decision on whether the OS can leave QEMU.

(Not specified, not scheduled, not committed: a direction from the design conversations of 2026-11-14 and 2026-11-15. Needs a spec, an acceptance check and an OD entry before it becomes a slice.)

## Note (2026-11-25): the PCI part is planned in the IOMMU epic (T42)

One ECAM host, bus 0 and MSI-X are no longer only a direction: they are committed work of the IOMMU epic, specified in `docs/specs/IOMMU-SPEC.md` (section 9) and decided by OD148 and OD163 to OD166. **IOMMU8** builds PCI enumeration, BAR assignment and the capability walk in `devmgr` (the kernel has no PCI code) with the pure crates `pcicore` and `virtio`; **IOMMU9** builds MSI-X. The precondition "an IOMMU" for handing a PCI device to a driver is **planned** (state and order: `work/T42.md`); it is met when IOMMU12's epic smoke is green. What stays in this topic: bridges and buses other than bus 0 (the grouping of functions that share a requester id is T276), the older MSI capability, PCI INTx (never admitted: OD165), hotplug, and all of USB.

## Note (2026-11-27): the precondition "an IOMMU" is met (IOMMU12)

The IOMMU epic's last slice (IOMMU12, `epic/iommu-drivers`) has merged into its epic: every DMA device the kernel admits is a PCI function
behind the IOMMU (`virtio,mmio` is off the device-window allowlist, and its slots and sources are refused), no physical address is
learnable from user mode (`vmo_phys_addr` and the old `vmo_create_dma` are NOT_SUPPORTED library stubs, their numbers retired), and the
hostile-driver matrix (a driver process with exactly a claim's handles, aiming its device and its messages at everything it must not
reach) is in the suite. The precondition is met in the build; it is met on `main` when the epic's smoke is green and the epic merges.
What stays in this topic is unchanged: bridges and buses other than bus 0 (T276), the older MSI capability (T280), PCI INTx (never
admitted: OD165), hotplug, and all of USB.
