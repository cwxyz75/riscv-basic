---
id: Q-KONLY-SERVERS
kind: decision
title: KERNEL-V1-SPEC tier 1's kernel-only boot has "no servers", but the IOMMU device tests in shard k start devmgr and blkd
area: testing
horizon: long
refs: [KERNEL-V1-SPEC, TESTING, T42, IOMMU8, IOMMU9, IOMMU10]
---

KERNEL-V1-SPEC section on the testing regime describes tier 1 as "a **kernel-only boot**: the kernel, `init` and the
kernel-facing tests of the suite, with no servers, at one, two and four harts". The IOMMU tickets (IOMMU8, IOMMU9, IOMMU10,
all on `epic/iommu-drivers`) put their device tests in shard `k` at 1, 2 and 4 harts, and those tests now start `devmgr`
(IOMMU9, #598) and `devmgr` plus `blkd` on the PCI disks (IOMMU10, #599): the PCI claim path, the vector grants and the
driver-restart tests exercise kernel calls only through those servers. Raised by the IOMMU10 engineer on 2026-11-27.

The spec and the tickets disagree. Options:

- **A.** Amend tier 1 to allow the device servers the kernel-facing device tests need (`devmgr`, a driver under test),
  started by the test itself and torn down by it; the boot stays "no system servers" (no `fsd`, `netd`, session).
- **B.** Keep tier 1 server-free: move the server-driven device tests out of shard `k` into the full suite shards, and keep
  in shard `k` only tests that drive the device by hand with the root resource (as IOMMU8's first version did).

**Recommendation:** A. The tests drive kernel paths (domains, grants, the teardown order) that a real driver reaches only
through the supervisor, and they start and stop the servers themselves. No spec was changed; the code follows the tickets.
