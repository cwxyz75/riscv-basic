---
id: Q-IOMMU-NONLEAF
kind: decision
title: IOMMU-SPEC 6.1 and 15.2 - which invalidation covers a table frame an unmap freed
area: drivers
horizon: long
refs: [IOMMU3, IOMMU-SPEC, T42]
---

Found by the Fable review of IOMMU3 (PR #580, finding 1). IOMMU-SPEC 6.1 step 1 asks for "the invalidation that covers every page
of the range" and step 4 frees the table frames the unmap emptied after the fence. An `IOTINVAL.GVMA` with an address (AV=1) is
specified by the RISC-V IOMMU specification for the leaf translation of that address; whether it must also drop the IOMMU's cached
**non-leaf** entries (a page-walk cache) is not something the spec's section 15.2 lists as an assumption, and a page-wise
invalidation followed by freeing a table frame would let a page-walk cache point into a freed (and reused) frame: IO3 for table
frames. QEMU and `iommucore::sim` cache only final translations, so no test can see it.

What IOMMU3 built (within the spec's text: the whole-context form also covers every page): an unmap or a map's roll-back that
detached a table frame queues `IOTINVAL.GVMA` for the whole soft context (`kcore::iodomain::inval_for`), and only a short unmap that
emptied no table is invalidated page by page. Pinned by `an_unmap_that_empties_a_table_invalidates_the_whole_soft_context`.

For the owner:

1. Settle it in the spec: a 6.1 sentence ("a range whose unmap empties a table is invalidated for the whole soft context") and a
   15.2 assumption row (the address form drops leaf entries only; the whole-context form drops non-leaf ones), with the check that
   settles it (the IOMMU spec text, and a page-walk cache added to `iommucore::sim` so a mutation of `inval_for` fails a test).
2. Or always invalidate the whole soft context (simplest; costs re-walks for the other mappings of the domain).
3. Or rely on a later IOMMU feature (an NL bit in newer IOMMU spec revisions) where present: not on the RVA23 QEMU target today.
