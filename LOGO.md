# LOGO language spec (draft v0.2)

The second language of the machine, chosen at boot (see `ROADMAP.md`, M9).
Like `BASIC.md`, this is the contract the implementation is built and tested
against. The v0.2 open questions are all settled below (see "Decisions"); if you
find a new ambiguity, mark it **(open)**, settle it here, and only then code it.

The dialect is a **UCBLogo subset**: where this document is silent, follow
UCBLogo's documented behaviour.

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
- Numbers and words are **separate types**. UCBLogo treats numbers as words
  (`first 123` is `1`); here a number is only a number and a word is only a
  word, and a word that looks like a number does not act as one. This keeps
  numbers as plain `f64` for the compiler.
- Booleans are the words `true` and `false`.

## Syntax

- Procedures have fixed arity, so `fd 50 + 10` parses by consuming one input.
- Infix operators: `+ - * / = < >`, with `*` `/` above `+` `-` above the
  comparisons.
- **Unary minus** follows UCBLogo: a `-` is *unary* (a negative number) at the
  start of an instruction or expression, after an infix operator or `[`, or
  when it has a space before it and none after it. Otherwise it is binary
  subtraction. So `fd -50`, `3 + -2` and `3 * -2` negate; `3 - 2` and `3-2`
  subtract; and `print 3 -2` is two inputs (`3`, then `-2`), which is
  `You don't say what to do with -2`. Golden tests pin each case.
- `:name` is a variable reference, `"word` a quoted word, `[...]` a list.
- Definitions: `to name :a :b ... end`.
- Scoping is **dynamic**, as in classic LOGO.

## Turtle model

- Canvas: **640x480** pixels, 32-bit XRGB, origin at the centre, y up.
- Heading 0 is straight up, degrees clockwise. Starts at home, heading 0, pen down.
- Primitives: `forward`/`fd`, `back`/`bk`, `left`/`lt`, `right`/`rt`, `penup`/`pu`,
  `pendown`/`pd`, `home`, `clearscreen`/`cs`, `clean`, `setxy`, `setheading`,
  `setpencolor` (0-15 palette), `hideturtle`/`ht`, `showturtle`/`st`,
  `pos`, `heading`, `xcor`, `ycor`.
- Off-canvas behaviour: **`wrap`** only in v0.2 (the turtle leaves one edge and
  comes back at the opposite one, UCBLogo's default). `window` and `fence` are
  not provided yet.
- **The turtle sprite** is a small triangle drawn at the turtle's position and
  heading, and erased by drawing it again with XOR (so no pixels need saving).
  `hideturtle`/`ht` and `showturtle`/`st` control it.

## Control, data and I/O

- `repeat n [..]`, `if cond [..]`, `ifelse cond [..] [..]`, `stop`, `output`/`op`,
  `run [..]`, `wait n` (60ths of a second).
- `make "name value`, `thing "name` (same as `:name`).
- `print`, `show`, `type`, `random n`, `sqrt`, `sin`, `cos`, `arctan` (degrees),
  `abs`, `int`, `round`.
- Lists and words, minimal set: `first`, `last`, `butfirst`/`bf`, `butlast`/`bl`,
  `fput`, `lput`, `list`, `sentence`/`se`, `count`, `item`, `emptyp`, `listp`,
  `wordp`, `numberp`. This is the whole v0.2 set: no `map`, `filter` or
  `foreach` (they need procedures as values), only `run` and `repeat` take
  lists of instructions.

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
- **Memory:** arena allocation and **no garbage collector in v0.2**.
  Per-instruction-line data is reclaimed when the line finishes; `make` and
  procedure data lives until reboot or redefinition. A line that allocates
  without bound gets `Out of memory`, and the prompt recovers. A collector is
  revisited only if a showpiece program needs one.
- **Code buffer:** per-line arena for REPL code, freed after the line runs;
  per-procedure buffers for `to` definitions.
- **Late binding:** calls to user procedures go through a procedure table, so
  redefinition just updates one entry.
- **Runtime calls:** generated code inlines floating-point arithmetic and
  calls Rust runtime functions through a fixed table for everything else
  (turtle moves, list operations, variable access, errors).
- **Backend: a hand-written RV64 emitter (decision D16), not Cranelift.** The
  compiled code is small and simple (double arithmetic, branches, calls into
  the runtime table); the work of LOGO is in the runtime. Code generation is a
  template scheme with no register allocator: every IR value has a stack slot,
  and each operation loads into temporaries, computes and stores. That is
  slower than optimised code but easy to verify, and turtle graphics are bound
  by drawing, not arithmetic. The emitter is unit-tested on the host against
  known RV64 encodings (and `llvm-objdump`), and the whole path by the
  differential tests. The IR is kept free of backend detail so another backend,
  such as Cranelift, could be added later.
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

## Decisions

The v0.2 questions, as settled:

1. **Dialect:** a UCBLogo subset.
2. **Numbers and words:** two separate types.
3. **Canvas:** 640x480, origin at the centre, `wrap` at the edges.
4. **List processing:** the minimal set above, no higher-order procedures.
5. **Unary minus:** UCBLogo's rule, written out under Syntax.
6. **Garbage collection:** none in v0.2 (arenas, reclaimed per line).
7. **Turtle sprite:** a small triangle, erased by XOR.
8. **Backend:** a hand-written RV64 emitter (D16).
