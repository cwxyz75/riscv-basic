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
