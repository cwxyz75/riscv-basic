---
id: Q-IOMMU-IRQ
kind: decision
title: Device interrupts after the move to virtio-pci: wired INTx only, or MSI-X with MSI remapping
area: drivers
horizon: long
refs: [T42, IOMMU1, OD148, T262, X-BUS]
---

A PCI device can interrupt in two ways, and under an IOMMU they differ in what a hostile driver can do.

**A. INTx only (recommended for this epic).** MSI-X stays disabled. On QEMU `virt` the PCI host wires INTA to INTD to APLIC
sources 32 to 35 (the tree's `interrupt-map`; rotated by slot), which are level-high: exactly what the interrupt objects handle
today (T262's T59 part: every source is level-high, message id = source + 1, ids below 64). A device's domain contains **no**
mapping of an interrupt-controller page, so a device told to send an MSI faults in the IOMMU and the fault is reported: a driver
cannot use its device to forge another device's interrupt. Cost: four lines. Devices in slots 0 to 3 get one each; a fifth device
shares a line, and invariant D5 ("one live object per source") has no sharing. The test machines use at most four DMA devices (two
disks, network, entropy), so the limit bites the first time a machine has five.

**B. MSI-X now.** Each device gets its own message. Needs: PCI MSI-X table programming in `devmgr`, message ids allocated per
device (not source + 1), more than 64 ids per hart file (T262's T183 part), and MSI translation in the IOMMU: the extended device
context's MSI page table (`msiptp`) so the device's write reaches only the interrupt file it was given. From memory of the
specification (IOMMU0 checks it) an MSI page table confines the **page** (which hart's file), not the interrupt **identity**
written, so a hostile driver could still raise any identity on that file unless each device gets its own guest interrupt file or a
memory-resident file. That is a design of its own.

Questions:
1. A or B for the epic? Recommended: **A**, with B recorded as a later slice under X-BUS (it is needed for more than four
   devices and for real hardware).
2. Under A, what when a fifth DMA device appears: refuse to claim it (recommended: loud, simple), or shared lines?
3. The IOMMU's own interrupts (fault queue, command queue): wired (APLIC sources 36 to 39, `fctl.WSI`) or MSI? Recommended: wired,
   as kernel-owned sources that no process can take (the same hook T14's plan needs for the console's receive interrupt).

Blocks IOMMU1. Source: T42 re-planning, 2026-11-25.

## Answer (owner, 2026-11-25)

**B: MSI-X with MSI remapping, in this epic.** The owner's words: "Let's be bold and go straight to plan B"; the standing policy
is "do what a proper OS would do". Option A (INTx only, four lines, refuse a fifth device) is rejected as the plan. Slice ids
below are the ids after the renumbering of the same day (the old IOMMU4 to IOMMU9 are now IOMMU5, 6, 8, 10, 11 and 12; IOMMU4,
IOMMU7 and IOMMU9 are new).

1. **B.** PCI INTx is never used: the PCI host's wired sources (32 to 35 on `virt`) are never admitted to `interrupt_create`, a
   device's INTx is disabled in its command register, and a DMA device without an MSI-X capability is not claimable (see
   Q-IOMMU-MSI 5). The design the old text called "a design of its own" was done the same day and is in IOMMU7 ("The isolation
   design"): one MSI page-table entry per granted vector, in **memory-resident interrupt file (MRIF) mode**, so the only message
   that ever reaches a hart's interrupt file is a notice whose identity and target the kernel chose. A driver that fully controls
   its device (and the device's MSI-X table) can raise the vectors it was granted and nothing else. The old text's worry is
   confirmed by QEMU 11.1.3's source (`hw/riscv/riscv-iommu.c`, `riscv_iommu_msi_write`): in basic-translate mode the device's
   data word is forwarded unchanged to the file page, so that mode confines the file and not the identity; MRIF mode is what
   closes it, and QEMU implements it.
2. **Moot** (no INTx lines, so no fifth-device limit). What bounds devices now is interrupt identities: 254 usable per hart file
   on QEMU `virt` (`riscv,num-ids = <0xff>`, identity 1 is the inter-hart interrupt), allocated per hart (Q-IOMMU-MSI 2).
3. **The IOMMU's own interrupts stay wired** (`fctl.WSI` = 1, APLIC sources 36 to 39 on `virt`), as kernel-owned sources that no
   process can take: the recommendation is **kept**, after checking the alternative. Why not MSI: (a) the wired line is a level
   that follows `ipsr`, so a fault that arrives while the handler runs cannot be lost and the handler needs no re-check protocol;
   with MSI the kernel must re-read `ipsr` after every acknowledgement; (b) the IOMMU's own messages are not translated by the
   IOMMU, so they gain nothing from the MSI page tables; (c) the tree gives both (`interrupts` and `msi-parent`), and
   `capabilities.IGS` reads "both" on this device, so nothing forces MSI. What MSI would add is a machine with an IMSIC and no
   APLIC, which is not this profile (KERNEL-V1-SPEC 2 requires both). Linux's platform driver prefers MSI and falls back to wired
   (from memory of `iommu-platform.c`, not re-read); that preference is about such machines. Two consequences of B for these
   sources: their message identities come from the same per-hart allocator as everything else (IOMMU4), not source + 1; and the
   kernel-owned-source hook is built by IOMMU4 alone, because T14's plan no longer builds one (OD146: the terminal becomes a
   virtio-console server, which is one more MSI-X device).

New questions that B raises are in **Q-IOMMU-MSI** (it also blocks IOMMU1).
