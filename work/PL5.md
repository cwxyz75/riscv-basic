---
id: PL5
kind: slice
title: Discovery, part 2: console, interrupt controller, device windows, reset
area: platform
horizon: long
blocked_by: [PL4]
refs: [KERNEL-V1-SPEC, OD126]
---

**How.** (1) Console: the NS16550A named by `/chosen/stdout-path` (its `reg`), else the SBI debug console extension; the literal `0x1000_0000` goes. (2) AIA: APLIC base, `riscv,num-sources` and each hart's IMSIC file address from the tree; `APLIC_BASE` and `abi::layout::MAX_IRQ` go (`interrupt_create` checks against the discovered count). (3) Physical VMOs: the kernel builds the list of device windows at boot from every `reg` range of nodes outside `/memory` that are not the console UART, the APLIC, the IMSICs or the test finisher; `abi::layout::DEVICE_WINDOWS` and `in_device_window` become a kernel table with the same rule (D1 unchanged). (4) Power off and reset through SBI SRST; the test finisher is used only when the tree has `sifive,test1` and the build is a test build.

**Acceptance, written first.** Host tests of the window builder on QEMU's real `virt` tree (dumped once with `-machine dumpdtb`, checked in under `kcore/tests/data/`) and on a synthetic one with an extra device. QEMU: `device_caps_need_the_resource` and the block and network driver tests pass unchanged; a VMO over the UART's range is refused.

**Review.** Fable (the window rule is a security boundary: D1).
