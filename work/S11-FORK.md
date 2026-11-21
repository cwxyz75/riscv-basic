---
id: S11-FORK
kind: slice
title: fork shim over copy-on-write snapshot VMOs
area: libc
horizon: long
blocked_by: [FZ1]
---

S11 (compatibility). Acceptance: ported software that needs `fork` runs. OD8 leaves room for it. The layered-API direction (X-LAYERED-API) would keep it a library feature, not a kernel one.

## Note (2026-11-18, plan 4: the v1 kernel)

The kernel primitives are in plan 4: `process_clone_region` and SNAPSHOT children (MM8), `process_map_info` (MM7), a thread started SUSPENDED with written registers (EX1). The shim itself (the C library: enumerate regions, clone each, duplicate the handle table by role, start the child's thread with `a0 = 0`) is follow-on work after the freeze. A `fork` child runs in its parent's job (SNAPSHOT requires it).
