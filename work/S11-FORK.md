---
id: S11-FORK
kind: slice
title: fork shim over copy-on-write snapshot VMOs
area: libc
horizon: long
---

S11 (compatibility). Acceptance: ported software that needs `fork` runs. OD8 leaves room for it. The layered-API direction (X-LAYERED-API) would keep it a library feature, not a kernel one.
