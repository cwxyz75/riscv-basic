# Roadmap

The task tracker. Tick a box only when its acceptance criterion has been
**verified** (for kernel work, that means booted in QEMU, not merely compiled).

M3 (interpreter, host-only) is independent of M0-M2 (kernel) and can proceed in
parallel, for example in a cloud session while the kernel work happens locally.

There are two tracks. **M0-M8 are the BASIC machine** (done except the optional
M7). **M9 and the L-series add LOGO** as a second language, chosen at a boot
menu; the choice is permanent for that boot, and switching means rebooting.
The language spec for the new track is `LOGO.md`.

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

## M7: Persistence (optional)

- [ ] Choose pflash "cartridge" or virtio-blk
- [ ] `SAVE`, `LOAD`, `DIR`

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

- [ ] JIT smoke test: emit `li a0, 42; ret` into RAM, `fence.i`, call it, check the result
- [ ] RV64 emitter (integer and double instructions, labels, calling convention)
- [ ] Runtime-call table; trap handler dumps the faulting address and the generated bytes
- [ ] Value representation (NaN-boxing) and arena allocation

### L5: Compile every statement

Acceptance: differential suite green under QEMU; every instruction line and
every `to` definition is compiled before it runs.

- [ ] REPL line: compile, run, discard the code buffer
- [ ] `TO ... END`: compile into a per-procedure buffer, late-bound through a table
- [ ] `RUN` compiles the list it is given
- [ ] Differential suite: interpreter vs compiled, transcripts and bitmaps, on the same corpus

### L6: LOGO showpiece

- [ ] Classic programs as golden images (spirals, a tree, the Koch snowflake)
- [ ] README screenshot or recording; tag `v0.2`

## Spikes (time-boxed experiments, do early)

- [x] Float formatting without surprises: `core::fmt` with `{:.8e}` rounds exactly, and the goldens plus `basic_test.py` agree on the host and under QEMU
- [x] Ctrl-C delivery over the QEMU serial port (0x03 arrives as a byte, also while a program runs; verified with piped input, not yet with an interactive terminal)
- [x] Allocator choice (D10): the `linked_list_allocator` crate (a bump allocator would leak on every statement)
- [x] `ramfb` on the RISC-V `virt` board with `-display cocoa` (L0): works, see `DESIGN.md`
- [ ] Can generated code run at all: emit, `fence.i`, jump, under QEMU (L4)
- [ ] Deciding how far to take NaN-boxing without a garbage collector (L4)
