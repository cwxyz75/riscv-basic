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
- [ ] Finer locking on the hot paths (not done, TIDY-UP T11; the plan is slice L, which keeps one lock for object state and carves out only what measurement justifies)
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

### Queue before S8 (proposed order; set 2026-11-14 after the design review)

The design review (`reviews/2026-11-14-design-review.md`) found a handful of structural things worth
settling before the network stack is built on them. This is the proposed order; the owner can reorder.
Slices A, W, V, T, R, SD, DM and I below each have a plan. L (locking) stays after S9.

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
10. S8, then S9; then L (locking carve-outs).

### W: Targeted wake and ports as registrations (planned, before S8; not started)

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
- [~] Step 4: ports as registrations: **port waits are woken by id** (a waiter registers on the port, whose signal follows its bindings and is re-evaluated on every commit that touched anything). **Not done**: the exact ready set with a first-come list and wake-one baton (PW), binding quotas instead of the 256 cap, per-handle binding lifetime (D17)
- [x] Step 5: the global wait queue (OD43) is deleted (`signal_queue`, the wake-all in `notify`, `wait_deadline`); every wait is registered and woken by id; the hand-kept syscall list went in step 3; documents updated; T12 resolved for the wake-all cost (multi-waiter ports can still see duplicate wakes until PW)
- [ ] Step 6: one-shot bindings and `port_rearm`

Estimated size (not measured): 6 to 7 PRs, comparable to S5c or S7b. The kernel review of this slice is a
Fable review.

### V: VMO commit and the shared-memory rule (planned, before S8; not started)

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

### A: The RVA23 user ABI (planned; not started)

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

### R: The service runtime (planned; not started)

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

- [~] Step 0: spec: a draft v0 is in `R-SPEC.md` (written unattended; the open questions need the owner) and OD (the loop, the accounted-connection protocol change, the backpressure policy) is still to be logged once they are answered
- [ ] Step 1: the service-loop library with its own tests (host-testable pieces pure)
- [ ] Step 2: accounted connections in the file protocol and `memfsd`; per-account quotas
- [ ] Step 3: `memfsd` on the library; then `consoled`, `launcher`, `devmgr`, `blkd`
- [ ] Step 4: documents; T28, T30 (launcher single client) and D11 resolved or narrowed

Estimated size (not measured): 4 to 6 PRs; a Fable review.

### DM: The device manager overhaul (planned; not started)

Design review D6: `devmgr` handles one device type, one client and one driver at a time. It becomes
table-driven (devices from the device tree, a driver per matching device, started and supervised), serves
several clients, and hands each driver exactly the authority its device needs. Needed before virtio-net.
Builds on slices R (the service loop) and I (interrupts); the existing block-driver tests stay green.

- [~] Step 0: spec: the request carries a device `index` (lowest virtio-mmio slot address first); authority handed to each driver is unchanged (its own MMIO window, one DMA buffer, its interrupt)
- [x] Step 1: table-driven discovery: `devmgr` finds every virtio block device and keeps a table, one driver slot per device, a per-device register mapping, and resets every device at the end
- [~] Step 2: two devices in use at once, from one client, are tested (the harness attaches a second disk with its own sector-0 pattern; `s6: devmgr served two block devices`); **not done**: several *clients* of the manager (it still serves one channel and exits when it closes) and other device kinds (`virtio-net` comes with S8)
- [~] Step 3: documents: partly (this note); T41 stays open (the device windows are still the QEMU `virt` ones)

Estimated size (not measured): 2 to 3 PRs.

### I: AIA interrupts (planned; not started)

Decision (2026-11-14, owner, OD75): AIA is the target, full stop. Today interrupt objects are PLIC-only, AIA
machines fall back to polling, and delivery goes to the boot hart only (design review D5, T43). Plan: interrupt
objects for APLIC and IMSIC (message-signalled, delivered to the hart that asked), the device tree parsed for
the AIA topology, the boot test machine and `make os-run` switched to AIA, the PLIC path removed once AIA does
everything it does; the block driver uses real interrupts on AIA instead of polling.

Acceptance (written first): the block-driver interrupt test on the AIA machine takes interrupts (not polling)
on a non-boot hart; the interrupt object tests (mask, ack, level and edge) pass on AIA; the whole suite on AIA
at one, two and four harts; a negative control that drops the interrupt acknowledgement.

- [ ] Step 0: spec (AIA topology, interrupt object semantics, per-hart delivery), tests first
- [ ] Step 1: APLIC/IMSIC support in the kernel and the device-tree parser (`kcore::fdt`)
- [ ] Step 2: interrupt objects on AIA with per-hart delivery; the block driver off polling
- [ ] Step 3: switch the default test machine and `make os-run`; retire the PLIC path; T43 resolved

Estimated size (not measured): 3 to 4 PRs; a Fable review.

### T: Kernel memory and threads (planned; not started)

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

- [~] Step 0: measurement done (`thread::stack_high_water`, test `kernel_stacks_stay_well_inside_their_size`): the deepest kernel stack in the whole suite, debug build, was about 5.6 KiB of 32 KiB, so kernel stacks are now **16 KiB** (halving the thread cost; the heap is still the 8 MiB fixed region). Still to write: spec and OD (the allocator, the stack region, kernel memory charging, the reserved interrupt stubs) and its acceptance tests
- [ ] Step 1: kernel stacks from their own region with guard pages
- [ ] Step 2: the slab allocator over frames replaces the fixed heap (interrupt-safe, fallible)
- [ ] Step 3: kernel memory charged to jobs; OD50 caps revisited
- [ ] Step 4: deferred reaper; thread creation in one syscall; pooled stacks; raised limits
- [ ] Step 5: the reserved interrupt stubs and documents; T13 resolved, T1 narrowed

Estimated size (not measured): 5 to 6 PRs; a Fable review.

### SD: Service directory and namespaces (planned; not started)

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

- [ ] Step 0: spec and OD (registry protocol, views, startup message change, `UNAVAILABLE`), tests first
- [ ] Step 1: the registry (pure directory logic in a host-tested crate, then the service on the service runtime)
- [ ] Step 2: generic named entries in the startup message; `rt` and the launcher; the closed `Role` enum retired for services
- [ ] Step 3: the shell grants a `/svc` view; existing servers publish themselves (`memfsd`, `consoled`, `launcher`)
- [ ] Step 4: clients reconnect on `UNAVAILABLE` in the service runtime; documents (T15, T30 narrowed)

Estimated size (not measured): 4 to 5 PRs; a Fable review.

### S8: Network, lower layers

Acceptance: the guest pings its gateway; UDP echo works through `hostfwd`.

Design rule decided ahead of the spec (OD74, direction): a network endpoint is a native control channel plus, for streams, a data socket the client creates and passes in, so the client pays for its buffers; datagrams are control-channel messages; BSD sockets are a shim over that. The message formats, flow control and the driver/stack split are still to be specified. Depends on slices W and V.

- [ ] virtio-net driver
- [ ] ARP, IPv4, ICMP, UDP
- [ ] DHCP client

### S9: TCP and HTTP

Acceptance: a hash-verified fetch from a host HTTP server; host `curl` against an
in-OS HTTP server serving memory-filesystem files; the lossy-link suite passes.

- [ ] TCP (state machine, retransmission, congestion control) with packet-script tests
- [ ] Sockets interface in the libc
- [ ] Simulated lossy/reordering link suite
- [ ] HTTP server demo; DNS resolver

### L: Kernel locking carve-outs (planned, proposed after S9; not started)

Why: the design review (finding D2) found that kernel state is global by construction (one `UserState`,
one registry of all objects, a job ledger inside the VMO store, a reachability sweep that needs every
handle table at once, a global run queue), so "push per-object locks down" is not an incremental step.
The recommended direction keeps **one lock for object, handle, VMO and job state** (short, pure code,
and it keeps the model tests' shape) and carves out only what measurably hurts. Basis:
`reviews/2026-11-14-design-review.md` (D2). Replaces the "rung 3, per-object locks" plan in
`OS-ARCHITECTURE.md` section 7 (as an OD, step 0). This is the "redo SMP" direction made concrete, in a
smaller form than first imagined.

Placement: after slice W (it needs W's prepare/commit/cancel wait API) and, as proposed, after S8/S9, so
that the carve-outs are chosen against a realistic multi-threaded load instead of our own test suite.
The owner may move it earlier.

Decisions taken (2026-11-14, owner): follow the reviewer's recommendations. The reachability sweep
**stays** (it is compatible with one retained object lock; revisit only if a later measurement says
otherwise). Carve-outs, each only if step 0's measurements show it pays: (1) the scheduler and wait/wake
path with real per-hart run queues under their own leaf lock; (2) user copies and page zeroing done
outside the big lock (pin, copy, re-lock); (3) mappings hold a reference on their VMO, so `reap_vmos`,
the scan of every address space, disappears; (4) candidates for lock-free fast paths that touch only
scheduler or futex state (`yield`, `sleep`, `clock_get`, futex wait and wake with a leaf table lock),
decided by the measurements.

Acceptance (written first): contention counters (BKL wait and hold time per syscall class, at `-smp 4`)
and a scaling test (N independent process pairs doing `channel_call` ping-pong, throughput against one
hart); a target set after the first measurement; a loom/shuttle model for every new lock (the order
BKL then scheduler lock, the scheduler lock a leaf); negative-control kernels that skip each new lock
and must fail their guarding test; the full existing suite, at `-smp 1, 2 and 4`.

- [ ] Step 0: spec and OD (supersedes rung 3; the sweep stays; where each new lock sits in the order), contention counters, the scaling test and the first measurement
- [ ] Step 1: mappings hold a VMO reference; `reap_vmos` removed (pure `kcore` change, model-tested)
- [ ] Step 2: per-hart run queues under a scheduler leaf lock, work placement and balancing (uses W's wake API)
- [ ] Step 3: user copies and page zeroing outside the big lock (needs a pin discipline against concurrent unmap, TLB shootdown and VMO commit, D3)
- [ ] Step 4: lock-free or leaf-locked fast paths chosen by the measurements
- [ ] Step 5: documents (INVARIANTS lock ordering, SYSCALLS execution model); T11, T38, T40 (and the range-shootdown idea, T39) revisited or resolved

Risks: step 3 is the dangerous one (a copy racing an unmap or a kill); every step is a Fable review.
Estimated size (not measured): 5 to 7 PRs, comparable to S5.

### S10: Namespaces and attributes

Acceptance: scripted attribute queries pass; a union-backed `/bin` demo works.

- [ ] Indexed attributes and `query` in the memory filesystem
- [ ] Union server
- [ ] Manifest-driven, confined-by-default shell

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

- **Cheap kernel threads.** One-to-one kernel threads made cheap (creation in one syscall, small
  kernel stacks, higher limits, direct hand-off on `channel_call`), pthreads as the POSIX shim over
  them, a native message-loop library for programs written for this OS. Measure first.
- **Redo SMP and the scheduler** (now planned as slice L above, in a smaller form than first imagined; kept here as the original direction). Replace the big kernel lock with finer locking or per-hart state
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
filesystem with crash-injection tests, a 9P bridge, graphics and input, scheduler
alternatives against the conformance suite, capability revocation, quotas,
self-hosting.
