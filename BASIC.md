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
