# BASIC language spec (draft v0.1)

A Microsoft/Commodore-flavoured BASIC with `f64` numbers. This is the contract
the interpreter is built and tested against. Items marked **(open)** are
proposals waiting for a decision; settle them here before coding them.

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
- Maximum input line length: 255 characters **(open)**.
- Ctrl-C at any point is BREAK: it abandons the line being typed, or stops a
  running program with `BREAK IN <line>`, then returns to `READY.`.

## Values and variables

- Two types: number (`f64`) and string (name ends in `$`). No integer type.
- Names: a letter followed by letters and digits; **all characters are
  significant** (unlike classic MS BASIC's first two) **(open)**.
- Unset numbers are `0`, unset strings are `""`.
- Arrays: `DIM A(10)`, `DIM N$(5,5)`; subscripts run `0` to the declared bound.
  Using an undeclared array implicitly dimensions it to 10. Up to 3 dimensions.

## Operators (highest precedence first)

1. `^` (exponent), left-associative like MS BASIC **(open)**
2. unary `-`, `+`
3. `*`, `/`
4. `+`, `-` (on strings, `+` concatenates)
5. `=`, `<>`, `<`, `>`, `<=`, `>=` (work on numbers or on strings, not mixed)
6. `NOT`
7. `AND`
8. `OR`

Comparisons yield `-1` (true) or `0` (false). `AND`/`OR`/`NOT` are bitwise on
the operands truncated to 32-bit signed integers, so `-1` is all ones **(open;
classic BASIC used 16-bit)**.

## Statements

| Statement | Notes |
|---|---|
| `REM text` | Comment. |
| `[LET] v = expr` | `LET` is optional. |
| `PRINT items` | `;` joins with no spacing, `,` advances to the next 14-column zone **(open)**, a trailing `;` or `,` suppresses the newline. `?` is shorthand. `TAB(n)` and `SPC(n)` may appear in the list. |
| `INPUT ["prompt";] v[, v...]` | Prints the prompt and then `? `. A bad number prints `?REDO FROM START` and asks again. |
| `IF expr THEN line` / `IF expr THEN stmt[: stmt...]` / `IF expr GOTO line` | No `ELSE` in v0.1 **(open)**. If false, the rest of the line is skipped. |
| `GOTO line`, `GOSUB line`, `RETURN` | |
| `ON expr GOTO l1,l2,...` / `ON expr GOSUB ...` | Out-of-range selector falls through. |
| `FOR v = a TO b [STEP c]` / `NEXT [v[,v...]]` | |
| `END`, `STOP` | `STOP` prints `BREAK IN <line>`. |
| `DIM name(n[,m...])[, ...]` | |
| `DATA ...`, `READ v[, ...]`, `RESTORE` | |
| `CLS`, `COLOR fg[,bg]`, `LOCATE row,col` | Emitted as ANSI escapes via `Io`. Colours 0-7. |
| `POKE addr,val` | Safety policy for addresses undecided **(open)**: probably restricted to a dedicated user-memory window, anything else is `?ILLEGAL QUANTITY ERROR`. |

## Commands (immediate mode)

`RUN [line]`, `LIST [from][-[to]]`, `NEW`. `SAVE`/`LOAD`/`DIR` arrive only if
the persistence milestone happens.

## Functions

- Numeric: `ABS`, `INT`, `SGN`, `SQR`, `SIN`, `COS`, `TAN`, `ATN`, `LOG`, `EXP`,
  `RND(x)`, `PEEK(addr)`, `FRE(0)` (bytes free), `TIMER` (seconds since boot as a
  float, no argument; **open**: name and units).
- String to number: `LEN`, `ASC`, `VAL`.
- Number to string: `STR$`, `CHR$`.
- String: `LEFT$`, `RIGHT$`, `MID$` (1-based).

## Number output

`PRINT` writes a number with a leading space where a minus sign would go, and a
trailing space. Integer-valued numbers print without a decimal point. Others
print with up to 9 significant digits, trailing zeros removed, positional
notation for magnitudes from 0.01 up to (but not including) 1e9, otherwise
scientific as `1.5E+20`. **(open: the exact rule; the golden tests will pin
whatever we choose.)**

## Errors

In immediate mode: `?<NAME> ERROR`. Inside a program: `?<NAME> ERROR IN <line>`.
After an error the program stops and the prompt returns.

`SYNTAX`, `TYPE MISMATCH`, `UNDEF'D STATEMENT`, `NEXT WITHOUT FOR`,
`RETURN WITHOUT GOSUB`, `OUT OF DATA`, `BAD SUBSCRIPT`, `REDIM'D ARRAY`,
`DIVISION BY ZERO`, `ILLEGAL QUANTITY`, `OVERFLOW`, `OUT OF MEMORY`.

## Open questions to settle

Rough order of importance:

1. Number printing rules (the biggest source of golden-test churn).
2. Tokenized program store versus storing plain text (D9 in `DESIGN.md`).
3. PEEK/POKE: what address space, if any, is safe to expose.
4. Full-length variable names versus two significant characters.
5. `^` associativity, and 16- versus 32-bit logical operators.
6. `ELSE`: leave out for authenticity, or add for sanity.
7. `PRINT` zone width, `TIMER` naming, maximum line length.
