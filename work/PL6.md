---
id: PL6
kind: slice
title: AIA only: delete the polling mode; inter-hart interrupts through the IMSIC
area: platform
horizon: long
blocked_by: [PL5, L12]
refs: [KERNEL-V1-SPEC, OD127]
---

**How.** (1) The kernel requires the AIA (PL1's check); the "interrupt controller: none" path, the polling fallbacks in `devmgr`/`blkd`/`netdrv` that exist only for that board, `make os-boot-test-polling` and its harness branches are deleted. (A driver's own safety poll, NET8, is a different thing and stays.) (2) Inter-hart interrupts: each hart's supervisor IMSIC file page is mapped at boot; `send_ipi(target)` writes the reserved identity (1) to the target file's `seteipnum_le`; the handler claims it through `stopei`. External interrupts move to identities 2 and up. The SBI IPI extension is no longer called. RFENCE stays SBI.

**Acceptance, written first.** `ipi_reaches_every_hart` and `a_wake_preempts_a_lower_priority_hart` pass at 2, 4 and 8 harts with an SBI-call counter showing zero IPI extension calls. `##BENCH wake_latency_event` at four harts is recorded before and after.

**Review.** Fable, soak.
