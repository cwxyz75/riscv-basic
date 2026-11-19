---
id: TS3
kind: slice
area: testing
horizon: near
title: The syscall layer on the host: `kernel/src/user.rs` behind a `Machine` trait
refs: [KERNEL-V1-SPEC, OD140]
---

- [ ] Step 3: move logic out of QEMU, incrementally: the syscall layer behind a small `Machine` trait so argument errors, rights, quotas and accounting run on the host against the real code with a fake machine (moving the matching suite tests over as each piece lands); host harnesses driving server protocols over in-memory channels (with slice R's `Service` trait); optional bounded model checking of small pure pieces (handle table, the slice T pool)

## Note (2026-11-18, plan 4: the v1 kernel)

Scoped for plan 4 to the kernel part (the server-protocol harnesses of the original entry move to the follow-on work and are not needed to close this item).

**How.** A new crate `ksys` (`no_std` + `alloc`, `forbid(unsafe_code)`), holding what is now `kernel/src/user.rs` minus machine access: process, handle, channel, event, socket, port, VMO, mapping, job and thread-management calls and their state (`UserState`). Machine access goes through `trait Machine` (copy in and out of the current address space through `kcore::usercopy` over a `PhysMem`; the current thread and hart; the clock; block and wake through the scheduler interface; TLB flush and shootdown requests as recorded events; the console). The kernel implements it over the real machine; `ksys/tests/` implements it with `Vec`-backed physical memory, a simulated clock and a scripted scheduler. Move in three PRs, each leaving the kernel a thin caller: **TS3a** the trait, dispatch, handles, channels, events, sockets, ports; **TS3b** VMOs, mapping, processes, jobs; **TS3c** waits, `channel_call`, futexes and thread management. For each call the matching error-case tests of the suite get host twins (the suite keeps one end-to-end test per call).

**Acceptance, written first.** Per PR: the host tests cover every error row of `SYSCALLS.md` for the moved calls (KT4's table shows them), run under Miri in the soak tier, and the QEMU suite is unchanged at 1, 2 and 4 harts. `kernel/src/user.rs` ends under 600 lines.

**Review.** Fable for TS3a (the trait's shape decides everything after it); default for b and c; soak each.
