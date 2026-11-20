---
id: PL6
kind: slice
title: AIA only: delete the polling mode; inter-hart interrupts through the IMSIC
area: platform
horizon: long
blocked_by: [PL5]
refs: [KERNEL-V1-SPEC, OD127]
---

**How.** (1) The kernel requires the AIA (PL1's check); the "interrupt controller: none" path, the polling fallbacks in `devmgr`/`blkd`/`netdrv` that exist only for that board, `make os-boot-test-polling` and its harness branches are deleted. (A driver's own safety poll, NET8, is a different thing and stays.) (2) Inter-hart interrupts: each hart's supervisor IMSIC file page is mapped at boot; `send_ipi(target)` writes the reserved identity (1) to the target file's `seteipnum_le`; the handler claims it through `stopei`. External interrupts move to identities 2 and up. The SBI IPI extension is no longer called. RFENCE stays SBI.

**Acceptance, written first.** `ipi_reaches_every_hart` and `a_wake_preempts_a_lower_priority_hart` pass at 2, 4 and 8 harts with an SBI-call counter showing zero IPI extension calls. `##BENCH wake_latency_event` at four harts is recorded before and after.

**Review.** Fable, soak.

Progress (2026-11-20): the kernel requires the AIA (boot panics without it); `send_ipi` writes message id 1 to the target's IMSIC supervisor file (pages mapped from the tree: `aiatree::Aia::imsic_base`), device sources are message ids 2 and up (so a source is at most 62), the external-interrupt trap claims every message and takes the lock only for a device message; the SBI IPI extension is counted (`debug_stat` 106) and `ipi_reaches_every_hart` checks 0. Polling: `os-boot-test-polling`, the harness branches and `blkd`'s polling mode are deleted. Left: T184. `##BENCH wake_latency_event` before/after is for the owner's soak run (`make os-bench`).
