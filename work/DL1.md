---
id: DL1
kind: slice
title: A dynamic loader and a dynamically linked C library (follow-on, after the freeze)
area: libc
horizon: long
blocked_by: [FZ1]
refs: [KERNEL-V1-SPEC, OD128, USER-VIEW-SPEC]
---

Not ordered in plan 4. What OD128 leaves for user space: a loader (`ld.so`: `PT_INTERP`, `DT_NEEDED`, symbol resolution, relocation, `dlopen`, ELF TLS), the C library and `rt` as shared objects with versioned symbols, private file mappings of their text through PRIVATE children of pager-backed VMOs (MM8, MM9, S11-MMAP), and the rule that the server protocols are private behind them. Needs its own spec amendment (USER-VIEW-SPEC) and acceptance check before it starts: a dynamically linked program that keeps running across a change of a server protocol it does not know about.
