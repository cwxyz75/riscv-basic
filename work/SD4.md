---
id: SD4
kind: slice
title: Servers publish in the registry; clients reconnect on UNAVAILABLE
area: ipc
horizon: near
---

- [ ] Step 4: the shell grants a `/svc` view; existing servers publish themselves and serve forwarded opens on a listener (`memfsd`, `launcher`, `devmgr`, `blkd`); clients (`rt::fs`, the shell) reconnect on `UNAVAILABLE`; documents (T15, T30 narrowed). **Deliberately deferred** until the first new service (the network stack, S8) needs it: migrating `init`, `sh` and every server now would rewire the whole boot path for no new capability, and the first real client of the directory should shape the reconnect policy
