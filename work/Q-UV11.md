---
id: Q-UV11
kind: decision
title: Self-hosting, and compiling C on the OS
area: ports
horizon: long
---

**Q11: the self-hosting bring-up path**: cross-build and run natively (Proposal), or bootstrap from tcc.

Finding of 2026-11-17 (the old work plan (retired 2026-11-25; git history), decisions to discuss): the pinned tcc 0.9.27 has no RISC-V back end. Options, none started: tcc from its development branch (a pinned commit archive), QBE with `cproc`, or neither (the OS stays cross-built on the host). The tcc entry in `third_party/fetch-manifest.txt` is unused.

Source: USER-VIEW-SPEC section 15; WORK-PLAN 2026-11-17.
