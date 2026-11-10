# riscv-basic

A toy "80s home computer" for QEMU's RISC-V `virt` board: it boots straight into
a BASIC prompt, and BASIC is the shell. Rust, `no_std`, running in S-mode on top
of OpenSBI. Built for amusement.

```
RISCV BASIC V0.1
32768 BYTES FREE

READY.
10 FOR I=1 TO 3
20 PRINT "HELLO";I
30 NEXT I
RUN
```

**Status:** the interpreter core (M3) works on the host and the kernel boots to
a line-edited serial console (M0, M1). Wiring them together is next. See
[ROADMAP.md](ROADMAP.md).

## Prerequisites (macOS)

```sh
brew install qemu
rustup toolchain install     # reads rust-toolchain.toml: toolchain, components, riscv64 target
```

## Commands

```sh
make test        # host tests for the interpreter and console crates (no QEMU needed)
make kernel      # build the kernel ELF
make run         # boot in QEMU (Ctrl-A then X to quit)
make debug       # boot paused, gdb stub on :1234
make boot-test   # non-interactive boot check, as CI runs it
make check       # everything CI runs
```

## Layout

| Path | What |
|---|---|
| `basic/` | The interpreter. `no_std`, no hardware, no `unsafe`; tested on the host. |
| `console/` | Line editor and ANSI helpers. `no_std`, no hardware, no `unsafe`; tested on the host. |
| `kernel/` | Boot code, UART driver, traps. Hosts the interpreter. |
| `scripts/` | Test helpers (`boot_test.py`). |
| `DESIGN.md` | Architecture, memory map, decision log. |
| `BASIC.md` | The language spec, including open questions to settle. |
| `ROADMAP.md` | Milestones with acceptance criteria; the task tracker. |
| `CLAUDE.md` | Working agreement for AI coding sessions. |
