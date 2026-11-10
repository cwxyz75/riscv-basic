# riscv-basic

A toy "80s home computer" for QEMU's RISC-V `virt` board: it boots straight into
a BASIC prompt, and BASIC is the shell. Rust, `no_std`, running in S-mode on top
of OpenSBI. Built for amusement.

## A session

This is real output from the booted machine (captured by the boot tests'
harness, with the typed lines echoed by the console as you would see them),
after pressing `1` at the boot menu. `BYE` reboots back to the menu:

```text
RISCV BASIC V0.1
32768 BYTES FREE

READY.
10 FOR I=1 TO 3
READY.
20 PRINT "HELLO";I
READY.
30 NEXT I
READY.
RUN
HELLO 1 
HELLO 2 
HELLO 3 
READY.
LIST
10 FOR I=1 TO 3
20 PRINT "HELLO";I
30 NEXT I
READY.
PRINT SQR(2);SIN(1)
 1.41421356  0.841470985 
READY.
A$="RISC-V": PRINT LEFT$(A$,4);"/";RIGHT$(A$,1)
RISC/V
READY.
POKE 2280652800,65: PRINT PEEK(2280652800)
 65 
READY.
NEW
READY.
10 INPUT "NAME";N$: PRINT "HI, ";N$
READY.
RUN
NAME? WORLD
HI, WORLD
READY.
PRINT 1/0
?DIVISION BY ZERO ERROR
READY.
```

## What it is

- **A BASIC in the Microsoft/Commodore tradition.** Line-numbered programs
  (stored tokenized, `LIST` detokenizes), immediate mode, `RUN`, `GOTO`,
  `GOSUB`/`RETURN`, `ON ... GOTO/GOSUB`, `IF ... THEN`, `FOR`/`NEXT` with `STEP`,
  `DIM` and arrays (up to 3 dimensions), `DATA`/`READ`/`RESTORE`, `INPUT`,
  numbers (`f64`) and strings, the usual maths and string functions, `RND`,
  `TIMER`, `PEEK`/`POKE` on real physical addresses, `CLS`/`COLOR`/`LOCATE`
  (ANSI escapes), `TAB`/`SPC`, and Ctrl-C to stop a running program.
  Errors look like `?SYNTAX ERROR IN 20`. The language is specified in
  [BASIC.md](BASIC.md).
- **A small kernel under it.** Boot in S-mode, 16550 UART console with line
  editing (backspace, Ctrl-U, Ctrl-C, arrow keys ignored), traps with a fatal
  report of `scause`/`sepc`/`stval`, a 10 ms timer tick, the FPU, and a heap.
  No paging, no processes: it is a home computer, not an OS. See
  [DESIGN.md](DESIGN.md).
- **Tested without hardware.** The interpreter runs on the host under
  `cargo test` against golden transcripts (`basic/tests/golden/`); QEMU boot
  tests type scripted sessions into the real kernel, including the demo
  programs below.

## Try it (macOS)

```sh
brew install qemu
rustup toolchain install     # reads rust-toolchain.toml: toolchain, components, riscv64 target
make run                     # boots to a language menu: press 1 for BASIC (Ctrl-A then X quits QEMU)
```

Other commands:

```sh
make test        # host tests for the interpreter and console crates (no QEMU needed)
make kernel      # build the kernel ELF
make run-logo     # boot with a graphics window; press 2 at the menu for the LOGO test pattern
make debug       # boot paused, gdb stub on :1234
make boot-test   # the scripted QEMU boot tests, as CI runs them
make check       # everything CI runs: fmt, tests, clippy, boot tests
```

## Programs to try

[`examples/`](examples) holds three programs. After `make run`, paste one into
the terminal (typed-ahead text is kept while BASIC is busy), then type `RUN`:

| Program | What it shows |
|---|---|
| [`guess.bas`](examples/guess.bas) | `INPUT`, `RND`, `IF`, `GOTO`, `STR$` |
| [`bottles.bas`](examples/bottles.bas) | `FOR ... STEP -1`, `GOSUB`, string functions |
| [`wumpus.bas`](examples/wumpus.bas) | arrays, `DATA`/`READ`, `GOSUB`, `INPUT`, a 20-room cave |

There is no `SAVE`/`LOAD` yet (the optional persistence milestone): programs
live only until the machine is switched off.

## Layout

| Path | What |
|---|---|
| `basic/` | The interpreter. `no_std`, no hardware, no `unsafe`; tested on the host. |
| `console/` | Line editor and ANSI helpers. `no_std`, no hardware, no `unsafe`; tested on the host. |
| `kernel/` | Boot code, UART driver, traps, timer, heap. Hosts the interpreter. |
| `examples/` | Demo BASIC programs (also used by the golden and boot tests). |
| `scripts/` | The QEMU boot tests. |
| `DESIGN.md` | Architecture, memory map, decision log. |
| `BASIC.md` | The language spec. |
| `ROADMAP.md` | Milestones with acceptance criteria; the task tracker. |
| `CLAUDE.md` | Working agreement for AI coding sessions. |

## Limits

Strings are ASCII and at most 255 characters; the program area is 32768 bytes
(a line costs its tokens plus 5); all arrays together hold at most 40000
elements; `PEEK`/`POKE` are unchecked on purpose, so a bad `POKE` can corrupt
the machine and a bad address crashes it with a trap report, just as it should.
