---
id: PM3
kind: slice
title: `system_suspend`: suspend to RAM through the SBI system-suspend extension
area: sched
horizon: long
blocked_by: [PM1]
refs: [KERNEL-V1-SPEC, OD132]
---

**How.** KERNEL-V1-SPEC 5.5. Probe the SBI SUSP extension at boot. `system_suspend(resource)` (root resource, MANAGE): take every other hart offline (PM1), mask device interrupts at the APLIC, save what the firmware does not preserve (the supervisor CSRs, the IMSIC enable state, the timer), call `sbi_system_suspend(SUSPEND_TO_RAM, resume_entry, opaque)`; the resume entry restores `stvec`, `satp`, `sscratch`, `senvcfg`, the interrupt controller and the timer, bring the harts that were online back, return OK. Without the extension: NOT_SUPPORTED, nothing changed. Waking is by any enabled interrupt (the caller arms what it wants). Device quiescence is the caller's job (`devmgr`), not part of this item.

**Acceptance, written first.** Host: the save and restore sequence as a model over a fake CSR set (everything saved is restored; nothing is restored that was not saved). QEMU: where the reference QEMU and OpenSBI offer SUSP, a test suspends with the real-time-clock alarm or a timer as the wake source and checks that threads, timers and a channel survive; where they do not, the test checks NOT_SUPPORTED and the item is closed on the host model plus that check, **and the PR says plainly that the suspend path itself was not run**.

**Review.** Fable.
