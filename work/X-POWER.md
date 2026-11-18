---
id: X-POWER
kind: topic
title: Power management
area: platform
horizon: someday
---

Layers, cheap to hard: tickless idle on a hardware-counter monotonic clock (the 1 ms tick today); clean shutdown and reboot (SBI SRST, PSCI on Arm) with a service-notification protocol and a deadline; hart park and unpark; a userspace `powerd` holding a power-control capability and owning policy; device power states through `devmgr`; DVFS and thermal (needs real hardware); suspend (probably never). Interrupt objects need a wake-source attribute. Park/unpark against IPIs and TLB shootdowns belongs under loom/shuttle first.

(Not specified, not scheduled, not committed: a direction from the design conversations of 2026-11-14 and 2026-11-15. Needs a spec, an acceptance check and an OD entry before it becomes a slice.)
