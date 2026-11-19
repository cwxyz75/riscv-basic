---
id: X-MEMORY
kind: topic
title: Memory maturity
area: kernel
horizon: someday
---

Pager-backed VMOs, a page or buffer cache, an out-of-memory policy, a per-process memory quota (Q9), and dynamic linking and shared libraries (to be confirmed: only static linking was seen).

(Not specified, not scheduled, not committed: a direction from the design conversations of 2026-11-14 and 2026-11-15. Needs a spec, an acceptance check and an OD entry before it becomes a slice.)

## Note (2026-11-18, plan 4: the v1 kernel)

Taken up for the kernel by OD133, OD135 and OD138: MM2 to MM10. What remains here: a page cache in the file servers, an out-of-memory policy above the kernel (the kernel only refuses), large pages for user mappings, and dynamic linking (DL1).
