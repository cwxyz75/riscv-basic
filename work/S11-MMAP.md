---
id: S11-MMAP
kind: slice
horizon: long
blocked_by: [MM9, FZ1]
area: libc
title: mmap of files, private and shared, in the C library and the file servers
---

S11 (compatibility): `mmap` private and shared; pager-backed VMOs. Who pays when a client dirties pages of a server's file mapped writable was deferred here by slice V (likely private copy-on-write pages to the mapper's job, shared pages to the VMO's owner).

## Note (2026-11-18, plan 4: the v1 kernel)

Q-OS4 is decided (OD133): the kernel half (pager-backed VMOs, blocking faults, PRIVATE children) is MM9 and MM8 in plan 4. This item is what remains in user space, after the freeze: `fsd` and `memfsd` as pagers of their file caches, the `GetVmo` reply as a pager-backed VMO, `mmap(MAP_PRIVATE)` as a PRIVATE child and `MAP_SHARED` as the VMO itself, and, when shared writable mappings matter, the reserved dirty-tracking extension. Who pays: private copies are charged to the mapper's job (COW2); shared pages to the server's.
