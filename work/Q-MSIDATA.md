---
id: Q-MSIDATA
kind: decision
title: iodomain_grant_vector's data word, a return value or an ABI constant
area: kernel
horizon: near
epic: iommu-kernel
---

IOMMU-SPEC 8.3 gives `iodomain_grant_vector(domain, index) -> (interrupt, msi_address, msi_data)`. The call returns at most two
values besides the status (`a1`, `a2`), and 8.2 fixes the data word at 1, the one identity enabled in each vector's file.

IOMMU7 (PR 586) returns the interrupt handle and the MSI-X address, and makes the data word the ABI constant
`abi::iodomain::MSI_DATA` (1). SYSCALLS.md documents this.

The ABI is to be frozen. The owner decides between:

- **(a)** Keep the constant, with a spec amendment to 8.3's signature.
- **(b)** Return the word as a third value. This needs a third return register in the syscall convention, or the pair packed into
  one register.
- **(c)** Another layout.

Nothing else waits on it: the drivers use the constant. The reviewer (Fable, PR 586) noted that the spec signature and the code
differ. The behaviour is equivalent while 8.2 fixes the word.
