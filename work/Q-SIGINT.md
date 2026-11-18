---
id: Q-SIGINT
kind: decision
title: Review policy findings of 2026-11-17 still open
area: signals
horizon: long
---

T148: the independent review's policy items (1), (3) and (5) are the owner's. (1) was decided 2026-11-18 (T149: SIGQUIT and SIGTSTP as the uncatchable escape) and (3) was built (OD120). Open: (5) SIGINT goes to the foreground roots only: a root that handles it (a shell waiting on a child) does not pass it down.

Source: T148.
