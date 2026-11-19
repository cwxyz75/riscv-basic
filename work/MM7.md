---
id: MM7
kind: slice
title: Regions split: `process_unmap` and `process_protect` over ranges, `process_map` ANYWHERE, `process_map_info`
area: kernel
horizon: long
blocked_by: [MM6, HD1]
refs: [KERNEL-V1-SPEC, OD133]
---

**How.** KERNEL-V1-SPEC 3.5, 4.5. `kcore::aspace`: regions carry `max_perms` (from the VMO handle's rights at map time: read always, write with WRITE, execute with EXECUTE) and a generation; `split_at(va)`; adjacent regions of the same VMO with contiguous offsets and equal permissions merge. `process_unmap(process, vaddr, length)`: every page in range, holes allowed, regions split at the edges. `process_protect(process, vaddr, length, flags)`: the range fully mapped or INVALID_ARGS with nothing changed; flags within `max_perms` or ACCESS_DENIED; never W and X; PTEs updated in place; a downgrade is followed by the batched shootdown, an upgrade to execute by PL7's flush. `process_map(..., ANYWHERE)` with `vaddr == 0`: choose with the kernel entropy pool (HD1) uniformly among page-aligned positions in free gaps of `[0x1_0000_0000, 0x30_0000_0000)` that fit (try 8 random positions, then the first fit from a random start), return the address in `a1`. `process_map_info(process, vaddr, buffer)`: INSPECT; 48-byte record (base, length, permissions, max permissions, VMO koid, VMO offset); returns the base of the next region or 0. The region count is charged per job (PL8). The C library's `mmap`, `munmap` and `mprotect` map onto these in the same PR for anonymous memory (file mappings wait for the follow-on work).

**Acceptance, written first.** `kcore/tests/aspace_model.rs` extended with range unmap, protect and split against a per-page reference (permissions, frame references), mutation-checked (a split that copies the wrong VMO offset; a protect that skips `max_perms`). QEMU: the "map twice the size and trim" allocator idiom; a guard page made by protect faults; write-then-execute flip runs code (the JIT pattern) and W+X is refused; protect to read-only while a sibling writes (negative control without the shootdown fails); two ANYWHERE mappings differ, and with `aslr=off` they are first-fit.

**Review.** Fable, soak.
