---
id: HD1
kind: slice
title: A kernel entropy pool
area: security
horizon: long
blocked_by: [PL4]
refs: [KERNEL-V1-SPEC, OD139]
---

**How.** KERNEL-V1-SPEC 7.1. At boot: read `/chosen/rng-seed` and `/chosen/kaslr-seed` (then overwrite them in the tree copy the device manager will see), read the `seed` CSR in a retry loop on harts whose ISA string has Zkr, read `time`; derive a ChaCha20 key with HKDF-SHA256 (`rngcore`, which has both, vector-tested). `kernel::entropy::fill(&mut [u8])` and `below(n) -> u64` (rejection sampling) behind a spinlock of its own class at the end of the order, next to `HEAP`. A flag `entropy_seeded` (false when there was no seed and no Zkr) is published; with it false the kernel does not randomise (first-fit addresses) and says so in the boot log and, later, the system data page. The pool is for kernel address choices only; user programs keep using `rngd`. QEMU `virt` supplies `rng-seed`.

**Acceptance, written first.** Host: the key derivation against HKDF and ChaCha20 vectors; `below(n)` is uniform by a chi-squared test with a fixed seed. QEMU: two boots produce different first outputs (`debug_stat`, test builds only); a boot with the seed properties stripped (`-machine dumpdtb`, edit, `-dtb`) reports unseeded and still passes the `k` shard.

**Review.** Fable.

Progress (2026-11-20): `rngcore::pool` (`derive_key`: SHA-256 of the framed inputs, then HKDF; `below`: rejection sampling; vector and chi-squared tests, mutation-checked), `kcore::fdt::chosen_seeds`, `kernel/src/entropy.rs` (the pool on its own `Entropy` lock class after `Heap`; Zkr read only on a hart whose ISA string lists it, bounded), the seeds zeroed in the tree copy `resource_dtb` hands out, `debug_stat` 107/108, the self-test `the_entropy_pool_is_seeded_as_the_tree_says`. Run once by hand: the kernel-only shard at one hart, with the seed stripped from QEMU's tree (`entropy: unseeded`, 241 tests pass) and with it (`entropy: seeded ... first output`, 241 tests pass). `make os-boot-test-entropy` (two boots differ, the stripped boot) is soak and was not run. Not done: the Zkr path is unexercised (QEMU's rva23s64 has no Zkr); the system data page flag comes with its slice.
