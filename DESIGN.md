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
| `0x1010_0000` | fw_cfg (QEMU's firmware-configuration device; how `ramfb` is set up) |
| `0x2000_0000` | pflash (candidate "cartridge" storage) |
| `0x8000_0000` | RAM base (128 MiB default); OpenSBI occupies the first 2 MiB |
| `0x8020_0000` | Kernel: `.text`, `.rodata`, `.data`, `.bss`, then a 4 MiB stack (the LOGO interpreter recurses) |

Proposed layout above the kernel: a fixed **32 KiB BASIC program area** (so the
banner's `BYTES FREE` is an honest, stable number) and a separate heap arena for
strings and arrays. Not yet decided; see D10.

## Boot flow

1. QEMU loads OpenSBI and our ELF; OpenSBI initialises the hart and jumps to
   `_start` in S-mode with `a0` = hart id, `a1` = device-tree pointer.
2. `_start` sets the stack, zeroes `.bss`, calls `kmain`.
3. `kmain` enables the FPU, installs the trap vector, initialises the UART and
   the heap, starts the 10 ms timer tick, then shows the boot menu (one keypress:
   `1` BASIC, `2` LOGO, not installed yet). For BASIC it creates the interpreter,
   prints the banner and runs the session loop forever; for LOGO it brings up the
   framebuffer (or, without a `ramfb` device, a canvas nobody sees) and runs the
   LOGO session loop. `BYE` cold-reboots the
   machine through SBI, which brings the menu back.
4. Other harts stay parked in OpenSBI; we ignore them.

## Crates

- **`basic`**: the interpreter. `no_std` + `alloc`, `forbid(unsafe_code)`, no
  hardware knowledge. Its only window to the machine is the `Io` trait
  (`write_str`, `read_line`, `break_pressed`, `millis`, later screen and
  PEEK/POKE primitives). Tests implement `Io` with in-memory buffers.
- **`cartfs`**: the disk format behind `SAVE`/`LOAD` (see "The disk"): header,
  free-sector map, fixed directory and chained sectors over a `BlockDevice`
  trait, with an in-memory disk for tests. `no_std`, `forbid(unsafe_code)`.
- **`console`**: the pure, hardware-free half of the console: the line editor
  (a byte-at-a-time state machine) and the ANSI escape helpers. `no_std`,
  `forbid(unsafe_code)`, unit-tested on the host.
- **`turtle`**: turtle graphics for LOGO: a `Canvas` trait, a Bresenham
  rasteriser and flood fill, an in-memory `Bitmap` for tests, and the `Turtle`
  (position, heading, pen, wrap at the edges, an XOR sprite). `no_std`,
  `forbid(unsafe_code)`, tested on the host against golden images; the kernel
  supplies a `Canvas` over the framebuffer.
- **`logo`**: the LOGO language: reader, arity-driven parser, a reference
  AST interpreter with dynamic scope, a structured IR (lowering plus an IR
  interpreter that must behave identically), and the RV64 code generator with
  its NaN-boxed runtime (`logo::jit`). The compiled engine is the default on
  the machine; the other two are its oracles. `no_std`, `forbid(unsafe_code)`; text and
  time through its own `Io` trait, the screen through `turtle::Canvas`; tested
  on the host with golden transcripts and golden images.
- **`rv64`**: a small RV64 assembler for the LOGO JIT (RV64I, M multiply, D
  double-precision, `fence.i`; labels with far-branch relaxation; a `li` that
  loads any 64-bit constant) and an emulator for it. `no_std`,
  `forbid(unsafe_code)`. Encodings are checked against LLVM's assembler; `li`,
  branches, FPU, memory and host calls by running the output in the emulator,
  which also runs the LOGO code generator's output in host tests.
- **`kernel`**: everything hardware. Implements `Io` over the UART, owns line
  editing, traps, timer, allocator, shutdown.

## The disk (persistence track, M7)

`SAVE`/`LOAD` use one shared disk image, `disk.img`, attached to QEMU as a
virtio block device (D21). Both languages see the same disk; each file is
tagged with the language it belongs to, and a name is unique across the disk.
The format is an old-school floppy (D22), implemented by the pure `cartfs`
crate over a `BlockDevice` trait, so it is host-tested against an in-memory disk
and the kernel only has to supply sector reads and writes.

```text
sector 0      header: "RVDISK01", total sectors u32, directory entries u32 (64)
sector 1      free map: bit n (LSB first) set = sector n in use
sectors 2-5   directory: 64 entries of 32 bytes
sectors 6..   data: next-sector u16 (0 = last) + 510 payload bytes
entry         name[16] NUL padded | kind u8 (0 free, 1 BASIC, 2 LOGO) | 0 |
              first sector u16 | length u32 | 8 zero bytes   (all little-endian)
```

The standard disk is 2 MiB (4096 sectors, about 2 MB of payload; at most 4096
sectors because the map is one sector). A file is at most 64 KiB. Names are 1-16
characters from `A-Z 0-9 . - _`, case-insensitive, stored upper-case. Files
are plain text: BASIC saves its `LIST` output, LOGO the text of its procedure
definitions. Saving over an existing name of the same kind replaces it (in
place in the directory, its old sectors reusable by the new copy); a name of the
other kind is a type mismatch. Writes go data, then map, then directory entry.
There is no journal: a power cut mid-save can lose that one file, as on a real
floppy.

## Console model

The serial terminal is the monitor. The line editor (logic in the `console`
crate, driven by the kernel over the UART) owns echo, backspace (`0x7f`/`0x08`,
echoed as `BS SP BS`), Ctrl-U, and Ctrl-C (echoed as `^C`, becoming
`Line::Break`); it also swallows arrow-key escape sequences, ignores non-ASCII
bytes, treats CR LF as one Enter, and holds at most 255 characters. The
interpreter only ever sees finished lines. Screen commands (`CLS`, `COLOR`,
`LOCATE`) become `Io` methods whose default implementations emit ANSI escapes.

## Graphics (LOGO track, spike L0)

The screen is QEMU's `ramfb` device: a framebuffer that lives in *our* RAM,
which QEMU reads. It is a plain system-bus device (`-device ramfb`; no PCI, no
virtio). To switch it on the kernel finds `etc/ramfb` in the `fw_cfg` file
directory and writes a 28-byte config block to it with an `fw_cfg` DMA transfer:
the framebuffer's address, the pixel format (`XR24`, i.e. `0x00RRGGBB` per
32-bit pixel), the width and height, and the stride. Resolution is **640x480**
(decision D19), a 1.2 MiB static array in `.bss`.

What worked and what bit us:

- Every `fw_cfg` field (selector, DMA descriptor, config block) is **big-endian**.
- The config block must be exactly **28 bytes**. A plain `repr(C)` struct is
  padded to 32 by its `u64`, and the DMA transfer is then rejected, so the
  struct is `repr(C, packed)`.
- Without `-device ramfb` the file `etc/ramfb` does not exist; `gfx::init`
  reports that instead of faulting.
- Tests need no window: the QEMU monitor command `screendump FILE` (over a
  loopback TCP monitor, because a macOS UNIX-socket path is limited to 104 bytes)
  writes a PPM of the current screen, and `scripts/logo_test.py` draws with LOGO and
  checks the screenshot pixel by pixel. `make run-logo` opens a real window;
  press `2` at the menu for LOGO.

The turtle crate draws through a `Canvas` of palette indices; the kernel's
`FbCanvas` keeps the indices in a shadow array (so fill and the XOR sprite can
read the picture back) and writes the palette colour to the framebuffer.

## Running generated code (LOGO JIT, spike L4)

Generated code is machine code in ordinary heap memory: paging is off and all of
RAM is executable, so the kernel copies the code into a `Vec<u32>` (so it is
4-aligned), executes `fence.i` so instruction fetch sees it, and calls it as a C
function. All of that, and nothing else unsafe, is in `kernel/src/jit.rs`. The
address range of the latest code is remembered so that a fatal trap inside
generated code prints a dump of every instruction word with the faulting one
marked. Two debugging keys at the boot menu exercise this: `3` runs JIT
self-tests (a constant, FPU arithmetic, a loop, branch conditions, 64-bit
tags) and `4` faults on purpose to show the dump.

### Compiled LOGO

Every LOGO line is lowered to the IR and compiled to an RV64 function before it
runs (`logo/src/jit/`): number literals, negation, `+ - *` on numbers with a
finiteness check, `Discard`, `repeat` and `if` are generated inline; everything
else is a call through a table of nine runtime services in the context block
(`ctx`), whose implementations are thin wrappers over the interpreters' own
functions, so every error message is shared. Values are NaN-boxed (numbers are
their bits; words and lists are arena handles under tags `0xFFF9`/`0xFFFA`;
*nothing* carries its producer's name). The kernel's `jit::execute` is the only
unsafe piece: it loads the code, builds `ctx` with `extern "C"` thunks and calls
it. The same code generator output also runs in the `rv64` emulator on the host,
and the whole golden corpus must agree across the AST interpreter, the IR
interpreter, the emulated compiled engine, and (on the real machine) the
compiled engine against the interpreter, down to the pixels.

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
| D10 | Memory: a fixed 32 KiB program area (a stored line costs its tokens + 5 bytes; `?OUT OF MEMORY` when full) plus a 16 MiB `alloc` heap for strings, variables and the interpreter's own data, managed by the `linked_list_allocator` crate | Decided |
| D11 | Makefile (not `just`/`xtask`) to keep prerequisites minimal | Decided |
| D12 | `PEEK`/`POKE` are raw and unrestricted, reaching any physical address through `Io`; a fault is a kernel crash (authentic, and no safety window to design) | Decided |
| D13 | Language semantics settled in `BASIC.md`: full-length variable names, left-associative `^`, 32-bit logical operators, no `ELSE`, 14-column `PRINT` zones, `TIMER`, 255-char lines, 9-significant-digit number output | Decided |
| D14 | Line editing lives in a separate pure `console` crate (host-testable), not in the kernel; the kernel only moves bytes between the UART and it | Decided |
| D15 | Traps: one entry point saves all integer and floating-point registers; a supervisor timer interrupt (10 ms tick, SBI `set_timer`) returns, every other trap is fatal and reports `scause`/`sepc`/`stval` before powering off. The `time` counter is assumed to run at QEMU virt's 10 MHz (a constant in `timer.rs`, not read from the device tree) | Decided |
| D16 | LOGO's JIT is a hand-written RV64 emitter (template code generation, stack-slot values, no register allocator), not Cranelift: the generated code is simple, the work is in the runtime, a dependency tree of unknown `no_std` bare-metal behaviour is not worth it, and a small emitter keeps the project readable and host-testable. The IR stays backend-neutral so a second backend could be added | Decided |
| D17 | LOGO language semantics settled in `LOGO.md`: a UCBLogo subset, separate number and word types, 640x480 with `wrap`, a minimal list set, UCBLogo unary-minus rule, arena memory with no GC, an XOR triangle turtle | Decided |
| D18 | The language is chosen at boot from a one-key menu and cannot be switched while running; changing language means rebooting (`BYE` in BASIC does a cold reboot through the SBI System Reset extension). One language's state never has to coexist with the other's | Decided |
| D19 | Graphics: QEMU `ramfb` (a framebuffer in our RAM, set up through `fw_cfg` DMA), 640x480, 32-bit XRGB; headless tests take screenshots with the monitor's `screendump` | Decided |
| D20 | LOGO runs as compiled RV64 by default; the AST and IR interpreters are kept as oracles (`.engine "ast`/`"ir`/`"jit` chooses). Generated code reaches the interpreter only through nine runtime services, whose implementations reuse the interpreter's own functions; values are NaN-boxed with a per-run arena | Decided |
| D21 | Persistence is a virtio-mmio block device (`disk.img`, polled, no interrupts), not pflash: sector reads and writes map directly onto the format, the image is an ordinary file the host can create and inspect, and no flash erase/program protocol is needed | Decided |
| D22 | The disk format is a fixed-directory floppy: header, free-sector map, 64 directory entries, chained data sectors, in the pure `cartfs` crate. One disk shared by both languages, files tagged by language, plain-text contents | Decided |
