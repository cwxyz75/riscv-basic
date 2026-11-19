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

## Progress (2026-11-19)

**TS3a** (PR pending): the crate `ksys` exists, with `UserMem` (copy in, verify-writable, copy-out-verified, the job ledger) as the machine seam, `wide_argument_error` (the 32-bit argument table) and `ipc_call` (handles, channels, sockets, events, ports, object info). `kernel/src/user.rs` calls it first in `dispatch`. Host tests in `ksys/tests/ipc_calls.rs` run the error rows of those calls against a fake machine. After the Fable review the seam is not a trait of its own: `Ctx` holds the real `kcore` address space, `Mm<M: PhysMem>` and `Vmos` (the ledger is `vmos.jobs`), so the host tests run the kernel's own usercopy and quota code over a `Vec`-backed `PhysMem`; a `Machine` trait is reserved for what is truly machine (clock, current thread, TLB records, scheduler requests) and arrives with TS3c, where a blocking call becomes an outcome the kernel executes after `Ctx` is dropped (no borrow lives across a block). `Faults` carries the negative-control switches. 23 host tests cover the error rows of the moved calls. Verified: kquick and the QEMU suite at `-smp 1`, `2`, `4`. Miri over `ksys` is in `os-miri` (soak tier; owed, in WORK-PLAN). Open from the review: KT4's table (`syscall-tests.tsv`) does not yet list the host twins; T171.
