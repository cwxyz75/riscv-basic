# Roadmap

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
is untouched. **S0, S1 and S2 are built; S3 onwards is not.** Each milestone ends in a demo and has a
runnable acceptance check, written before the code. Tick a box only when verified in
QEMU (for SMP work, in both normal and icount modes). Crate names are fixed by S0:
`abi`, `kcore`, `os-kernel`; more arrive with their milestones.

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

Built in slices (OD38). **S3a (done): the pure half**: `SYSCALLS.md` (ABI, status codes, rights, signals, handle/channel/event calls), `kcore::{handle, object, ipc}` with model tests, mutation-checked (all caught). Independent review found and I fixed: `Registry::sweep` leaked survivors' references; a user-driven cycle loop grew kernel memory without bound (now a pressure sweep, a per-endpoint byte cap and an object cap; honest limit: per-job quotas, Q9, are still needed for a real bound); `channel_write` error order disagreed with the spec (spec, code and model now agree); a txid read on a closed channel could wait forever; the random model now covers events, txid reads, limit-sized messages, queue-full and mid-run sweeps. **S3b (done): user mode.** `kcore::{elf, usercopy}` (validating static-ELF loader front end with a mutation fuzz; user-memory copies by page-table walk, never SUM), processes with page tables that share the kernel half, the U-mode trap path (`sscratch` swap), the syscall dispatcher for the S3a calls plus `debug_write`/`process_exit`/`thread_yield`/`clock_get`/`object_signals`, the embedded user programs (`rt` runtime, `init` = a 18-test user-level suite run in U-mode over the real ABI, `crash` = programs that misbehave on purpose), and kernel-side checks: every fault kind ends the process with the documented exit code while the kernel keeps running, teardown returns every frame and every kernel object. Two processes alive at once are verified (private memory under timer preemption and under yield switches). Not yet: process/thread/VMO syscalls, blocking waits, `channel_call`, per-process kernel locks (S3c/S5). Independent review of S3b found, and I fixed: no address-space switch when a thread resumed (needed a hook on both return paths; found by removing each), 32-bit syscall arguments silently truncated, `channel_read` validating the declared capacity instead of the message, a window where a new user thread was unknown to the kernel, a `crash` store test that would pass without the user-bit protection, and gaps in the test list; documented rather than fixed: syscalls are non-preemptible; `start`/`finish` use the global kernel memory state with interrupts *on* and are safe only because the boot thread that calls them outranks every user thread (a real kernel lock is needed before S3c lets user threads create processes); user-printed `##TEST` names are trusted.

Acceptance: a user test suite passes; every syscall's error cases are covered per
`SYSCALLS.md`; killing a process leaks no objects.

- [x] User mode, syscall entry, handle table with rights (S3a/S3b; verified in QEMU: 18 user-level tests over the real ABI, 7 fault kinds, teardown leak checks)
- [ ] Channels (with `call`), ports, events, sockets (channels and events done and tested in S3a/S3b; `call`, ports, sockets are S3c)
- [ ] Process, address-space and VMO objects; minimal ELF loader starts `init` from the boot image (the loader and `init` from the embedded image are done in S3b; process/VMO objects as handles are S3c)
- [ ] Process death releases everything (leak check)

### S4: First interactive system

Acceptance: a scripted shell session passes (`ls`, `cat`, `echo`, a pipeline);
Ctrl-C kills the foreground job.

- [ ] Launcher and startup message
- [ ] Memory filesystem server
- [ ] Console server; shell with builtins and external commands
- [ ] Pipes through the socket object

### S5: SMP

Acceptance: every earlier suite passes with `-smp 4` in normal and icount modes; a
long stress run reports no lock-order violations and no leaks.

- [ ] Secondary harts via SBI HSM; per-hart state and run queues
- [ ] IPIs and remote TLB shootdown
- [ ] Big kernel lock first, then per-object locks on the hot paths
- [ ] Lock-order checker in debug builds; stress suite

### S6: Storage drivers

Acceptance: a pattern written to the virtual disk reads back identical; the block
driver can be killed and restarted.

- [ ] Interrupt objects and device-memory VMOs
- [ ] Driver manager and device-tree parsing
- [ ] Userspace virtio-blk driver and block protocol

### S7: libc and ported software

Acceptance: BASIC goldens pass as a userland program; a Lua test script passes.

- [ ] C libc subset; threads on futexes
- [ ] `basic` crate as a userland program over a stdio-backed `Io`
- [ ] Lua port

### S8: Network, lower layers

Acceptance: the guest pings its gateway; UDP echo works through `hostfwd`.

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

Longer-term backlog (see `OS-ARCHITECTURE.md` section 16): a real on-disk
filesystem with crash-injection tests, a 9P bridge, graphics and input, scheduler
alternatives against the conformance suite, capability revocation, quotas,
self-hosting.
