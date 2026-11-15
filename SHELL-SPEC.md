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

## 3. Non-goals

POSIX sh compatibility (that is `/bin/sh`'s job); a `jq`-like query language; a general-purpose programming
language (use Lua); typed pipelines between programs (pipes carry bytes); floating point in the first version
(user programs have no FPU; INVARIANTS C7).

## 4. The value model (Decided, rules 1 to 8)

A **value** is one of: string, number, bool, null, list, map, **closure**, and (Open Q3) possibly an opaque **handle**.

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

The semantics follow `es`; this section fixes only what we change or add. The full grammar is written (with golden
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
  interactive use shows it and continues. (**Proposal**; Open Q5.)
* **Native scripts (Decided):** called as a **command**, a script follows command rules (output collected as lines,
  exit code as the status); loaded **in-process** with `import` (Decided; there is no `source`-style include) the local rules
  apply and it yields values.
* **Conditions (Decided 2026-11-15).** `if`, `while` and `until` take a **strict boolean**; anything else is an error.
  `succeeds (cmd)` turns a call into a boolean: true on exit 0, false on a non-zero exit (a failure), and an **error**
  if the command could not be started. It runs the command in **statement mode**: the output streams to the current
  stdout and nothing is collected, because only the exit status is wanted (write `> /dev/null` to silence it). The
  `try`/`catch` block form is the equivalent, and `catch` can tell the two
  cases apart by the failure's `kind` (`exit` or `spawn`). `&&`, `||` and `!` keep their success-based shell meaning.
* **Large output (Proposal; Open Q11).** Collecting happens only in value position, so a statement never collects.
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
  argument) a map or list is still an error (rule 2). **Accessor sugar** (`$m.key`, `$l(2)`) remains **Open Q1b**;
  until decided, `get-or`, `set-in`, `del-in`, `keys`, `len` are the access path, which is enough for golden tests.

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
literals; optional size suffixes (`4k`, `2M`) are **Open Q4**. Floating point waits for FP context saving
(INVARIANTS C7); soft-float is possible but not planned. Division semantics (integer or decimal) is **Open Q4**.

**Infix expressions (wanted: Decided 2026-11-15; shape: Proposal).** An expression island evaluates numbers and
booleans with infix operators and ordinary precedence, in any value position, as a condition, and as a value inside a
JSON-mode literal. Operands are variables, literals, nested calls in parentheses and nested islands. Operators:
arithmetic `+ - * / %` and unary `-`; ordering `< <= > >=` (numeric only, no coercion: a non-number is
an error); logic `and or not` on strict booleans, short-circuiting; `==` and `!=` are a **total structural equality** of any two values (Decided 2026-11-15; `eq` and `ne` are dropped)
(numbers compare by value, a string never equals a number, `null` is an ordinary operand, lists and maps compare structurally, and map equality ignores key order); `in` for membership; `matches` for patterns; `^` for string concatenation. `&&`, `||`
and `!` stay the success-based flow between commands. The markers are `calc` (yields a number, arithmetic only) and `is` (yields a boolean, arithmetic operands allowed), accepted in principle; the remaining details are **Open Q12**.

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
  return values, hooks. *Acceptance:* golden transcripts pass; on the common subset the output equals upstream `es`
  where it is installed on the host (a differential check, skipped with a message when absent); hook redefinition
  cannot change what a fake `Host` allows.
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

1. **Q1 (resolved 2026-11-15): blocks are `do ... end`.** **Q1b (direction agreed 2026-11-15: `.` is a general accessor):** `$m.key`, `$l.0`, module members (`text.slugify`;
   an `import` yields a map of functions), `$env.PATH`, and assignment through a path (`cfg.a.b = 5` is a functional
   update that rebinds `cfg`). Lean, not yet decided: no reserved key names (keys are arbitrary data, and JSON from
   outside will contain `status`, `len` and everything else); `.` selects data only and builtin operations stay
   functions, so nothing shadows anything; odd keys by quoting (`$m.'a.b'`), computed keys by `$m.($k)`; `$name.txt`
   is always an accessor, so concatenation is `^`, and applying `.` to a string is an error that names the fix;
   `$r.status` raises a `missing-key` failure when absent, and defaults stay `get-or`.
2. **Q2: system configuration format**: TOML or shell data literals (the owner passed; decide before SH6).
3. **Q3: handle values.** May a variable hold an opaque handle (a directory, a service connection), non-serializable
   like a closure? It makes the powerbox far more expressive, and it needs a rule for what a closure captures.
4. **Q4: number details**: division (integer or decimal), decimal scale and rounding, size suffixes.
5. **Q5: failure default** for scripts (stop on any uncaught failure, or only inside an explicit block).
6. **Q6 (resolved 2026-11-15): the shell is named `ish`**: "improved shell", and an honest "POSIX-ish" (it is only
   *ish* about POSIX). The script extension is `.ish`; `sh` stays free for the compat shell. Three characters. No command
   of that name exists on the machine checked; the known cost is search noise from the iSH iOS app (about 20,000 stars).
   Considered and set aside: `dunsh` (a pun on *dunce*, to go with *doofs*; the most distinctive), `mush`, `usher`, and
   `trash` (ruled out: `/usr/bin/trash` is a real command).
7. **Q7: shell-level job control** shape, once the terminal's foreground-job protocol exists.
8. **Q8 (resolved 2026-11-15): `ok` and `fail`.** There is no bare `return` (a POSIX `return 1` is a status; here it
   would return the value 1, a muscle-memory trap), and the diagnostic for it says to use `ok` or `fail`. `fail value
   [code]` takes a value, then an optional code.
9. **Q9 (resolved 2026-11-15): a strict-boolean `if`** with `succeeds (cmd)`, and `try`/`catch` as the block form.
10. **Q10 (resolved in part, 2026-11-15): `import` only**, no `source`-style include. Still open: the namespacing rules
    for `import`, and what a script run as a command does with a final non-command value (the lean: nothing is
    printed unless it prints; an interactive prompt echoes non-null values).
11. **Q11: large output.** The default capture cap (size and whether it is per call or per shell); the exact shape of
    `run`'s options map; the names and set of the stream-stage builtins (`each`, `fold`, `take`, others); whether
    early termination (`take`) closes the pipe so the producer sees a broken pipe.
12. **Q12: the infix marker and details** (2026-11-15: `calc` is accepted conceptually; the owner suggests a separate
    `is` for comparisons and booleans, for example `if (is $n != 0)`. Lean: `calc` yields a number and takes only
    arithmetic; `is` yields a boolean and may use arithmetic operands, so `(is $n + 1 > 3)` needs no nested `calc`;
    `(is not $done)`, `(is $x in $list)` and `(is $line matches 'ERROR*')` read as English; equality is a total
    structural `==` with no coercion, which would retire `eq`/`ne`. `calc` and `is` accepted in principle; structural `==` and the removal of `eq`/`ne` decided 2026-11-15.) `(= ...)` (terse, but reads oddly after an assignment: `n = (= $n + 1)`),
    or `(calc ...)` (readable, greppable); or no marker in conditions and assignments, found by a reserved-operator
    scan, which I think is too clever. Also: `/` (exact decimal, or integer on integers, with `//` for integer
    division), whether the operators exist as prefix functions for `map` and `fold` (`add`, `lt`, ...), and compound
    assignment (`+=`).

## 13. Shortcuts to record in `TIDY-UP.md` when built

No floating point (T-number to link: FP context saving); no streaming parsers; lexical `..` and the cwd-after-rename
deviation (shared with the libc, see the namespace decision); `/etc/trusted` as the temporary grant source; YAML
absent or a strict subset.
