---
id: X-OTP
kind: topic
title: Borrow from OTP for the userland services
area: ipc
horizon: someday
---

A shared service-loop library (like `gen_server`), a supervisor library with restart strategies and a restart-intensity limit instead of rebuilding the whole stack (T15), a name registry and reconnecting clients (T30), state that outlives a service restart (T16), and the TCP stack as a state machine (like `gen_statem`) under a connection supervisor. The kernel side of the idea is "share nothing, send messages between harts".

(Not specified, not scheduled, not committed: a direction from the design conversations of 2026-11-14 and 2026-11-15. Needs a spec, an acceptance check and an OD entry before it becomes a slice.)
