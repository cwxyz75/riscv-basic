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

## Turtle graphics in detail

Fixes the rules the first graphics layer is built and tested against (the
`turtle` crate, milestone L1). Everything here is integer-exact where it can be,
so golden images are stable.

- **Canvas and palette.** A canvas is a grid of pixels, each holding a palette
  index 0 to 15. The machine's canvas is 640x480; the host tests use tiny ones
  (and any even size works). The palette, as UCBLogo's: 0 black, 1 blue, 2 green,
  3 cyan, 4 red, 5 magenta, 6 yellow, 7 white, 8 brown, 9 tan, 10 forest,
  11 aqua, 12 salmon, 13 purple, 14 orange, 15 grey. The background starts at 0
  and the pen at 7.
- **Coordinates to pixels.** With a canvas `w` wide and `h` high, logo point
  `(x, y)` is column `w/2 + round(x)` and row `h/2 - round(y)` (integer
  division; `round` is `floor(v + 0.5)`; rows count down from the top). The
  turtle keeps exact `f64` coordinates; only drawing rounds.
- **Lines.** A move from P0 to P1 with the pen down draws the straight line
  between the two rounded endpoints, both included, with this Bresenham:
  `dx = |x1-x0|`, `dy = |y1-y0|`, `err = dx - dy`; plot; stop at the end point;
  `e2 = 2*err`; if `e2 > -dy` then `err -= dy` and step x; if `e2 < dx` then
  `err += dx` and step y; repeat. Pixels outside the canvas are skipped.
- **Moving.** `forward d` moves `d` along the heading: `x += d*sin(h)`,
  `y += d*cos(h)` (`h` in degrees, 0 is up, clockwise); `back d` is `forward -d`.
  `left`/`right` turn; the heading is always kept in `[0, 360)`.
  `setxy`, `setheading`, `home` (to `(0,0)`, heading 0, drawing nothing),
  `clearscreen` (clear the canvas to 0 and go home), `clean` (clear the canvas,
  turtle untouched), `setpencolor n` (0 to 15) and `penup`/`pendown` work as in
  UCBLogo. `setxy` draws a line like a move when the pen is down.
- **Wrap.** The turtle lives on a torus: coordinates run over the closed
  interval `[-w/2, w/2] x [-h/2, h/2]`. A move that would leave it is drawn up
  to the edge, the turtle reappears at the opposite edge, and the rest of the
  move continues from there. (Pixels exactly at the right and bottom edges would
  fall one past the canvas and are skipped, so those edge points show only on
  the far side.) One move is cut off after 4096 edge crossings.
- **`fill`.** Flood-fills, with the pen colour, the 4-connected region of
  pixels that have the same colour as the pixel under the turtle, whether the
  pen is up or down.
- **The sprite.** When shown, a triangle is toggled onto the canvas: the tip
  8 pixels ahead of the turtle and two corners 7 pixels away, at 150 degrees to
  the right and to the left of the heading (30 degrees off straight back),
  joined by three lines drawn with each line's
  last pixel left out (so shared corners are toggled once). "Toggled" means the
  pixel's colour is XORed with 15, so drawing it again erases it exactly. The
  sprite is removed before any drawing operation and put back after, so it never
  ends up in the picture, and `fill` never sees it.

## Reader, evaluation and errors in detail

The rules the reference interpreter (milestone L2) is built and tested against.

- **Reading a line.** Words are runs of letters, digits and `_ . ?` starting
  with a letter, `_`, `.` or `?`; they stop at spaces, brackets, parentheses,
  quotes, colons and the infix characters `+ - * / = < >`. `"word` is a quoted
  word that runs to a space, bracket or parenthesis (`"` alone is the empty
  word). `:name` is a variable reference. Numbers are digits with an optional
  fraction and an optional exponent (`12`, `3.5`, `1e3`, `2.5e-3`). A `;`
  starts a comment that runs to the end of the line. Names of procedures and
  variables are case-insensitive (folded to lower case); quoted words and the
  contents of lists keep their case.
- **Lists.** `[ ... ]` is a list: its contents are not evaluated. Inside a list,
  a number-looking item is a number and everything else is a word whose text is
  kept exactly (`"hi` and `:x` keep their quote and colon). A line that ends
  inside a list is `Missing ]`; a stray `]` is `Unexpected ]`. Code is data:
  `run` and `repeat` rebuild source text from a list (items joined with single
  spaces, nested lists in brackets, numbers in their printed form) and read it
  again, so `[fd -50]`, `[print "hi]` and `[3 - 2]` mean what they look like.
- **Unary minus** is decided while reading, by the rule under Syntax: it is
  unary at the start of an instruction, after an infix operator, `(` or `[`, or
  after a space when not followed by a space. A unary minus directly before a
  number makes that number negative; before anything else it negates the
  following operand.
- **Instructions and arity.** Every procedure has a fixed number of inputs.
  An instruction line is a sequence of instructions, each read and run before
  the next (so an error leaves earlier instructions done). Each input of a
  prefix procedure is a full expression, so `fd 50 + 10` is `fd 60`. Infix
  operators bind tighter than procedure inputs: `*` and `/` first, then `+`
  and `-`, then `=` `<` `>`, all left to right; `( )` groups. An unknown name is
  read as a procedure with no inputs, so `foo 10` reports `I don't know how to
  foo` before it ever looks at `10`. Parentheses only group: calling a
  procedure with a variable number of inputs, as in `(sum 1 2 3)`, is not
  supported.
- **Commands and operations.** A command does something; an operation also
  outputs a value. An instruction must be a command: a value with nothing to
  use it is `You don't say what to do with 3`. An input that is a command
  (outputs nothing) is `fd didn't output to print`.
- **Truth.** The words `true` and `false`. `if` and `ifelse` need one of them
  (else `if doesn't like 3 as input`). `=` is true for equal numbers, for words
  that match ignoring case, and for lists whose items are equal in turn; a
  number and a word are never equal. `<` and `>` need two numbers.
- **Procedures.** `to name :a :b` starts a definition; the lines that follow
  (prompt `> `) are its body until a line `end`. Defining again replaces it.
  A body is read each time the procedure is called, so it may use procedures
  defined later. Variables: inputs live in the procedure's frame, and a
  variable is found by searching the frames of the running procedures from
  newest to oldest, then the globals (**dynamic scope**). `make "name value`
  sets the variable where it is found, or creates a global if there is none;
  `thing "name` and `:name` read it (`name has no value` if there is none).
  `stop` ends the running procedure; `output value` (`op`) ends it with a
  value; either outside a procedure is `Can only do that inside a procedure`.
  Nesting deeper than 400 procedure calls is `Out of memory`.
- **The primitives of v0.2.** Turtle: `forward fd back bk left lt right rt
  penup pu pendown pd home clearscreen cs clean setxy setheading seth
  setpencolor setpc hideturtle ht showturtle st fill`, and the operations `pos`
  (a list `[x y]`), `heading`, `xcor`, `ycor`. Control: `repeat n [..]`, `if`,
  `ifelse`, `run`, `stop`, `output`/`op`, `wait n` (n/60 seconds), `bye`
  (reboot to the boot menu). Data: `make thing print show type random sqrt sin
  cos arctan abs int round`, with `and or not`. Lists and words: `first last
  butfirst bf butlast bl fput lput list sentence se count item emptyp listp
  wordp numberp`. `sin`, `cos` and `arctan` use degrees; `int` truncates toward
  zero; `round` rounds halves away from zero; `random n` is a whole number from
  0 to n-1 (n a number of at least 1); `item n` counts from 1.
- **Printing.** `print` writes its input and a newline: a list without its
  outer brackets (`a b [c d]`), a word as it is, a number as below. `show`
  keeps the outer brackets. `type` is `print` without the newline. A number
  prints like C's `%.12g`: 12 significant digits, trailing zeros and a bare
  decimal point removed, whole numbers without a point, and scientific form
  (`1e+20`, `1.5e-07`) when the decimal exponent is below -4 or at least 12.
  Negative zero prints as `0`.
- **Errors**, each a single line that abandons the rest of the line and returns
  to the prompt: `I don't know how to foo`; `foo has no value`; `Not enough
  inputs to foo`; `You don't say what to do with 3`; `foo doesn't like bar as
  input` (`bar` shown as `print` would, lists with brackets); `foo didn't
  output to bar`; `Missing ]`; `Unexpected ]`; `Can only do that inside a
  procedure`; `Out of memory`; `Missing )` and `Unexpected )` for unbalanced
  parentheses; `Number out of range` when arithmetic overflows; and `Stopped!`
  when Ctrl-C interrupts a running instruction (checked before each primitive
  runs). Division by zero is
  `/ doesn't like 0 as input`.

## The IR (milestone L3)

Between the parser and any backend sits a small structured intermediate
representation. It carries no backend detail, so the interpreter in this
milestone and the RV64 code generator later are two ways of running the same
thing, and the interpreter becomes the oracle for the compiler.

- **Shape.** A line (or a procedure body line, or the text of a list) lowers to
  a flat sequence of instructions over numbered **temps**. A temp holds *a value
  or nothing* (a command leaves nothing), plus the name of the instruction that
  produced it, for error messages. Instructions: `Const` (a number, word or list
  literal), `Var` (dynamic lookup, `name has no value` if unset), `Neg`,
  `Infix` (`+ - * / = < >`), `Call` (a primitive or a procedure, by name, late
  bound, on the values of its input temps), `Discard` (a statement's temp must
  hold nothing, else `You don't say what to do with ...`), `Repeat`, `If`
  (which also serves `ifelse`), and `Fail` (raise an error).
- **Order.** Temps are numbered in the order they are produced, operands before
  the instruction that uses them, and an instruction's own result last, so the
  numbering of any program is fixed and testable. Inputs are evaluated left to
  right, exactly as in the AST interpreter.
- **Control flow is inline when it can be.** `repeat n [..]`, `if c [..]` and
  `ifelse c [..] [..]` whose list inputs are literal lists lower to `Repeat` and
  `If` instructions holding nested blocks, the lists' text read and lowered once.
  When an input is anything else (a variable holding a list, say) the
  instruction stays a `Call` and the list is read and lowered when it runs.
  `run` is always a `Call`. A `Repeat`/`If` leaves nothing in its temp, like the
  commands they stand for.
- **Errors keep their place.** A line is read instruction by instruction in the
  interpreter, so an error in a later instruction must not stop earlier ones
  from running. Lowering therefore turns an unreadable instruction (`Not enough
  inputs to fd`, an unbalanced `)`) into a `Fail` instruction at that position
  and stops; everything before it still runs first. (`Missing ]` and `Unexpected
  ]` are found when the line is read and, as before, stop it before anything
  runs.)
- **Procedures.** A procedure's body is lowered when it is called, not when it
  is defined, so it may use procedures defined later and a procedure redefined
  with different inputs is seen correctly. (A later version may remember the
  lowered body until the next definition; the result must not change.)
- **Equivalence rule.** Running the IR must give exactly the output, drawing,
  errors and side effects of the AST interpreter, instruction for instruction.
  The whole golden corpus runs under both and must agree.

## Compilation to RV64 (milestones L4 and L5)

The IR is compiled to RISC-V machine code, which is then run. Every instruction
line typed at the prompt, every line of a procedure body when the procedure runs,
and every list handed to `run` (or to a non-literal `repeat`/`if`/`ifelse`) is
compiled before it executes; the code is discarded or cached, never
interpreted. The AST and IR interpreters remain as oracles.

- **Values (NaN-boxing).** Every value is one 64-bit word. A number is its
  `f64` bits as they are (numbers are always finite, so their top 16 bits are
  below `0xFFF0`). Anything else has its top 16 bits `0xFFF9` (a **word**),
  `0xFFFA` (a **list**) or `0xFFFB` (**nothing**, the "no value" of a command),
  with a 48-bit payload: an index into the value arena for words and lists, and
  for *nothing* the pool index of the name of whatever produced it (so `fd
  didn't output to print` can still name `fd`). A word is therefore any value
  whose top 16 bits are `0xFFF9` or more, which generated code tests with one
  shift and compare.
- **The arena.** Words and lists that exist while a unit of code runs live in an
  arena owned by that run and freed when it ends; values stored by `make` or
  passed to procedures are copied out into the interpreter's own storage, so
  nothing outlives the arena. The arena holds at most 262144 values; a run that
  needs more stops with `Out of memory`.
- **A unit of code** compiles to one function, `status = unit(ctx)`, following
  the standard RISC-V calling convention: `ctx` in `a0`, status in `a0` (0 =
  finished, 1 = stopped by an error or by `stop`/`output`, the details waiting in
  the runtime), return address in `ra`, `s0` saved and used to keep `ctx`.
  Temps and the `repeat` counters live in the function's stack frame; the
  arguments of a call are copied to a small array at the bottom of the frame.
- **The runtime-call table.** `ctx` begins with a table of function addresses,
  one per runtime service; generated code loads an address from it and calls it
  with `ctx` as the first argument. Services return two words in `a0`/`a1`: a
  value (or count, or truth) and an error flag. The flag is checked after every
  call and, when set, the unit returns status 1 at once. The services: `var`,
  `const` (a word or list literal; numbers are loaded inline), `neg`, `infix`,
  `call` (any primitive or procedure, by name pool index, late bound), `discard`,
  `repeat_count`, `truth` and `fail`.
- **Inline code.** Number literals, `Neg` (flip the sign bit), `Add`, `Sub` and
  `Mul` on two numbers (with a finiteness check, falling back to the runtime
  service for the error), `Discard` of a command, `Repeat` and `If` (with their
  loops and branches) are generated inline. Everything else goes through the
  runtime, which uses exactly the code the interpreters use.
- **Choosing the engine.** The primitive `.engine "ast`, `.engine "ir` or
  `.engine "jit` switches the engine for what is typed afterwards; on the
  machine the default is `jit`. `.engine "jit` is `.engine doesn't like jit as
  input` on a build that has no way to run machine code.
- **Equivalence.** Compiled code must give the same output, drawing and errors
  as the interpreters on the whole golden corpus, on the host (through an
  emulator) and on the real machine.

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

## The disk: `save`, `load`, `dir`, `erasefile`

The machine has one disk, shared with BASIC (format and limits: `DESIGN.md`,
"The disk"); each file is tagged with the language that wrote it. These four are
primitives like any other (so a procedure cannot be given one of these names).

- **`save name`** (command): writes every defined procedure, sorted by name, as
  its source text: `to name :input ...`, the body lines exactly as typed, then
  `end`, each line ending in a newline. Variables, the turtle and the picture are
  not saved. Saving over an existing LOGO file of that name replaces it; with no
  procedures defined it saves an empty file. `name` must be a word (`save "shapes`).
- **`load name`** (command): reads the file and runs its lines as if they had
  been typed at the prompt, but without prompts or echo: a `to ... end` block
  defines a procedure (replacing one of that name), any other line is run.
  Comments (`;`) and blank lines are ignored. The first error stops the load and
  is reported as usual; what was defined or run before it stays. A `to` with no
  `end` before the end of the file is `Missing end` and defines nothing. A
  file may itself `load` another; more than 8 loads nested is `Out of memory`
  (so a file that loads itself ends). A BASIC file is an error (below).
- **`dir`** (command): lists every file on the disk, both languages, in the same
  layout as BASIC's `DIR`: name left-aligned in 16 columns, two spaces, the
  language (`BASIC` or `LOGO`) left-aligned in 5, two spaces, the size in bytes
  right-aligned in 6, then a last line `n BLOCKS FREE.`.
- **`erasefile name`** (command): deletes the LOGO file `name`.

Names follow BASIC's rules: 1 to 16 characters from `A-Z 0-9 . - _`,
case-insensitive, shown upper-case.

Disk errors, in the usual one-line form:

| Message | When |
|---|---|
| `No disk attached` | the machine has no disk |
| `Disk is not formatted` | there is a disk but no file system on it |
| `Disk error` | a read or write failed, or the disk's structures are damaged |
| `File not found` | `load` or `erasefile` of a name that does not exist |
| `File is not a LOGO file` | the name exists but belongs to BASIC |
| `Bad file name` | see Names |
| `Disk full` | no free blocks, all 64 directory entries used, or a file over the size limit; the disk is left unchanged |
| `Missing end` | see `load` |

A name that is not a word is `save doesn't like 5 as input`, and a missing input
is `Not enough inputs to save`, as for any primitive.

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
