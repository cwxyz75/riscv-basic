---
id: S11-MMAP
kind: slice
title: mmap private and shared; pager-backed VMOs
area: kernel
horizon: long
blocked_by: [Q-OS4]
---

S11 (compatibility): `mmap` private and shared; pager-backed VMOs. Who pays when a client dirties pages of a server's file mapped writable was deferred here by slice V (likely private copy-on-write pages to the mapper's job, shared pages to the VMO's owner).
