---
id: X-POWER
kind: topic
title: Power management
area: platform
horizon: someday
---

Layers, cheap to hard: tickless idle on a hardware-counter monotonic clock (the 1 ms tick today); clean shutdown and reboot (SBI SRST, PSCI on Arm) with a service-notification protocol and a deadline; hart park and unpark; a userspace `powerd` holding a power-control capability and owning policy; device power states through `devmgr`; DVFS and thermal (needs real hardware); suspend (probably never). Interrupt objects need a wake-source attribute. Park/unpark against IPIs and TLB shootdowns belongs under loom/shuttle first.

(Not specified, not scheduled, not committed: a direction from the design conversations of 2026-11-14 and 2026-11-15. Needs a spec, an acceptance check and an OD entry before it becomes a slice.)

## Note (2026-11-18, plan 4: the v1 kernel)

Taken up for the kernel by OD132: no tick (L13), idle states (PM2), harts offline and online (PM1), suspend (PM3), a placement policy (PM4). What remains here: heterogeneous harts and energy-aware placement, frequency scaling (a driver plus per-hart load figures), driver quiescence for suspend, and an audit of user-space polling (`netd`'s safety poll, the C library's heap lock).
