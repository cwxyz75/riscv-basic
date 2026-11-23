# KT14: the audit of every `unsafe` in the kernel binary (2026-11-23)

Four fresh-context Fable reviewers, each given a set of files and the inventory (`scripts/unsafe_inventory.py --list`), read every `unsafe` block, function and impl of `kernel/src` (162), traced its callers, and judged whether the `// SAFETY:` comment states a condition, whether the condition holds on every path, and whether user mode or another hart can make it false. One row per block is in `kt14-unsafe-audit.tsv` (file, hash, verdict, the condition). The docs check (`scripts/check_docs.py`, KT14) fails when a block is not in the list or a row names a block that is gone, so a new or changed block is audited again. `scripts/unsafe_inventory.py --self-test` plants a new and a changed block and sees both refused.

**Result.** 119 ok, 34 weak (true, but the comment does not say what makes it true), 8 bug. Nothing found is a memory-corruption path reachable from user mode except the first below, which is fixed:

- **Fixed in this PR.** `user/trap.rs` `current_space` (the copy-out of a big `channel_read` or `socket_read`, after the object lock is let go) took the address space from the hart's `cur_as`; after a switch point the hart names whichever process last returned to user mode on it, so the bytes could be copied into another process (or a torn-down one). It now uses the calling thread's own binding. Also found while reading the same path: the AB8 entry check on the full path read the hart's library range, so a `futex_wait` that slept in the quiet path and woke on another hart could be refused as an illegal instruction; the full path now uses the thread's binding (`call_from_library_of_thread`).
- **T239** aliased `&mut` through a nested `with_state` and the FP tables' `state()` (UB by the language rules; no corruption path found).
- **T240** panic-path defects (the console's line state after `break_lock`; the panic trace reading through a guard page).
- **T241** smaller findings (a stale token install, an unlocked frame read in `general_of`, unchecked firmware addresses, local-only TLB flush when a kernel mapping becomes valid).
- **T242** the 34 weak comments, to be rewritten in one pass.

KT14 itself stays open: it is blocked by the end of L20, whose remaining pieces change blocks (each changed block is audited again through the hash check), and its record is to be completed when L20 is done.
