---
id: Q-IOMMU-FAULTDIR
kind: decision
title: IOMMU-SPEC 7.2's "A message is a write": every MSI only, or every message-signalled transaction?
area: drivers
horizon: long
refs: [IOMMU-SPEC, T42]
---

IOMMU-SPEC 7.2 ends the description of the fault console line with "A message is a write." It is unclear what "a message" covers.
Read narrowly, it says a faulting MSI is reported with direction `write` (an MSI is a memory write on the bus, so this is true and
harmless). Read broadly, it could be taken to say that any device transaction the IOMMU classes as a message is reported as a
write, which would also cover requests that are not memory writes (for example ATS or page-request messages, which this kernel does
not enable: section 14). Raised by the IOMMU2 engineer (#578) while building the fault record decoder.

**Recommendation:** it means MSIs only. Amend 7.2 to read "An MSI is reported as a write." No code change: the decoder already
reports an MSI fault as `write`, and the other message kinds are non-goals.

Answer it when IOMMU8 or a later slice touches the fault path; it blocks nothing.
