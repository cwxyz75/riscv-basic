---
id: PL2
kind: slice
title: Per-thread vector state
area: kernel
horizon: long
blocked_by: [PL1]
refs: [KERNEL-V1-SPEC, OD127]
---

**How.** KERNEL-V1-SPEC 2.2, the twin of slice A's FP design (`kcore::fp`, `kernel/src/fp.rs`). `vlenb` read at boot on every hart; disagreement is a boot panic. A thread starts with `sstatus.VS` Off; the illegal-instruction path tries `vec::first_use` after `fp::first_use`: allocate `32 * vlenb + 5 * 8` bytes (v0 to v31, `vstart`, `vcsr`, `vl`, `vtype`, and a token), zeroed, charge the job (kernel memory; with MM5 this is exact, before it a counted stand-in recorded in T57), set VS to Initial and retry. On a trap from user mode with VS Dirty: save (`vsetvli` to the saved `vtype`/`vl` on restore; whole-register `vs8r.v` loads and stores at `vlenb`). Restore by token on return. Generalise `kcore::fp`'s model into a state-unit model with two instances rather than copying it. The kernel is compiled without V and runs with VS Off (the vector twin of FP3). `user_vector_instruction_is_fatal` is replaced.

**Acceptance, written first.** `kcore/tests/fp_model.rs` runs for the vector unit too (same random threads, harts, preemption, migration), mutation-checked (skip the save). QEMU `vector_state_is_private_to_each_thread_and_process` at 1 and 4 harts with `-cpu rva23s64,vlen=128` and `vlen=256` (two boots): a thread's first use reads zeros; patterns survive preemption and migration; a second process never sees them. A thread that never uses V costs no vector memory (ledger). INVARIANTS gains VS1.

**Review.** Fable, soak.

Progress (2026-11-20): `kcore::fp` is one model with two instances (`StateUnit`, `UnitThread<R>`, `FpThread`/`VecThread`, `VecRegs`, `is_vector_insn`); `kcore/tests/vec_model.rs` is the vector twin of `fp_model.rs` (mutation-checked: skipping the save, never reloading); `arch/riscv64/fp.rs` saves and restores the vector unit (whole-register `vs8r.v`/`vl8re8.v`, `vsetvl` for `vl`/`vtype`, an illegal saved `vtype` restored as illegal), `vlenb` read on the boot hart and checked on every other, the first vector instruction told from an FP one by the instruction in `stval`. `user_vector_instruction_is_fatal` is replaced by the suite test `vector_state_is_private_to_each_thread_and_process`. Run once by hand: the kernel-only shard at four harts on `-cpu rva23s64,vlen=256` (241 tests pass). Not run: `vlen=128`, one hart (the epic gate runs one, two and four harts on the default 128; soak the rest). Vector state is not charged to the job (T57, MM5).

Review round (2026-11-20, Fable): the lock-free trap paths (`user_fp_dirty`: yield, tick, inter-hart interrupt) ignored a dirty vector unit and could switch threads without saving it (the first four-hart boot passed regardless: the old suite test was not sensitive); fixed, and checked by a boot with the fix reverted, which fails `vector_state_is_private_to_each_thread_and_process`. Also fixed: `vstart` saved before the whole-register stores and restored after, a vector-FP first instruction (`vfadd.vv`) falling through to the FP unit, the save written in place (no allocation in the trap path), the `vlenb` assert at the architectural maximum, and edge tests (first instruction `vsetvli` or a vector load, `vill` and `vstart` across a yield, varied element width). Boots run by hand: four harts at `vlen=256` and at the default 128, both 241 tests. Left: no ledger check for vector memory (T57, MM5); the stval-0 path is untested.
