---
id: X-LOGGING
kind: topic
title: Logging
area: platform
horizon: someday
---

A preallocated kernel ring buffer (structured records, never blocks, usable in the panic path) with serial as one consumer, plus a userspace `logd` for persistent and queryable logs (shared-memory rings, capability-carried source identity, per-source rate limits, file and network sinks, a shell `log` command). Verdict lines stay on their own path. The lock-order placement against the BKL, heap and UART is the design question. Would let T14's bridge go.

(Not specified, not scheduled, not committed: a direction from the design conversations of 2026-11-14 and 2026-11-15. Needs a spec, an acceptance check and an OD entry before it becomes a slice.)
