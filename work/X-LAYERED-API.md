---
id: X-LAYERED-API
kind: topic
title: A layered API
area: security
horizon: someday
---

The native API (handles, rights, ports, VMOs, jobs, protocols) is the real one; a POSIX-ish library is a shim over it. Decide which POSIX features get a shim and which are refused (candidates to refuse or keep outside the core: `fork`, asynchronous signals, users and permission bits, ambient paths, ptys). Note S11 above still plans a `fork` shim; this direction would keep it a library feature, not a kernel one. Rule for S8/S9: the network server gets a native protocol and BSD sockets are a shim over it, not the reverse.

(Not specified, not scheduled, not committed: a direction from the design conversations of 2026-11-14 and 2026-11-15. Needs a spec, an acceptance check and an OD entry before it becomes a slice.)
