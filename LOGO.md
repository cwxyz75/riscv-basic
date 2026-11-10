# LOGO language spec (draft v0.1)

The second language of the machine, chosen at boot (see `ROADMAP.md`, M9).
Like `BASIC.md`, this is the contract the implementation is built and tested
against. Items marked **(open)** are proposals waiting for a decision; settle
them here before coding them.

## Why LOGO, and what "compiled" means here

LOGO is Lisp without the parentheses: code is data. `repeat 4 [fd 100 rt 90]`
takes a list that is really a program, and `run` executes any list at runtime.
So the compiler has to exist at runtime anyway, and the design goal is:

> Every top-level instruction line, and every `to ... end` definition, is
> compiled to RV64 machine code before it runs.

The same approach as an SBCL-style REPL: read a form, compile it, run it,
discard it (procedures keep their code).

## Pipeline

```
text --reader--> lists (words, numbers, lists)
     --parser--> AST (arity-driven prefix parsing + infix operators)
     --lowering--> IR
     --backend--> RV64 machine code, jumped to after `fence.i`
```

Two independent implementations of the same semantics keep us honest:

1. **Reference interpreter** over the AST (milestone L2). Runs on the host, so
   it is tested with golden transcripts and golden images under `cargo test`.
2. **Compiler** (L3-L5). Its generated code only runs on RISC-V, so it is
   tested **differentially**: the same programs run through the interpreter and
   the compiled path under QEMU, and transcripts and rendered bitmaps must match.

An IR interpreter on the host (L3) is a cheap middle oracle between the two.

## Session

After the menu, the screen shows a banner and a `?` prompt (as in UCBLogo).
Typing happens in the serial terminal; the turtle draws in the QEMU window.
`BYE` reboots to the boot menu.

## Values

- Number: `f64`. Word: a string, written `"word`. List: `[a b [c d]]`.
- Whether numbers are also words, as in UCBLogo, or a separate type: **(open;
  leaning towards separate types for the first version)**.
- Booleans are the words `true` and `false`.

## Syntax

- Procedures have fixed arity, so `fd 50 + 10` parses by consuming one input.
- Infix operators: `+ - * / = < >`, with `*` `/` above `+` `-` above the
  comparisons. Unary minus follows UCBLogo's rules **(open: how closely)**.
- `:name` is a variable reference, `"word` a quoted word, `[...]` a list.
- Definitions: `to name :a :b ... end`.
- Scoping is **dynamic**, as in classic LOGO.

## Turtle model

- Canvas: 640x480 pixels, origin at the centre, y up **(open: resolution)**.
- Heading 0 is straight up, degrees clockwise. Starts at home, heading 0, pen down.
- Primitives: `forward`/`fd`, `back`/`bk`, `left`/`lt`, `right`/`rt`, `penup`/`pu`,
  `pendown`/`pd`, `home`, `clearscreen`/`cs`, `clean`, `setxy`, `setheading`,
  `setpencolor` (0-15 palette), `hideturtle`/`ht`, `showturtle`/`st`,
  `pos`, `heading`, `xcor`, `ycor`.
- Off-canvas behaviour: `wrap`, `window` or `fence`, one default **(open)**.

## Control, data and I/O

- `repeat n [..]`, `if cond [..]`, `ifelse cond [..] [..]`, `stop`, `output`/`op`,
  `run [..]`, `wait n` (60ths of a second).
- `make "name value`, `thing "name` (same as `:name`).
- `print`, `show`, `type`, `random n`, `sqrt`, `sin`, `cos`, `arctan` (degrees),
  `abs`, `int`, `round`.
- Lists and words, minimal set: `first`, `last`, `butfirst`/`bf`, `butlast`/`bl`,
  `fput`, `lput`, `list`, `sentence`/`se`, `count`, `item`, `emptyp`, `listp`,
  `wordp`, `numberp`.

## Errors

Classic LOGO wording, no line numbers:

- `I don't know how to foo`
- `foo has no value`
- `Not enough inputs to foo`
- `You don't say what to do with 50`
- `foo doesn't like bar as input`
- `Out of memory`

After an error the current instruction line is abandoned and the prompt returns.

## Runtime design (proposed)

- **Value representation:** NaN-boxing: a normal `f64` is a number, and
  non-number values (words, lists) live in the NaN payload space as pointers.
  One 64-bit machine word per value, no separate type tag in generated code.
- **Memory:** arena allocation, no garbage collector at first. Per-instruction-line
  data is reclaimed when the line finishes; `make` and procedure data lives longer
  **(open: a real collector later, if a loop that allocates forever hurts)**.
- **Code buffer:** per-line arena for REPL code, freed after the line runs;
  per-procedure buffers for `to` definitions.
- **Late binding:** calls to user procedures go through a procedure table, so
  redefinition just updates one entry.
- **Runtime calls:** generated code inlines floating-point arithmetic and
  calls Rust runtime functions through a fixed table for everything else
  (turtle moves, list operations, variable access, errors).
- **Executing generated code:** RAM is writable and executable from S-mode with
  paging off; emit, then `fence.i`, then jump.

## Testing

- Golden transcripts (`.logo` in, text out) and **golden images** (turtle
  programs render through the host `Canvas` to a bitmap compared byte for byte).
- Differential tests: the same corpus through the interpreter and the compiled
  path, compared exactly.
- A tiny JIT smoke test lands first (L4): emit `li a0, 42; ret`, call it, check
  the result, so the "can we run generated code at all" question is settled
  before any real codegen exists.

## Open questions to settle

Rough order of importance:

1. Which LOGO to follow: a UCBLogo subset (proposed) versus something smaller.
2. Numbers and words: one type or two.
3. Canvas size and what happens at the edges (`wrap`/`window`/`fence`).
4. How much list processing to support in v0.1.
5. Unary-minus and infix parsing fidelity.
6. Garbage collection: when to stop getting away without one.
7. Whether the turtle sprite is drawn, and how it is erased cheaply.
