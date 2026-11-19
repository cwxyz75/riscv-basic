---
id: SD4
kind: slice
title: Servers publish in the registry; clients reconnect on UNAVAILABLE
area: ipc
horizon: long
---

- [ ] Step 4: the shell grants a `/svc` view; existing servers publish themselves and serve forwarded opens on a listener (`memfsd`, `launcher`, `devmgr`, `blkd`); clients (`rt::fs`, the shell) reconnect on `UNAVAILABLE`; documents (T15, T30 narrowed). **Deliberately deferred** until the first new service (the network stack, S8) needs it: migrating `init`, `sh` and every server now would rewire the whole boot path for no new capability, and the first real client of the directory should shape the reconnect policy

## Note (2026-11-18, plan 4: the v1 kernel)

Plan 4 (OD141) parks this until the kernel is frozen (FZ1): it is user-space work that plan 3 left open, and it is not ordered now. Nothing in plan 4 depends on it.
