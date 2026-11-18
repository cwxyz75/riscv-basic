---
id: Q-OS5
kind: decision
title: The exec model: spawn-and-replace or a kernel reset-process operation
area: kernel
horizon: long
---

**Q5: `exec` model:** spawn-and-replace vs a kernel "reset process" operation.

Note (2026-11-18): `posix_spawn`, `execve` and `vfork` exist in the libc over the spawn grant (OD118); check whether that answers this before asking.

Source: OS-ARCHITECTURE section 18.
