---
id: S11-PROCSRV
kind: slice
title: The process server
area: session
horizon: long
---

- [~] `posix_spawn` (done, OD118), a signals subset (first part done 2026-11-17, OD119: handlers, masks, `EINTR`, `alarm`, `kill` within a chain; delivery only at waits; the host is the oracle), the process server

What is left of this line is the process server (PIDs, `wait`); see also Q-UV5.
