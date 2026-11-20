---
id: PL3
kind: slice
title: User code built for RVA23U64; cache-block instructions enabled for user mode
area: libc
horizon: long
blocked_by: [PL2]
refs: [KERNEL-V1-SPEC, OD127]
---

**How.** (1) `senvcfg`: set CBZE, CBCFE and CBIE (flush) on every hart at start; boot fails if a bit does not stick. (2) User Rust: a target specification or `-C target-cpu`/`target-feature` giving the RVA23U64 mandatory set (`+v,+zba,+zbb,+zbs,+zicond,+zcb,+zfa,...`: take the list from the profile text and record it in the PR); `rt`, `init/` programs and servers rebuilt. (3) C (`libc` build script, BusyBox, Lua): `-march=rva23u64` where the pinned clang accepts it, else the explicit extension string; `-mabi=lp64d`. (4) OD76's "vector stays off" and tidy items T54/R16 are closed by this.

**Acceptance, written first.** A C test and a Rust test each execute a vector loop and `cbo.zero` over a page and check the result; the Lua and C transcripts, BusyBox and the BASIC goldens pass unchanged. A disassembly check finds at least one vector instruction in the built C library (so the flags are real).

**Review.** Default, soak. If a ported program miscompiles with vector on, build that program without it, record a tidy item, and go on.

Progress (2026-11-20): `senvcfg` CBZE/CBCFE/CBIE on every hart (boot stops if a bit does not stick); user Rust built with `+v,+zvbb,+zba,+zbb,+zbs,+zicond,+zcb,+zfa,+zicbom,+zicboz,+zihintpause,+zkt,+zimop,+zcmop,+zawrs` (T187: three of them are unstable features), user C with `-march=rva23u64` or the same set spelled out or the LLVM-17 subset; a Rust test (crash mode 98: vector loop and `cbo.zero`) in `vector_state_is_private_to_each_thread_and_process`, a C check in the transcript (`rva23()`, the same lines from the host build), `scripts/check_vector_codegen.py` in `make build` (246 and 250 vector instructions in the Lua and C-test archives). With the whole user build using vector registers the FP/vector return and trap paths had to stop taking the big lock for a thread that has used a unit (T166, fixed here): per-hart claims (`fs_claim`/`vs_claim` and the address of the claimed thread's saved copy) let `fp::return_to_user` return with no lock and `fp::light_save` save dirty state on the lock-free trap paths; a claim is dropped (and the old hart's token forgotten) when another hart takes the thread's state over. The privacy test and the edge tests became single assembly blocks (compiled code now uses the vector registers too). Boots run by hand: the kernel-only shard at four harts, three times (two red, causes fixed, then 241 tests pass). Not run: the C and Lua transcripts, BusyBox and the BASIC goldens (shard a: the epic gate), one and two harts, `vlen=256`.
