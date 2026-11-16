# Roadmap

> **Repository layout note.** The home computer (M and L series) now lives under `home/`
> and the OS kernel is `kernel/` (it was `os-kernel/`). Older entries below use the paths of
> their time: `kernel/` there means `home/kernel/`, and `make run`, `make boot-test` and
> `make kernel` became `make home-run`, `make home-boot-test` and `make home-kernel`.

The task tracker. Tick a box only when its acceptance criterion has been
**verified** (for kernel work, that means booted in QEMU, not merely compiled).

M3 (interpreter, host-only) is independent of M0-M2 (kernel) and can proceed in
parallel, for example in a cloud session while the kernel work happens locally.

There are three tracks. **M0-M8 are the BASIC machine** (all done, including
M7 persistence). **M9 and the L-series add LOGO** as a second language, chosen at a boot
menu; the choice is permanent for that boot, and switching means rebooting.
The language spec for the new track is `LOGO.md`.

A third track, the **S-series** at the end of this file, is the OS: a separate
kernel binary and new crates, specified in `OS-ARCHITECTURE.md`. It does not change
the home computer described by M0-M9 and L0-L6.

## Done so far (baseline)

- [x] Repo scaffold: workspace, Makefile, CI workflow, boot-test script
- [x] Planning docs: DESIGN, BASIC, ROADMAP, CLAUDE
- [x] `basic` crate skeleton with the `Io` seam and golden-test harness (host tests pass)
- [x] Kernel M0 code compiles and boots locally (`make check` green on macOS)

## M0: Toolchain smoke test

Acceptance: `make boot-test` passes locally and in CI. QEMU prints
`hello from S-mode` and exits cleanly through SBI shutdown.

- [x] Prerequisites installed (`brew install qemu`, `rustup toolchain install`)
- [x] `make kernel` builds
- [x] `make run` shows the hello line (checked via the boot test's captured output)
- [x] `make boot-test` passes; CI green

## M1: Console

Acceptance: a scripted boot test types a line (including a backspace) and sees
it echoed correctly.

- [x] UART receive (polling, line-status register) and a proper init
- [x] Line editor: echo, backspace, Ctrl-U, Ctrl-C returns `Line::Break`
- [x] ANSI helpers (clear screen, colour, cursor position)

## M2: Traps and timer

Acceptance: a deliberate fault prints `scause`/`sepc`/`stval` and shuts down; a
tick counter advances.

- [x] Trap vector (`stvec`) and a handler that reports the cause
- [x] Timer through the SBI TIME extension; `millis()` source
- [x] Enable the FPU (`sstatus.FS`)

## M3: Interpreter core (host-first)

Acceptance: golden tests cover each item, all passing under `make test`.

- [x] Settle the top open questions in `BASIC.md`
- [x] Tokenizer and the tokenized program store
- [x] Expression evaluator (numbers, precedence, comparisons)
- [x] Immediate mode: `PRINT`, `LET`
- [x] Program entry, `LIST`, `NEW`, `RUN`
- [x] `GOTO`, `IF/THEN`, `FOR/NEXT`
- [x] Number formatting per spec

## M4: Integration

Acceptance: a boot test pipes a BASIC program into the serial port and checks
its output.

- [x] Kernel depends on `basic`; global allocator
- [x] Kernel `Io` over the UART; banner with a real `BYTES FREE`
- [x] Session loop runs on boot

## M5: Language completion

Acceptance: golden tests for every statement and function in `BASIC.md`.

- [x] `GOSUB/RETURN`, `ON ... GOTO/GOSUB`
- [x] Strings and string functions
- [x] `DIM` and arrays
- [x] `DATA/READ/RESTORE`, `INPUT`
- [x] Math functions via `libm`; `RND`
- [x] Complete error set with line numbers

## M6: Screen and feel

- [x] `CLS`, `COLOR`, `LOCATE`; BREAK handling during `RUN`
- [x] `TIMER`; `PEEK`/`POKE` per the settled policy (golden-tested on the host, and `basic_test.py` / `trap_test.py` exercise them under QEMU)

## M7: Persistence

Spec: DESIGN.md, "The disk" (D21, D22). Acceptance: a program saved in one boot
is loaded and run in the next boot, in both languages, with a fresh QEMU process
in between; the image is also checked from the host.

- [x] Choose pflash "cartridge" or virtio-blk (virtio-blk, D21)
- [x] `cartfs`: the disk format as a pure crate, host-tested (M7a)
- [x] BASIC `SAVE`, `LOAD`, `DIR`, `SCRATCH` (M7b; host goldens against an in-memory disk; the machine has no disk until M7d)
- [x] LOGO `save`, `load`, `dir`, `erasefile` (M7c; host goldens on all three engines against an in-memory disk)
- [x] Kernel virtio-blk driver, `make` creates `disk.img`, persistence boot tests (M7d; `disk_test.py` on QEMU: save, reboot, load; fresh process; LOGO; a file made by the independent host tool; no disk; blank disk)

## M8: Showpiece

- [x] Demo programs as golden tests (guess-the-number, 99 bottles, Hunt the Wumpus), plus `demo_test.py` running them on the real machine
- [x] README with a recorded session; tag `v0.1`

## M9: Boot menu

Acceptance: a boot test sends `1` or `2` at the menu and lands in the chosen
language; `BYE` reboots back to the menu.

- [x] Menu at boot (`1) BASIC  2) LOGO`), one keypress; option 2 says "not installed yet" and re-prompts until the LOGO track lands
- [x] Kernel hands the console to `basic::Interpreter` or, later, the LOGO runtime
- [x] `BYE` in BASIC: SBI System Reset (cold reboot) back to the menu
- [x] Update the QEMU test scripts to send the menu keystroke first
- [x] Update README and DESIGN (boot flow, decision log); record the "language is chosen at boot, no switching" decision

## LOGO track

Spec: `LOGO.md`. The graphics layer (L0-L1) is useful to BASIC too (`PSET`,
`LINE`) if we ever want it there. L2 can proceed on the host in parallel with L0-L1.

### L0: Framebuffer spike

Acceptance: on the LOGO path only, the QEMU window shows a test pattern drawn by
the kernel.

- [x] `ramfb` working: configure via fw_cfg, write pixels, window visible (`-device ramfb -display cocoa`) (verified by `fb_test.py` with monitor screenshots, pixel for pixel; `make run-logo` opens the window, which I have not looked at myself)
- [x] Decide resolution and pixel format (640x480, XRGB8888; D19)
- [x] Add a `make run-logo` style target; keep BASIC boot unchanged
- [x] Record what worked and what didn't in `DESIGN.md`

### L1: Canvas and turtle primitives

Acceptance: host tests render turtle operations to a bitmap and compare against
golden images; the same API draws in the QEMU window.

- [x] `Canvas` trait (pixel, line, fill, clear); a host backend that renders to an in-memory bitmap
- [x] Line rasteriser, pen colours, fill
- [x] Turtle state (position, heading, pen) with golden images
- [x] Kernel `Canvas` over the framebuffer (a turtle square drawn on screen, checked pixel by pixel in `fb_test.py`)

### L2: LOGO interpreter (reference semantics)

Acceptance: golden transcripts and golden images for each item, under `make test`.

- [x] Settle the top open questions in `LOGO.md`
- [x] Reader: words, numbers, lists, `:var`, `"word`, infix operators, arity-driven prefix parsing
- [x] Evaluator: turtle commands, `REPEAT`, `IF/IFELSE`, `MAKE/THING`, `PRINT/SHOW`
- [x] `TO ... END`, `OUTPUT/STOP`, recursion, dynamic scope
- [x] Core list and word operations; `RUN`
- [x] Error messages
- [x] Runs on the machine behind the boot menu (terminal in, framebuffer out; `logo_test.py` checks text and a screenshot pixel by pixel)

### L3: IR

Acceptance: an IR interpreter on the host matches the L2 interpreter on the
whole golden corpus.

- [x] IR definition and lowering from the AST
- [x] IR interpreter; differential test against L2 (the whole golden corpus runs under both engines; mutation-checked)

### L4: RV64 backend

Acceptance: generated code runs on the machine and passes the smoke test, then
the arithmetic goldens.

- [x] JIT smoke test: emit `li a0, 42; ret` into RAM, `fence.i`, call it, check the result (menu key `3` also runs FPU, loop and branch self-tests on the real machine)
- [x] RV64 emitter (integer and double instructions, labels, calling convention)
- [x] Runtime-call table; trap handler dumps the faulting address and the generated bytes
- [x] Value representation (NaN-boxing) and arena allocation

### L5: Compile every statement

Acceptance: differential suite green under QEMU; every instruction line and
every `to` definition is compiled before it runs.

- [x] REPL line: compile, run, discard the code buffer
- [x] `TO ... END`: every body line is compiled before it runs (as its own cached unit, keyed by its text and dropped when a procedure is defined), and calls are late-bound through the runtime `call` service
- [x] `RUN` compiles the list it is given
- [x] Differential suite: interpreter vs compiled, transcripts and bitmaps, on the same corpus (host: all three engines against the hand-derived files, with the compiled one in the RV64 emulator; machine: `logo_diff_test.py`, byte-identical screenshots)

### L6: LOGO showpiece

- [x] Classic programs as golden images (spirals, a tree, the Koch snowflake): a spiral, a tree, a Koch snowflake and rotated squares, with expected pictures from an independent Python renderer (`scripts/verify_demo_images.py`, re-checked in CI), reproduced pixel for pixel by all three engines on the host and compiled-vs-interpreted on the machine
- [x] README screenshot or recording; tag `v0.2`

## Spikes (time-boxed experiments, do early)

- [x] Float formatting without surprises: `core::fmt` with `{:.8e}` rounds exactly, and the goldens plus `basic_test.py` agree on the host and under QEMU
- [x] Ctrl-C delivery over the QEMU serial port (0x03 arrives as a byte, also while a program runs; verified with piped input, not yet with an interactive terminal)
- [x] Allocator choice (D10): the `linked_list_allocator` crate (a bump allocator would leak on every statement)
- [x] `ramfb` on the RISC-V `virt` board with `-display cocoa` (L0): works, see `DESIGN.md`
- [x] Can generated code run at all: emit, `fence.i`, jump, under QEMU (L4): yes, see `DESIGN.md`
- [x] Deciding how far to take NaN-boxing without a garbage collector (L4): per-run arena (262144 values), values copied into interpreter storage by `make`/calls, `Out of memory` beyond that

## OS track (S-series)

Spec: `OS-ARCHITECTURE.md` (design in sections 2-16, decisions in 17, open
questions in 18). A separate kernel binary and new crates; the home computer above
is untouched. **S0 through S7 are built (see the boxes); S8 onwards is not.** Each milestone ends in a demo and has a
runnable acceptance check, written before the code. Tick a box only when verified in
QEMU (for SMP work, in normal mode; icount runs only one hart, TIDY-UP T21; say plainly which was run). The crates
are `abi`, `kcore`, `proto`, `memfs`, `rt`, `libc`, `cfmt`, `shell-core`, `init` and the kernel binary `kernel/`
(it was `os-kernel` in the older entries below).

### S0: Foundation

Acceptance: a host test run and a headless boot test both gate `make` targets and
run in CI.

- [x] Workspace members for the new crates (`abi`, `kcore`, `os-kernel`); the new kernel binary builds
- [x] Host test setup including loom/shuttle and Miri (`kcore::latch` is the first thing under them; mutation-checked: a non-atomic `take` is caught by both loom and shuttle; Miri passes; CI has a Miri job, `make check` does not need nightly)
- [x] Stub kernel in QEMU prints structured test lines and exits through the test finisher with a pass/fail code (a `fail-demo` build proves the harness detects failure; mutation-checked)
- [x] `INVARIANTS.md` and `SYSCALLS.md` skeletons
- [x] `make os-test` and `make os-boot-test` exist and are added to `make check` (and CI)

### S1: Memory

Acceptance: model-based tests agree with the reference model on the host; in QEMU,
map/unmap/fault sequences return allocation counts to baseline.

- [x] Frame allocator and frame database with refcounts (`kcore::frame`, model-tested against a reference model; mutation-checked)
- [x] Sv39 on, kernel in the upper half, kernel heap (`kcore::pagetable` + `os-kernel`; verified in QEMU: `paging_enabled`, `runs_in_the_upper_half`, `low_half_is_empty`, W^X walk of the live table)
- [x] Page-fault handler with lazy commit through the memory-object (VMO) interface (`kcore::aspace`; QEMU tests for lazy read/write, refused faults, shared VMOs and zero leaks over 200 cycles. Not covered in QEMU: instruction-fetch faults (host only), RAM size is assumed 128 MiB)

Independent review (fresh-context agent, before merge) found: a spurious-fault
livelock when an existing mapping cannot satisfy the access (fixed, tested, mutation-
checked); an unhandleable expected instruction-fetch fault arm (now fatal); the device
tree inside the frame database range (top 2 MiB reserved); region size unbounded
(capped at 1 GiB); a mislabelled map error (`FaultError::MapFailed`); weak tests (added
NoMemory, huge-leaf, PTE-bit, region-argument and overlap tests, full-section and
whole-direct-map QEMU checks). Left as documented limits: the TLB flush after unmap is
the caller's job (stated in the API docs), `kmem::get` is aliasing-by-convention until
S2 adds a lock, and `FrameDb` refcount overflow is untested.

### S2: Threads and scheduling (one hart)

Acceptance: a high-priority thread stays within a latency bound while CPU hogs run;
the scheduler conformance suite passes.

- [x] Thread object, per-thread kernel stacks, context switch (`os-kernel/src/thread.rs`; 32 KiB heap stacks with canaries; voluntary and trap-time switches; per-thread `sstatus` in the trap frame)
- [x] Timer preemption and the single block/wake path (1 ms SBI tick; `Sched::block_current`/`wake`/`sleep_until` with a wake token; wait queues; no-lost-wakeup tests on host and QEMU)
- [x] Fixed-priority scheduler with a real-time band, as a boot-selected class; conformance suite (`kcore::sched`: `FixedPriority` and `RoundRobin` run through one random model test against an independent reference model, plus hand-derived scenarios; class chosen by Cargo feature, OD32; both classes boot in QEMU. Not done: the starvation watchdog that falls back to the default policy; priority donation (hook only))

Independent review (fresh-context agent, before merge) found, and I fixed: time-slice
starvation within a level (a preempted thread got a fresh slice every time, so a
periodic higher-priority thread could starve its peer; slices are now per thread and a
preempted thread keeps what it had left; the random model shared the flaw, so a
hand-derived scenario now catches it), `join` hanging on self, unknown or already
joined threads (now returns false), stale wait-queue entries swallowing wakeups,
a leak check blind to the scheduler core's records, a single-word stack canary (now an
8-word guard checked on both sides of a switch, with a test that the detector fires),
and a round-robin run that could not prove the feature took effect (a structured
`scheduler_class_is_*` line is required). Documented rather than fixed: a wake token is
discarded by `sleep_until` (stated in the API docs), the atomics grep gate (INVARIANTS
C5) is still TODO, and wall-clock jitter bounds were widened.

### S3: User mode, syscalls, handles, channels, VMOs

Built in slices (OD38). **S3a (done): the pure half**: `SYSCALLS.md` (ABI, status codes, rights, signals, handle/channel/event calls), `kcore::{handle, object, ipc}` with model tests, mutation-checked (all caught). Independent review found and I fixed: `Registry::sweep` leaked survivors' references; a user-driven cycle loop grew kernel memory without bound (now a pressure sweep, a per-endpoint byte cap and an object cap; honest limit: per-job quotas, Q9, are still needed for a real bound); `channel_write` error order disagreed with the spec (spec, code and model now agree); a txid read on a closed channel could wait forever; the random model now covers events, txid reads, limit-sized messages, queue-full and mid-run sweeps. **S3c-1 (done): waiting and calling.** `object_wait` with an absolute deadline, `channel_call` (write + wait for the matching transaction id), the bootstrap channel every process starts with (handle 1), one kernel-wide wait queue that every signal-changing syscall wakes (OD43), `WaitQueue::wait_until` (pure, tested), and a kernel-side helper that plays the remote server on the kernel's end of init's bootstrap channel for 11 new user-level tests (88 QEMU tests). Independent review found, and I fixed: a sweep destroyed the kernel's ends of bootstrap channels (roots omitted the kernel's own handles; hidden because the sweep test ran last), a missing wake-up after object creation could leave waiters asleep, a stale wait-queue entry could cut a `sleep_ticks` short, `object_wait` on a vanished handle quietly timed out, an undocumented set of errors after a `channel_call` request is sent, timing windows too tight for a loaded host (and the round-robin class), and missing tests (request handles, wide arguments, past deadlines, a wait woken by PEER_CLOSED while blocked, a sweep with the helper alive). Not testable yet: a user-signal wake while blocked (init is single-threaded). **S3c-2a (done): VMOs and processes.** `vmo_create/read/write/get_size`, `process_create/self/map/unmap/start/kill/info`, the TERMINATED signal, kill-on-last-handle-close, a registry graveyard for objects that need kernel cleanup (OD45), VMOs that outlive their last handle while mapped, a reaper thread for detached kernel threads, and a user-level ELF launcher in `init` that builds children from a program-image VMO (14 new QEMU tests: argument checks, exit codes, contained faults, parent/child IPC, killing spinning and blocked children, leak loops; 107 tests total). Independent review found one real bug, fixed: closing the last handle of a mapped VMO destroyed it, so the mapping's first lazy fault killed the process (regression test `vmo_closed_while_mapped_keeps_its_pages`; mutation-checked). Also hardened: double teardown of a never-started process, a cleanup flag that could strand graveyard work. Known and deferred: a kill reaches a Running process only at its next trap; creation still relies on interrupts-off syscalls rather than a real lock. **S3c-2b (done): threads in a process.** `thread_create`, `thread_exit`, `thread_info`, thread objects (TERMINATED, exit code, at most 8 per process), whole-process end on `process_exit`/kill/fault with the last thread tearing down (OD46), a killed-before-first-run check, 9 new QEMU tests including the previously untestable user-signal wake of a blocked thread and four multi-threaded child programs (115 tests). The real kernel lock is deferred to SMP (S5; OD46). Two independent reviews (default model and Fable) found, and I fixed: a user program could panic the kernel by creating threads until the kernel heap ran out (now a system-wide limit of 128 user threads, NO_MEMORY); `handle_replace` did not wake a sibling thread waiting on the replaced handle; a kill arriving between a new thread's kill check and its first user instruction was missed (interrupts now stay off across that window); a late `thread_exit` could override a kill's exit code; spec gaps. 128 QEMU tests in all. **S3c-3 (done): sockets and ports** (OD48): `socket_create/write/read`, `port_create/bind/unbind/wait`, a 7-test host model suite (including a random stream against a byte-queue model) and 12 new QEMU tests (blocked reader/writer wake-ups, a child draining a socket to end of stream, port fairness and timeouts, killing a child blocked in `port_wait`). A wake-up audit by Fable found no gap in the existing syscalls (one low-severity extra notify and a harness race were fixed) and showed that a missing notify is masked by any other notify since the waiter last ran, so wake-up tests must let the waiter settle right before the action under test. The default-model review of the slice found, and I fixed: a user process could panic the kernel by filling many sockets (now a system-wide 2 MiB budget for queued channel and socket data, NO_MEMORY beyond it; the registry keeps a `meter`), bindings to destroyed objects still counted against a port's limit and its key space until the next wait (now pruned by every port operation), a possible spurious TIMED_OUT in `port_wait` (now a poll/sleep loop), and a wide `port_bind` mask reported before the type and rights checks. **S3d (done): trust-boundary hardening** (OD50). A second narrow Fable audit (trap path, user copies, argument validation) confirmed that the trap entry/exit asm, `usercopy` and per-syscall argument checks hold, and found four real issues, all fixed with regression tests: three kernel panics from heap exhaustion inside the documented caps (the registry slot table doubling, unbounded regions per address space, unmetered message headers and handle entries) and the vector unit left enabled in user mode on RVA23 CPUs (now off; the boot test also runs on `-cpu rva23s64`, which is what makes the vector test sensitive). 145 QEMU tests. **S4a (done): the first slices of the interactive system** (S4-SPEC.md, PRs 51-55): the `KILL` process right (OD51), the pure `proto` crate with every S4 wire format (hand-derived hex goldens, mutation fuzz, round-trip properties), the user runtime heap / startup-message reader / blocking socket helpers (OD53), and the interactive boot: boot image archive VMO, polling serial bridge, supervisor `init` v0, a scripted serial acceptance check (OD52). Fable's review of S4a found and I fixed: a program could forge *and* swallow kernel verdict lines (the escaper tracked the program's line, not the UART's; now one shared `kcore::linestate`, host property-tested), lossy UTF-8 corruption of forwarded bytes, `write_all` failing above 64 KiB, blocked reads/writes spinning on handles without WAIT, plus stricter decoders (unreferenced handles, ambiguous namespaces, overlapping boot-image extents, undefined flag/right bits) and spec/doc drift. **S4b (done pending review): the launcher.** `rt::spawn` builds a process from an ELF in userland with the kernel's own validating parser (the first services use it directly), the `launcher` service takes the program as a VMO extent and holds no filesystem authority, and `rt::launch::spawn_via` is its client. It returns a process handle without `MANAGE`, refuses bad images/extents/unreferenced handles without leaks, and drops a client that sends an oversized message. 4 QEMU tests (mutation-checked twice) and `run <program>` in `init`, driven by the scripted serial test. User programs are now built without debug info (the boot image stays small; the launcher accepts 4 MiB images). **S4c (done pending review): the memory filesystem** (OD54): the pure `memfs` crate (scenario tests plus a random walk against an independent path-keyed model, three mutations checked) and the `memfsd` server (10 QEMU tests: the boot image tree, `/tmp` as the only writable place, sandboxing of paths, rights that only shrink, protocol violations, quotas and the connection limit, unlink-while-open, `GetVmo` feeding the launcher, paged listings, and a slow client that stalls only itself, which found and fixed a spin on a channel's WRITABLE signal), a client library (`rt::fs`), and `ls`/`cat`/`write` test commands in `init` driven by the scripted serial test. Boot-image names are now absolute paths (`bin/hello`, `etc/trusted`, `home/...`). **S4d (done): the console server.** `consoled` owns the serial socket and serves one terminal session; its line discipline is the pure `proto::tty` state machine over `console::LineEditor` (17 host tests, four mutations checked), the control protocol gained a reply and the `Tty` startup role, and `init`'s `console` test command plays the shell on the terminal socket. `scripts/s4d_console_test.py` (hand-derived expectations, with a built-in check that it can fail) covers echo and line delivery, BS/DEL/Ctrl-U, escape sequences, Ctrl-D, Ctrl-C with and without a registered foreground job (exit code -130), a 100 000-byte flood with exact shown+dropped accounting, and a clean finish. Optimising the OS crates in the dev profile (PR 61) also made `make check` about three times faster and exposed a lost-output bug in the serial bridge at init exit (fixed). **S4e (done): the shell.** The pure `shell-core` crate (grammar: stages, quotes, comments, limits; lexical `cd` paths; 9 host tests including a no-panic sweep over every short string of a small alphabet), the programs `echo`, `cat`, `ls`, `probe` and `spin`, the shell `sh` (pipelines over socket pairs, job registration with `consoled`, `[exit N]` reports, a confined powerbox with `/etc/trusted`), and `init` as supervisor (fresh session on shell exit, `halt`). `scripts/s4e_shell_test.py` drives a full session with hand-derived expectations: builtins, programs and their errors, pipelines, the confinement probe, Ctrl-C on jobs and pipelines (no extra prompt), Ctrl-D and `exit` restarts, and `halt` followed by the kernel's leak checks. Shortcuts: T31-T33. **S4.5 (done): jobs and limits** (OD56-OD58, PRs 65 and 66). A tree of job accounts with limits on processes, threads, memory pages (VMO pages plus page-table estimate) and child jobs, charged along the whole ancestor chain (`kcore::job`, host model test, mutation-checked); five syscalls; `process_create`/`vmo_create` take a job; no ambient job authority; the launcher puts every program in a job of its own, `init` runs a session job, and the shell session shows a memory hog, a process bomb and a thread bomb refused with the system unharmed. A Fable review of the kernel half found a kernel panic (a refused `process_start` was not atomic), uncharged page tables, an uncapped job count and a root use-count overflow, all fixed; a CI-only memory failure exposed that every suite test fetched a fresh copy of the boot image (fixed). **S4.5c (done): object and IPC quotas** (OD59, PR 68): objects are charged to the creating job and queued channel/socket data to the job that created the endpoint, so the global IPC budget and object cap are only backstops (a confined program can no longer starve the system of either); the end-of-session ledger check now covers objects and queued bytes too.

**S3b (done): user mode.** `kcore::{elf, usercopy}` (validating static-ELF loader front end with a mutation fuzz; user-memory copies by page-table walk, never SUM), processes with page tables that share the kernel half, the U-mode trap path (`sscratch` swap), the syscall dispatcher for the S3a calls plus `debug_write`/`process_exit`/`thread_yield`/`clock_get`/`object_signals`, the embedded user programs (`rt` runtime, `init` = a 18-test user-level suite run in U-mode over the real ABI, `crash` = programs that misbehave on purpose), and kernel-side checks: every fault kind ends the process with the documented exit code while the kernel keeps running, teardown returns every frame and every kernel object. Two processes alive at once are verified (private memory under timer preemption and under yield switches). Not yet: process/thread/VMO syscalls, blocking waits, `channel_call`, per-process kernel locks (S3c/S5). Independent review of S3b found, and I fixed: no address-space switch when a thread resumed (needed a hook on both return paths; found by removing each), 32-bit syscall arguments silently truncated, `channel_read` validating the declared capacity instead of the message, a window where a new user thread was unknown to the kernel, a `crash` store test that would pass without the user-bit protection, and gaps in the test list; documented rather than fixed: syscalls are non-preemptible; `start`/`finish` use the global kernel memory state with interrupts *on* and are safe only because the boot thread that calls them outranks every user thread (a real kernel lock is needed before S3c lets user threads create processes); user-printed `##TEST` names are trusted.

Acceptance: a user test suite passes; every syscall's error cases are covered per
`SYSCALLS.md`; killing a process leaks no objects.

- [x] User mode, syscall entry, handle table with rights (S3a/S3b; verified in QEMU: 18 user-level tests over the real ABI, 7 fault kinds, teardown leak checks)
- [x] Channels (with `call`), ports, events, sockets (S3a-S3c; `channel_call` S3c-1, sockets and ports S3c-3)
- [x] Process, address-space and VMO objects; minimal ELF loader starts `init` from the boot image (S3b, S3c-2a)
- [x] Process death releases everything (leak check: `many_children_do_not_leak`, `process_teardown_returns_every_frame`, and the init-exit checks)

### S4: First interactive system

Acceptance: a scripted shell session passes (`ls`, `cat`, `echo`, a pipeline);
Ctrl-C kills the foreground job.

- [x] Launcher and startup message (S4b)
- [x] Memory filesystem server (S4c)
- [x] Console server; shell with builtins and external commands (S4d, S4e)
- [x] Pipes through the socket object (S4e; the scripted shell session in `scripts/s4e_shell_test.py` is the acceptance check, with Ctrl-C killing the foreground job)
- [x] Jobs and per-program limits (S4.5a-S4.5c, OD54-OD59)

### S5: SMP

Acceptance: every earlier suite passes with `-smp 4` in normal mode (icount only for one hart: TIDY-UP T21); the stress run reports no lock-order violations and no leaks. Built in slices S5a-S5e (`S5-SPEC.md`, OD60-OD62): parked secondaries, the big kernel lock and multi-hart scheduler core, harts scheduling (user threads migrate, kernel threads stay on the boot hart), TLB shootdown and kill IPI, churn stress.

- [x] Secondary harts via SBI HSM; per-hart state (`satp`, timers, idle threads) and a multi-hart scheduler core with **one global run queue per priority under the big kernel lock** (S5a-S5c; per-hart run queues are slice L)
- [x] IPIs and remote TLB shootdown (S5a, S5d; negative controls fail as required)
- [x] Big kernel lock (S5b)
- [ ] Finer locking on the hot paths (not done, TIDY-UP T11; the plan is slice L, `SCHED-SMP-SPEC.md` tasks 10 to 20)
- [x] Lock-order checker in debug builds; stress suite (S5b, S5e)

### S6: Storage drivers

Acceptance: a pattern written to the virtual disk reads back identical; the block
driver can be killed and restarted.

- [x] Interrupt objects and device-memory VMOs
- [x] Driver manager and device-tree parsing
- [x] Userspace virtio-blk driver and block protocol

### S7: libc and ported software

Acceptance: BASIC goldens pass as a userland program; a Lua test script passes.
Spec: `S7-SPEC.md` (slices S7a BASIC, S7b futexes, S7c libc, S7d Lua, S7e upstream Lua tests).

- [x] C libc subset; threads on futexes
- [x] `basic` crate as a userland program over a stdio-backed `Io`
- [x] Lua port (S7d: Lua 5.4.8, acceptance script against a host build)
- [x] Upstream Lua test suite under `bin/lua` (S7e: 24 files, each its own test)

### TS: Tiered testing (decided 2026-11-14; steps 0 to 2 done, step 3 spread over later slices)

Why: one PR now takes over an hour from first run to merge, and it grows with every test. The cause is mostly structural: `make check`
boots QEMU about 18 to 20 times, each boot running the same roughly 250-test suite (including the upstream Lua scripts, the BASIC
corpus and the C transcript, whose results do not depend on the hart count, the interrupt controller or the scheduler); I run it locally
(about 27 minutes) and then CI runs it again (10 to 12 minutes in parallel); and the local boots run one after another on a 10-core
machine. The previous adjustment (the `quick` target, "full check once per PR") drifted back because nothing enforced it.

Decisions (owner, 2026-11-14, OD83): the tiers below; one thin Ubuntu CI job as the merge gate (host tests plus the smoke boot), the full
Linux matrix only at tier 2; a nightly tier-2 run on `main` and a recommended (never required) soak label on kernel, SMP and security
slices; the independent oracles (upstream Lua, the C transcript against the host, the BASIC corpus) move to tier 2, run in smoke only
when their own component changed.

| Tier | When | Budget | Contents |
|---|---|---|---|
| 0, unit | every edit | under 1 minute | host unit, model and property tests; fmt, clippy, docs checks |
| 1, smoke | every PR, the merge gate | about 8 to 10 minutes of wall clock | one QEMU boot (default AIA machine, 2 harts) of the kernel-focused subset of the suite; the release smoke; the home-computer boot; path-triggered extras (a change under `third_party/lua`, `libc` or `basic` also runs that component's group) |
| 2, soak | on demand: PR label, manual dispatch, nightly on `main`, before a release; never mandatory | none | the full matrix (1, 2 and 4 harts, round-robin, release, RVA23, polling, no disk), negative controls and mutation checks (also when the guarded files change), upstream Lua, the BASIC corpus, churn and leak tests, benchmarks, Miri, loom and shuttle with large seed counts |

Principles: logic goes to the host wherever it can (pure `kcore` and `proto` crates, model and property tests, mock machines and in-memory
channels); QEMU is kept for what only the machine shows (traps, MMU and TLB, SMP races, the AIA, FP state, boot, real drivers). A new test
declares its tier; the default rule is pure logic on the host, syscall semantics in smoke, long or exhaustive in soak. The rules are
enforced mechanically (below), not by habit.

Acceptance (written first): the measured table of step 0 exists; a PR that changes only a kernel source file passes its gate in the smoke
budget; `make check` means tier 0 and 1 only; `check_docs.py` fails if `TESTING.md`, the Makefile and the CI file disagree
or if a soak target appears in the PR gate. (The registry tier tag and its check were dropped from steps 1 and 2 on the evidence of step 0:
the whole suite at four harts takes 35 s, so smoke runs all of it and needs no per-test tier; `TESTING.md` says what to do when it outgrows the budget.)

- [x] Step 0: measure. Per-test guest ticks and host wall time in the boot output, and a profile table (which tests and which boots cost what), kept in `TESTING.md`
- [x] Step 1: `make smoke` / `make soak` (parallel boots, `SOAK_JOBS`), `check` is `smoke`; the CI job split into the thin gate (`ci.yml`) and the on-demand soak workflow (`soak.yml`: nightly, label, dispatch). Measured and dropped: QEMU icount is slower, not faster; sharding and tier tags are not needed while the smoke boot is 35 s
- [x] Step 2: `TESTING.md` (the strategy, the tier rules, the budgets) and its mechanical enforcement (CI timeouts equal to the budgets; `check_docs.py` consistency checks; the registry tier check); `CLAUDE.md`, the memory notes and the roadmap rules updated: locally only tier 0 and the targeted test, CI smoke is the gate, auto-merge on green
- [ ] Step 3: move logic out of QEMU, incrementally: the syscall layer behind a small `Machine` trait so argument errors, rights, quotas and accounting run on the host against the real code with a fake machine (moving the matching suite tests over as each piece lands); host harnesses driving server protocols over in-memory channels (with slice R's `Service` trait); optional bounded model checking of small pure pieces (handle table, the slice T pool)

Estimated size (not measured): steps 0 to 2 a day of work in two or three PRs; step 3 spread over the following slices.

### Queue before S8 (history: the order set 2026-11-14 after the design review; S8 and S9 have since been built, and the state of each slice is in its own section below)

The design review (`reviews/2026-11-14-design-review.md`) found a handful of structural things worth
settling before the network stack is built on them. This is the proposed order; the owner can reorder.
Slices A, W, V, T, R, SD, DM and I below each have a plan. L (locking) is redefined by `SCHED-SMP-SPEC.md` (OD85 to OD96): its
first tasks come before S8 and the measured lock peels after S9, as set out in the slice below.

0. **Slice TS: tiered testing** (steps 0 to 2 done; step 3 spread over later slices).
1. Tidy the stale contract documents (T55, design review D14): cheap, and everything else leans on them.
2. Slice A: the RVA23 user ABI (hardware floating point). A flag day for all user code and C, so first,
   before more is ported and before the thread benchmark fixes its baseline.
3. Slice W: targeted wake and ports as registrations.
4. Slice V: VMO commit and the shared-memory rule.
5. Slice T: kernel memory and threads (a frame-backed kernel allocator, kernel stacks outside the heap,
   cheap creation); the structural part of cheap threads.
6. Slice R: the service runtime (service loop, accounted connections, one backpressure policy); `memfsd`
   ported to it.
7. Slice SD: the service directory and namespaces (goes with R).
8. Slice DM: the device manager overhauled (table-driven, several devices and clients).
9. Slice I: AIA interrupts.
9a. Slices **B** (benchmark and latency instrumentation), **SC** (two-band scheduler, priority API, nanosecond time), the
    thread API (`thread_spawn`) and the block/wake protocol: tasks 1 to 10 of `SCHED-SMP-SPEC.md`, so the service runtime and the
    network servers are written once against the final thread API.
10. S8; then tasks 11 and 12 (per-hart queues, the scheduler out of the big kernel lock) after its first milestone, with the
    network stack as the load; then S9; then the measured lock peels (tasks 15 to 20).

### W: Targeted wake and ports as registrations (built except step 4's remainder, which is L task 21)

Why: the design review (finding D1) found that the kernel wakes every blocked thread after almost
every syscall (one global wait queue, OD43), and that signals and ports leave nowhere to attach a
targeted wake. With the cheap-threads direction (OD73) that is a design defect, and a network server
makes it worse, so it is fixed before S8, not after S9. Basis: the Fable design recommendation (kept in
`reviews/2026-11-14-wake-path-design.md`, with `reviews/2026-11-14-design-review.md` for the findings), to be turned into a
spec and an OD (superseding OD43, amending OD48) as step 0.

Decisions taken (2026-11-14, owner): keep signals derived and the single big kernel lock, no new lock;
a dirty log in `kcore::Registry` plus a pure `kcore::wait::WaitTable` (weak registrations, thread waiters
and port bindings, a `commit()` that returns exactly the threads to wake); ports become registrations
with level-triggered semantics kept and a first-come ready list (the report order changes from rotation
to oldest-ready-first, accepted); a port wakes one waiter, first in first out; opt-in one-shot bindings
with `port_rearm` **in this slice**; closing, narrowing or sending the handle a binding was made through
ends the binding (D17); the 256-binding cap goes in favour of per-job quotas that can be set; the global
wait queue is retired completely (futex, kill, IRQ, kernel reads, thread join all wake by thread id).

Acceptance (written first): wake counters in the `##` output and a deterministic QEMU test that bystander
waiters are not woken; the thread and `channel_call` benchmark with 0 and N bystanders (before and after);
a shuttle/loom model of the wake/sleep protocol (retires T38); model tests of `kcore::wait` against the
old wake-everyone behaviour as the oracle; mutation checks (drop a touch, no baton hand-over, deadline
before poll); a `fault-skip-touch` negative-control kernel; every existing test still green.

- [x] Step 0: spec (`W-SPEC.md`, OD80), wake counters (`debug_stat`) and the measured bystander baseline (`wake_baseline_bystanders_and_counters`); the `channel_call` benchmark and the T38 shuttle model move to step 2, where the wake path is first rewritten
- [x] Step 1: the dirty log and touch rules in `kcore` (`Registry::touch`, `touch_forced`, `take_dirty`; `get_mut` touches the object and its peer; handle removal is a forced touch), no behaviour change, with a completeness property test (`kcore/tests/touch_complete.rs`, mutation-checked); the kernel drops the log after each syscall until step 3
- [x] Step 2: wake by thread id for futex and kill (futex sleepers are on no queue: `wait_deadline_private`, `wake_blocked`; `futex_wake` and every kill path wake exactly the right threads; test `futex_sleepers_are_not_woken_by_unrelated_syscalls`, and the kill test fails without the by-id wake). The T38 shuttle model and the `channel_call` benchmark are not done: they move to step 3, where the object waits are rewritten and the benchmark is the acceptance check
- [x] Step 3: `kcore::wait` thread waiters (`object_wait`, `channel_call`, a kernel thread's read wait; the IRQ path commits): the dirty log is committed after every syscall and trap from user mode and before such a wait sleeps; measured: the 60-round-trip ping-pong went from 241 spurious wakes (361 blocks) to 0 (120 blocks), with four bystanders 0 extra; model-tested against the old rule (`kcore/tests/wait_model.rs`, mutation-checked), negative control `fault-skip-forced-touch`. Port waits still use the global queue (step 4). Still not done: the T38 shuttle model and a `channel_call` round-trip benchmark
- [~] Step 4: ports as registrations: **port waits are woken by id** (a waiter registers on the port, whose signal follows its bindings and is re-evaluated on every commit that touched anything). Binding quotas instead of the 256 cap: done in step 6. **Not done** (T60, T61): the exact ready set with a first-come list and wake-one baton (PW), per-handle binding lifetime (D17)
- [x] Step 5: the global wait queue (OD43) is deleted (`signal_queue`, the wake-all in `notify`, `wait_deadline`); every wait is registered and woken by id; the hand-kept syscall list went in step 3; documents updated; T12 resolved for the wake-all cost (multi-waiter ports can still see duplicate wakes until PW)
- [x] Step 6: one-shot bindings (`port_bind` flags, `ONESHOT`) and `port_rearm` (syscall 84), kcore model test (mutation-checked) and `port_oneshot_rearm_and_many_bindings` in QEMU; the 256-binding cap is gone (a binding is charged to the port's job and the global budget, 600 bind in the suite)

Estimated size (not measured): 6 to 7 PRs, comparable to S5c or S7b. The kernel review of this slice is a
Fable review.

### V: VMO commit and the shared-memory rule (done)

Why: the design review (finding D3) found that a page fault on a VMO whose creator's job is over its
limit ends the process that touched it, whoever owns the VMO, and that there is no way to commit pages
ahead of time. That is latent today (nobody maps a foreign VMO) but a driver and a network stack passing
packets through shared rings depends on it, so it is fixed before S8. Basis:
`reviews/2026-11-14-design-review.md` (D3). Independent of slice W: either order.

Decision taken (2026-11-14, owner): **the creator pays, up front.** The creator commits a VMO's pages
before sharing it; the charge (job quota and physical frames) happens then, when the creator can still
handle a refusal, and recipients touch pages that are already paid for, so they can never fault on
quota. Rejected: toucher pays and mapper pays (a client could drain a server's budget). Deferred to
S11 (`mmap`, T16): who pays when a client dirties pages of a server's file mapped writable (likely
private copy-on-write pages to the mapper's job, shared pages to the VMO's owner).

Questions for step 0 (spec), with my leanings: (1) **a COMMIT right**, so a recipient cannot force the
creator's job to be charged by calling `vmo_commit` on a VMO it was given (duplicate narrowing drops it);
(2) **the rule enforced by the kernel, not only written down**: mapping a VMO created by another job
requires the mapped range to be already committed (a new error, not a fault later), which removes the
fault-kill of a foreign toucher by construction; (3) commit allocates the frames now (so a later frame
shortage cannot fault either), zero-fill on commit or on first use; (4) atomic: a refused commit changes
nothing and the quota is credited back exactly.

Acceptance (written first): a client at its page limit hands a server a VMO and the server maps and touches
it without dying (the client's commit is what is refused); `vmo_commit` over the limit returns an error with
nothing changed and the job ledger exact; the runtime heap under a tight job returns a failed allocation
instead of a fault (a variant of the hog test; retires the heap half of T26); a recipient without COMMIT
cannot commit; mapping a foreign uncommitted range is refused; destroy credits exactly; model tests of the
commit logic in `kcore` (commit then touch never charges again; commit atomic; partial ranges); mutation
checks; a negative-control kernel that skips the commit charge.

- [x] Step 0: spec and OD81, `SYSCALLS.md` entry, INVARIANTS V1-V3, acceptance tests (`kcore/tests/vmo_commit.rs`, QEMU `vmo_commit_charges_up_front_and_foreign_maps_need_it`)
- [x] Step 1: `kcore` commit logic (`Vmos::commit`, atomic, credit on destroy; mutation-checked)
- [x] Step 2: the `vmo_commit` syscall (47) and the COMMIT right (bit 13); `process_map` refuses an uncommitted stranger's VMO (BAD_STATE). Decision I took: "stranger" means a VMO charged to a job that is neither the caller's nor the target process's, so a parent building a child (VMOs charged to the parent or to the child's job) stays lazy
- [x] Step 3: the runtime heap commits each chunk it grows (exhaustion is a failed allocation)
- [x] Step 4: documents (T26 narrowed to stacks, which still fault in their own job), D3 resolved

Estimated size (not measured): 3 to 4 PRs. Kernel and security sensitive: a Fable review.

### A: The RVA23 user ABI (done; the scalar bit-manipulation and vector extensions are deliberately not used)

Decision (2026-11-14, owner, OD76): user code follows RVA23. Today user programs and all C are soft-float
`rv64imac`/`lp64`, the kernel keeps FP and vector state off (OD40, OD50), and the Rust user target is
`riscv64imac` (design review D13, T20). Soft float makes Lua and BASIC slow, and moving later is a flag day
for all ported code, so this goes first.

Plan: the kernel enables the FP state and saves and restores it per thread (lazily using the dirty bit, so
threads that never touch FP pay nothing; the cost enters the thread benchmark baseline); user Rust moves to
the hardware-float target with the RVA23 scalar extensions; C and the C library move to the matching
`-march` and `lp64d` (the variadic assembly stubs, `setjmp`/`longjmp`, `va_list` and the thread control
block all change shape); the vector extension stays off for user code until a program needs it. Because
this is an ABI change, all of it lands together.

Acceptance (written first): every existing test, with the FP-using suites doing real hardware float; a test
that FP state survives context switches, preemption by another thread, and SMP migration (two threads
interleaving FP computations that must match a reference); a test that FP use by one process cannot be seen
by another; the Lua and C transcripts unchanged (they compare against the host); the Lua benchmark before
and after; a negative control that skips the FP save.

- [x] Step 0: the protocol spec (`kcore::fp`, INVARIANTS FP1-FP3) and the acceptance tests first (the ABI text, extensions and flags, comes with step 2)
- [x] Step 1: kernel FP enable, save/restore (lazy: unit off until first use, saved on trap when dirty, reloaded only when the hart does not already hold the state), per-thread state, tests (`kcore/tests/fp_model.rs`, QEMU `fp_state_is_private_to_each_thread_and_process`)
- [x] Step 2: Rust user target (`riscv64gc-unknown-none-elf`) for `rt`, `init` and every program; the kernel build embeds them from the new path
- [x] Step 3: C: `-march=rv64gc -mabi=lp64d` (the C test program and Lua), `setjmp`/`longjmp` save `fs0`-`fs11` (`jmp_buf` is 26 words; the C transcript checks a callee-saved FP register survives a `longjmp`, mutation-checked); the variadic stubs and `os_call_on_stack` needed no change; Lua float workload 5.2 times faster
- [x] Step 4: documents (OD40/OD50 superseded in OD76, S7-SPEC), T20 resolved (R18). Not done, deliberately: the scalar bit-manipulation extensions and vector (no measured gain; default QEMU CPU lacks them)

Estimated size (not measured): 3 to 5 PRs; a Fable review (kernel context switching).

### R: The service runtime (built; step 3 mostly done, step 4's documents not written)

Decision (2026-11-14, owner, OD77): one service loop in `rt`, and accounted connections. Fixes design review
D7 (nobody uses `channel_call`), D8 (backpressure solved five ways), D11 (servers cannot tell clients apart)
and D12 (five hand-rolled loops, the network stack would be the sixth).

Plan: a library for servers (port, per-connection state, held replies, a single backpressure policy that
settles T28, a protocol-violation policy, timeouts); a typed client side that makes the request/reply path
real so that later optimisations such as direct hand-off have something to attach to; accounted
connections (a server hands out a fresh account for a root connection and per-account limits apply to
everything opened or cloned from it); then `memfsd` ported to it as the proof, with `consoled`, `launcher`
and the others after; supervisor restart is a later step, not this slice.

Acceptance (written first): `memfsd` passes all its existing tests unchanged on the library; a client that
floods a server with connections exhausts only its own account; a slow client stalls only itself under the
single backpressure policy; the same protocol-violation behaviour for every ported server; leak checks
exact; the benchmark for a request/reply round trip.

- [x] Step 0: spec (`R-SPEC.md`; the five open questions were answered by the owner on 2026-11-14, all as recommended) and OD84
- [x] Step 1: the pure core and the runtime. The pure core is built: the `svc` crate (accounts with nesting and all-or-nothing charges along the chain, the connection table with never-reused keys, held replies with a first-held-first order and a timeout), model-tested against an independent recomputation of usage and mutation-checked. The runtime is `rt::service` (`Core`: port, connections, accounts, the one hold-and-poll backpressure policy, the one violation policy, `add_conn`'s endpoint hygiene; the `Service` trait and `run`). **Not done**: the typed request/reply client (`channel_call` callers)
- [x] Step 2: accounted connections: the `Account` op (op 8) in the file protocol (`proto::file`, S4-SPEC), in `memfsd` (every Open/Clone is charged to the connection's account; accounts are reaped when idle) and in `rt::fs` (`Fs::account`); `memfsd_accounts_confine_a_client` (mutation-checked). The launcher handing a program an accounted root connection is not done
- [~] Step 3: `memfsd` is on the library (251 suite tests pass unchanged at one and four harts; every connection is in the root account, the pool 200 connections and 2 MiB held); `launcher`, `devmgr` and `blkd` are on it too (one connection each today; `Core` now takes the largest request in bytes and handles, and `exit_with`). `consoled` stays on its own loop: it bridges two byte streams (the serial socket and the session socket) with a drop-oldest policy of its own, which the runtime deliberately leaves to the service (R-SPEC section 2); only its small control channel would fit the runtime, and mixing the two loops costs more than it buys
- [ ] Step 4: documents; T28, T30 (launcher single client) and D11 resolved or narrowed

Estimated size (not measured): 4 to 6 PRs; a Fable review.

### DM: The device manager overhaul (built: table-driven, several devices and clients; T41 and some documents open)

Design review D6: `devmgr` handles one device type, one client and one driver at a time. It becomes
table-driven (devices from the device tree, a driver per matching device, started and supervised), serves
several clients, and hands each driver exactly the authority its device needs. Needed before virtio-net.
Builds on slices R (the service loop) and I (interrupts); the existing block-driver tests stay green.

- [~] Step 0: spec: the request carries a device `index` (lowest virtio-mmio slot address first); authority handed to each driver is unchanged (its own MMIO window, one DMA buffer, its interrupt)
- [x] Step 1: table-driven discovery: `devmgr` finds every virtio block device and keeps a table, one driver slot per device, a per-device register mapping, and resets every device at the end
- [x] Step 2: two devices in use at once, from one client, are tested (the harness attaches a second disk with its own sector-0 pattern; `s6: devmgr served two block devices`); **several clients**: `Connect` (op 2, carrying the new client's endpoint) adds a connection through any existing one, served by the same loop, limited by the pool (16) and `Quota`; the manager lives until the last client goes (in `devmgr_starts_blkd_for_the_disk`). Other device kinds (`virtio-net`) come with S8
- [~] Step 3: documents: partly (this note); T41 stays open (the device windows are still the QEMU `virt` ones)

Estimated size (not measured): 2 to 3 PRs.

### I: AIA interrupts (done; the Fable review is still owed)

Decision (2026-11-14, owner, OD75): AIA is the target, full stop. Today interrupt objects are PLIC-only, AIA
machines fall back to polling, and delivery goes to the boot hart only (design review D5, T43). Plan: interrupt
objects for APLIC and IMSIC (message-signalled, delivered to the hart that asked), the device tree parsed for
the AIA topology, the boot test machine and `make os-run` switched to AIA, the PLIC path removed once AIA does
everything it does; the block driver uses real interrupts on AIA instead of polling.

Acceptance (written first): the block-driver interrupt test on the AIA machine takes interrupts (not polling)
on a non-boot hart; the interrupt object tests (mask, ack, level and edge) pass on AIA; the whole suite on AIA
at one, two and four harts; a negative control that drops the interrupt acknowledgement.

- [x] Step 0: spec (SYSCALLS.md interrupt_create/ack, OD75) and the acceptance checks, written first (counters, the boot-script check, the negative control)
- [x] Step 1: APLIC/IMSIC support in the kernel (`kernel/src/aia.rs`; the tree is read with `kcore::fdt::parse`)
- [x] Step 2: interrupt objects on AIA with per-hart delivery; the block driver off polling (verified in QEMU at one, two and four harts)
- [x] Step 3: the default test machine and `make os-run` on AIA; the PLIC path deleted; plain `virt` is the polling fallback (`make os-boot-test-polling`); T43 resolved, its residual is T59

Done as one PR rather than three (it was small). The Fable review is still to do.

Measured: one PR.

### T: Kernel memory and threads (built: steps 1 to 5 of the list below; kernel-object memory is still not charged to jobs, T1 and T13)

Decision (2026-11-14, owner, OD79): design review D9. The fixed 8 MiB kernel heap, chosen in S1 for simplicity
(OD29), is half the problem: every kernel cap (OD50) is hand-tuned to fit it (T13), and 32 KiB kernel stacks
taken from it mean 128 threads use half of it. The machine has 256 MiB. Plan: kernel memory comes from frames,
kernel stacks are not heap, kernel memory is charged to the job that causes it (T1), and creating a thread
becomes cheap. Cancellation and interruptible waits are wanted but deferred (T56); this slice only reserves
their places.

Decided (owner, 2026-11-14): a kernel stack is allocated by the kernel from frames and **billed to the creating job**, with a per-job free list so creation stays cheap; the alternative of the creator handing the kernel its own (kernel-only) VMO was considered and rejected as not worth the extra object kind and lifetime handling.

Plan: (1) measure the real kernel stack high-water mark in the existing tests, then give kernel stacks their
own frame-backed region with a guard page and a size decided from the measurement; (2) replace the fixed-region
heap with a size-class (slab) allocator over frames that grows on demand (it must stay usable from interrupt
context, and every kernel allocation path must fail with NO_MEMORY, never panic: OD50); (3) charge kernel
memory (starting with stacks) to jobs so the OD50 caps become quotas where possible (T1, T13); (4) a deferred reaper frees a thread's
own stack and thread object; (5) creating a thread in one syscall with pooled stacks, and the per-process and
per-job thread limits raised (8 and 4 today); (6) reserve, not implement: an `INTERRUPTED` status that waits
may return, a per-thread interrupt flag, a `thread_interrupt` call returning `NOT_SUPPORTED`, written into
`SYSCALLS.md`.

Acceptance (written first): create and destroy ten thousand threads with no leak (frames, objects, ledger);
two thousand live threads fit and run; a kernel stack overflow hits the guard page and is reported, not
silent corruption (a negative control that removes the guard must fail the test); the heap grows and shrinks
(returns frames) under an allocation churn test; every OD50 cap test still ends in NO_MEMORY and never a panic;
the thread and `channel_call` benchmark before and after; the whole suite at one, two and four harts.

- [~] Step 0: measurement done (`thread::stack_high_water`, test `kernel_stacks_stay_well_inside_their_size`): the deepest kernel stack in the whole suite, debug build, was about 5.6 KiB of 32 KiB, so kernel stacks are now **16 KiB** (halving the thread cost; the heap is still the 8 MiB fixed region). Spec and OD written: `T-SPEC.md`, OD82 (a pool with its own lock refilled at safe points, slab classes, guard-paged stacks with an emergency stack, a kernel-memory level in the job quota, deferred reaper, one-syscall creation). The acceptance tests are written with each step
- [x] Step 1 (was 3 in T-SPEC): kernel stacks from frames: a 1 GiB window of its own (`abi::layout::KSTACK_WINDOW_BASE`), 32 KiB slots with an unmapped 16 KiB guard below a 16 KiB stack, slots recycled on a free list (their frames stay), the trap entry recognises a stack pointer in a guard by arithmetic and switches to a per-hart emergency stack to report `##PANIC kernel stack overflow` (`make os-boot-test-overflow`, `--expect-panic`; the canary words stay as a second line); leak checks account for slots (`kmem::free_frames_accounted`). Smoke green; soak run on the PR
- [x] Step 2: the growable heap: an 8 MiB boot heap plus a grown heap in a 256 MiB window (`abi::layout::HEAP_WINDOW_BASE`) extended 2 MiB at a time from the frame database at safe points (the tail of every syscall, the idle loop) when fewer than 4 MiB are free; allocation never grows inline. It does not shrink. `heap_grows_at_a_safe_point`; the thread limits rise to 2048 per process and 4096 in all, and `thread_limit_and_slots_are_reused` now keeps over two thousand threads alive at once. T13's hand-tuned OD50 caps are not yet recomputed
- [x] Step 3 (partly): kernel stacks are charged to the thread's job (four pages per live thread; `thread_stacks_are_charged_to_the_job_and_credited_back`). The memory of kernel objects themselves is still not charged and the OD50 caps are not revisited (T1, T13 stay open)
- [x] Step 4: thread creation was already one syscall (`thread_create` starts the thread); stacks are pooled (slots recycled); the deferred reaper already frees detached threads; the limits are raised from 8 and 128 to 256 per process and 1024 in all; `thousands_of_threads_come_and_go` creates and ends 2000 threads in one process with the end-of-suite leak checks watching
- [x] Step 5: the reserved cancellation places: `INTERRUPTED` (-14) in the status table and `thread_interrupt` (syscall 63, validates its handle, answers NOT_SUPPORTED; `thread_interrupt_is_reserved`), SYSCALLS.md says every wait may one day return INTERRUPTED. T1 narrowed (stacks charged). **Not done**: step 2 (the growable heap) and with it T13, which stays open

Estimated size (not measured): 5 to 6 PRs; a Fable review.

### SD: Service directory and namespaces (built through step 3; step 4, moving the old servers and clients onto it, is open and no longer waiting: S8 and S9 are the first services on it)

Decision (2026-11-14, owner, OD78): design review D10. A **registry service** (not `init`) holds the directory.
A service publishes a name by handing the registry a listener channel; a client opens `/svc/<name>` with the file
protocol's open-with-endpoint, which the registry forwards to the listener; a dead service's entry goes stale
(its listener closes) and the client sees `UNAVAILABLE` and retries (no queueing in the registry); a program is
handed a *view* with only the names it may use (that is the grant); the startup message's closed `Role` enum
gives way to generic named entries with arbitrary prefixes (a few kernel-recognised roles stay); for now the
shell hardcodes what it grants. **Deliberately undecided, to revisit with Q10:** passing a service connection to
another program, and revocation. Builds on slice R.

Acceptance (written first): publish, open, unpublish; a client sees `UNAVAILABLE` while a service is down and
succeeds after it republishes; a program whose view lacks a name cannot open it; arbitrary namespace prefixes
in the startup message; the registry itself tolerates a hostile publisher and a hostile client (limits per
account, OD77); leak checks exact; the existing programs unchanged.

- [x] Step 0: spec (`SD-SPEC.md`) and OD97
- [x] Step 1: the pure directory logic (`svc::directory`: names, listeners, stale entries, views; model tests, mutation-checked)
- [x] Step 2 (the registry service): `registryd` on `rt::service`, the `PUBLISH` right (bit 16), ops 16 to 18 (`Publish`, `Unpublish`, `View`) and status `UNAVAILABLE` (-109) in `proto::file`, `Role::ServiceRegistry`, `Fs::publish/unpublish/view`, and `registry_publish_open_unavailable_and_views` (publish, forwarded open answered by the service, no hijack, views list and hide, stale then UNAVAILABLE then republished, unpublish; mutation-checked). `rt::service` gained `other_event`, `Core::port` and `has_conn`
- [x] Step 3 (was 2 here; numbering follows SD-SPEC): the startup message's namespace accepts arbitrary absolute prefixes (`proto::startup::valid_prefix`, up to 8 entries; `/svc` is the one the registry uses); `rt::startup` already looks entries up by prefix, and the launcher validates the message through the same decoder. The closed `Role` enum is **not** retired for services yet
- [ ] Step 4: the shell grants a `/svc` view; existing servers publish themselves and serve forwarded opens on a listener (`memfsd`, `launcher`, `devmgr`, `blkd`); clients (`rt::fs`, the shell) reconnect on `UNAVAILABLE`; documents (T15, T30 narrowed). **Deliberately deferred** until the first new service (the network stack, S8) needs it: migrating `init`, `sh` and every server now would rewire the whole boot path for no new capability, and the first real client of the directory should shape the reconnect policy

Estimated size (not measured): 4 to 5 PRs; a Fable review.

### S8: Network, lower layers

Acceptance: the guest pings its gateway; UDP echo works through `hostfwd`.

Architecture proposal: `NET-DESIGN.md`; draft specification with the acceptance checks written first: `NET-SPEC.md` (steps 0 to 6 there). Neither is accepted by the owner yet.

Design rule decided ahead of the spec (OD74, direction): a network endpoint is a native control channel plus, for streams, a data socket the client creates and passes in, so the client pays for its buffers; datagrams are control-channel messages; BSD sockets are a shim over that. The message formats, flow control and the driver/stack split are still to be specified. Depends on slices W and V.

Progress (NET-SPEC steps; boxes tick only when verified in QEMU at `-smp 2` and `-smp 4`): steps 1 and 2 (`netwire`, `netsim`, `netcore`, `supervise`) and the pure `dhcp` client are merged (host-tested; H5 smoltcp interop not done: TIDY-UP T86); steps 3 to 5 are built on one branch: `netring` (loom-checked doorbell handshake), `devmgr`'s `ClaimNet`, `netdrv`, `netsup`, `netd`, `proto::net`, `rt::net`, `dhcpc`, `ping`, `udpecho`, `netinfo`, and the QEMU tests (loopback against slirp, ping the gateway with static configuration and with DHCP, UDP echo through `hostfwd` with the host's own UDP stack as the client and a clean packet capture, accounts and authority, killing each child, the negative controls, the flood); run at one and four harts on the AIA machine and at two harts on plain `virt`. Step 6's remainder is in TIDY-UP T87.

- [x] virtio-net driver (verified in QEMU: smp1 and smp4 on AIA, smp2 on plain virt)
- [x] ARP, IPv4, ICMP, UDP (as above; the guest pings its gateway, the host's UDP stack gets its datagrams echoed)
- [x] DHCP client (as above: QEMU's own server configures the interface)

### S9: TCP and HTTP

Acceptance: a hash-verified fetch from a host HTTP server; host `curl` against an
in-OS HTTP server serving memory-filesystem files; the lossy-link suite passes.

Draft specification with the acceptance checks written first: `S9-SPEC.md` (accepted by the owner 2026-11-15, with the recommendations of its section 17).

Progress (2026-11-15): step 0 (slirp TCP probe and capture) and step 1 (the data-path prototype, `S9-STEP1-RESULT.md`) are done and merged. Step 2 is in progress: `netwire::tcp` (segment view, builder, options; goldens from the real capture) and `nettcp` part 1 (sequence arithmetic, reassembly and send buffers, the RTO estimator, the connection state machine with RFC 9293's figures as packet scripts, RFC 5961 checks, delayed and quick ACKs, persist, Nagle, retransmission with backoff and the 100 s limit). Step 3 (2026-11-15): slow start, congestion avoidance, fast retransmit and NewReno recovery, the idle restart, checked against a separate model of RFC 5681/6582 (300 seeds) and over a bottleneck link with loss (H14, H16; the smoltcp peer, H15, waits on T86). Step 4a: TCP inside `netcore` (connection table, listeners and posted accepts, TIME-WAIT records and SYN reuse, orphans, loopback, keyed ISNs and ports with SipHash-2-4 against its published vectors, resets for unknown connections, ICMP soft errors), host-tested over `netsim` (`netcore/tests/tcp.rs`). Step 4b: `proto::net` stream kinds, ops, statuses and notices (goldens), `rt::net` streams and listeners (the end-of-file rule of 8.5), `netd` streams (client-created sockets, one-shot bindings, accounts and `stream_bytes`, orphans, resets, restart), and three QEMU tests at `-smp 4` (loopback echo and a megabyte, half-close, abort, refusal, quotas, bad handles, a stack restart). Step 4c: the host's own TCP as the independent peer, both ways through slirp: a guest listener echoing 200,000 bytes for a host client through `hostfwd=tcp` (clean end-of-file), and a guest client reading 300,000 bytes from and sending 100,000 to a host server; the capture of those runs (771 TCP segments) passes the checksum checker. Step 5 (2026-11-15): `rngcore` (SHA-256, HMAC, HKDF, ChaCha20 and a fast-key-erasure generator, all against their RFC vectors), the virtio-rng claim in `devmgr`, the `rngd` service (`/svc/random`) and `netsup` giving `netd` a key derived from it. Step 6: BSD sockets in the C library (`socket`, `bind`, `connect`, `listen`, `accept`, `send`/`recv` and the `*to`/`*from` forms, `shutdown`, names and options, `poll`, `select`, `fcntl(O_NONBLOCK)`, `inet_pton`/`inet_ntop`, `getrandom`; 256 descriptors), the host-tested `sockshim` decisions, UDP loopback in `netcore`, and a C test program on the OS (`ctest sockets`, about 40 checks) run by the boot suite. Step 7 (DNS client, HTTP/1, `fetch`, `httpd`, `getaddrinfo`, host web fixtures): done. Step 8 (2026-11-15): the QEMU lossy relay (`scripts/net_lossy_relay.py`, `make os-boot-test-lossy`: loss 2%, duplication 1%, reordering 5%, 5 ms on TCP frames, seeds 1 to 3 at `-smp 2` and `-smp 4`, and seeds 1 to 12 at `-smp 4`, all passing for the fetch, `httpd` and echo transfers by hash), `##BENCH tcp_loopback_1mib` and `tcp_echo_rt_4k`, a churn test (`net_tcp_churn`: 100 connections in sequence and 3 rounds of 32 at a time in the smoke boot; 2,000 and 40 rounds with `bench=full`, which has not been run), INVARIANTS NET11 to NET24 and OD102 to OD109. The whole suite (291 tests) passes at `-smp 1`, `2` and `4`. Not done, recorded in TIDY-UP T102: the guest-to-host-server upload behind the relay (slirp resets it; cause not established), a negative control for the relay run, the remaining benchmark rows and a baseline, random client kills and `netd` restarts inside the churn, the 20-seed sweep and the full-length churn.

- [x] TCP (state machine, retransmission, congestion control) with packet-script tests (steps 2 and 3)
- [x] Sockets interface in the libc (step 6)
- [x] Simulated lossy/reordering link suite (the host-side suite, and the QEMU lossy relay at `-smp 2` and `-smp 4`; see T102 for what the relay run does not cover)
- [x] HTTP server demo; DNS resolver (step 7)

### L: Scheduler, SMP and threads (specified in SCHED-SMP-SPEC.md, OD85 to OD96; tasks 1 to 4 and 6 to 10 done, 11 to 28 not started)

Supersedes the earlier plan for this slice (the design review's D2 carve-outs, kept as history in the review). The whole
design is `SCHED-SMP-SPEC.md`: domain spinlocks in one checked order with the big kernel lock shrinking to the object lock
(OD85), one block/wake path (OD86), per-hart run queues with RT and normal bands (OD87), priority donation and a PI futex
(OD88), priority as a job capability (OD89), RT budgets (OD90), nanosecond time with one-shot timers and timer slack (OD91),
`thread_spawn` and quota-derived thread limits (OD92), ASIDs and ranged shootdowns (OD93), direct hand-off (OD94), the `klock`
crate (OD95) and measurement (OD96). Slices named in the list: **B** benchmark, **SC** two-band scheduler, **TH** threads,
**TM** timers, **RT** real-time, **L** lock peels, **W** the remainder of targeted wake.

**Gate (owner, 2026-11-14; status 2026-11-15: S8 and S9 are done, so tasks 11 and 12 are overdue and 15 to 20 are unlocked):** tasks 1 to 10 before S8; tasks 11 and 12 after S8's first milestone; tasks 13, 14 and 21 to 28
may follow S8; tasks 15 to 20 after S9, in the order the contention profile gives (the order given is a prediction). Stopping
after task 12 leaves a coherent system; stopping after task 20 leaves the final execution model. **Deferred:** the EEVDF-style
hierarchical fair class (only its stubs and preconditions are built), splitting the object lock, shared futexes, suspend and
resume. The object lock stays one lock (owner defers to the spec; revisit on its measured trigger).

- [~] Task 1 (slice B, Built; **done**: `##BENCH` (harness check, kernel filter), `debug_stat` 6 and 7, `make os-bench` in the soak tier, `bench/baseline.tsv`, `bench_compare.py` with a self-test that a 3x slowdown fails, and four benchmarks: `thread_create_join`, `wake_latency_event`, `channel_call_rt` with 0 and 100 bystanders; **not done**: `pair_scaling`, `bkl_profile`, `irq_off_max`, `rt_latency`, and restating 5.2's targets as absolute numbers): Benchmark suite, `##BENCH`, `make os-bench`, the baseline table in `TESTING.md`; restate 5.2's targets as absolute numbers. *Acceptance, written first:* the harness rejects a malformed `##BENCH`; the baseline file exists; a deliberate 3x slowdown fails `bench_compare.py`. *Needs:* nothing. *Review:* default.
- [x] Task 2 (slice B, Built; done: the `latency-trace` feature, `make os-latency`, the test, the first numbers in `TESTING.md`; the RT histogram waits for the real-time probe of task 8): `latency-trace`: interrupts-off maximum with site, BKL wait and hold per syscall, the RT histogram (kernel-thread probe). *Acceptance, written first:* `the_latency_trace_can_see_a_long_section` (a test section of known length is reported within 20%); restate 3.12's budget with measured values. *Needs:* 1. *Review:* default.
- [~] Task 3 (slice B, Built; **done: `wake_many` (T62) and the per-thread cleanup guard (T63)**; **not done: the clock (T40), see TIDY-UP T40 for why it is deferred**): Debts the peels would inherit: `wake_many` (T62), the cleanup guard (T63), the scheduler clock from `time` (T40). *Acceptance, written first:* the T62 scenario (one low waker, two high waiters, a hog, one hart) as a failing test first; `ticks_do_not_stretch_under_lock_contention`. *Needs:* nothing. *Review:* Fable (small).
- [x] Task 4 (slice B, Built; done: the model, the T65 fix and the wfi test, in `kcore/tests/idle_concurrency.rs`, `kernel/src/thread.rs` and `kernel/src/smp_tests.rs`): The T38 shuttle model of today's idle/wake/IPI protocol; fix T65; `wfi_returns_with_interrupts_masked`. *Acceptance, written first:* the model fails on each of its three negative controls and passes on the code. *Needs:* nothing. *Review:* default.
- [x] Task 5 (slice paper, done: the ODs, `INVARIANTS.md` (the lock table, TLB2), the reserved syscall numbers, TIDY-UP; accepted by use): Accept this document: the ODs, the planned lock table in INVARIANTS, reserved syscall numbers, ROADMAP, TIDY-UP (section 8). *Acceptance, written first:* `check_docs.py` passes; no code. *Needs:* nothing. *Review:* owner.
- [x] Task 6 (slice SC, Stub (structure; behaviour as today); **stage one done: `kcore::sched2`** (bands and `SchedAttr`, the per-thread record, `PrioQueues` with an occupancy mask, the class interface with `Enq`/`Deq`, `FixedPrio` and `RtClass`, `HartRq` with strict precedence in `pick`, a nanosecond clock; tested by `kcore/tests/sched2_differential.rs`: 400 seeds of random operations give the same `current` and thread states as the old `sched::Sched` after every step, mutation-checked, plus the RT band's own rules). **stage two done (6b): the kernel runs the bands** — `sched2::TwoBand`, the two bands as one `SchedClass` under the existing multi-hart core (shared-queue mode: one set of queues, a current thread per hart): real-time (32 to 63) above normal with strict precedence, the normal band rotating as before, real-time FIFO within a level (no slice rotation; the round-robin flag waits for per-hart classes), a preempted thread at the front of its level; tested by `kcore/tests/two_band.rs` (differential against the old fixed-priority class at 1, 2 and 4 harts, the real-time rules, the default constructor) and the multi-hart random model, mutation-checked, the whole suite booted at one and four harts. Per-hart `HartRq` queues, push and pull and the conformance suite per class belong to task 11 **: `kcore::sched` v2: `SchedAttr`, `ThreadSched`, the new `SchedClass`, `HartRq` hosting RT + normal + idle, `PrioQueues`, nanosecond clock and accounting hooks. The kernel still runs it in **shared-queue mode** (one set of class queues for all harts, per-hart `cur`, as today) under the BKL. *Acceptance, written first:* differential test against `sched_v1`; conformance suite per class; AC1; the whole suite unchanged. *Needs:* 3. *Review:* default.
- [x] Task 7 (slice SC, Stub (graph built, unused); **done**: `kcore::donate` (add, set_base, donate, revoke, retarget, remove, audit; a bounded change list; cycles, depth beyond 8 and double donation refused) tested against a reference that recomputes every effective priority from the edges (mutation-checked), and `HartRq::set_effective`, the hook that applies a change, tested across bands): `kcore::donate`, wired to `HartRq::set_effective`; no producer of edges yet. *Acceptance, written first:* DN1 to DN3 against the recompute reference, mutation-checked. *Needs:* 6. *Review:* default.
- [~] Task 8 (slice SC, Built (API, bounds); Stub (budget enforcement); **done**: job `max_prio`, `rt_budget` and `cpu_weight` in `kcore::job` (PR1 and PR2 model-tested), `job_set_sched` (96), `thread_set_sched` (65), `thread_get_sched` (66), `job_info` selector 6, inheritance and clamping at thread creation and when a job's maximum is lowered, `thread_priority_is_bounded_by_the_job` (mutation-checked); **not done**: RT budget accounting and `job_info` selectors 7 and 8, `bin/rtdemo` and its negative control, the launcher and `devmgr` policy of 4.9 (programs still run at 10 and every job has the root's 63): Authority and the priority API: job `max_prio`, `rt_budget`, `cpu_weight`; `job_set_sched`; `thread_set_sched`, `thread_get_sched`; inheritance and clamping; RT accounting without enforcement; `bin/rtdemo`. *Acceptance, written first:* PR1, PR2 in the job model; `a_confined_program_cannot_raise_its_priority`; the user-level RT latency demo with its negative control. *Needs:* 6. *Review:* Fable.
- [x] Task 9 (slice TH, Built; **done** (PR #134, Fable-reviewed): `thread_spawn`, the stack arena, a1 = tid, tp, `MAX_REGIONS` 16384, libc `pthread` on it, launcher 128 threads; **not done**: the `thread_create_join` benchmark ratio (not measured), removing `MAX_THREADS_PER_PROCESS` (TIDY T83)): `thread_spawn` with a kernel-provisioned stack, the stack arena, tid in `a1`, `tp`; limits from quotas; `MAX_REGIONS` raised; `libc` and `rt` moved to it; launcher default 128 threads (Q5). *Acceptance, written first:* `ten_thousand_threads_leak_nothing`; `two_thousand_threads_live` (after slice T step 2); every error case of the entry; `thread_create_join` at most half the baseline. *Needs:* slice T step 2 for the 4096 backstop. *Review:* Fable.
- [x] Task 10 (slice L, Built; **done**: the `klock` crate (14-class table, per-hart `Held` checker, `Spin`; pair-table tests, loom, shuttle, Miri; the kernel's checker and heap lock run on it) and the park protocol (`kcore::park`, PK1: a wake counts only for a prepared or blocked thread, never remembered; `kcore/tests/park_concurrency.rs` under loom and shuttle with three negative controls; `Sched` and `HartRq` converted, the wake token and `WaitQueue` deleted, the kernel's four wait sites and its kernel-thread queues on `prepare_block`/`block`; the whole suite at one and four harts). **Not done**: the run-queue lock `RQ` (task 11), so the protocol runs under the big lock; `Call{txid}` registrations (task 21))
- [~] Task 11 (slice L, Built; **pure part done** (2026-11-16): `kcore::sched_smp::Sched2`, one `HartRq` per hart with placement, push marks and pull, GP1 and GP2 checked on random walks over 1 to 8 harts against an independent reference, four hand-run mutations caught; **kernel wiring, first step done** (2026-11-16): the kernel runs `Sched2` (one run queue per hart, placement, one push mark per thread, pull), still as one value under the big kernel lock; Fable-reviewed (`reviews/2026-11-16-l11b-scheduler.md`, findings fixed or recorded in T107); the whole suite passes at `-smp 1`, `2` and `4`, and the SMP soak targets, polling and round-robin kernels pass; **not done**: the per-hart `RQ` spin locks, `need_resched`, `finish_switch` and the dead list, the idle loop of 3.9, the watchdog, the negative-control kernels): Per-hart run queues under `RQ` locks taken *inside* the BKL; push and pull; `need_resched` (X7); `finish_switch`, the dead list and reap by the hart; the idle loop of 3.9; the watchdog. *Acceptance, written first:* GP1, GP2 on 1 to 8 harts against the independent reference; the idle model updated; `rt_threads_take_the_harts`; negative-control kernels `fault-skip-poke`, `fault-early-reap`, `fault-skip-pull`. *Needs:* 10. *Review:* Fable, soak.
- [~] Task 12 (slice L, Built: first peel done; **step one done** (2026-11-16): the scheduler core is behind its own lock of class `RQ` (one lock for all harts), and a timer tick or reschedule IPI that interrupts *kernel* code takes no big kernel lock unless the hart must switch threads (`idle_harts_take_no_big_lock_for_their_ticks`: hart 1 took the lock 160 times for 80 ticks before, 0 now); a thread switched away from is held out of every other hart's reach until its switch completes (invariant TR1); **not done**: ticks and IPIs that interrupt user mode (the kill check at the return to user mode needs the lock), `thread_yield`, `clock_get` and pure sleeps, per-hart `RQ` locks, the lock released at a switch (G4); see TIDY-UP T108): The scheduler leaves the BKL: the lock is released at a switch (G4), the timer and IPI traps and `thread_yield`, `clock_get` and pure sleeps stop taking it. *Acceptance, written first:* `bkl_profile` shows zero BKL acquisitions from the tick; `pair_scaling` for futex-free yield pairs; the suite at 1, 2, 4. *Needs:* 11. *Review:* Fable, soak.
- [ ] Task 13 (slice TM, Built): Per-hart timer queues, one-shot programming (Sstc probed, SBI fallback), slice timer only when needed; **timer slack** (4.11); the nanosecond ABI itself is in task 6. *Acceptance, written first:* timers model with the slack bounds and the interrupt-count property; `sleep_wakes_on_time` to 100 us for an RT thread; a hart with one runnable thread takes no timer interrupt (counter). *Needs:* 12. *Review:* default.
- [ ] Task 14 (slice L, Built): Futex peel: bucket locks; `futex_wake` leaves the BKL; `futex_wait` holds it only to translate the address. *Acceptance, written first:* loom futex model extended (kill, timeout); `pair_scaling` futex ratio at least 3.0 at four harts. *Needs:* 12. *Review:* Fable.
- [ ] Task 15 (slice L, Built): Mappings hold a VMO reference; `reap_vmos` deleted (pure `kcore`; slice L's old step 1). *Acceptance, written first:* `aspace_model` with the reference count; the leak suites. *Needs:* nothing. *Review:* default.
- [ ] Task 16 (slice L, Built (locks exist, not yet relied on)): `JOBS`, `VMO`, `FRAME` as their own locks (still nested inside the BKL), the ledger moved out of the VMO store, per-hart frame caches. *Acceptance, written first:* the existing models; loom for the frame cache; exact frame oracles with caches. *Needs:* 10, 15. *Review:* default.
- [ ] Task 17 (slice L, Built): Memory peel: per-process `AS`; page faults and user copies leave the BKL; chunked copies with preemption points. *Acceptance, written first:* loom: two faults, fault against unmap; `fault-skip-as-lock`; `pair_scaling` for faults and 64 KiB copies; B1 restated. *Needs:* 16. *Review:* Fable, soak.
- [ ] Task 18 (slice L, Built): TLB: the ASID allocator, `ran_on`, batched range and ASID shootdown before the free, no flush on switch. *Acceptance, written first:* TLB1, TLB2; `fault-asid-reuse-no-flush`; the two existing negative controls. *Needs:* 17. *Review:* Fable, soak.
- [ ] Task 19 (slice L, Built): `PROCS` and `TT`; process lifecycle, kill and teardown out of the BKL, chunked; preemption points in the remaining long legacy operations; the sweep decision (section 10). *Acceptance, written first:* `kill_a_spinner_on_another_hart`, teardown suites; `rt_latency` under teardown and sweep churn within B3. *Needs:* 17. *Review:* Fable, soak.
- [ ] Task 20 (slice L, Built: final SMP model): The end of the BKL: `LEGACY` becomes `OBJ` with a commit-on-drop guard; object syscalls take it explicitly; syscalls run with interrupts on; K1 to K5 retired. *Acceptance, written first:* W-SPEC's loom model over the real guard; `wait-audit`; the whole budget of 3.12 asserted in soak; `pair_scaling` for channels at least 2.5 or T70's trigger evaluated. *Needs:* 19. *Review:* Fable, soak.
- [ ] Task 21 (slice W, Built): The W remainder: `Call{txid}` registrations, the exact ready set and wake-one baton (T60), binding lifetime (T61). *Acceptance, written first:* W-SPEC section 5's port model and mutations. *Needs:* nothing. *Review:* Fable.
- [ ] Task 22 (slice RT, Built): Donation through `channel_call` at receive. *Acceptance, written first:* `a_call_lends_its_priority_to_the_server`; the `ipc_model` edge audit; loom reply/timeout/kill. *Needs:* 7, 8, 21. *Review:* Fable.
- [ ] Task 23 (slice RT, Built): PI futex. *Acceptance, written first:* PF1, PF2; the three QEMU tests of 6.2. *Needs:* 7, 8, 14. *Review:* Fable.
- [ ] Task 24 (slice TH, Built): Direct hand-off: the sync wake hint and donation at wake. *Acceptance, written first:* `channel_call_rt` at most 60% of the baseline on one hart; no IPI counted on a same-hart call. *Needs:* 11, 21, 22. *Review:* default.
- [ ] Task 25 (slice RT, Built): RT budgets enforced: hart throttle, job demotion, window events; `rr` rotation. *Acceptance, written first:* NM1 and demotion on the simulated clock; `a_runaway_rt_thread_does_not_hang_the_system`. *Needs:* 8, 13. *Review:* Fable.
- [ ] Task 26 (slice TH, Built): `thread_interrupt` built; INTERRUPTED from every wait (T56). *Acceptance, written first:* each blocking call interrupted while blocked and before blocking; the flag is not lost between waits. *Needs:* 10. *Review:* default.
- [ ] Task 27 (slice TH, Built): `rt::looper` and the debug deadlock checks; pthread mapping of priorities and PI mutexes. *Acceptance, written first:* a looper test program; a held-lock `call` panics in debug; the libc thread tests. *Needs:* 9, slice R. *Review:* default.
- [ ] Task 28 (slice TM, Built): Tickless idle and tickless single-runner. *Acceptance, written first:* the idle model with the periodic tick removed; idle harts take zero timer interrupts over a second (counter). *Needs:* 13, 11. *Review:* default.

Every task keeps the system bootable and green; every kernel or security task marked Fable gets an independent review in its
own small PR, and the lock-peel tasks also get a soak run (`make soak`), at `-smp 1, 2 and 4`.

### F: doofs, the disk filesystem (draft spec in `FS-SPEC.md`; not started; after S9)

Acceptance: the thirteen checks in `FS-SPEC.md` section 11 (tree model, exhaustive crash testing, `memfs` differential, corruption,
independent `fsck`, full volume, QEMU restart, confinement, symlinks, discovery with nothing compiled in, restart, hostile
superblock, clock). Steps are in `FS-SPEC.md` section 8: step 0 (attributes, indexes and `query` in `memfs`, S10's first bullet
moved ahead), C1 (the wall clock, slice C), 1 to 4 (host only: tree, format, semantics, crash harness), 5a to 5f (the QEMU side:
`proto::blk` discard and device info, SD step 4 narrowed, `fsd`, `volmgr`, the mount table, the boot test) and 6 (attributes on
disk, independent Fable review). The mount table is data, not a kernel object: a volume is identified by its uuid, named by
`/etc/mounts` (a mapping, uuid to local name) as `/svc/vol/<name>`, and granted to programs by a separate list (`/etc/trusted`,
later the S10 manifest).

- [x] Step 0 (2026-11-16): attributes, indexes and `query` in `memfs` and `proto::file`, plus symlinks (never followed, relative targets that cannot climb above the connection root) and modification times; the semantics are FS-SPEC 9.1; hand-derived scenarios, a random walk against an independent path-keyed model (60 seeds, 500 steps, queries compared with a brute-force evaluation), eight of ten hand mutations caught (the other two are equivalent), wire goldens and a no-panic sweep in `proto`. `memfsd` does not serve the new operations yet (TIDY-UP T109)
- [x] Step C1 (2026-11-16): wall clock service `clockd` (`/svc/clock`, `proto::clock`, `rt::clock`), `ClaimRtc` in `devmgr` (a read-only VMO of the goldfish RTC's register page; the page is now a kernel device window, SYSCALLS.md), and `clockmodel` (never backwards, a backward correction slewed at half speed; three hand mutations caught). Verified against QEMU's device tree: `google,goldfish-rtc` at `0x101000`, TIME_LOW latches TIME_HIGH. The QEMU test (`clock_service_reports_the_rtc`) boots with `-rtc base=2030-06-15T12:00:00,clock=vm`: the service says a time within ten minutes after the base with quality FromRtc, never backwards, advancing with the sleep; a second service whose device manager is gone says Unset and counts from boot; a malformed request gets BAD_REQUEST. Not done: `fsd` now stamps files with this service's time when `volmgr` gives it a view (T116)
- [x] Step 1 (2026-11-16): `fsfmt` crate: CRC-32C, the block device trait, the 32-byte pointer with its check field, node encode/decode with strict validation, the block pool (generation-tagged retire/reclaim, snapshot pins) and the copy-on-write B+tree (`get`, `insert`, `delete`, `range`, `commit`, `verify`); a random walk against a `BTreeMap` with retained snapshots (FI2: no block reachable from a snapshot is on the free list; no leak after a drain), a flipped-bit test; four hand mutations caught (stale child pointer, missed empty-node removal, split bound off by one, missing retire)
- [x] Step 2 (2026-11-16): the format in `fsfmt` (`superblock`: two rings of four slots, uuid, label, generation, checksum; `volume`: `mkfs`, `mount` (newest slot whose tree verifies, free map rebuilt by the walk), `commit` (flush, slot, flush; the last commit retained until the next is durable), `check` (the `fsck` report)) and the host tools `mkfs-doofs`, `fsck-doofs`, `dump-doofs` (`doofs-tools`; a hyphen, not a dot, because Cargo crate names cannot contain one). Tests: remount and crash round trips, a torn newest slot falls back, a foreign volume's block is caught, an overwrite-forever run on a 60-block volume, a flush/write order log, a random workload with commits, crashes and remounts against a model of the last commit; five of six hand mutations caught by the first run and the sixth (no release of the last commit) by the overwrite-forever test
- [x] Step 3 (2026-11-16): the `doofs` crate: files (extents of 4 KiB data blocks with check fields, holes read as zeros), directories, symlinks, rename, quotas with the `memfs` accounting, open-but-unlinked nodes kept as orphan records and reaped at the next mount, every operation a transaction (a failed one puts the tree, the pool and the counters back; the retry after a commit reclaims space), a 64-block reserve for the deleting paths, and `doofs::fsck::check_fs`. Differential test against `memfs` (10 seeds of 1200 operations by default, `DOOFS_SEEDS=n` for more; the same error or the same bytes, listing, kinds, sizes, mtimes, parents and quota counters, with remounts, and `fsck` and the no-reachable-block-is-free check after each sample). It found two `memfs` accounting bugs, fixed here: a symlink's target bytes were dropped from its cost on rename, and a refused symlink had already touched its directory's mtime. Eight of eleven hand mutations were caught by the first run, the rest by tests added after (byte-limit boundary, the pool-level reserve, the FI2 check in the walk); one (a rollback that keeps the counters) is equivalent today because counters change last
- [x] Step 4 (2026-11-16): the crash harness (`doofs/tests/crash.rs`): a recording device, every one- and two-operation workload from thirteen operations (create, mkdir, write, truncate, rename, rename-over, unlink, fsync) and sampled three-operation ones, a crash after every device operation with the unflushed writes dropped, kept, torn (half a block) and randomly mixed, each image mounted, checked (`check`, `check_fs`), compared with the `memfs` model for the last durable commit or one in flight, and remounted again (about 23,000 scenarios, 11 s); the corruption test (`doofs/tests/corruption.rs`: every 61st bit of every used block of an image, 7,800 mounts: no panic, no wrong bytes, a data flip is a read error). Five hand mutations caught (no flush before the slot, none after, free-everything at the end of a transaction, oldest-slot mount, no check on data reads); a sixth (reclaim before the slot is durable) is equivalent while one thread runs. Not done from FS-SPEC check 5: the independent `fsck` written by a separate context (T113)
- [~] Steps 5a to 5f. **Done (2026-11-16/17):** 5c `fsd` (see below), 5d `volmgr` (`init/src/bin/volmgr.rs`: asks `devmgr` for every block device, probes each with `fsfmt::probe` (eight block reads, geometry and label checked), reads `etc/mounts` through the pure `mounts` crate, decides with `mounts::plan` (names only from the table, a cloned disk refused), starts one `fsd` per volume with listener channels and publishes `vol-uuid-<hex>` and `vol-<name>`; restarts a dead server), 5e the table (`bootfs/etc/mounts`; options `ro` and `sub=`), 5f in part: the QEMU test `volmgr_serves_volumes_by_the_table` (junk disk ignored, the table names the volume, a read-only grant cannot write, everything is killed and started again and the file is still there) and the host `fsck-doofs` (now also the filesystem layer) run on the second disk after every strict boot. **Grants (5e/5b, 2026-11-17):** the interactive session now starts `devmgr`, `registryd`, `clockd` and `volmgr` (best effort: no disk, no volumes); `sh` opens `vol-<name>` for the volumes a trusted program's `/etc/trusted` line names and passes them at `/mnt/<name>` (`ls store`, `cat store`, `ctest store` in the shipped file); libc and `ls`/`cat` resolve paths inside such an entry; `rt::fs::open_retry` retries `UNAVAILABLE`; the session's end asks every volume to commit first. Shell test (`scripts/s4e_shell_test.py`): a host-made doofs disk, a C program makes/reads/renames/removes files at `/mnt/store`, a file survives `exit` and the new session, an untrusted program has no volume, and the host `fsck-doofs` accepts the disk afterwards. **Not done:** 5a (discard), the supervision restart is not exercised in QEMU (T118), images built by the host tools and attached in either order with a duplicate and blank disk (needs a third and fourth disk, which shifts device numbers the older tests rely on, T118), the independent checker (T113)
- [x] Step 6, the implementation (2026-11-16): attributes, user and built-in indexes and queries on disk (`doofs::attrs`: attribute records, index definitions, entries `(index, escaped key, id)` kept by `put_inode`/`set_name`/`attr_*`, back-fill at `make_index`, a range scan filtered to the subtree with `(key, id)` paging), served by `fsd` (`ATTR_*`, `MAKE_INDEX`, `QUERY`; the wire's query ids are now 64-bit) and exercised in QEMU (`fsd_keeps_files_across_a_restart`: set, index, query, sync, an unsynced change, kill, remount, query again). The differential test against `memfs` covers attributes, indexes and queries (30 seeds clean; eight hand mutations caught), `fsck` checks the index entries, the crash harness includes attribute and index operations (about 46,000 scenarios). **Not done: the independent review of the whole slice (Fable, its own PR), which this unattended run does not start**

### UV: The user's view of the system, and the native shell (specified, **nothing built beyond the survey**; drafted 2026-11-15)

The command-line side of the system: the canonical layout of a session's namespace, POSIX as a porting surface judged by a
named corpus, advisory POSIX metadata, the terminal, sessions and profiles, `/proc`, and the es-style native shell. The specs
are `USER-VIEW-SPEC.md` (the track and its steps UV0 to UV10), `SHELL-SPEC.md` (the shell language), `SHELL-CORPUS.md` (ten
scripts and the findings that shaped the syntax) and `UV0-GAP-SURVEY.md` (the libc and `rt` against the target programs). Q4 (flat merged install layout) and Q12 (the confined default namespace) were resolved on 2026-11-17; authentication, enrolment and recovery were designed on 2026-11-16 (`USER-VIEW-SPEC.md` section 10.1: an `authd` service separate from the session manager, three scenario-tied enrolment routes, and a recovery policy that is configurable per system, physical-console reset being the default); the profile format is decided at UV5. Other open
questions are in `USER-VIEW-SPEC.md` section 15 (Q2 dynamic linking, Q5 PIDs, Q13 to Q15 recovery policy, home-key derivation and multi-factor, and others). Decisions to log on acceptance are in its section 14 (OD-UV1 to OD-UV11, provisional).

Already built that the track builds on: per-program namespaces and the registry (SD), the `/etc/trusted` grants of volumes at
`/mnt/<name>` (filesystem step 5e), the doofs attributes and indexes (step 6; UV3's storage), the libc over the file protocol.

- [x] UV0: the gap survey (`UV0-GAP-SURVEY.md`, #157). Table only; no code
- [ ] UV1: `..`, the cwd string and path-annotated fds (libc changes, the `shell-core::resolve` clamp; the server keeps refusing a raw `..`)
- [ ] UV2: the skeleton root and the canonical layout (launcher-built root, `/usr` links, `/dev` servers, private `/tmp` per job, `/mnt`)
- [ ] UV3: advisory POSIX attributes (`posix.mode/uid/gid` on doofs and `memfs`, `stat` populated, hard links refused)
- [ ] UV4: the terminal's raw mode, events and window size
- [ ] UV5: the session manager, profiles and autologin (replaces the `/etc/trusted` list as the way a program gets its table)
- [ ] UV5a: `authd`, enrolment (owner-claim setup session, seed credential, dev autologin) and physical-console recovery (`USER-VIEW-SPEC.md` 10.1); later, **configurable recovery** (a second credential, a recovery key, no recovery) for the few systems that need more than physical access, and home-key derivation from the credential (Q13, Q14)
- [ ] UV6: `/proc` (the adapter, driven by `ps`, `free`, `nproc`, `top`)
- [ ] UV7: detach and reattach
- [ ] UV8: BusyBox NOMMU as the first real userland
- [ ] UV9: `/run` and `AF_UNIX` (when a port needs it); UV10: the Rust `std` on our libc and uutils (with S11)
- [ ] The native shell (`SHELL-SPEC.md`): not yet cut into steps; it lands alongside UV4 to UV5

### S10: Namespaces and attributes

Acceptance: scripted attribute queries pass; a union-backed `/bin` demo works.

- [x] Indexed attributes and `query` in the memory filesystem (done as filesystem step 0, 2026-11-16, and on disk in step 6; see slice F)
- [ ] Union server
- [ ] Manifest-driven, confined-by-default shell (the design is in slice UV above: `USER-VIEW-SPEC.md`, `SHELL-SPEC.md`; the manifest replaces `/etc/trusted`)

### S11: Compatibility

Acceptance: ported software that needs `fork` runs.

- [ ] `fork` shim over copy-on-write snapshot VMOs
- [ ] `mmap` private and shared; pager-backed VMOs
- [ ] `posix_spawn`, a signals subset, the process server

### Directions under discussion (not committed)

**These are directions, not finished plans.** They came out of design conversations after S7 and are
written down so they are not lost. Some have since been logged in `OS-ARCHITECTURE.md` section 17 as
"decided (direction)" (OD73, OD74 and OD76-OD79): the owner has chosen the direction, but the spec, the
acceptance checks and the place in the slice order are still to be written. The planned slices above (A, W,
V, T, R, SD, DM, I, L) are the concrete versions; the items below are the original wording.

- **Cheap kernel threads.** *(Specified: `SCHED-SMP-SPEC.md`.)* One-to-one kernel threads made cheap (creation in one syscall, small
  kernel stacks, higher limits, direct hand-off on `channel_call`), pthreads as the POSIX shim over
  them, a native message-loop library for programs written for this OS. Measure first.
- **Redo SMP and the scheduler** *(Specified: `SCHED-SMP-SPEC.md`.)* (now planned as slice L above, in a smaller form than first imagined; kept here as the original direction). Replace the big kernel lock with finer locking or per-hart state
  with messages between harts; per-hart run queues and a wake path that wakes one thread (T11, T12,
  T38, T39). Better done after S8 and S9 give a realistic multi-threaded load to tune against.
- **A layered API.** The native API (handles, rights, ports, VMOs, jobs, protocols) is the real one;
  a POSIX-ish library is a shim over it. Decide which POSIX features get a shim and which are
  refused (candidates to refuse or keep outside the core: `fork`, asynchronous signals, users and
  permission bits, ambient paths, ptys). Note S11 above still plans a `fork` shim; this direction
  would keep it a library feature, not a kernel one. Rule for S8/S9: the network server gets a native
  protocol and BSD sockets are a shim over it, not the reverse.
- **Borrow from OTP for the userland services.** A shared service-loop library (like `gen_server`),
  a supervisor library with restart strategies and a restart-intensity limit instead of rebuilding the
  whole stack (T15), a name registry and reconnecting clients (T30), state that outlives a service
  restart (T16), and the TCP stack as a state machine (like `gen_statem`) under a connection
  supervisor. The kernel side of the idea is "share nothing, send messages between harts".

Longer-term backlog (see `OS-ARCHITECTURE.md` section 16): a real on-disk
filesystem with crash-injection tests, graphics and input, scheduler
alternatives against the conformance suite, capability revocation, quotas,
self-hosting; **IPv6** (distant future, added at the owner's request 2026-11-15: NET-SPEC and S9 are IPv4 only on purpose, and `netwire`, `netcore` and `nettcp` should keep addresses behind small types so it stays possible).

### Future topics (noted 2026-11-15; not specified, not scheduled, not committed)

**Topics only.** These came out of a design conversation; each needs a spec, an acceptance check and an
OD entry before it becomes a slice. Nothing here has been checked against the code beyond a keyword
search, so some of it may already be partly covered by a slice above. Rough grouping, not an order.

*Portability and hardware*

- **A second architecture (AArch64).** Approach if ever wanted: an in-tree `arch` layer, not kernel forks.
  Refactor on RISC-V first (arch code behind one module: boot, trap frame, context switch, per-CPU
  register, timer, IPI, TLB and cache maintenance, FP state; the page-table code in `kcore` made generic
  over a PTE format; an interrupt-controller trait; a barrier abstraction), then bring Arm up under
  `qemu-system-aarch64 -M virt` and run the same suite as the oracle. First step would be a read-only audit
  of RISC-V assumptions leaking into `kcore` and `user.rs`. Needs its own profile-floor decision beside OD47.
- **A real driver model that is not QEMU `virt`.** Drivers are already userspace programs holding
  capabilities, and `devmgr` reads the DTB (slice DM). What is still QEMU-shaped: `DEVICE_WINDOWS` and
  `MAX_IRQ` in `abi` are constants, not derived from the DTB; `devmgr` matches only virtio-mmio block, with no
  `compatible`-to-driver table; the kernel UART bridge (T14); trust-based DMA with no IOMMU or cache
  maintenance (T42); no shared virtio-queue crate. Also: fault-injecting fake devices as an independent
  oracle for drivers, more QEMU machine types (PL011, SDHCI), and one real board as a periodic (not merge)
  hardware gate.
- **Drivers informed by Linux and the BSDs.** Not a line-by-line translation: Linux is GPL-2.0, so a
  translation is a derivative work, and its drivers are tied to kernel frameworks. Preferred shape: extract
  a prose hardware spec (datasheet plus Linux and BSD behaviour and quirks), write the driver from that
  spec over a typed MMIO layer, and review it against the sources in a fresh context. BSD-licensed drivers
  are the easier source. Needs the target board chosen first, and a licence decision.

*Foundations*

- **Logging.** A preallocated kernel ring buffer (structured records, never blocks, usable in the panic
  path) with serial as one consumer, plus a userspace `logd` for persistent and queryable logs (shared-memory
  rings, capability-carried source identity, per-source rate limits, file and network sinks, a shell `log`
  command). Verdict lines stay on their own path. The lock-order placement against the BKL, heap and UART
  is the design question. Would let T14's bridge go.
- **Wall-clock time.** RTC driver, a time service, timezones, optional time sync (Q11). Needed by file
  timestamps, TLS certificate validation and log records.
- **Terminals.** A tty/pty layer with a line discipline, job control and window size, or an explicit
  decision to refuse ptys (see the layered-API direction above).
- **Process model completeness.** The process server, PIDs and `wait`, and real signal semantics
  (Ctrl-C is a kill today); overlaps S11.
- **Memory maturity.** Pager-backed VMOs, a page or buffer cache, an out-of-memory policy, a per-process
  memory quota (Q9), and dynamic linking and shared libraries (to be confirmed: only static linking was
  seen).

*Operations*

- **Power management.** Layers, cheap to hard: tickless idle on a hardware-counter monotonic clock
  (the 1 ms tick today); clean shutdown and reboot (SBI SRST, PSCI on Arm) with a service-notification
  protocol and a deadline; hart park and unpark; a userspace `powerd` holding a power-control capability and
  owning policy; device power states through `devmgr`; DVFS and thermal (needs real hardware); suspend
  (probably never). Interrupt objects need a wake-source attribute. Park/unpark against IPIs and TLB
  shootdowns belongs under loom/shuttle first.
- **Service supervision and boot configuration.** Dependency order, restart policy, health checks and a
  place for system configuration; the shutdown protocol is the same mechanism seen from the other side
  (builds on slice R and the OTP direction above).
- **Crash handling and debugging.** Core dumps or a crash-report record, and a debug capability (the
  `ptrace` equivalent).
- **Filesystem safety.** Crash consistency, an fsck equivalent, snapshots or backup. Decide before the
  doofs on-disk format is fixed (slice F).
- **Observability beyond logs.** Counters, tracing and profiling hooks, generalising the latency trace.

*Security*

- **An authority and login model.** There are no uids by design; still open is how a session's
  capabilities are built, how a program asks for more, and what "login" means.
- **Crypto and TLS.** A hash, signature and TLS stack and a certificate store (HTTPS, ssh, signed updates);
  the "write it here" rule for the kernel and network stack probably should not apply to cryptography.
- **Authentication and hardening.** A credential store; ASLR, stack protection, W^X across userland;
  syscall fuzzing as a standing check if soak does not already cover it.

*Lifecycle*

- **Packaging and updates.** How software is installed; a signed system image with A/B or rollback.
- **Internationalisation.** UTF-8 throughout, locale and timezone data.

*More components (a second, broader pass, same date)*

- **Boot chain.** What loads the kernel on real hardware (OpenSBI plus U-Boot or UEFI), partition and
  boot-menu handling, a boot-config format, A/B slots, a recovery or rescue mode. QEMU `-kernel` hides it.
- **Bus enumeration.** PCIe (config space, BARs, MSI/MSI-X, hotplug) and USB (host controller, hubs, class
  drivers for storage, keyboard, serial, network). The decision on whether the OS can leave QEMU.
- **Storage stack below the filesystem.** Partition tables, volume management, disk encryption, SMART and
  error reporting, TRIM, an I/O scheduler.
- **Input and display.** Keyboard, mouse, framebuffer or GPU, a compositor, fonts and text rendering, a
  clipboard, and isolation of screen and input between programs. Extends the "graphics and input" backlog item.
- **Audio, wireless, Bluetooth.** Each a driver plus a policy service; optional.
- **Network management and services beyond S8/S9.** Packet filter or firewall, NAT, a DHCP server, IPv6,
  VPN, a time-sync client, mDNS, and a network configuration manager (interfaces, routes).
- **Scheduled work.** A cron or timer service.
- **Common libraries and formats.** Compression and archives (tar, zip), a media-type registry, a
  standard serialisation for the native protocols.

*More cross-cutting properties*

- **ABI and protocol stability.** A versioning and deprecation policy for syscalls and `proto::*` (how a
  new kernel runs old binaries, how a field is added). Cheap now, expensive after more servers exist; the
  one to raise earliest.
- **Fault containment and recovery.** Hardware and software watchdogs, hung-versus-crashed policy, memory
  error handling, and a "what survives which failure" matrix. Ties together slice R, supervision and shutdown.
- **Audit and accountability.** Tamper-resistant records of security-relevant events (capability grants,
  process starts, authentication, policy changes), distinct from diagnostic logging.
- **Sandboxing untrusted code.** A helper that runs a program with a deliberately narrowed capability set,
  and limits on resource exhaustion and covert channels beyond quotas.
- **Hardware-rooted security.** Measured or secure boot, a TPM or similar, firmware update, architecture
  memory protections (PMP/Smepmp; pointer authentication and BTI on Arm).
- **Real-time guarantees.** Worst-case analysis behind the real-time band: bounded lock hold times,
  priority inheritance, an interrupt-latency budget, which paths may allocate.
- **Resource management as policy.** End-to-end limits beyond quotas and jobs: I/O bandwidth, network
  shaping, per-service CPU shares, and what happens when limits conflict.
- **Determinism and reproducibility.** Reproducible builds and a record of which sources and tests produced
  which image.
- **Formal or model-based assurance for the core.** A formal model or machine-checked proofs of handle
  unforgeability and rights monotonicity (optional; the capability design suits it).

*Ecosystem and human factors*

- **Developer experience.** A debugger, profiler, symbolised crash traces, and a self-hosting toolchain.
- **Documentation.** Manual pages or equivalent, a programming guide for the native API, references
  generated from the specs; the audience is not yet defined.
- **Interoperability.** FAT/exFAT for removable media at minimum, network file protocols beyond 9P.
- **Accessibility and input.** Keyboard layouts, input methods, screen-reader hooks; painful to retrofit
  once a display layer exists.
- **Licence and provenance tracking.** Which parts are original, which are ported (Lua, libc pieces), and
  under what terms; becomes real as soon as anything comes from Linux or the BSDs.
- **Review and assurance record for the AI-written process.** Which review caught what, which invariants
  have independent oracles and which do not, a periodic fresh-context audit (TIDY-UP and `reviews/` do much
  of this already).
