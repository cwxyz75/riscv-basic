# BASIC language spec (draft v0.1)

A Microsoft/Commodore-flavoured BASIC with `f64` numbers. This is the contract
the interpreter is built and tested against. The v0.1 open questions are all
settled below; if you find a new ambiguity, mark it **(open)**, settle it here,
and only then code it.

## Session

After the kernel prints the banner, the interpreter loops: print `READY.`, read
a line, handle it.

```
RISCV BASIC V0.1
32768 BYTES FREE

READY.
```

- A line that starts with a **line number** (0 to 63999) is stored in the
  program, replacing any existing line with that number. A number alone deletes
  that line.
- Any other line runs immediately (**immediate mode**).
- Keywords and variable names are case-insensitive; `LIST` shows keywords in
  upper case. String literals keep their case.
- Maximum input line length: 255 characters. The console stops accepting
  characters beyond that.
- Ctrl-C at any point is BREAK: it abandons the line being typed, or stops a
  running program with `BREAK IN <line>`, then returns to `READY.`.

## Program entry, tokenizing and LIST

Details of how a typed line becomes a stored program line (D9: tokenized).

- Leading spaces on a line are ignored. A line number is the run of digits at
  the start; spaces after it are dropped. A number above 63999 (or too long to
  fit) is `?SYNTAX ERROR`. A number with nothing after it deletes that line;
  deleting a line that does not exist does nothing, silently.
- Input is ASCII only. Any character outside ASCII is `?SYNTAX ERROR`.
- **Keywords** are recognised anywhere outside a string literal, `REM` text and
  `DATA` items, case-insensitively, taking the longest match at each position
  (see the name-splitting rule under Values). `?` is read as `PRINT`.
  The keyword list: `REM LET PRINT INPUT IF THEN GOTO GOSUB RETURN ON FOR TO
  STEP NEXT END STOP DIM DATA READ RESTORE CLS COLOR LOCATE POKE RUN LIST NEW
  AND OR NOT ABS INT SGN SQR SIN COS TAN ATN LOG EXP RND PEEK FRE TIMER LEN ASC
  VAL STR$ CHR$ LEFT$ RIGHT$ MID$ TAB SPC`.
- Outside string literals, `REM` text and `DATA` items, letters are folded to
  upper case (so variable names are stored upper case). String literals (an
  unterminated one runs to the end of the line), `REM` text, and `DATA` items
  (up to the next `:` outside quotes) are stored exactly as typed. Spaces and
  all other characters are kept as typed.
- `LIST` prints each line as the line number, one space, then the line
  detokenized: keyword text upper case, nothing inserted around it, so
  `10 print"hi"` lists as `10 PRINT"hi"`.
- `LIST` takes an optional range: `LIST` (all), `LIST 20` (that line),
  `LIST 20-30`, `LIST 20-` (from 20), `LIST -30` (up to 30). Lines in the range
  print in line-number order; an empty or reversed range prints nothing. Bad
  numbers or other text after `LIST` are `?SYNTAX ERROR`.
- `NEW` (with nothing after it) deletes the program.

## Program memory

- The program area is **32768 bytes**. A stored line costs its token bytes
  (the line body after the number, as stored) plus **5 bytes** of overhead
  (as in classic BASIC: a link, the line number and a terminator).
- A line that would not fit is `?OUT OF MEMORY ERROR`, and the program is left
  exactly as it was; a replacement that would not fit leaves the old line in
  place. A replacement is judged by its net cost (the old line's bytes are
  given back). Deleting lines and `NEW` give bytes back.
- `BYTES FREE` in the banner is 32768 minus the bytes used. (`FRE(0)` will
  report the same number.) Variables, strings and arrays live in a separate
  heap and are not counted here.

## Values and variables

- Two types: number (`f64`) and string (name ends in `$`). No integer type.
- Names: a letter followed by letters and digits; **all characters are
  significant** (unlike classic MS BASIC's first two). Keywords are
  recognised anywhere in a name, as in classic BASIC: the tokenizer scans left
  to right and takes the first keyword it finds, so `TOTAL` is `TO` followed by
  `TAL`, and `SCORE` is `SC` `OR` `E`. Program authors must avoid keywords
  inside names. This is deliberate (authenticity) and a known trap.
- Unset numbers are `0`, unset strings are `""`.
- Arrays: `DIM A(10)`, `DIM N$(5,5)`; subscripts run `0` to the declared bound.
  Using an undeclared array implicitly dimensions it to 10. Up to 3 dimensions.

## Operators (highest precedence first)

1. `^` (exponent), left-associative like MS BASIC, so `2^3^2` is `64`
2. unary `-`, `+`
3. `*`, `/`
4. `+`, `-` (on strings, `+` concatenates)
5. `=`, `<>`, `<`, `>`, `<=`, `>=` (work on numbers or on strings, not mixed)
6. `NOT`
7. `AND`
8. `OR`

Comparisons yield `-1` (true) or `0` (false). `AND`/`OR`/`NOT` are bitwise on
the operands truncated to 32-bit signed integers, so `-1` is all ones (classic
BASIC used 16-bit; we deliberately differ). An operand outside the 32-bit signed
range is `?ILLEGAL QUANTITY ERROR`.

## Statements

| Statement | Notes |
|---|---|
| `REM text` | Comment. |
| `[LET] v = expr` | `LET` is optional. |
| `PRINT items` | `;` joins with no spacing, `,` advances to the next 14-column zone, a trailing `;` or `,` suppresses the newline. `?` is shorthand. `TAB(n)` and `SPC(n)` may appear in the list. |
| `INPUT ["prompt";] v[, v...]` | Prints the prompt and then `? `. A bad number prints `?REDO FROM START` and asks again. |
| `IF expr THEN line` / `IF expr THEN stmt[: stmt...]` / `IF expr GOTO line` | No `ELSE` in v0.1 (left out deliberately, for authenticity). If false, the rest of the line is skipped. |
| `GOTO line`, `GOSUB line`, `RETURN` | |
| `ON expr GOTO l1,l2,...` / `ON expr GOSUB ...` | Out-of-range selector falls through. |
| `FOR v = a TO b [STEP c]` / `NEXT [v[,v...]]` | |
| `END`, `STOP` | `STOP` prints `BREAK IN <line>`. |
| `DIM name(n[,m...])[, ...]` | |
| `DATA ...`, `READ v[, ...]`, `RESTORE` | |
| `CLS`, `COLOR fg[,bg]`, `LOCATE row,col` | Emitted as ANSI escapes via `Io`. Colours 0-7. |
| `POKE addr,val` | Writes one byte to a **raw physical address**. No safety net: POKEing the kernel, the stack or the interpreter's own memory corrupts them, as on the real machines. `addr` must be a non-negative integer-valued number below 2^53 and `val` an integer from 0 to 255, otherwise `?ILLEGAL QUANTITY ERROR`. An address that does not exist faults in the kernel (see Faults below). |

## Commands (immediate mode)

`RUN [line]`, `LIST [from][-[to]]`, `NEW`. `SAVE`/`LOAD`/`DIR` arrive only if
the persistence milestone happens.

## Functions

- Numeric: `ABS`, `INT`, `SGN`, `SQR`, `SIN`, `COS`, `TAN`, `ATN`, `LOG`, `EXP`,
  `RND(x)`, `PEEK(addr)` (one byte from a raw physical address, same address
  rules as `POKE`), `FRE(0)` (bytes free), `TIMER` (seconds since boot as a
  float, no argument).
- String to number: `LEN`, `ASC`, `VAL`.
- Number to string: `STR$`, `CHR$`.
- String: `LEFT$`, `RIGHT$`, `MID$` (1-based).

## Expressions, `PRINT` and `LET` in detail

Fills in the operator table above. Numbers only, plus string literals as
`PRINT` items; string variables and string expressions arrive with the
strings milestone (a name ending in `$` is `?SYNTAX ERROR` until then).

- **Number literals:** digits with an optional fraction and an optional
  exponent: `5`, `5.`, `.5`, `1.5E3`, `1E-3`. An `E` with no digits after it is
  `?SYNTAX ERROR`.
- **Variables:** a numeric variable not yet assigned is `0`. Names are
  case-insensitive. Because keywords are split out of names (see Values), a
  name that contains a keyword cannot be used: `LET TOTAL=1` is
  `?SYNTAX ERROR`.
- **Chaining:** operators of equal precedence group left to right, including
  comparisons (`1<2<3` is `(1<2)<3`).
- **`^`:** the base is a number, variable or parenthesised expression. The
  exponent may carry a sign, so `2^-1` is `0.5`. Unary minus binds looser than
  `^`: `-2^2` is `-4`.
- **Arithmetic faults:** dividing by zero (including `0/0`) is `?DIVISION BY
  ZERO ERROR`. Any other result that is infinite (including a literal too large
  to represent) is `?OVERFLOW ERROR`. A result that is not a number, such as
  `(-8)^(1/3)`, is `?ILLEGAL QUANTITY ERROR`.
- **Logic:** `NOT`, `AND` and `OR` truncate each operand toward zero to a
  32-bit signed integer, so `NOT 1` is `-2` and `5.9 AND 7` is `5`.
- **`LET`:** `[LET] name = expr`. Assigning does not print anything.
- **`PRINT`:** a list of items separated by `;` or `,`. An item is a string
  literal or an expression. Two items with no separator between them
  (`PRINT "A" 1`, `PRINT 1 2`) are `?SYNTAX ERROR`. `;` adds nothing. `,`
  moves to the next 14-column zone (always at least one column; a position
  already on a zone boundary moves to the next one). A trailing `;` or `,`
  suppresses the newline. `PRINT` alone prints just a newline. Columns count
  characters since the start of the output line.
- **Prompt and errors:** `READY.` and error messages always start on a fresh
  line: if the cursor is not at column 0 (after `PRINT 1;`), a newline is
  printed first. Output already produced before an error stays.

## Running programs: `RUN`, `GOTO`, `IF`, `FOR`, `END`, `STOP`

- **Statement separator:** `:`. A program line or an immediate line may hold
  several statements, run left to right. Extra or leading colons are ignored.
  `REM` swallows the rest of the line, colons included.
- **`RUN [line]`:** clears all variables and active `FOR` loops, then runs the
  stored program from its lowest line, or from `line` (`?UNDEF'D STATEMENT
  ERROR` if there is no such line). The program itself is untouched. Execution
  goes in line-number order; running past the last line ends the program and
  returns to `READY.`. Variables keep their values after the run, so they can
  be inspected at the prompt.
- **`GOTO line`:** continues at `line`. If there is no such line it is
  `?UNDEF'D STATEMENT ERROR` (with `IN <line>` when it happens in a program).
  Typed at the prompt, `GOTO line` starts running the program at that line
  *without* clearing variables.
- **`IF expr THEN line`**, **`IF expr THEN statement[: statement...]`**,
  **`IF expr GOTO line`:** the condition is true when non-zero. When false,
  the rest of the line (everything after the `IF`) is skipped. `THEN` may be
  followed by another `IF`.
- **`FOR v = a TO b [STEP c]` / `NEXT [v[,v...]]`:** `a`, `b` and `c` are
  evaluated once, when the `FOR` runs; `STEP` defaults to 1. As in classic
  BASIC the test happens at `NEXT`, so the body always runs at least once.
  At `NEXT`, `v` is increased by the step; the loop repeats while `v <= b`
  (step zero or positive) or `v >= b` (negative step), else it ends and
  execution continues after the `NEXT`. A loop may start and end in the middle
  of a line. Starting a `FOR` on a variable that already has an active loop
  discards that loop and any loops inside it. `NEXT` with no name ends the
  innermost loop; `NEXT v` first discards any loops inside `v`'s loop;
  `NEXT I,J` is `NEXT I` then `NEXT J`. A `NEXT` with no matching active loop
  is `?NEXT WITHOUT FOR ERROR`. Loops typed at the prompt must start and end on
  one line (active loops are cleared at the start of every typed line).
- **`GOSUB line` / `RETURN`:** `GOSUB` jumps to `line` and remembers the
  statement after it; `RETURN` jumps back there. Subroutines may nest, up to
  1000 deep (deeper is `?OUT OF MEMORY ERROR`, which stops a runaway
  recursion). `RETURN` with nothing to return to is `?RETURN WITHOUT GOSUB
  ERROR`. `RUN` forgets any pending returns. `GOSUB` typed at the prompt runs
  the subroutine, and its `RETURN` then ends the run (back to `READY.`); the
  rest of the typed line is not executed.
- **`ON expr GOTO l1,l2,...` / `ON expr GOSUB l1,l2,...`:** `expr` is
  truncated toward zero; if the result is `n` with `1 <= n <= number of lines`
  control goes to the nth line, otherwise (zero, negative, too large) execution
  simply continues with the next statement. A missing target line is `?UNDEF'D
  STATEMENT ERROR`, but only if it is the one chosen. `ON ... GOSUB` returns to
  the statement after the whole `ON`.
- **`END`:** stops the program silently. **`STOP`:** stops it and prints
  `BREAK IN <line>`; typed at the prompt, `STOP` prints just `BREAK`.
- **Errors in a program** stop it and print `?<NAME> ERROR IN <line>` using
  the line that was executing. Syntax is only checked when a line runs.
- `LIST` and `NEW` also work inside a program; `NEW` ends it.
- No way to interrupt a runaway program exists until BREAK handling arrives
  (milestone M6), so golden tests must not contain endless loops.

## Machine access: `PEEK`, `POKE`, `TIMER`

- **`PEEK(addr)`** reads one byte from the physical address `addr` and gives it
  as a number from 0 to 255. **`POKE addr, val`** writes one byte. Both are
  unchecked (decision D12). `addr` must be a non-negative integer-valued number
  below 2^53 (so it is exactly representable) and `val` an integer-valued
  number from 0 to 255; anything else is `?ILLEGAL QUANTITY ERROR`, raised
  before the machine is touched. `PEEK` needs its parentheses; a missing `(` or
  `)` is `?SYNTAX ERROR`. `POKE` takes exactly two arguments.
- **`TIMER`** is the number of seconds since boot as a number (milliseconds
  divided by 1000). It takes no parentheses or argument.
- Arguments are evaluated left to right, and a `TIMER` or `PEEK` inside an
  expression is read at the moment the expression reaches it.

## Number output

`PRINT` writes a number as a sign field, the digits, and a trailing space: a
space where a minus sign would go for non-negative numbers, `-` for negative
ones. The digits are the value rounded to 9 significant digits, then:

- If the magnitude is `0`, or from `0.01` up to (but not including) `1e9`:
  positional notation, trailing zeros (and a bare decimal point) removed.
  Integer-valued numbers therefore print with no decimal point. A leading zero
  is printed (`0.5`, not `.5`). Negative zero prints as `0`.
- Otherwise: scientific, a mantissa with trailing zeros removed, then `E`, a
  sign, and at least two exponent digits: `1.5E+20`, `1E+09`, `1E-03`.

Examples: `PRINT 3` gives ` 3 `, `PRINT -2.5` gives `-2.5 `, `PRINT 1/3` gives
` 0.333333333 `, `PRINT 1E9` gives ` 1E+09 `. The golden tests pin this rule.

## Errors

In immediate mode: `?<NAME> ERROR`. Inside a program: `?<NAME> ERROR IN <line>`.
After an error the program stops and the prompt returns.

`SYNTAX`, `TYPE MISMATCH`, `UNDEF'D STATEMENT`, `NEXT WITHOUT FOR`,
`RETURN WITHOUT GOSUB`, `OUT OF DATA`, `BAD SUBSCRIPT`, `REDIM'D ARRAY`,
`DIVISION BY ZERO`, `ILLEGAL QUANTITY`, `OVERFLOW`, `OUT OF MEMORY`.

## Faults

`PEEK` and `POKE` are unchecked, so touching an address with nothing behind it
raises a hardware access fault. That is a kernel crash, not a BASIC error: the
trap handler (M2) prints `scause`/`sepc`/`stval` and powers off. The host
interpreter never sees this; its test `Io` simply returns or stores bytes.
