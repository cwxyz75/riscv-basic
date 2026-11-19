---
id: PL7
kind: slice
title: Instruction-cache flush when a frame is first mapped executable
area: kernel
horizon: long
blocked_by: [L18]
refs: [KERNEL-V1-SPEC]
---

**How.** In the fault path, when a page is mapped with execute permission and the frame was not already executable in this address space: `fence.i` locally, then SBI `remote_fence_i` to the other online harts, before the mapping is visible (under the address-space lock). `process_protect` to executable does the same for the range. Invisible on QEMU.

**Acceptance.** A counter test: mapping N code pages makes N flush events and re-faulting them makes none. Correctness on hardware is by review only: say so in the PR.

**Review.** Default.
