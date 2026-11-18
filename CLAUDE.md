# CLAUDE.md

Project: a toy RISC-V computer for QEMU (`virt`), in Rust, with two tracks:

- **The home computer** (BASIC and LOGO behind a boot menu): finished, v0.2. It
  lives in `home/` (its kernel is `home/kernel/`, plus the language crates). Read
  `home/DESIGN.md`, `home/BASIC.md` and `home/LOGO.md`
  before working on it.
- **The OS track** (S-series): a multicore capability microkernel with userspace
  servers, a network stack and a POSIX-ish layer, written entirely by AI with the
  human as product owner. Spec: `docs/specs/OS-ARCHITECTURE.md` (the documents are indexed in
  `docs/README.md`). Read it before working on it, then
  `WORK-PLAN.md` (what is next) and `work/README.md` (the work queue). What is left to do is
  in `work/`, one file per item, queried with `scripts/work.py`; never read the whole
  directory. How the system got here is in `docs/records/` and git history.

Work on one track must not change the other's contract (see "Layout and
boundaries").

## Commands

- `make test`: host tests for the home-computer crates (`home/basic/`, `home/console/`, ...). Fast; run constantly.
- `make home-kernel` / `make home-run` / `make home-debug`: build / boot / boot-paused-for-gdb.
- `make home-boot-test`: non-interactive QEMU boot check. Needs `qemu-system-riscv64`.
- `make check` (an alias of `make smoke`, tiers 0 and 1: fmt, tests, clippy, one boot of the suite at four harts, the home boot, the release smoke; about three minutes warm). `make soak` is the full matrix (on demand, tier 2); `make check-all` is both. The testing tiers and their rules are in `TESTING.md` (OD83).
- OS track: `make kernel` / `make os-test` (host tests plus loom and shuttle) /
  `make os-boot-test` (headless QEMU, structured output) / `make os-miri` (needs
  nightly; the soak workflow runs it). OS-track cargo commands
  run from inside `kernel/`, like `home/kernel/`.
- Kernel cargo commands must run from inside the kernel's own directory (each has its
  own `.cargo/config.toml` that sets the target). The Makefile handles this. The two
  kernels are different packages: `kernel` (OS track) and `home-kernel` (home computer).

## Layout and boundaries

The home computer lives under `home/` (all paths in the next block are relative to it);
the OS track's crates (`abi/`, `kcore/`, `rt/`, `init/`, `kernel/`) are at the top level.

- `basic/` is `no_std` + `alloc`, `#![forbid(unsafe_code)]`, and knows nothing
  about hardware. All machine access goes through the `Io` trait. If you feel
  the need for `unsafe` or an address in `basic/`, add a method to `Io` instead.
- `console/` is the pure half of the console (line editor, ANSI helpers):
  `no_std`, `#![forbid(unsafe_code)]`, host-tested, no hardware access.
- `logo/` is the LOGO interpreter (reader, parser, evaluator): `no_std`,
  `#![forbid(unsafe_code)]`, host-tested with golden transcripts and images; its
  only window to the machine is `logo::Io` and `turtle::Canvas`.
- `rv64/` is the RV64 assembler for the LOGO JIT: `no_std`,
  `#![forbid(unsafe_code)]`, host-tested (LLVM-derived encoding vectors, an
  emulator for `li` and branches). Only `home/kernel/src/jit.rs` executes generated code.
- `turtle/` is the pure graphics half of LOGO (canvas trait, rasteriser, turtle):
  `no_std`, `#![forbid(unsafe_code)]`, host-tested against golden images.
- `examples/` holds the demo BASIC programs; golden tests load them with a
  `<LOAD examples/NAME.bas>` line and boot tests type them over the serial port.
- `cartfs/` is the disk format for SAVE/LOAD (directory, free map, chained
  sectors) over a `BlockDevice` trait: `no_std`, `forbid(unsafe_code)`, host-tested.
- `home/kernel/` owns everything hardware: boot, UART, virtio-blk, traps, timer,
  allocator. Every `unsafe` block gets a `// SAFETY:` comment saying why it holds.
- **Home-computer kernel only:** `home/kernel/` has no paging, no user mode, no
  processes and no SMP. These are its non-goals (see `home/DESIGN.md`) and stay that
  way. Do not grow `home/kernel/` toward the OS track. The OS track gets its own kernel
  binary and new crates (provisional names in `OS-ARCHITECTURE.md` section 16), and
  may reuse the pure crates (`basic`, `console`, `logo`, `turtle`, `rv64`,
  `cartfs`) without forking them.

## Working agreement

- One milestone (or a slice of one) per branch/PR. `main` always builds and boots.
- Interpreter behaviour is specified by `home/BASIC.md` and pinned by golden tests in
  `home/basic/tests/golden/`. Write the golden test first, watch it fail, then implement.
  Never run `BLESS=1` blindly: read the diff of the `.out` files.
- **Test cost is the bottleneck, so spend it deliberately (tiers: `TESTING.md`).** While iterating run
  `make quick` or the one targeted test. The merge gate is tiers 0 and 1 (`make smoke`, which CI runs
  too); merge on a green gate. The soak tier (`make soak`, `soak.yml`: the full matrix, negative controls,
  Miri) is on demand, nightly and on the `soak` label, never required: run it by hand only for kernel,
  SMP, memory or security changes. Do not run the whole matrix before every push. Batch related slices
  into one PR (several commits); keep kernel/security slices that get a Fable review in their own small
  PR. Run mutation checks once per new test, not once per push. New tests go where `TESTING.md` says:
  logic on the host, syscall behaviour in the suite, only what the machine shows in QEMU.
- If `home/BASIC.md` is ambiguous or wrong, stop and ask. Don't silently decide a
  language semantic; settle it in the spec, then code it.
- Only close a work item (`scripts/work.py done`) when its acceptance criterion has actually
  been verified (for kernel work, that means booted in QEMU, not just compiled).
- **Documents come in three kinds** (`docs/README.md`): specs (`docs/specs/`: the end state,
  versioned; a change is a dated amendment appended at the end, never a rewrite of the body),
  reference (`docs/reference/`: SYSCALLS, INVARIANTS, TESTING, changed in place with the code)
  and records (`docs/records/`: reviews, decisions, measurements, notes; append-only).
- **Work lists and documentation are separate.** Plans, to-dos, progress and "not built yet"
  go in `work/` and `WORK-PLAN.md`, never in a spec. A spec describes the end state; it has
  no opinion about how far the build has got.
- Binary size is not a goal. Prefer simple over small; `core::fmt` is fine.
- Record real design choices in the decision log in `home/DESIGN.md`.
- If something can't be verified in your environment (e.g. no QEMU), say so
  plainly rather than assuming it works.

## OS track (S-series)

Everything above still applies (one milestone per PR, `main` always builds and
boots, `// SAFETY:` comments, `make check`, which is the smoke gate). In addition:

- The spec is `docs/specs/OS-ARCHITECTURE.md`. Design decisions are logged in
  `docs/records/decisions.md` (OD numbers, append-only), not in `home/DESIGN.md`. Open questions are `kind: decision` items
  (`scripts/work.py decisions`): stop and ask rather than guessing.
- All code is AI-written and the human is the product owner, so nothing relies on a
  human reading the code. Every milestone needs a **runnable acceptance check,
  written before the code**.
- Prefer **independent oracles** (upstream test suites, host tools, real clients,
  an independent reference model) to tests written by the same context that wrote
  the code. When a milestone is done, an independent review pass with fresh context
  attacks it against `INVARIANTS.md` and `SYSCALLS.md` before it merges.
- Core logic lives in host-testable `no_std`, `#![forbid(unsafe_code)]` crates behind
  narrow traits, tested with model-based and property tests; concurrency-sensitive
  logic is also run under loom/shuttle and Miri.
- `INVARIANTS.md` (handle unforgeability, rights monotonicity, lock ordering, memory
  ownership) and `SYSCALLS.md` (every syscall: preconditions, rights, errors) describe
  the system as it is. Every change preserves them, and the lock ordering is not negotiable.
- Architecture target: **RVA23** (OD47). The OS track does not support hardware below that
  profile; the home computer under `home/` keeps its rv64gc contract.
- **Shortcuts become tidy items in `work/`.** Any stand-in, ambient authority, deferred review
  finding or "good enough for now" gets an item (`kind: tidy`, `work.py next T` for its id) in
  the same PR (what, why acceptable, risk, proper fix and when). A question only the owner
  can answer is a `kind: decision` item. Fixed items are closed with `work.py done`.
- Everything must be diagnosable headlessly: structured serial output, QEMU's exit
  device for pass/fail. Debugging by eye is not an available tool.
- Close an OS-track item only when verified in QEMU. For SMP work that means the
  normal mode at `-smp 2` and `-smp 4`; icount runs only one hart (T21). Say plainly which was run.
- Always leave a bootable, green system. No big-bang rewrites.
- Small utility crates are allowed (the repo already uses `linked_list_allocator` and
  `libm`), each with its own OS decision-log entry. The kernel, network stack,
  filesystem and libc are written here, not imported.
