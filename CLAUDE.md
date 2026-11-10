# CLAUDE.md

Project: a toy 80s-style BASIC computer for QEMU RISC-V (`virt`), in Rust. Read
`ROADMAP.md` (what's next), `DESIGN.md` (how it fits together) and `BASIC.md`
(the language) before starting work.

## Commands

- `make test`: host tests for `basic/` and `console/`. Fast; run constantly.
- `make kernel` / `make run` / `make debug`: build / boot / boot-paused-for-gdb.
- `make boot-test`: non-interactive QEMU boot check. Needs `qemu-system-riscv64`.
- `make check`: fmt, tests, clippy, boot test. Run before calling anything done.
- Kernel cargo commands must run from inside `kernel/` (it has its own
  `.cargo/config.toml` that sets the target). The Makefile handles this.

## Layout and boundaries

- `basic/` is `no_std` + `alloc`, `#![forbid(unsafe_code)]`, and knows nothing
  about hardware. All machine access goes through the `Io` trait. If you feel
  the need for `unsafe` or an address in `basic/`, add a method to `Io` instead.
- `console/` is the pure half of the console (line editor, ANSI helpers):
  `no_std`, `#![forbid(unsafe_code)]`, host-tested, no hardware access.
- `kernel/` owns everything hardware: boot, UART, traps, timer,
  allocator. Every `unsafe` block gets a `// SAFETY:` comment saying why it holds.
- No paging, no user mode, no processes. These are non-goals (see `DESIGN.md`).

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
