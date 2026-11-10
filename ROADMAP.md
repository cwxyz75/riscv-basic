# Roadmap

The task tracker. Tick a box only when its acceptance criterion has been
**verified** (for kernel work, that means booted in QEMU, not merely compiled).

M3 (interpreter, host-only) is independent of M0-M2 (kernel) and can proceed in
parallel, for example in a cloud session while the kernel work happens locally.

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

- [ ] Trap vector (`stvec`) and a handler that reports the cause
- [ ] Timer through the SBI TIME extension; `millis()` source
- [ ] Enable the FPU (`sstatus.FS`)

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

- [ ] Kernel depends on `basic`; global allocator
- [ ] Kernel `Io` over the UART; banner with a real `BYTES FREE`
- [ ] Session loop runs on boot

## M5: Language completion

Acceptance: golden tests for every statement and function in `BASIC.md`.

- [ ] `GOSUB/RETURN`, `ON ... GOTO/GOSUB`
- [ ] `DIM` and arrays; strings and string functions
- [ ] `DATA/READ/RESTORE`, `INPUT`
- [ ] Math functions via `libm`; `RND`
- [ ] Complete error set with line numbers

## M6: Screen and feel

- [ ] `CLS`, `COLOR`, `LOCATE`; BREAK handling during `RUN`
- [ ] `TIMER`; `PEEK`/`POKE` per the settled policy

## M7: Persistence (optional)

- [ ] Choose pflash "cartridge" or virtio-blk
- [ ] `SAVE`, `LOAD`, `DIR`

## M8: Showpiece

- [ ] Demo programs as golden tests (guess-the-number, 99 bottles, Hunt the Wumpus)
- [ ] README with a recorded session; tag `v0.1`

## Spikes (time-boxed experiments, do early)

- [ ] Float formatting without surprises (`core::fmt` versus hand-rolled)
- [ ] Ctrl-C delivery over the QEMU serial port
- [ ] Allocator choice (D10): bump versus free-list versus the `linked_list_allocator` crate
