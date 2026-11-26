---
id: Q-IOMMU-UNMAPORDER
kind: decision
title: "iodomain_unmap's error order makes BAD_STATE unreachable"
area: kernel
horizon: near
refs: [IOMMU6, IOMMU-SPEC, SYSCALLS]
---

**Found building IOMMU6 (2026-11-26).** IOMMU-SPEC 4.4 gives `iodomain_unmap`'s errors in this order: BAD_HANDLE; WRONG_TYPE;
ACCESS_DENIED; INVALID_ARGS (not exactly a live mapping of this domain); BAD_STATE (the domain was destroyed: its mappings went
with it). A destroyed domain has no live mapping, so the INVALID_ARGS before it always applies and BAD_STATE can never be
returned.

**What is built (IOMMU6, as the spec reads):** an unmap on a destroyed domain is INVALID_ARGS, and BAD_STATE is not reached
(`ksys/src/iodomain.rs` `iodomain_unmap`; SYSCALLS.md says so; the host test `iodomain_unmap_rows` pins it).

**The question for the owner:** keep it as it is (amend 4.4 to drop BAD_STATE or to say it is unreachable), or put BAD_STATE
before "not a live mapping", so that a driver can tell "my domain was destroyed" from "a wrong address". `iodomain_map` on a
destroyed domain is already BAD_STATE. The second option makes the two calls consistent, and is what the first IOMMU6 draft did.
Either one is a one-line change in `iodomain_unmap` plus the test row.
