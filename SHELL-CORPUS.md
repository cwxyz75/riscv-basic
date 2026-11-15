# SHELL-CORPUS: realistic scripts for the native shell (draft v0, 2026-11-15)

Status: **a design probe, not a specification.** `SHELL-SPEC.md` fixes the value model and the call rules; it does not
fix a grammar. This file writes ten realistic scripts and a few interactive transcripts in a **placeholder syntax** to
find out where the model is awkward *before* the grammar is chosen. Everything in the placeholder syntax is
provisional; the findings at the end are the point. When the grammar exists (step SH0), these scripts become its first
golden transcripts.

The shell is called `ish` (SHELL-SPEC Q6, resolved: "improved shell", and "POSIX-ish"). Scripts are `.ish`.

## 0. The placeholder syntax (what the examples assume)

```
# comment
x = 5                            # assignment; the right side is in VALUE position
names = ls /etc                  # an external command in value position: a list of lines
n = len $names                   # a function call in value position: its returned value
v = foo (bar $x) baz             # ( ) EVALUATES its contents (grouping); it does not make a block
'literal'   'a' ^ $x             # single quotes are literal; ^ concatenates (as in rc)
{ "a": 1, "b": [1, 2] }  [1, 2]  # JSON literals (decided)
do ... end                       # a BLOCK (code that is not run yet); placeholder for SHELL-SPEC Q1
@ a b do ... end                 # an anonymous function;  fn name a b do ... end  defines one
if cond do A end                 # cond must be a boolean; also: if cond do A else B end
for x in LIST do ... end         # while cond do ... end
ok v        fail v [code]        # the two exits from a function (decided);  exit n  ends the process
try do ... end catch e do ... end        # an expression: yields the value of whichever block ran
succeeds (cmd)                   # true on exit 0, false on non-zero, an error if it cannot start (decided)
run (cmd) { options }            # { "status", "out", "err" }, never raises on non-zero (decided)
a | b      cmd > file   >[2=1]   # pipelines and redirection as in es
print v  eprint v                # print a value (strings raw, others as JSON) to stdout or stderr
GLOB                             # *.txt in a word position expands to a list (not yet designed)
```

Infix islands (SHELL-SPEC Q12, accepted in principle): `(calc $n + 1)` yields a number; `(is $n > 0)` yields a boolean and
may use arithmetic operands. Operators: `+ - * / %`, `== != < <= > >=`, `and or not`, `in`, `matches`, `^`; `==` is a
structural equality of any two values. The scripts below use them (rewritten 2026-11-15).

Library functions used and not yet specified are collected in finding 17.

## 1. `boot.ish`: start services from a manifest and check they came up

```
manifest = from-json (read-file /etc/boot.json)
started  = {}
for svc in (get-or $manifest services []) do
    name = get-or $svc name ''
    for dep in (get-or $svc needs []) do
        if (is not $dep in $started) do
            fail { "kind": "order", "service": $name, "missing": $dep } 3
        end
    end
    job     = spawn (get-or $svc cmd [])       # runs in the background; a job value
    started = set-in $started [$name] $job
    if (is not (wait-ready $name 5000)) do        # polls /svc/<name>; true or false
        eprint 'service did not come up:' $name
        fail { "kind": "timeout", "service": $name } 4
    end
end
print 'all services up:' (join ', ' (keys $started))
```

Exercises: JSON in, maps, `fail` with a map and a code, a job value stored inside a map.

## 2. `edit-config.ish`: change two nested keys and write the file back atomically

```
path = /etc/profiles/alice.json
cfg  = from-json (read-file $path)
cfg  = set-in $cfg [shell history size] 5000
cfg  = set-in $cfg [env EDITOR] kilo
tmp  = $path ^ '.new'
print (to-json $cfg { "indent": 2 }) > $tmp
mv $tmp $path                                   # rename over: atomic (FS-SPEC F4)
```

Exercises: immutable update by rebinding, a value printed into a file, atomic replace. (Note the unquoted words in
`[shell history size]`: a list literal of bare words is a list of strings; see finding 12.)

## 3. `testrun.ish`: run every test in parallel and summarise the failures

```
tests = /tests/*.test                            # GLOB
jobs  = []
for t in $tests do
    jobs = append $jobs { "test": $t, "job": (spawn-run $t) }     # background; a job value
end
failed = []
for j in $jobs do
    r = wait (get-or $j job null)                # r is a run map
    if (is (get-or $r status 1) != 0) do
        failed = append $failed { "test": (get-or $j test ''), "status": (get-or $r status 1), "err": (get-or $r err '') }
    end
end
if (is (len $failed) > 0) do
    print (to-json $failed { "indent": 2 })
    exit 1
end
print 'all' (len $tests) 'passed'
```

Exercises: a glob as a list, background jobs and `wait`, numeric comparison, exit code from a summary.

## 4. `bigscan.ish`: count ERROR lines per hour in a gigantic compressed log, streaming

```
counts = zcat /var/log/big.log.gz | fold {} @ acc line do
    if (is not $line matches 'ERROR*') do ok $acc end
    hour = slice (nth 1 (split ' ' $line)) 0 2
    ok (set-in $acc [$hour] (calc 1 + (get-or $acc $hour 0)))
end
print (to-json $counts)
```

Exercises: the large-output design (an external stage feeding an in-process `fold`), the pipeline's value being its
last stage's value, an early `ok` inside a function body.

## 5. `probe.ish`: a configure-style loop that tests which C functions exist

```
tmp = mktemp-dir
defer do rm -r $tmp end                           # cleanup when this script ends
results = {}
for feat in [posix_spawn clock_gettime strlcpy] do
    src = 'int main(void) { void *p = (void *)&' ^ $feat ^ '; return !p; }'
    print $src > $tmp ^ '/t.c'
    found = succeeds (cc -o $tmp/t $tmp/t.c > /dev/null >[2=1])
    results = set-in $results [$feat] $found
end
print (to-json $results { "indent": 2 }) > config.json
```

Exercises: `succeeds` in a loop, quoting C source, temp files, cleanup on exit.

## 6. `sandboxed.ish`: run an untrusted tool with narrow grants

```
grants = { "/work": "rw", "/tmp": "rw", "/svc/net": "r" }
r = confine $grants do
    run (tool --check /work/input.dat) { "max": 1048576 }
end
if (is (get-or $r status 1) == 0) do
    print 'ok'
else
    eprint (get-or $r err '')
    exit 2
end
```

Exercises: grants as a JSON map (a natural fit), a block yielding a value, `run` with an options map, if/else.

## 7. `profile.ish`: what a session runs at login

```
export EDITOR kilo
export PATH /bin                                 # /bin is a union; PATH only matters for ported software
fn prompt do
    ok ((short-cwd) ^ ' % ')                     # the prompt hook returns a VALUE
end
fn greet do
    print 'welcome,' (whoami)
end
greet
```

Exercises: a hook function whose result is a value, a function that prints, environment export.

## 8. `deploy-check.ish`: query a JSON endpoint and compare versions

```
url  = http://localhost:8080/version
resp = run (curl -s -f $url)
if (is (get-or $resp status 1) != 0) do
    fail { "kind": "unreachable", "url": $url } 5
end
info = try do
    from-json (get-or $resp out [])              # `out` is a list of lines: from-json joins them
end catch e do
    fail { "kind": "bad-json", "cause": $e } 7
end
want = '1.4.2'
have = get-or $info version ''
if (is $have != $want) do
    fail { "kind": "version", "have": $have, "want": $want } 6
end
print 'version ok' $have
```

Exercises: failure kinds as data, `try` as an expression, string equality distinct from numeric `==`.

## 9. `report.ish`: CSV in, grouped totals out as TSV, with exact decimals

```
rows   = from-csv (read-file sales.csv) { "header": true }      # a list of maps; every value is a string
totals = {}
for r in $rows do
    region = get-or $r region unknown
    amount = to-decimal (get-or $r amount '0')
    totals = set-in $totals [$region] (calc (get-or $totals $region 0) + $amount)
end
table = map (@ k do ok { "region": $k, "total": (get-or $totals $k 0) } end) (sort (keys $totals))
print (to-tsv $table { "columns": [region total] })
```

Exercises: CSV and TSV round trip, exact decimal arithmetic, a map built from expressions.

## 10. `slug.ish`: a small CLI tool with arguments, a usage error and an imported library

```
import lib/text.ish as text
fn usage do
    eprint 'usage: slug [-u] NAME...'
    fail null 2
end
flags = {}
names = []
for a in $args do
    if (is $a == '-u') do
        flags = set-in $flags [upper] true
    else if (starts-with $a -) do
        usage
    else
        names = append $names $a
    end
end
if (is (len $names) == 0) do usage end
for n in $names do
    s = text.slugify $n                          # a qualified call into the imported script
    print (if (get-or $flags upper false) do upper $s else $s end)     # if as an expression
end
```

Exercises: `$args`, `import`, a failure that becomes the process's exit code, if-as-expression, `else if`.

## 11. Interactive transcripts (what the prompt does)

```
% ls /bin                       # a statement: output streams
% names = ls /bin               # an assignment: nothing printed
% len $names
42                              # a non-null value is echoed at the prompt
% get-or { "a": 1 } b 0
0
% (ls /bin)                     # value position at the prompt: echoed as ...?  (finding 14)
% succeeds (grep -q root /etc/passwd)
true
% run (false)
{ "status": 1, "out": [], "err": "" }
```

## 12. Findings (ranked; each says what the scripts showed)

**High: these affect the grammar or the model.**

1. **Parentheses cannot be both grouping and blocks.** Writing `if (cond) (then)` is ambiguous: is `(then)` run now or
   later? In es every `{ }` is a deferred fragment and `if` is an ordinary function that runs the ones it wants, so
   there is no ambiguity there. With `( )` also meaning "evaluate this call in value position" (scripts 1 to 9 need
   that constantly), blocks need a *different* delimiter. **This reverses my earlier recommendation of parentheses for
   code blocks.** The examples use `do ... end` for blocks and `( )` for evaluation. **Decided 2026-11-15 (Q1): `do ... end`;** a keyword
   pair is the only option I found that works without a second punctuation pair.
2. **Prefix arithmetic and comparison is noisy (infix is wanted, Decided 2026-11-15; the marker is SHELL-SPEC Q12).** `if (not (== (get-or $r status 1) 0))` and
   `(+ 1 (get-or $acc $hour 0))` read badly after three levels. The scripts want an **infix expression form** for
   numbers and booleans, in some marked island (for example `(= $n + 1)` or `(= $a < $b and not $c)`), with `==`, `<`, `+`,
   `and`, `or`, `not` inside it. Needed for number handling (Q4) anyway; options to compare once we pick.
3. **The "command in last position" footgun is real.** `fn files d do ls $d end` followed by `x = files /tmp` prints
   the listing and leaves `x` as `null`, because the last command runs and prints (decided). That is correct and easy
   to get wrong. **Proposal:** a function called in value position whose last position is a bare command raises a
   diagnostic error (naming `ok (cmd)` and statement use), instead of silently returning `null`.
4. **There is no way to capture what a function printed.** Value position gives a function's *returned value*, never
   its output, so wrapping a printing function (script 7's `greet`) for capture needs a primitive such as
   `lines-of do greet end` (a block's statement-mode output as a list of lines). Needed in the stdlib.
5. **Pipeline failure semantics need a rule.** In script 4, if `zcat` fails midway while `fold` has consumed half the
   stream, which stage's failure is raised, and does `fold`'s value exist? Proposal: a pipeline raises the failure of the
   **first** stage that failed (a pipefail default), and an earlier stage's failure wins over a later one's.
6. **Scoping of assignment inside blocks is undefined.** In script 8 the value comes out of `try` as an expression
   precisely because assigning inside the block might not reach the enclosing scope. Needs a rule: lexical scope,
   `x = v` updates a visible `x` or creates a local, closures capture by value (values are immutable, so this is safe).

**Medium: the design handles it, but a decision or a primitive is missing.**

7. **Job values.** Scripts 1 and 3 need `spawn` and `wait`, with a job as a value: a non-serializable handle value, which
   argues **yes** to SHELL-SPEC Q3 (handles as values). It also stores jobs inside maps (`set-in $started [$name] $job`);
   `to-json` of such a map is an error, which is the right behaviour (rule 3).
8. **Persistent maps and lists.** Script 4's `fold` rebuilds a map per line; with plain copying that is quadratic. The
   evaluator's maps and lists must be persistent or copy-on-write with a unique-owner fast path.
9. **`read-file` and `write-file`.** `from-json (cat file)` round-trips through line splitting, so a JSON file's
   newlines and trailing newline are mangled and joined back. Scripts 1, 2 and 9 use `read-file` (one string) instead.
   Likewise `print x > file` stands in for a `write-file`.
10. **Accessor sugar and qualified names (Q1b).** `get-or $r status 1` everywhere, and `text.slugify`, show the need for
    a short path form (`$r.status`, `text.slugify`) with a default, such as `$r.status or 1`, kept explicit.
11. **Strings.** `^` concatenation is noisy for building messages and paths. The scripts also need a multi-line
    string (the C source in script 5 would be a here-document in sh). Options: interpolation in double quotes
    (`"dir/$name"`), or a formatting function. Needs a rule that also works inside JSON-mode literals.
12. **Bare words in list literals.** `[shell history size]` is easy to write and conflicts with "barewords are not
    values inside JSON literals" (SHELL-SPEC section 5). Either a separate word-list literal (`words shell history size`)
    or require quotes in all list literals; the scripts quoted some and not others, which is itself the finding.
13. **Expressions inside literals.** `{ "test": $t, "status": (get-or $r status 1) }` needs `( )` expressions as values
    inside JSON mode, not just `$x`. The spec said `$x` only; it should allow a parenthesized call.
14. **Interactive echo of a list.** `(ls /bin)` at the prompt: show lines, or JSON? A list of strings is friendlier as
    lines; any other value as JSON. Needs a rule.
15. **Globbing.** Script 3 expands `*.test` to a list in a word position. The spec is silent. A no-match glob should
    be an **error by default** (not a literal, not silently empty), with an explicit form to allow empty.
16. **Cleanup.** Scripts 5 and 7 want `defer do ... end` (run on script or function exit, success or failure).

**Low: surface decisions.**

17. **Stdlib inventory the corpus needed** (a first list, none of it specified yet): `get-or`, `has`, `keys`, `len`,
    `set-in`, `append`, `nth`, `slice`, `join`, `split`, `map`, `filter`, `fold`, `each`, `take`, `sort`, `matches`,
    `starts-with`, `upper`, `eq`, `to-decimal`, `from-json`, `to-json`, `from-csv`, `to-tsv`, `read-file`, `write-file`,
    `mktemp-dir`, `print`, `eprint`, `spawn`, `spawn-run`, `wait`, `wait-ready`, `run`, `succeeds`, `defer`, `confine`,
    `import`, `export`, `lines-of`, `cwd`, `whoami`.
18. **Two negations.** `not` for booleans and `!` for success-based flow (decided) will confuse; write both in the spec
    with a one-line rule each.
19. **`==` versus `eq` (resolved 2026-11-15: `==` is a total structural equality and `eq`/`ne` are dropped; the rest of this entry is superseded).** Numeric `==` and a separate string equality was decided in principle; scripts use both
    constantly, so the names should be settled together with the infix form (finding 2).
20. **`if` as an expression and `else if`.** Both were natural to write. Settle with the grammar.
21. **Environment and `PATH`.** With `/bin` a union, `PATH` is only for ported software; `export` is a statement but
    the environment is state. Probably the environment is also a map value (`env`), read-only in the shell.

## 13. What the corpus says about the model

* The decided rules held up: nothing needed a change to the value model, the position rule, `ok`/`fail`, or
  `succeeds`. `confine` grants as a JSON map and `run` options as a JSON map were pleasant.
* The pressure points are **syntax**, not semantics: blocks versus grouping (1), infix expressions (2), strings (11, 12,
  13) and accessors (10). Those are what the grammar step must settle before any evaluator exists.
* Three runtime behaviours the spec did not yet have: the diagnostic for a footgun (3), pipefail (5), job values (7).
