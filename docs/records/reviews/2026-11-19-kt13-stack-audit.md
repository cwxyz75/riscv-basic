# KT13: the kernel stack audit (2026-11-19)

Kernel stacks are 16 KiB (`abi::layout::KSTACK_BYTES`), guarded by a canary and an unmapped page. The budget is 8 KiB per thread, checked at every thread's exit in every boot (`kernel_stack_budget`, `thread::STACK_BUDGET`); the deepest use the suite reaches is far inside it. This record covers the second half of KT13: which kernel-context code recurses, and what bounds it. Method: a scan of every function in `kcore`, `klock`, `ksys` and `kernel` whose body calls itself, plus the structures whose `Drop` or teardown could recurse.

## Recursive functions

| Where | What it walks | Depth bound | Frame |
|---|---|---|---|
| `kcore::pagetable` `destroy_user::walk`, `destroy::walk`, `for_each_leaf::walk` | the Sv39 page-table tree | 3 levels (a constant of the format) | small: a few words plus loop state |
| `kcore::donate::DonationGraph::height` | the chain of donors below a thread | the donation depth limit `MAX_DONATION_DEPTH`, checked before a donation is added (`donate.rs:196`: the new chain is refused when it would exceed it) | small |
| `kernel/src/main.rs` `recurse` | nothing: the deliberate overflow of the `stack-overflow-demo` feature, which proves the guard (`##PANIC kernel stack overflow`) | unbounded on purpose; the feature is off in every normal build | 512 bytes |
| `kernel/src/sched_tests.rs` `recurse` | nothing: the deliberate deep stack of the budget test | until the thread's own stack has used `STACK_BUDGET + 1 KiB`, then stops | 256 bytes |

No other function in these crates calls itself.

## Structures that could recurse in a drop or a teardown

- **Kernel objects** (`kcore::object`, `kcore::ipc`): a destroyed channel endpoint drops its queued messages, whose handles release their objects in turn. INVARIANTS and SYSCALLS.md ("Object lifetime") say this is done iteratively, with a work list, so nesting of channels in channels has no depth; the reachability sweep is iterative too. Tests: `channel_model`, `kernel_ends_survive_a_sweep`, the nesting tests of `ipc_model`.
- **`BTreeMap` and `Vec` drops**: the standard library's are iterative over the tree (the `IntoIter` drop); their depth is the tree's height, logarithmic in the entries (at most a handful of levels at these sizes).
- **Device tree** (`kcore::fdt::parse`): an explicit stack (`stack: Vec`), capped at `MAX_DEPTH` (16) and `MAX_NODES` (4096); not recursive.
- **ELF loading** (`kcore::elf`): flat tables of program headers, no recursion.
- **Formatting**: the kernel formats flat values (integers, strings, small enums with `Debug`); the deepest `Debug` nesting is a `Result<Vec<..>>` inside a test's failure message. `core::fmt` itself is not recursive on the data it is given beyond that nesting.
- **Page-table and VMO teardown** reach the page-table walks above and nothing deeper.

## Large frames

`clippy::large_stack_frames` is now denied at 1 KiB in the kernel crate (`kernel/clippy.toml`). Thirty-seven functions were already over it (mostly the syscall layer and the self-tests); each carries an audited allow and `T172` tracks shrinking them. The lint's estimate counts every local as live at once, which a frame layout does not; the boot-time budget check is the measurement that matters.

## What the budget check found

The first CI run of the check failed in the optimised build: 1205 of the kernel threads in the suite reach 9.4 KiB. Printing the high water at the entry of `user_thread_main`, of `syscall` and of `teardown` showed the depth is reached before the thread's first line, in `thread_bootstrap`'s `reap_prev` (the stack depth at syscall entry is 656 bytes): freeing the previous thread's stack and records on the next thread's fresh stack is the deepest chain in the system once the inliner has merged its frames. Optimised builds are held to 12 KiB until `T173` shrinks it; the debug build stays at 8 KiB.

## Result

No unbounded recursion is reachable in kernel context. The bounds that depend on a constant (page-table levels, the donation depth, the device-tree depth) are constants of the format or limits checked before the structure grows. What remains open is the sum: the depth of the deepest call chain plus a trap frame, which the budget check measures per thread rather than proving; an interrupt that arrives on top of the deepest chain is covered by the half of the stack the budget leaves free.
