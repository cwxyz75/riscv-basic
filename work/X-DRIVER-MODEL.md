---
id: X-DRIVER-MODEL
kind: topic
title: A real driver model that is not QEMU `virt`
area: drivers
horizon: someday
---

Drivers are already userspace programs holding capabilities, and `devmgr` reads the DTB (slice DM). What is still QEMU-shaped: `DEVICE_WINDOWS` and `MAX_IRQ` in `abi` are constants, not derived from the DTB; `devmgr` matches only virtio-mmio block, with no `compatible`-to-driver table; the kernel UART bridge (T14); trust-based DMA with no IOMMU or cache maintenance (T42); no shared virtio-queue crate. Also: fault-injecting fake devices as an independent oracle for drivers, more QEMU machine types (PL011, SDHCI), and one real board as a periodic (not merge) hardware gate.

(Not specified, not scheduled, not committed: a direction from the design conversations of 2026-11-14 and 2026-11-15. Needs a spec, an acceptance check and an OD entry before it becomes a slice.)

## Note (2026-11-25): three of these points are planned in the IOMMU epic (T42)

"Trust-based DMA with no IOMMU" is being replaced: a kernel-managed IOMMU with a domain per device, DMA and interrupt isolation, is specified (`docs/specs/IOMMU-SPEC.md`) and decided (OD148, OD162 to OD166), and planned as IOMMU2 to IOMMU12 (state and order: `work/T42.md`). The precondition "an IOMMU" for a driver that is not trusted is therefore **planned**, and met when IOMMU12's epic smoke is green. "No shared virtio-queue crate" is **IOMMU8** (the `virtio` crate). "`devmgr` matches only virtio-mmio block" changes with **IOMMU8** to **IOMMU11** (PCI enumeration and a class table; virtio-mmio leaves the OS track). Not covered by the epic and still here: cache maintenance for DMA, `MAX_IRQ` and the other constants, the UART bridge (T14), fake devices as oracles, other machine types and a real board (QEMU's IOMMU is the only one the epic tests).
