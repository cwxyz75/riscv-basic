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
| `0x8020_0000` | Kernel: `.text`, `.rodata`, `.data`, `.bss`, then a 64 KiB stack |

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
   prints the banner and runs the session loop forever. `BYE` cold-reboots the
   machine through SBI, which brings the menu back.
4. Other harts stay parked in OpenSBI; we ignore them.

## Crates

- **`basic`**: the interpreter. `no_std` + `alloc`, `forbid(unsafe_code)`, no
  hardware knowledge. Its only window to the machine is the `Io` trait
  (`write_str`, `read_line`, `break_pressed`, `millis`, later screen and
  PEEK/POKE primitives). Tests implement `Io` with in-memory buffers.
- **`console`**: the pure, hardware-free half of the console: the line editor
  (a byte-at-a-time state machine) and the ANSI escape helpers. `no_std`,
  `forbid(unsafe_code)`, unit-tested on the host.
- **`turtle`**: turtle graphics for LOGO: a `Canvas` trait, a Bresenham
  rasteriser and flood fill, an in-memory `Bitmap` for tests, and the `Turtle`
  (position, heading, pen, wrap at the edges, an XOR sprite). `no_std`,
  `forbid(unsafe_code)`, tested on the host against golden images; the kernel
  supplies a `Canvas` over the framebuffer.
- **`kernel`**: everything hardware. Implements `Io` over the UART, owns line
  editing, traps, timer, allocator, shutdown.

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
  writes a PPM of the current screen, and `scripts/fb_test.py` checks colour bars
  and a grey ramp pixel by pixel. `make run-logo` opens a real window; press `2`
  at the menu to draw the test pattern.

The turtle crate draws through a `Canvas` of palette indices; the kernel's
`FbCanvas` keeps the indices in a shadow array (so fill and the XOR sprite can
read the picture back) and writes the palette colour to the framebuffer.

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
