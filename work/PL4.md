---
id: PL4
kind: slice
title: Discovery, part 1: memory, the load address, the device tree pointer, the timer frequency
area: platform
horizon: long
blocked_by: [PL1, MM2]
refs: [KERNEL-V1-SPEC, OD126]
---

**How.** KERNEL-V1-SPEC 2.3. (1) Early boot (`boot.rs`) is position independent: it computes where the image was loaded and builds the early table from that, not from `KERNEL_PHYS`. (2) `kmain` parses the tree at the pointer from `a1` (`kcore::fdt`, extended as needed): `/memory` ranges, `/reserved-memory`, the memory reservation block, the tree's own extent, the initial ramdisk if any. The frame allocator (MM2) is given exactly the usable ranges. `RAM_BASE`, `RAM_SIZE` and "the top 2 MiB" leave `abi` and `kmem.rs`. (3) `timebase-frequency` feeds one `Timebase { mult, shift }` used by the clock, the timer and the latency trace; `TICK_CYCLES` goes. (4) Hosts tests of `kcore::fdt` with synthetic trees: RAM at `0x4000_0000`, two memory ranges, a reserved range in the middle, a tree placed low in RAM, a 24 MHz timebase.

**Acceptance, written first.** QEMU boots of the `k` shard with `-m 128M`, `-m 1G`, `-m 4G` and `-m 8G` report the right frame count each (a suite test compares `debug_stat` total frames with the size the harness passed). The host tests above. No `RAM_SIZE` or `0x8000_0000` literal remains outside tests (grep in the docs check).

**Not verifiable here, said plainly.** QEMU `virt` always puts RAM at `0x8000_0000` and the timebase at 10 MHz, so a different base and frequency are tested only on the host parser and by review.

**Review.** Fable, soak.
