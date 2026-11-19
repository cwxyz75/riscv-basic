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
