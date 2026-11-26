---
id: Q-IOMMU-WORKBOUND
kind: decision
title: IOMMU-SPEC 11 - the bound on work done under IOMMU (taken with interrupts off) and under VMO for a DMA VMO
area: drivers
horizon: long
refs: [IOMMU3, IOMMU6, IOMMU-SPEC, T42]
---

Found by the Fable review of IOMMU3 (PR #580, finding 13). IOMMU-SPEC 11.1 takes `IOMMU` with interrupts off (the fault handler
takes it), but nothing in the spec bounds the work one call does under it:

* `iodomain_map` and `iodomain_unmap` write or clear one leaf per page of the range, and a range is bounded only by the job's
  I/O-page limit and the window (2^18 pages less the interrupt window): up to about 262,000 leaf writes with interrupts off;
* the destruction's completion (6.3) detaches every table frame of the domain and flattens its mapping records under `IOMMU`;
* `vmo_create_dma_in` (4.7) zeroes every page of the new VMO under `VMO` (the rest of the VMO store works in steps of
  `RANGE_STEP`, T206's `irq::window`), with no per-object limit.

IOMMU3's store (`kcore::iodomain`) is a host model; nothing runs with interrupts off yet. IOMMU6 wires it into the kernel and needs
the rule before it does. Options for the owner:

1. A per-call limit on pages (`iodomain_map` refuses more than N pages: INVALID_ARGS), N chosen for a bounded lock hold; a large
   buffer is mapped in several calls. The destruction's completion stays proportional to the domain's size (bounded by the window).
2. Steps: a map or unmap of many pages releases and retakes `IOMMU` every `RANGE_STEP` pages (the range stays reserved or pending,
   so the protocol of 6.1 holds across the steps), and the destruction detaches in steps likewise.
3. Take `IOMMU` with interrupts enabled except in the fault handler's drain (a different lock rule for the fault path).

And for `vmo_create_dma_in`: zero in `RANGE_STEP` steps outside `VMO` (the VMO is not yet visible), as the other bulk VMO paths do.
