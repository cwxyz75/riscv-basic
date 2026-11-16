# SHELL-SPEC: the native shell (specification, draft v0, 2026-11-15)

Status: **draft written from the 2026-11-15 design discussion with the owner. Nothing is built.** Items marked
**Decided** were answered by the owner in that discussion; items marked **Proposal** are the drafter's and need a
yes; items under **Open** (section 12) are deliberately undecided. None of it is in the decision log
(`OS-ARCHITECTURE.md` section 17) yet. When accepted it amends **OD20** (the shell is a POSIX-ish powerbox):
the powerbox role stays, the POSIX-ish syntax does not.

Basis: the current shell is `init/src/bin/sh.rs` over the pure `shell-core` (a line grammar of `stage ('|' stage)*`,
quotes, `#` comments, lexical `cd`/`pwd`; no variables, no control flow, no expansion), the confinement rules of OD20
(confined by default; `/etc/trusted` names the programs that get a read-only root), the service directory (OD97)
and the terminal and session design discussed with this spec (section 9).

## 1. Roles

"The shell" is four jobs. They may be four programs.

| Role | Program | Why |
|---|---|---|
| Interactive default | **the native shell** (this spec) | ergonomics, and it has to understand tables, views and handles |
| Powerbox and launcher | **the native shell** | it decides what table and handles a child gets (OD20); that is authority, so it is ours |
| Internal scripting | **the native shell** | boot scripts, service manifests, test harnesses, in-OS tooling; one language we control |
| `/bin/sh` for ported software | **dash or BusyBox ash** (a port, not written here) | `configure`, `make`, installers; POSIX conformance only; also our conformance oracle |

A script runs with the authority of the shell that started it, so a ported `sh` script never widens anything.
`#!/bin/sh` selects the compat shell; our own scripts name the native shell in the shebang. Heavier logic goes to Lua
(already shipped), not into a second general-purpose language inside the shell.

## 2. Decisions taken in the discussion (all **Decided** by the owner)

1. The native shell is **es-style**: rc's semantics (no re-splitting of expansions; quoting only for syntax) plus
   closures, higher-order functions, exceptions, and syntax that is sugar over redefinable hook functions.
2. **dash/ash is `/bin/sh`**, compat only. Lua stays the answer for algorithms.
3. **The data model is the JSON data model plus closures** (section 4). Maps and nested lists are first-class.
4. **Pipelines stay byte streams.** Structure lives in values, and is converted at the edges by primitives.
5. Rules 1 to 8 of section 4 (one-element list is not a string; nested value as argument is an error; closures do not
   serialize; strings are UTF-8; numbers keep i64 and exact decimals; ordered maps with string keys and duplicate keys
   an error; datetimes are tagged strings for now; missing key and `null` are never ambiguous, with `get-or` as the
   default accessor).
6. Formats: **JSON, CSV, TSV first; TOML later; YAML only as a very strict subset (or not at all)**. Not a `jq` clone:
   basic parse, emit, access and update only.
7. Whether system configuration is TOML or shell data literals is **not decided** (the owner passed for now).
8. **JSON-first literals.** `{ }` is a **map literal** and `[ ]` a **list literal**, so a JSON document pasted into
   the shell is a valid value. The es use of `{ }` for a block of code is therefore **given up**, and the syntax for
   a code block is **`do ... end`** (decided 2026-11-15, after the script corpus showed that parentheses cannot be
   both blocks and grouping; `SHELL-CORPUS.md` finding 1). Parentheses mean grouping and evaluation in value position.
9. **Calls and results (Decided 2026-11-15; section 5).** Every callee returns a value; an external program called for
   its value returns its output as a list of lines (newline split); a command runs and streams in statement position
   and is captured in value position; a command in a function's last position runs and prints; a function has two
   exits (success with a value, failure with a value and a code) and `exit` ends the process; invalid UTF-8 output is an
   error; a native script run as a command follows command rules, loaded in-process it follows local rules.
10. **The two exits are `ok` and `fail`; `if` is strict-boolean with `succeeds` (and `try`/`catch`); scripts are
    loaded with `import` only** (Decided 2026-11-15; section 5).
11. **The shell is named `ish`** ("improved shell"), scripts `.ish` (Decided 2026-11-15; Q6).
12. **Grammar v2** (Decided 2026-11-16, in a syntax review with the owner; section 5.2). It keeps the data model and
    replaces the surface. A command takes words and an expression computes values, with one rule to tell them apart; `calc`
    and `is` go; bindings are created with `=` and changed only with `set`; backtick templates; `??` for a missing key;
    command prefixes (`succeeds`, `run`, `spawn`, `with-env`); jobs; `parse-args`; `lossy-lines`. The design test was:
    **every rule can be said in one sentence, without "except" and without examples**, and least surprise wins over
    ruthless consistency where they conflict (the one exception kept is rule I1). Where sections 5, 5.1 and 7 disagree
    with section 5.2, **5.2 wins**.

## 3. Non-goals

POSIX sh compatibility (that is `/bin/sh`'s job); a `jq`-like query language; a general-purpose programming
language (use Lua); typed pipelines between programs (pipes carry bytes); floating point in the first version
(user programs have no FPU; INVARIANTS C7).

## 4. The value model (Decided, rules 1 to 8)

A **value** is one of: string, number, bool, null, list, map, **closure**, and an opaque **handle** (Q3, resolved).

1. A one-element list is **not** the same as a string; `"a"` and `["a"]` differ. A command's arguments **splice**
   lists, one level, as rc does (`cmd $xs` passes each element as a word).
2. A list or map used as a **word** (an argument, a redirection target) is an **error**; serializing is explicit
   (`to-json $x`).
3. A closure, or any value containing one, cannot be serialized: `to-json` of it is an error. Same for handles.
4. **Strings are UTF-8.** File names are UTF-8 (the file servers enforce it). Bytes that are not text go through byte
   streams or an explicit encoding primitive (`to-base64`, `from-base64`).
5. **Numbers** are JSON-compatible text, but kept as two kinds: 64-bit integers and **exact decimals**; no silent
   conversion to a double. Overflow is an error, never wraparound. A numeric operator on a non-number is an error.
6. **Maps** are ordered by insertion, keys are strings, a duplicate key is an error (on parse and on construct).
7. **Datetimes** are strings carrying a tag until the time design exists.
8. **`get-or m key default`** is the default accessor, so a missing key can never be confused with a key holding
   `null`. The strict form that raises on a missing key has a different name (`require`).

Values are **immutable**; updates produce new values (`set-in`, `del-in`). There is no aliasing.

**Resource limits.** Every parser has caps on nesting depth, total size and expansion (section 6); input from a file
or a network is untrusted.

## 5. The language (**Proposal**, except where marked)

The semantics follow `es`; this section fixes only what we change or add. **Grammar v2 (section 5.2) supersedes the
surface syntax written here** (`succeeds (cmd)`, `run (cmd)`, the `calc`/`is` markers, "a command in the last position
runs and prints" for any callee); the semantics stand. The full grammar is written (with golden
transcripts) before the evaluator, in step SH0.

* **Commands and pipelines:** `cmd a b c`, `a | b`, `a ; b`, newlines, `&&`, `||`, `!`; redirection as in es
  (`> f`, `>> f`, `< f`, `>[2=1]`). Words are lists; `$x` splices; `$#x` counts; `~ subject pattern` matches.
* **Closures (Decided 2026-11-15):** a code block is written `do ... end`; it is a program fragment and a value.
  `@ a b do ... end` is an anonymous function and `fn name a b do ... end` defines one. **`( )` is grouping and
  evaluation** in value position, never a block. `{ }` is a map literal.
* **Everything is a callee that returns a value (Decided 2026-11-15).** The shell is data-structure first. A shell
  function or block returns its **last expression** or an explicit `ok value` (the pair is `ok`/`fail`, Decided; there
  is no bare `return`); with nothing to return it returns `null`. An **external program called for its value** returns its output as a **list of lines**
  (split on newline only, trailing newline dropped, empty output is the empty list, a single line is still a one-element
  list). **Output that is not valid UTF-8 is an error**; an explicit bytes capture exists for the rest. A variable
  returns its own value, so `$x` is a list as a value and splices only as a word.
* **Position decides how a command runs (Decided).** As a **statement** it runs and its output streams to the current
  stdout; the value is discarded, so nothing leaks into a result. In **value position** (right-hand side of an
  assignment, an argument, an explicit return) it is captured. A command in the **last position of a function** runs and
  prints and the function returns `null`, unless written as an explicit return of the capture, so `make` as the last
  line streams.
* **Two exits from a function (Decided):** a **success** carrying a value, and a **failure** carrying a value and an
  exit code (default 1): `ok value` and `fail value [code]`. A failure leaves **the function**, not the script. **`exit` is separate**
  and ends the process with a code. A non-zero exit of an external program is the same kind of failure.
* **Exceptions:** a failure *is* the exception mechanism (`throw` and the failure form are one thing); its value is
  any value, for example a map with `kind`, `message`, `where`. `catch` handles it. `run (cmd)` returns
  `{ "value" or "out", "err", "status" }` and never raises for a non-zero exit. A command that **cannot be started**
  (not found, no permission) is always an error and never a status.
* **Failure in scripts:** an uncaught failure stops a script, prints its value to stderr and exits with its code;
  interactive use shows it and continues. (**Decided** 2026-11-17, Q5.)
* **Native scripts (Decided):** called as a **command**, a script follows command rules (output collected as lines,
  exit code as the status); loaded **in-process** with `import` (Decided; there is no `source`-style include) the local rules
  apply and it yields values.
* **Conditions (Decided 2026-11-15).** `if`, `while` and `until` take a **strict boolean**; anything else is an error.
  `succeeds (cmd)` turns a call into a boolean: true on exit 0, false on a non-zero exit (a failure), and an **error**
  if the command could not be started. It runs the command in **statement mode**: the output streams to the current
  stdout and nothing is collected, because only the exit status is wanted (write `> /dev/null` to silence it). The
  `try`/`catch` block form is the equivalent, and `catch` can tell the two
  cases apart by the failure's `kind` (`exit` or `spawn`). `&&`, `||` and `!` keep their success-based shell meaning.
* **Large output (Decided 2026-11-17, Q11).** Collecting happens only in value position, so a statement never collects.
  For gigantic output: (1) **redirect** (`> file`, `> /dev/null`) or **pipe** it; a pipeline's value is its last
  stage's value and its intermediates are never collected; (2) **stream through an in-process stage**: `cmd | each
  (line) (...)`, `cmd | fold init (acc line) (...)`, `cmd | take n` are builtins that read lines from stdin as they
  arrive, so memory stays bounded, and they need no new value kind; (3) a **default capture cap** (per shell; the
  number is a tuning decision) makes an accidental huge capture fail loudly: the shell stops reading, closes the pipe
  so the producer sees it, and raises a failure of kind `capture-limit` instead of exhausting its job's quota;
  (4) `run (cmd) { "out": ..., "err": ..., "max": ... }` takes a JSON options map, where `out` and `err` are
  `"capture"` (default), `"inherit"`, `"discard"` or `{ "file": path }`.
* **Hooks:** syntax is sugar over hook functions (`%pipe`, `%and`, `%seq`, `%open`, and so on), redefinable in a scope.
  Redefining a hook changes behaviour, **not authority**: authority is the handles a shell holds and nothing else
  (INVARIANT to add, section 11).
* **Literals (Decided in principle):** `{ "a": 1, "b": [1, 2] }` and `[1, 2]` are values, read with JSON's own rules
  for strings (escapes, `\u`) and numbers, so valid JSON is always a valid literal. **Proposal:** the lexer switches
  to a JSON mode inside a literal; `$x` (and `$#x`) may appear where a JSON value may, so `{ "name": $n }` builds a
  map from a variable; barewords are **not** allowed as values, so a mistyped word is an error, never a string.
  Literal values pass to builtins, closures and the `data` primitives; at the **exec boundary** (an external program's
  argument) a map or list is still an error (rule 2). **Accessor sugar** (`$m.key`, `$l(2)`) is **Decided** (Q1b); `get-or`, `set-in`, `del-in`, `keys`, `len` remain the function forms.

## 5.1 Grammar v1 (as built; superseded by grammar v2, section 5.2)

This section records what `ish/` implements today, until step SH2b replaces it. Everything here is implemented in `ish/` (`parser.rs`) and pinned by `ish/tests/golden/*.t`. Where the sections above left a
shape open, the choice made is marked **(chosen)**; changing one is a small edit in the parser and the goldens.

**Lexical.** Scripts are UTF-8. `#` starts a comment at the start of a word; a backslash before a newline continues the line.
Newlines and `;` end statements; newlines are ignored inside `( )`, `[ ]`, `{ }` and after `|`, `&&`, `||`. `do`, `end`, `else`
and `catch` are reserved wherever they appear unquoted **(chosen: so `if c do ls end` works on one line)**; quote them to
use them as text. A **bareword** runs to whitespace or one of `( ) [ ] { } ; | & > < $ ' " ^`; one containing `*` or `?` is a
pattern the host expands (no match is an error). `'text'` is literal (`''` is a quote, newlines allowed); `"text"` is a JSON
string with escapes **everywhere** **(chosen: no interpolation, so a pasted JSON document means what it says; build text with
`^` or `format`)**. Adjacent pieces form one word; `^` joins words explicitly, with or without blanks.

**Variables.** `$name` (letters, digits, `_`), `$#name` (count), and a path: `$m.key`, `$l.0`, `$m.'a.b'`, `$m.($k)`, `$m.$k`.
Assignment targets are `name = value` and `name.key.key = value` (an update that rebinds `name`); the `=` must stand alone,
so `x=5` is a word.

**Statements.** `fn name params do ... end`, `if COND do ... [else if COND do ...] [else ...] end` (one `end` closes the whole
chain), `for x in WORDS do ... end` (lists splice one level), `while COND do ... end`, `until COND do ... end`,
`try do ... end catch e [code] do ... end`, `defer do ... end` (at function or script exit, last in first out), `break`,
`continue`; COND is one word and must be a boolean. Anything else is a **command**: `head word... redirection...`, joined into
pipelines (`|`), lists (`&&`, `||`) and negation (`! cmd`). A statement that is a single non-bareword word (`$x`, `(calc ..)`,
`'text'`, a literal) is an **expression statement**.

**Values in words.** `( ... )` evaluates statements in value position; `[ ... ]` and `{ ... }` are JSON-mode literals (strings
in `"`, numbers, `true false null`, `$x`, `( )`, nesting, commas; **a bareword is an error there**); `do ... end` is a closure
and `@ a b do ... end` a function; `(calc ...)` is a number and `(is ...)` a boolean. A lone bareword is text, except that in
value position it reads as a number, `true`, `false` or `null`; **assigning a lone bareword that names a function or builtin is
an error that says to write `(name)` or `'name'` (chosen)**. An assignment with more than one word on the right is a call:
`names = ls /bin` is the lines of `ls`.

**Calls.** The head names a shell function, a builtin or a program, in that order, or is a closure value (`$f x`) or
`module.function`. A shell callee gets its arguments as **values** (a list stays one list); a program gets **words** (a list
splices, a pattern expands, a map or closure is an error). Position decides the rest (section 5): statement, value (captured as
lines) or last-in-function. `ok v` leaves the nearest function (at script level it ends the script); `fail v [code]` raises.
`succeeds (cmd)` and `run (cmd) [options]` need the command in parentheses.

**Islands.** Inside `(calc ...)` or `(is ...)`: numbers (decimal, `0x`, `0b`), `'text'`, `"text"`, `$x`, literals, `true false
null`, and `( )`, which **groups** when what follows is an island expression ending at the `)` (`(calc ($a + 1) * 2)`), and is otherwise a **call** (`(len $x)`, `($f 1 2)`, `(calc ...)`); a lone `($x)` groups **(chosen, revised 2026-11-17 after review)**. Operators, low to high:
`or`, `and`, `not`, `== != < <= > >= in matches`, `^`, `+ -`, `* / // %`, unary `-`. `calc` must yield a number and `is` a boolean.

**Redirections.** `> f`, `>> f`, `< f`, `>[2=1]`, `>[2] f`; on shell callees too.

**Pipelines.** Programs are joined by host pipes. A shell callee may start a pipeline (its output is buffered, up to the capture
cap, then fed in) and `each`, `map`, `filter`, `fold`, `take` (or one shell function) may end it, reading lines as they arrive;
`take` closes the pipe and the producer's exit status is then ignored. A shell callee in the middle is an error **(chosen)**. A
pipeline fails if any stage fails, naming the first.

**Failures.** A failure has a value and a code and a kind: `exit` (a program exited non-zero), `spawn` (it could not be started:
never caught by `succeeds`, `&&`, `||`, `!` or `run`), `user` (`fail`), `error` (the language). `try ... catch` catches all of them.
An uncaught failure prints its value to stderr as `ish: ...` and ends the script with its code.

## 5.2 Grammar v2 (**Decided** 2026-11-16; not yet implemented: step SH2b)

One sentence per rule. **(new)** marks a change from grammar v1 (section 5.1), **(changed)** a revision of something v1
has; unmarked rules are as built. The value model (section 4) is unchanged.

**Values.**
* **V1.** A value is text, a number, a boolean, `null`, a list, a map, a closure or a handle, and every value is immutable.
* **V2.** A number is a 64-bit integer or an exact decimal kept in canonical form (`1.10` is `1.1`); arithmetic never
  converts a non-number, and overflow is an error.
* **V3.** `==` is structural equality: values of different kinds are never equal (`'1' == 1` is false), and maps compare
  without regard to key order; `in` uses the same equality. (A typed `==` that raised on a kind mismatch was considered
  and withdrawn: its rules for nested values and `in` could not be said in one sentence.)
* **V4.** A list, map, closure or handle cannot be a program's argument, and closures and handles cannot be serialized.

**Lexical.**
* **L1.** Scripts are UTF-8; `#` at the start of a word begins a comment; a backslash before a newline continues the line.
* **L2.** A newline or `;` ends a statement, except inside `( )`, `[ ]` and `{ }` and after `|`, `&&` and `||`.
* **L3. (changed)** Text is quoted in one of three ways: `'…'` takes every character as written (`''` writes a quote,
  newlines allowed); `"…"` is a JSON string with JSON escapes and no interpolation; `` `…` `` is a template (T1).
  `'…'` is the default in documentation and examples.
* **L4.** Pieces written next to each other form one word, and `^` joins words explicitly.
* **L5.** `do`, `end`, `else`, `catch` and `set` are reserved wherever they appear unquoted.

**Templates.**
* **T1. (new)** A backtick string is a template: `$name` inserts a variable and stops at the name (so `.` after it is
  text: `` `$base.txt` ``), `${expr}` inserts an expression (`` `${$args.0}` ``, `` `${len $xs}` ``), backslash escapes are
  JSON's plus `\$` and `` \` ``, and a template that starts with a newline drops that newline and the common indentation.
* **T2. (new)** A hole inserts text as it is and a number, boolean or `null` as its JSON text; a list, map, closure or
  handle in a hole is an error (join a list explicitly). `format` is dropped.

**The two modes.**
* **M1.** A statement that starts with a bareword, or with `$f` followed by an argument, is a **command**, and its words
  are text.
* **M2. (new)** These places are **expressions**: the right side of `=` and `set`, a condition up to `do`, the inside of
  `( )`, a slot of a `[ ]` or `{ }` literal, and a template hole.
* **M3. (new)** In an expression, a leading bareword, or a second token that is not an operator, makes it a **call**;
  anything else is a **formula**. (This replaces the v1 rule that a parenthesis in an island "groups when it can"; it also
  settles `>`, which is a comparison in a formula and a redirection in a call.)
* **M4. (new)** A call inside a formula is the whole expression or sits in parentheses (`if (len $xs) > 0 do`,
  `if has $m 'k' do`).
* **M5. (changed)** A bareword that reads as a JSON number, `true`, `false` or `null` is that value; any other bareword is
  text in a command and a call in an expression. A program receives a number word exactly as written (`echo 1.10`
  prints `1.10`).
* **M6. (new)** Parentheses and conditions run a lone bareword (`(date)`); the right side of `=` and `set` and a literal
  slot reject it, with an error that names both fixes (`'date'` for text, `(date)` to run it). Paths are not exempt: an
  exemption would make `./notes` and `notes` mean different things.

**Bindings.**
* **B1. (new)** `x = v` creates a binding, and it is an error if `x` is already bound in this function (the script's top
  level counts as one function; a parameter may reuse a top-level name).
* **B2. (new)** `set x = v` changes an existing binding, and it is an error if there is none; `set x.a.b = v` rebinds
  `x` to an updated copy.
* **B3. (new)** A binding made inside a block ends at its `end`, so a value chosen in a branch is written
  `x = if c do 1 else 2 end`.
* **B4.** Closures capture by value.

**Variables and paths.**
* **P1.** `$name` is a variable's value and `$#name` its length.
* **P2. (changed)** A path (`.key`, `.0`, `.'a.b'`, `.($k)`, `.$k`) can follow a variable or a parenthesized value
  (`(split ' ' $line).1`); a missing key raises `missing-key`, `.` on text is an error, and there are no negative indices.

**Commands.**
* **C1.** The head names a shell function, a builtin or a program, looked up in that order, or is a closure (`$f`) or a
  module member (`text.slugify`).
* **C2.** A shell callee receives values (a list stays one value); a program receives words (lists splice one level, a
  glob expands and no match is an error, a map, closure or handle is an error).
* **C3.** A command as a statement streams its output and has no value; in value position a program's output is its
  list of lines and a shell callee gives its result.
* **C4. (new)** `succeeds`, `run [options]`, `spawn [options]` and `with-env map` are prefixes that take the rest of the
  pipeline as their command (`found = succeeds grep -q root /etc/passwd`).
* **C5.** The `=` of an assignment stands alone, so `x=5` is a word.

**Expressions.**
* **E1. (changed)** Operators from loosest to tightest: `or`; `and`; `not`; `== != < <= > >= in matches`; `^`; `+ -`;
  `* / // %`; unary `-`; `??`.
* **E2.** Arithmetic and ordering take only numbers, and `and`, `or` and `not` take only booleans and short-circuit.
* **E3.** `/` is exact division (half-even at 18 places), `//` floor division and `%` its matching remainder.
* **E4.** `in` tests membership in a list (structurally) or a key in a map; `matches` tests text against a glob pattern.
* **E5. (new)** `p ?? d` gives `d` when the key or index in the path `p` is missing, and only then (a key holding `null`
  gives `null`); it is sugar for `get-or`.

**Control flow.**
* **F1.** `if C do … else if C do … else … end` is closed by one `end`; `while`, `until`, `break` and `continue` as usual.
* **F2. (changed)** A condition is an expression up to `do` and must be a boolean (`if $n > 3 and not $done do`).
* **F3.** `for x in WORDS do … end` takes command-mode words.
* **F4.** `try do … end catch e [code] do … end` gives the value of the block that ran.
* **F5.** `defer do … end` runs when the function or script exits, last in first out.

**Functions.**
* **N1.** `fn name params do … end` defines a function, `@ params do … end` is a closure, and `$args` holds the arguments.
* **N2. (stated)** A function's result is the value of its last statement; a call written as a statement has no value.
  (Unlike v1, this does not depend on what kind of callee the call names.)
* **N3. (new)** Using a function that has no value for its value is an error that names the fix
  (`total ended with a call statement, so it has no value; write (fold …) or ok (fold …)`).
* **N4.** `ok v` returns early, `fail v [code]` raises, `exit n` ends the process; there is no `return`.
* **N5.** `import path` returns a map of the script's top-level names; no search path, cached per run, a cycle is an error.

**Failures.**
* **X1. (changed)** A failure has a value, a code and a kind: `exit`, `spawn`, `user`, `error`, `missing-key`,
  `capture-limit`, `timeout` or `interrupt`.
* **X2.** `succeeds`, `run`, `&&`, `||` and `!` absorb `exit` failures and never `spawn` failures.
* **X3.** An uncaught failure stops a script and prints `ish: …` with its code; interactive use shows it and continues.
* **X4. (new)** `!`, `&&` and `||` join commands and `not`, `and` and `or` join booleans; using one where the other
  belongs is an error that names the right one.

**Jobs.**
* **J1. (new)** `job = spawn [options] cmd …` starts the rest of the pipeline (a program or a shell callee) in the
  background and returns a job handle at once.
* **J2. (new)** `spawn` takes `run`'s options map (`out`, `err`, `max`); output is passed through by default.
* **J3. (new)** `wait $job` returns the same map as `run` (`status`, `out`, `err`) and never raises for a non-zero exit, so
  `spawn` then `wait` is `run` in the background.
* **J4. (new)** `wait-any $jobs` waits for the first of a list of jobs and returns `{ "job": …, "result": … }`.
* **J5. (new)** `poll $job` returns `null` while the job runs and its result map once it has ended, without blocking.
* **J6. (new)** `kill $job` ends the job (the `KILL` right on its handle; no signals in v1).
* **J7. (new)** `wait { "timeout": ms } $job` raises `timeout` and leaves the job running; `run { "timeout": ms } cmd`
  kills the command, then raises `timeout`.
* **J8. (new)** When a script ends, for any reason, its unfinished jobs are killed, unless started with
  `spawn { "detach": true }`.
* **J9. (new)** An interrupt is an `interrupt` failure raised at the next statement boundary or inside a wait, so `defer`
  runs and `try` can catch it; a second interrupt ends the script at once, skipping the remaining `defer` blocks.
* **J10. (new)** A job is a handle (Q3): it can be stored and captured but not serialized, and a job started inside
  `confine` keeps the narrowed table.

**Arguments, environment and bytes.**
* **A1. (new)** `parse-args spec $args` returns a map: flags not given are `false`, `--help` prints the usage, and a bad
  flag prints the usage to stderr and fails with code 2.
  `parse-args { "usage": "slug [-u] [--out FILE] NAME...", "flags": { "-u": "upper", "--out": { "name": "out", "value": true } }, "rest": "names", "min": 1 } $args`
* **A2. (new)** `$env` is an ordinary binding: every child gets the value of `$env` when it starts, `set env.EDITOR =
  'kilo'` changes it for later children, and a value in it that is not text is an error naming the key; there is no
  `export`.
* **A3. (new)** `with-env map cmd` runs one command with those variables added to `$env`.
* **A4. (new)** `lossy-lines` is a stream stage that turns invalid UTF-8 into U+FFFD; plain lines stay strict, so
  corruption is never silent unless the script asks for it (`zcat big.log.gz | lossy-lines | fold …`).

**Pipelines and redirection.**
* **R1.** `>`, `>>`, `<`, `>[2=1]` and `>[2] f` work on any command, shell callees included.
* **R2.** A shell callee may start a pipeline and the stream stages (`each`, `filter`, `map`, `fold`, `take`,
  `lossy-lines`) may end one; any other shell callee in the middle is an error.
* **R3.** A pipeline fails if any stage fails, naming the first; when `take` closes the pipe the producer's status is ignored.
* **R4.** A capture is capped at 1 MiB unless `run`'s `max` option says otherwise.

**Interactive.**
* **I1. (new)** At the prompt, and only there, a top-level call to a builtin or shell function that returns a non-null
  value shows that value (`len $names` prints `42`); scripts are unaffected. This is the one exception to N2, kept
  because the prompt is where people look at values.
* **I2.** When stdin is not a terminal there is no prompt and no colour.

**Examples** (SHELL-CORPUS scripts 4 and 10 in grammar v2):

```
counts = zcat /var/log/big.log.gz | lossy-lines | fold {} @ acc line do
    if not $line matches 'ERROR*' do ok $acc end
    hour = slice (split ' ' $line).1 0 2
    set acc.($hour) = ($acc.($hour) ?? 0) + 1
    $acc
end
print (to-json $counts)
```

```
text = import lib/text.ish
opts = parse-args { "usage": "slug [-u] NAME...", "flags": { "-u": "upper" }, "rest": "names", "min": 1 } $args
for n in $opts.names do
    s = text.slugify $n
    print (if $opts.upper do (upper $s) else $s end)
end
eprint `slugged ${len $opts.names} names`
```

## 6. Data primitives (**Decided** scope; **Proposal** shape)

A pure crate **`data`** (`no_std` + `alloc`, `#![forbid(unsafe_code)]`, host-tested) holds the value type (without
closures, which belong to the evaluator), the parsers and the emitters. The shell's builtins are thin wrappers, and
standalone tools (`fromjson`, `tojson`, ...) use the same crate.

| Format | Scope | Independent oracle |
|---|---|---|
| JSON | full, including `\u` escapes and surrogate pairs; position in errors | JSONTestSuite (must-accept, must-reject, implementation-defined cases) |
| CSV | RFC 4180: quoted fields, embedded newlines and quotes; header option | RFC 4180 vectors, plus a parse-emit-parse property test |
| TSV | a defined escape scheme for tab, newline, backslash | property test |
| TOML | later (SH6); comment-preserving edit mode for in-place edits, canonical emitter for generated output | `toml-test` |
| YAML | only a very strict subset, or not at all: 1.2 core schema, no tags, no merge keys, alias expansion capped | `yaml-test-suite` (subset) |

Primitives: `from-json`, `to-json`, `from-csv`, `to-csv`, `from-tsv`, `to-tsv` (stream or word to value, and back),
`get-or`, `require`, `set-in`, `del-in`, `keys`, `values`, `len`, `has`. **Limits**: depth, size and (YAML) expansion
are parameters with defaults, and exceeding one is an error value. Streaming is not in the first version.

OD27 applies if a third-party parser is considered: a decision-log entry per crate. JSON and CSV we write.

## 7. Numbers (Decided rules; **Proposal** operators)

Separate numeric operators from string and pattern matching, so `10 < 9` is never a string comparison by accident
(`==`, `<`, `+`, ... are numeric; `~` stays the pattern match; string equality has its own spelling). Integers are
checked i64. Decimals are exact (parse, format and round-trip), with a defined scale and rounding. Hex and binary
literals; size suffixes are not in v1 (Q4). Floating point waits for FP context saving
(INVARIANTS C7); soft-float is possible but not planned. Division: `/` is exact decimal, `//` floor integer division (Q4).

**Infix expressions (wanted: Decided 2026-11-15; shape: Proposal).** An expression island evaluates numbers and
booleans with infix operators and ordinary precedence, in any value position, as a condition, and as a value inside a
JSON-mode literal. Operands are variables, literals, nested calls in parentheses and nested islands. Operators:
arithmetic `+ - * / %` and unary `-`; ordering `< <= > >=` (numeric only, no coercion: a non-number is
an error); logic `and or not` on strict booleans, short-circuiting; `==` and `!=` are a **total structural equality** of any two values (Decided 2026-11-15; `eq` and `ne` are dropped)
(numbers compare by value, a string never equals a number, `null` is an ordinary operand, lists and maps compare structurally, and map equality ignores key order); `in` for membership; `matches` for patterns; `^` for string concatenation. `&&`, `||`
and `!` stay the success-based flow between commands. The markers are `calc` (yields a number, arithmetic only) and `is` (yields a boolean, arithmetic operands allowed), accepted in principle; the remaining details are settled in Q12. **Superseded by grammar v2 (2026-11-16):** there are no markers; a formula is any expression that does not start with a call (section 5.2, M2 and M3).

## 8. Capability verbs (OD20 continues; **Proposal**)

* `ns` lists the process's namespace table (prefix and server); `/proc/self/ns` is the same data as text.
* `bind` and `mount` change **this shell's** table (and so its children's), using handles the shell holds.
* `confine <grants> { ... }` runs a fragment, or a command, with a narrowed table and views; grants are paths in the
  current table, `/svc` names, and rights. This is the powerbox as something a script can read.
* `/etc/trusted` stays the initial source of "programs that get a read-only root" until a grant syntax replaces it.
* A Shill-style **declared authority** for scripts (a header naming what the script needs) is a possible later layer
  and is not designed here; the hook mechanism leaves room for it.

## 9. Processes, the terminal and sessions

* **Spawn-native.** External commands start through the launcher with an explicit table, never `fork`.
* **The evaluator takes an explicit I/O context** (no process-global fd table), because fragments in a pipeline run as
  **threads** in the shell and each needs its own stdin and stdout. All machine access goes through a `Host` trait
  (spawn, open, pipe, wait, time), so the evaluator is pure and tested on the host with a fake `Host`.
* **Terminal** (design, discussed separately): the terminal is a server object with a pluggable backend (UART,
  framebuffer, channel/pty, network). Cooked mode for simple programs; **raw mode for the shell**. Control events
  (interrupt, suspend, hangup, resize) are protocol events the terminal delivers to its **foreground job**, which the
  shell sets by handing over a job handle with `KILL` (and later suspend). The libc maps events to signals for ported
  software. A session is a job, a namespace table built from a **profile**, an optional terminal and an environment;
  a user is a profile (servers never see identities); sessions are **detachable** (the job outlives the backend).
* **Interactive layer:** the `console` crate's editor becomes a library the shell uses in raw mode (history,
  completion aware of the namespace and of attributes, multi-line). It must work on a dumb serial console in QEMU.
  When stdin is not a terminal there are no prompts, no colour and stable messages (software-facing mode).

## 10. Steps, each with an acceptance check written before the code

* **SH0: this spec, the script corpus (`SHELL-CORPUS.md`, a first draft with findings), and the grammar with golden transcripts.** *Acceptance:* the owner accepts the open points; a
  transcript format (input, output, return value, exit status) is defined and a corpus exists before any evaluator.
* **SH1: the `data` crate: value type, JSON, CSV, TSV.** *Acceptance:* JSONTestSuite passes (every must-accept
  accepted, every must-reject rejected, implementation-defined cases documented); CSV and TSV pass vectors and a
  round-trip property test; depth, size and duplicate-key limits are tested; one mutation check per new test.
* **SH2: the evaluator on the host.** Lexer, parser, evaluator over a `Host` trait: lists, closures, exceptions,
  return values, hooks. *Acceptance:* golden transcripts pass; hook redefinition cannot change what a fake `Host`
  allows. (The differential check against upstream `es` is dropped as of 2026-11-16: grammar v2 shares too little with
  `es` for it to mean anything.)
* **SH2b: grammar v2 on the host** (section 5.2). *Acceptance, written before the code:* the goldens are rewritten in v2
  first (and fail on the v1 parser), one golden or more per rule marked new or changed, including every error message
  that names a fix (M6, B1, B2, N3, X4, T2); the ten SHELL-CORPUS scripts rewritten in v2, plus five new ones (a service
  supervisor with restarts, a tool using `parse-args`, a configuration file rendered from a template, cleanup on
  interrupt, a log scan over invalid UTF-8 with `lossy-lines`); **independent oracles**: the `data` crate against
  Python's `json` and `csv`, decimal arithmetic against Python's `decimal`, pipeline exit statuses against
  `bash -o pipefail` on commands both can run, and a parser fuzz run for SHI4. Jobs (J1 to J10) are tested on the fake
  `Host` here and in QEMU with SH4b.
* **SH4b: grammar v2 in the OS.** Jobs, `kill`, timeouts, interrupts and `$env` over the launcher. *Acceptance:* the
  shell test in QEMU (`-smp 2` and `-smp 4`, said plainly) plus steps for spawn, wait, kill, a timeout and an interrupted
  script whose `defer` runs.
* **SH3: numbers.** *Acceptance:* a table of arithmetic cases including overflow and decimal round-trips; operators
  never coerce a non-number.
* **SH4: in the OS, replacing `sh.rs`.** Launcher integration, pipelines, `confine`, `ns`, `/etc/trusted`, and every
  behaviour the suite checks today. *Acceptance:* the existing shell tests in the suite pass unchanged (QEMU, `-smp 2`
  and `-smp 4`, said plainly), plus transcripts for each new verb.
* **SH5: the interactive layer.** Needs the terminal server's raw mode. *Acceptance:* a scripted pty-style session
  through the serial console: editing, history, completion, interrupt, and the no-prompt mode on a pipe.
* **SH6: TOML and the strict YAML subset.** *Acceptance:* `toml-test` (and the chosen subset of `yaml-test-suite`).
* **SH7: the compat shell.** dash or BusyBox ash as `/bin/sh`, after the libc has `vfork` or `posix_spawn`.
  *Acceptance:* five real `configure` scripts run; dash's results agree with ours on a shared corpus.

## 11. Invariants to add when built

* **SHI1** A shell's authority is the handles it holds; no builtin, hook redefinition or value can create a handle
  the shell was not given.
* **SHI2** `confine` only narrows: the child table's rights are a subset of the parent's.
* **SHI3** A fragment in a pipeline sees only its own redirected streams.
* **SHI4** Parsers terminate on every input within their limits; a limit hit is an error value, never a panic.

## 12. Open points (stop and ask)

1. **Q1 (resolved 2026-11-15): blocks are `do ... end`.** **Q1b (resolved 2026-11-17): `.` is a general data accessor.**
   `$m.key`, `$l.0`, module members (`text.slugify`; an `import` yields a map of functions), `$env.PATH`, and assignment
   through a path (`cfg.a.b = 5` is a functional update that rebinds `cfg`). `.` selects data only and builtin
   operations stay functions, so nothing shadows anything; there are no reserved key names; odd keys are quoted
   (`$m.'a.b'`), computed keys are `$m.($k)`; `$name.txt` is always an accessor, so concatenation is `^`, and applying `.`
   to a string is an error that names the fix; a missing key or an out-of-range index raises a `missing-key` failure,
   and defaults stay `get-or`. No negative indices in v1.
2. **Q2: system configuration format**: TOML or shell data literals. **Deferred** (2026-11-17): decided before SH6.
3. **Q3 (resolved 2026-11-17): handles are values.** A variable may hold an opaque handle (a directory, a service
   connection). It cannot be serialized and is an error as a word, like a closure. Handles come only from named builtins
   (the directory and service openers, added in SH4), never from parsed text. A closure captures its variables by value,
   handles included; since a closure cannot be serialized it cannot carry authority out of the shell (SHI1 holds).
4. **Q4 (resolved 2026-11-17): number details.** `/` is exact decimal division; a result that does not terminate rounds
   half-even at 18 fractional digits. `//` is floor integer division and `%` the matching floor remainder. `+`, `-` and
   `*` are exact; exceeding the digit cap is an error. No size suffixes in v1 (`k` is ambiguous between 1000 and 1024;
   they are cheap to add later).
5. **Q5 (resolved 2026-11-17): failure default.** A script stops on an uncaught failure, prints its value to stderr and
   exits with its code; interactive use shows the failure and continues. Failures inside `succeeds`, `try`, `&&`, `||`
   and `!` do not stop a script. A pipeline fails if any stage fails (pipefail is the default), and the failure value
   names the stage.
6. **Q6 (resolved 2026-11-15): the shell is named `ish`**: "improved shell", and an honest "POSIX-ish" (it is only
   *ish* about POSIX). The script extension is `.ish`; `sh` stays free for the compat shell. Three characters. No command
   of that name exists on the machine checked; the known cost is search noise from the iSH iOS app (about 20,000 stars).
   Considered and set aside: `dunsh` (a pun on *dunce*, to go with *doofs*; the most distinctive), `mush`, `usher`, and
   `trash` (ruled out: `/usr/bin/trash` is a real command).
7. **Q7: shell-level job control** shape, once the terminal's foreground-job protocol exists. **Deferred** (2026-11-17): decided with SH5.
8. **Q8 (resolved 2026-11-15): `ok` and `fail`.** There is no bare `return` (a POSIX `return 1` is a status; here it
   would return the value 1, a muscle-memory trap), and the diagnostic for it says to use `ok` or `fail`. `fail value
   [code]` takes a value, then an optional code.
9. **Q9 (resolved 2026-11-15): a strict-boolean `if`** with `succeeds (cmd)`, and `try`/`catch` as the block form.
10. **Q10 (resolved 2026-11-17): `import` only**, no `source`-style include. `import path` returns a map of the script's
    top-level functions and values and is never bound implicitly. The path resolves against the importing script's
    directory, or is an absolute path in the shell's table; there is no search path in v1. Modules are cached per shell
    run; a cycle is an error. A script run as a command prints nothing for a final non-command value; the interactive
    prompt echoes non-null values.
11. **Q11 (resolved 2026-11-17): large output.** The default capture cap is 1 MiB per call, overridden by `run`'s `max`
    option. The stream-stage builtins in v1 are `each`, `filter`, `fold` and `take`. `take` closes the pipe, so the producer
    sees a broken pipe and its status 141 is not reported as a failure. The options map is as written in section 5.
12. **Q12 (resolved 2026-11-17): `calc` and `is`.** `calc` yields a number and takes only arithmetic; `is` yields a
    boolean and may use arithmetic operands (`(is $n + 1 > 3)`), so it needs no nested `calc`; `(is not $done)`,
    `(is $x in $list)` and `(is $line matches 'ERROR*')` read as English. `==` is a total structural equality with no
    coercion, which retires `eq`/`ne`. No operator-as-function names (`add`, `lt`) in v1, since `@ a b calc $a + $b`
    covers `map` and `fold`; no compound assignment (`+=`) in v1. **Amended 2026-11-16 by grammar v2:** the `calc` and
    `is` markers are dropped (section 5.2, M2 to M4: `if $n + 1 > 3 do`, `x = $a + $b`); `==` stays a total structural
    equality.
13. **Q13 (resolved 2026-11-16): grammar v2.** The owner's syntax review; the result is section 5.2 and decision 12 in
    section 2. Also settled there: bindings are created with `=` and changed with `set` (the scoping rule finding 6 of
    SHELL-CORPUS asked for), templates (finding 11), `??` (finding 10), the no-value error (finding 3), jobs (finding 7),
    `parse-args`, `with-env` and `lossy-lines`.

## 13. Shortcuts to record in `TIDY-UP.md` when built

No floating point (T-number to link: FP context saving); no streaming parsers; lexical `..` and the cwd-after-rename
deviation (shared with the libc, see the namespace decision); `/etc/trusted` as the temporary grant source; YAML
absent or a strict subset.
