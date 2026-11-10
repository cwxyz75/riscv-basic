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

**Status:** planning baseline. The interpreter is a stub and the kernel is an
M0 hello-world that boots in QEMU. See [ROADMAP.md](ROADMAP.md).

## Prerequisites (macOS)

```sh
brew install qemu
rustup toolchain install     # reads rust-toolchain.toml: toolchain, components, riscv64 target
```

## Commands

```sh
make test        # host tests for the interpreter (no QEMU needed)
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
| `kernel/` | Boot code, UART console, traps. Hosts the interpreter. |
| `scripts/` | Test helpers (`boot_test.py`). |
| `DESIGN.md` | Architecture, memory map, decision log. |
| `BASIC.md` | The language spec, including open questions to settle. |
| `ROADMAP.md` | Milestones with acceptance criteria; the task tracker. |
| `CLAUDE.md` | Working agreement for AI coding sessions. |
