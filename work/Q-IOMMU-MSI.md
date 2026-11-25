---
id: Q-IOMMU-MSI
kind: decision
title: MSI-X under the IOMMU: is MRIF required, how identities are allocated, where a vector is delivered, limits, and devices without MSI-X
area: drivers
horizon: long
refs: [T42, IOMMU1, IOMMU4, IOMMU7, Q-IOMMU-IRQ, OD148, OD47, OD127, T262]
---

Q-IOMMU-IRQ was answered B (MSI-X with MSI remapping, 2026-11-25). The isolation design is in IOMMU7; these are the choices in it
that only the owner can make. Each has a recommendation, framed as what a proper OS that scales to 64 harts and many devices
does; "as recommended" is a complete answer.

1. **Is `capabilities.MSI_MRIF` a profile requirement (refuse to boot without it)?** Identity isolation rests on MRIF mode. The
   RISC-V IOMMU specification makes it optional; QEMU 11.1.3 has it (read in its source, and the platform device's capability
   register has bit 23 set). Recommended: **required, no fallback** (the rule of OD47 and OD127: RVA23, AIA and Sstc are enforced
   the same way). Alternative: fall back to basic-translate entries on an IOMMU without MRIF. That fallback confines a device to
   one hart's interrupt file but lets it raise **any** identity there (another device's interrupt, the inter-hart interrupt): a
   hostile driver could then forge interrupts for other drivers and storm a hart, though it still could not touch memory. The spec
   would have to state that as a threat-model limit; real hardware would close it only with MRIF or one guest interrupt file per
   device.
2. **Interrupt identities: allocated per hart, or one global space?** Per hart: each hart's file has its own identities (254 on
   QEMU, up to 2047 on hardware), so the system has 254 times the hart count; moving a vector to another hart takes a new identity
   there and a retire step for a message still in flight to the old one (the protocol Linux's IMSIC driver uses, read in
   `irq-riscv-imsic-state.c`). Global: one identity is reserved on every hart, a move is one write, and the whole system has 254
   vectors on QEMU. Recommended: **per hart** (it is what scales; the retire step also removes the "message in flight at
   re-creation" stand-in of T262's T59 part for wired sources).
3. **Where is a vector delivered, and does it follow the driver thread?** Wired sources today re-aim at every `interrupt_ack` (one
   APLIC write). For an MSI vector a move is a rewrite of its MSI page-table entry, an invalidation command and a fence under the
   IOMMU's one lock. Recommended: **no automatic follow**. The kernel places a vector on the hart of the first wait or
   acknowledgement, moves it when that hart goes offline, and otherwise only on an explicit call by the interrupt handle's holder
   (a "deliver here" operation; the spec names it). Alternative: re-aim at acknowledgement whenever the hart differs (uniform with
   wired sources, but a thread that migrates, or a hostile one that bounces, puts IOMMU commands on the acknowledgement path).
4. **Limits and who pays.** Recommended: at most 256 vectors per device (one frame of MSI page table); a **per-job limit on
   interrupt vectors** in the job limits (like the I/O-mapped page limit of Q-IOMMU-ABI 4), charged to the job the domain was
   created for; the MSI page table and the interrupt files in memory are kernel bytes of that job (OD152). Alternative: no per-job
   limit, only the per-device cap and identity exhaustion (then one job can take every identity of a hart).
5. **A DMA device without MSI-X.** Recommended: **not claimable**; `devmgr` says so loudly and leaves it with bus mastering off.
   The PCI host's INTx sources are never admitted, so D5's "one object per source" never meets a shared line. (QEMU's virtio-pci
   devices all have MSI-X; the IOMMU test device needs no interrupt.) Alternative: admit INTx for such a device (brings back the
   four-line limit and sharing for exactly the devices least worth it).
6. **Who may grant a vector?** Recommended: the holder of the device domain's handle with a new right (the supervisor: `devmgr`,
   `netsup`), never the driver; the driver receives interrupt handles that can wait and acknowledge and nothing that names an
   identity, a hart file or an address. The MSI-X table is programmed by the supervisor where the device keeps it in a BAR of its
   own (QEMU's virtio-pci: BAR1), but **isolation does not depend on that**: a device whose table sits beside its registers can be
   pointed anywhere by its driver and is still confined by the IOMMU.

Blocks IOMMU1 (the spec states the answers) and, through it, IOMMU4 and IOMMU7. Source: the re-planning after Q-IOMMU-IRQ's
answer, 2026-11-25.

## Answer (owner, 2026-11-25)

All six accepted as recommended, with a note added to 1:

1. `MSI_MRIF` is a profile requirement: the kernel refuses to boot without it, no fallback (OD47, OD127). **Known fallback design, not built:** one IMSIC guest interrupt file per device with basic MSI translation, as VFIO does for passthrough. It gives identity isolation without MRIF but caps the device count at the guest files available (QEMU's tree shows 0 to 7 per hart). Build it only if the project ever targets hardware without MRIF; record it then as its own slice. MRIF is little exercised in practice (Linux's IOMMU driver does not use MSI page tables), so IOMMU7's acceptance check is written and seen to fail first, to learn early whether QEMU's MRIF path works.
2. Interrupt identities are allocated per hart, with a retire step for a message in flight.
3. No automatic follow of the driver thread: a vector is placed on the hart of the first wait or acknowledgement, moved when that hart goes offline, and otherwise only by an explicit "deliver here" call by the handle's holder.
4. At most 256 vectors per device; a per-job limit on interrupt vectors (needed against identity exhaustion); the MSI page table and interrupt files are kernel bytes of that job (OD152).
5. A DMA device without MSI-X is not claimable (`devmgr` says so loudly and leaves bus mastering off). A shared wired line is itself an isolation hole; a first-class INTx path waits for a real device that needs it.
6. Only the holder of the device domain's handle with the new right (the supervisor) grants vectors; the driver gets interrupt handles that wait and acknowledge and name nothing else.

