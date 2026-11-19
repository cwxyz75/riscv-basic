---
id: X-ABI-STABILITY
kind: topic
title: ABI and protocol stability
area: ipc
horizon: someday
---

A versioning and deprecation policy for syscalls and `proto::*` (how a new kernel runs old binaries, how a field is added). Cheap now, expensive after more servers exist; the one to raise earliest.

(Not specified, not scheduled, not committed: a direction from the design conversations of 2026-11-14 and 2026-11-15. Needs a spec, an acceptance check and an OD entry before it becomes a slice.)

## Note (2026-11-18, plan 4: the v1 kernel)

Taken up for the kernel by OD128 (the system library is the contract) and built by AB7 to AB9. What remains of this topic is the `proto::*` half: versioning the server protocols, which becomes moot for programs that link the C library dynamically (DL1) and remains for servers talking to each other.
