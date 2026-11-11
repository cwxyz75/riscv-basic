# CLAUDE.md

Project: a toy RISC-V computer for QEMU (`virt`), in Rust, with two tracks:

- **The home computer** (BASIC and LOGO behind a boot menu): finished, v0.2. It
  lives in `kernel/` and the language crates. Read `ROADMAP.md` (M and L series),
  `DESIGN.md`, `BASIC.md` and `LOGO.md` before working on it.
- **The OS track** (S-series): a multicore capability microkernel with userspace
  servers, a network stack and a POSIX-ish layer, written entirely by AI with the
  human as product owner. Spec: `OS-ARCHITECTURE.md`. Read it, plus the S-series in
  `ROADMAP.md`, before working on it. S0 (foundation), S1 (memory) and S2 (threads and scheduling) are done; S3 (user mode, syscalls, handles) is next.

Work on one track must not change the other's contract (see "Layout and
boundaries").

## Commands

- `make test`: host tests for `basic/` and `console/`. Fast; run constantly.
- `make kernel` / `make run` / `make debug`: build / boot / boot-paused-for-gdb.
- `make boot-test`: non-interactive QEMU boot check. Needs `qemu-system-riscv64`.
- `make check`: fmt, tests, clippy, boot test. Run before calling anything done.
- OS track: `make os-kernel` / `make os-test` (host tests plus loom and shuttle) /
  `make os-boot-test` (headless QEMU, structured output) / `make os-miri` (needs
  nightly; CI runs it). `make check` covers all but Miri. OS-track cargo commands
  run from inside `os-kernel/`, like `kernel/`.
- Kernel cargo commands must run from inside `kernel/` (it has its own
  `.cargo/config.toml` that sets the target). The Makefile handles this.

## Layout and boundaries

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
  emulator for `li` and branches). Only `kernel/src/jit.rs` executes generated code.
- `turtle/` is the pure graphics half of LOGO (canvas trait, rasteriser, turtle):
  `no_std`, `#![forbid(unsafe_code)]`, host-tested against golden images.
- `examples/` holds the demo BASIC programs; golden tests load them with a
  `<LOAD examples/NAME.bas>` line and boot tests type them over the serial port.
- `cartfs/` is the disk format for SAVE/LOAD (directory, free map, chained
  sectors) over a `BlockDevice` trait: `no_std`, `forbid(unsafe_code)`, host-tested.
- `kernel/` owns everything hardware: boot, UART, virtio-blk, traps, timer,
  allocator. Every `unsafe` block gets a `// SAFETY:` comment saying why it holds.
- **Home-computer kernel only:** `kernel/` has no paging, no user mode, no
  processes and no SMP. These are its non-goals (see `DESIGN.md`) and stay that
  way. Do not grow `kernel/` toward the OS track. The OS track gets its own kernel
  binary and new crates (provisional names in `OS-ARCHITECTURE.md` section 16), and
  may reuse the pure crates (`basic`, `console`, `logo`, `turtle`, `rv64`,
  `cartfs`) without forking them.

## Working agreement

- One milestone (or a slice of one) per branch/PR. `main` always builds and boots.
- Interpreter behaviour is specified by `BASIC.md` and pinned by golden tests in
  `basic/tests/golden/`. Write the golden test first, watch it fail, then implement.
  Never run `BLESS=1` blindly: read the diff of the `.out` files.
- If `BASIC.md` is ambiguous or wrong, stop and ask. Don't silently decide a
  language semantic; settle it in the spec, then code it.
- Only tick a `ROADMAP.md` box when its acceptance criterion has actually been
  verified (for kernel work, that means booted in QEMU, not just compiled).
- Binary size is not a goal. Prefer simple over small; `core::fmt` is fine.
- Record real design choices in the decision log in `DESIGN.md`.
- If something can't be verified in your environment (e.g. no QEMU), say so
  plainly rather than assuming it works.

## OS track (S-series)

Everything above still applies (one milestone per PR, `main` always builds and
boots, `// SAFETY:` comments, `make check`). In addition:

- The spec is `OS-ARCHITECTURE.md`. Design decisions are logged in its section 17
  (OD numbers), not in `DESIGN.md`. Open questions are in its section 18: stop and
  ask rather than guessing.
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
  ownership) and `SYSCALLS.md` (every syscall: preconditions, rights, errors) are
  exist (skeletons from S0, filled in by each milestone). Every change preserves them, and the lock ordering is not negotiable.
- Everything must be diagnosable headlessly: structured serial output, QEMU's exit
  device for pass/fail. Debugging by eye is not an available tool.
- Tick an S-series box only when verified in QEMU. For SMP work that means both
  normal and icount modes; say plainly if only one was run.
- Always leave a bootable, green system. No big-bang rewrites.
- Small utility crates are allowed (the repo already uses `linked_list_allocator` and
  `libm`), each with its own OS decision-log entry. The kernel, network stack,
  filesystem and libc are written here, not imported.
