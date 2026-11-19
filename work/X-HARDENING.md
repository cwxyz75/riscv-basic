---
id: X-HARDENING
kind: topic
title: Authentication and hardening
area: security
horizon: someday
---

A credential store; ASLR, stack protection, W^X across userland; syscall fuzzing as a standing check if soak does not already cover it.

(Not specified, not scheduled, not committed: a direction from the design conversations of 2026-11-14 and 2026-11-15. Needs a spec, an acceptance check and an OD entry before it becomes a slice.)

## Note (2026-11-18, plan 4: the v1 kernel)

Taken up for the kernel by OD139: HD1 to HD5, KT3, KT8, KT9. What remains here: the credential store, building shadow stacks, landing pads and pointer masking behind the places HD5 reserves, stack-clash protection in the user toolchain, and X-SPECULATION.
