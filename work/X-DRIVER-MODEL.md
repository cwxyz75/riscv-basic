---
id: X-DRIVER-MODEL
kind: topic
title: A real driver model that is not QEMU `virt`
area: drivers
horizon: someday
---

Drivers are already userspace programs holding capabilities, and `devmgr` reads the DTB (slice DM). What is still QEMU-shaped: `DEVICE_WINDOWS` and `MAX_IRQ` in `abi` are constants, not derived from the DTB; `devmgr` matches only virtio-mmio block, with no `compatible`-to-driver table; the kernel UART bridge (T14); trust-based DMA with no IOMMU or cache maintenance (T42); no shared virtio-queue crate. Also: fault-injecting fake devices as an independent oracle for drivers, more QEMU machine types (PL011, SDHCI), and one real board as a periodic (not merge) hardware gate.

(Not specified, not scheduled, not committed: a direction from the design conversations of 2026-11-14 and 2026-11-15. Needs a spec, an acceptance check and an OD entry before it becomes a slice.)
