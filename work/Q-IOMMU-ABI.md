---
id: Q-IOMMU-ABI
kind: decision
title: The DMA calls under an IOMMU: does vmo_phys_addr go, who pays, and may the frozen ABI change
area: drivers
horizon: long
refs: [T42, IOMMU1, OD148, T262, KERNEL-V1-SPEC]
---

With a kernel-managed IOMMU a driver programs its device with I/O virtual addresses, so nothing in user mode needs a physical
address any more. Four linked questions that fix the call surface IOMMU1 writes down. Each has a recommendation; "as recommended"
is a complete answer.

1. **Is `vmo_phys_addr` removed, and the `DMA` right given a new meaning?** Recommended: yes. `vmo_phys_addr` is deleted when the
   last virtio-mmio driver is gone (IOMMU12); the `DMA` right (bit 12) comes to mean "this VMO may be mapped into a device domain".
   Invariant D2 becomes "no physical address is learnable from user mode". This removes a symbol from `libsys/symbols-v1.txt` and a
   call number: a flag-day ABI change. Alternative: keep the call (it then leaks kernel layout for no use).
2. **May the v1 ABI take the new calls** (a device-domain object, map, unmap; see IOMMU1 for the list) and lose one, before or
   after FZ1? OD148 says FZ1 timing is free; KERNEL-V1-SPEC 9 says a new call is a spec amendment. Recommended: yes, as an amendment
   of KERNEL-V1-SPEC, whenever the epic lands.
3. **Does a DMA VMO stay physically contiguous?** Under an IOMMU the device sees contiguous I/O addresses whatever the frames are,
   so `MAX_DMA_PAGES` (64, the buddy order) and "NO_MEMORY for want of an aligned block" lose their reason. Recommended: keep
   `vmo_create_dma` as it is through the epic (a pinned, committed, zeroed VMO; contiguity harmless), and record a tidy item to
   drop the contiguity and the 64-page limit afterwards. Alternative: change it in the epic (more churn in IOMMU6).
4. **Who pays, and what bounds it (T262's T41 and T84 parts)?** OD148 said these go with the model and the driver steps.
   Recommended: (a) the page charge of a DMA VMO goes to a job the creator names (a `vmo_create_dma` variant with a job handle, as
   `vmo_create` has under OD57), so `netsup`'s or the driver's job pays, not `devmgr`'s; (b) the system-wide DMA backstop (RAM / 16)
   stays as a backstop and a **per-job limit on I/O-mapped pages** is added to the job limits; (c) the IOMMU's own table frames are
   charged to the domain creator's job as kernel bytes (OD152's rule for page tables). Alternative for (b): no per-job limit, the
   job's page limit is enough.

Blocks IOMMU1 (the spec states the answer). Source: T42 re-planning, 2026-11-25; T262 (parts T41, T84).

## Answer (owner, 2026-11-25)

Accepted as recommended for 1, 2 and 4, with 3 changed:

1. `vmo_phys_addr` goes (at IOMMU12); `DMA` means "may be mapped into a device domain"; D2 becomes "no physical address is learnable from user mode".
2. The new calls and the removal are an amendment of KERNEL-V1-SPEC, whenever the epic lands.
3. **Changed:** a DMA VMO is NOT promised physically contiguous, and the 64-page limit and the "no aligned block" `NO_MEMORY` go **in this epic** (IOMMU6), not in a later tidy item. The spec says contiguity is not guaranteed. The VMO stays pinned, committed, zeroed and fixed (no decommit or resize).
4. The creator names the job that pays (a `vmo_create_dma` variant with a job handle); the RAM/16 backstop stays and a per-job limit on I/O-mapped pages is added; the IOMMU's table frames are charged to the domain creator's job as kernel bytes (OD152).

