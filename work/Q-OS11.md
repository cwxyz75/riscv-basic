---
id: Q-OS11
kind: decision
title: Wall-clock time: what is left to decide
area: fs
horizon: long
---

**Q11: Entropy and time:** entropy is resolved (virtio-rng, `rngd`, OD108). The wall clock is proposed in `FS-SPEC.md` section 7: a `clockd` service over a real-time clock claimed through `devmgr`, kernel time stays monotonic (OD91); not decided until the owner accepts the spec.

Note (2026-11-18): `clockd` is built (FS-SPEC step C1); what may remain is time zones and time sync.

Source: OS-ARCHITECTURE section 18.
