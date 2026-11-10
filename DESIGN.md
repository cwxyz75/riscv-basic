# Design

## Goals and non-goals

**Goals**
- Boot QEMU's RISC-V `virt` machine straight to a `READY.` prompt; BASIC is the shell.
- The 80s feel: banner, `BYTES FREE`, line-numbered programs, terse `?SYNTAX ERROR`s.
- Small enough that one person can read all of it.
- Every piece testable, the interpreter on the host without any emulator.

**Non-goals**
- Multitasking, processes, user mode, virtual memory/paging, SMP.
- A real filesystem (until the optional persistence milestone), networking.
- Real hardware, graphics beyond ANSI text, POSIX/Unix compatibility.
- Binary size. (An 8 KB ROM was the 80s constraint; we don't have it.)

## Platform

- QEMU `virt`, RV64GC, one hart in use. OpenSBI (`-bios default`) runs in
  M-mode and hands control to our kernel in S-mode at `0x8020_0000`.
- `satp = 0` (bare): physical addresses everywhere, no page tables.
- Rust stable, target `riscv64gc-unknown-none-elf`. Hardware double-precision
  floats; `sstatus.FS` must be enabled before any float code runs.

## Memory map (QEMU `virt`)

| Address | What |
|---|---|
| `0x0010_0000` | Test/finisher device (used by OpenSBI for power-off) |
| `0x0200_0000` | CLINT (timer; we use it through SBI, not directly) |
| `0x0c00_0000` | PLIC (interrupt controller; unused until interrupts matter) |
| `0x1000_0000` | NS16550A UART0, our console |
| `0x1000_1000+` | virtio-mmio slots (only if we do virtio-blk) |
| `0x2000_0000` | pflash (candidate "cartridge" storage) |
| `0x8000_0000` | RAM base (128 MiB default); OpenSBI occupies the first 2 MiB |
| `0x8020_0000` | Kernel: `.text`, `.rodata`, `.data`, `.bss`, then a 64 KiB stack |

Proposed layout above the kernel: a fixed **32 KiB BASIC program area** (so the
banner's `BYTES FREE` is an honest, stable number) and a separate heap arena for
strings and arrays. Not yet decided; see D10.

## Boot flow

1. QEMU loads OpenSBI and our ELF; OpenSBI initialises the hart and jumps to
   `_start` in S-mode with `a0` = hart id, `a1` = device-tree pointer.
2. `_start` sets the stack, zeroes `.bss`, calls `kmain`.
3. `kmain` (from M2 on) installs the trap vector, enables the FPU, and
   initialises the heap. From M4 on it creates the interpreter, prints the
   banner and runs the session loop.
4. Other harts stay parked in OpenSBI; we ignore them.

## Crates

- **`basic`**: the interpreter. `no_std` + `alloc`, `forbid(unsafe_code)`, no
  hardware knowledge. Its only window to the machine is the `Io` trait
  (`write_str`, `read_line`, `break_pressed`, `millis`, later screen and
  PEEK/POKE primitives). Tests implement `Io` with in-memory buffers.
- **`kernel`**: everything hardware. Implements `Io` over the UART, owns line
  editing, traps, timer, allocator, shutdown.

## Console model

The serial terminal is the monitor. The kernel's line editor owns echo,
backspace (`0x7f`/`0x08`), Ctrl-U, and Ctrl-C (which becomes `Line::Break`);
the interpreter only ever sees finished lines. Screen commands (`CLS`, `COLOR`,
`LOCATE`) become `Io` methods whose default implementations emit ANSI escapes.

## Interpreter shape (proposed)

- Program lines stored **tokenized** (keywords become single bytes), sorted by
  line number; `LIST` detokenizes. Authentic and compact, a bit more work than
  storing text.
- Values: `f64` and strings. Errors are one enum that maps to the classic
  messages (`?SYNTAX ERROR IN 20`); user input must never cause a Rust panic.
- `RUN` polls `Io::break_pressed()` between statements.

## Error and panic policy

A kernel panic prints the message and powers off through SBI. The interpreter
reports every problem as a BASIC error and returns to `READY.`.

## Testing layers

1. **Host unit tests** (`cargo test -p basic`): fast, no emulator.
2. **Golden tests** (`basic/tests/golden/*.bas` + `.out`): a program is typed
   at the prompt, the full transcript must match. `BLESS=1` regenerates.
3. **Boot tests** (`scripts/boot_test.py`): boot the real kernel in QEMU, feed
   scripted input to the serial port, check the output.
4. **CI** runs 1 and 2 on every push, and 3 in a QEMU-equipped job.

## Decision log

| # | Decision | Status |
|---|---|---|
| D1 | Rust (stable), `no_std` | Decided |
| D2 | RV64GC on QEMU `virt`, OpenSBI firmware, kernel in S-mode | Decided |
| D3 | No paging, user mode or processes | Decided |
| D4 | Numbers are `f64` (hardware FP; `libm` for transcendental functions) | Decided |
| D5 | The terminal is the monitor; ANSI escapes for screen control | Decided |
| D6 | Binary size is not a goal | Decided |
| D7 | Console is direct 16550 MMIO from M0; SBI is used for shutdown and timer (revised from an earlier plan to bootstrap via SBI putchar) | Decided |
| D8 | `basic` is `forbid(unsafe_code)`; hardware access only through `Io` | Proposed |
| D9 | Tokenized program store (packed bytes, keywords as single-byte tokens; `LIST` detokenizes) | Decided |
| D10 | Memory: fixed 32 KiB program area plus an `alloc`-backed heap; which allocator is open | Open |
| D11 | Makefile (not `just`/`xtask`) to keep prerequisites minimal | Decided |
| D12 | `PEEK`/`POKE` are raw and unrestricted, reaching any physical address through `Io`; a fault is a kernel crash (authentic, and no safety window to design) | Decided |
| D13 | Language semantics settled in `BASIC.md`: full-length variable names, left-associative `^`, 32-bit logical operators, no `ELSE`, 14-column `PRINT` zones, `TIMER`, 255-char lines, 9-significant-digit number output | Decided |
