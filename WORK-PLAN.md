# Work plan (second plan, agreed with the owner, 2026-11-17 night; the first plan is below as history)

**Standing instruction from the owner: carry on with this plan autonomously until it is complete or the owner interrupts, whichever
comes first. Do not stop to ask; decide, log (OS decision log, TIDY-UP) and go on. Merge green PRs. Do not vendor third-party
sources (they are fetched, pinned by sha256, never committed).** If the
conversation was compacted, read this file and `git log` / `gh pr list` to see where things stand, and tick items here as they land.

**Overnight instruction from the owner (2026-11-17): the owner is asleep. Keep working through plan 2 until one of these: you truly
cannot continue, the plan is complete, or the owner interrupts (which means they have woken). Do not stop to ask. For any important
decision, use your best judgement, write the decision down (the OS decision log and a line in the "Decisions to discuss in the
morning" list at the end of this file, with what was chosen, the alternative and why) and carry on. `fork()` is out of scope tonight
(owner: ignore it). Do not start kernel locking tasks, namespaces or anything else that needs the owner's choice. Merge green PRs.
Say plainly what was and was not run.**

## Plan 2 (written 2026-11-17, before bedtime; sizes are guesses)

Rules for the night: the lighter local gate (`make fmt-check lint docs-check`, the one targeted test, `make check` when the image or a
program list changes, `make os-serial-test` when console, sessions, auth, profiles or the trust list change, `-smp 4` only for kernel
work; CI runs the rest); never force-push (merge main into the branch); batch small slices into one PR; a fresh-context review for
kernel and authority slices; shortcuts into TIDY-UP.md; decisions into the OS decision log.

1. [ ] **Merge the docs PR** (this file, branch `workplan-update`).
2. [x] (done in PR 227) **Leftovers of plan 1** (small): host tests for `sscanf` and the signal table (T134(3)); the profile field that turns the spawn
   grant off (T136); say in TIDY-UP what is still open in T139.
3. [x] (done in PR 229; `LIBC-GAPS.md` has the measured result, T147 the shortcuts; `strtold` is left) **Complete libc, phase 1: measure, then the cheap batch.** Run `scripts/libc_gap_survey.py` over every fetched program (tcc, dash,
   coreutils, gnutar, busybox, gzip, sqlite) and write the ranked gap list into a document (`UV0-GAP-SURVEY.md` or a new
   `LIBC-GAPS.md`). Then, each with a C test (the host's libc as oracle where it can be) and a note in TIDY-UP: `getopt_long` moved
   from `third_party/compat` into the libc (with `getopt_long_only`, permutation, and a host differential test); `error.h`
   (`error`, `error_at_line`); `strings.h` (`bcmp`, `bcopy`, `index`, `rindex`); `alloca.h`; `creat`, `ftello`, `fscanf`;
   `execlp`/`execvp` over the spawn grant; `sigwait`; the `*at` family (`fstatat`, `unlinkat`, `mkdirat`, `renameat`, `symlinkat`,
   `readlinkat`, `fchmodat`, `linkat` as refused) on the existing path code; basic wide characters (`mbrtowc`/`wcrtomb` for UTF-8,
   `uchar.h`, `wcwidth`). Re-run the GNU tar measurement afterwards and record the new numbers in T145.
4. [x] (done in PR 228; T146 lists the rest) **F step 0b: names are UTF-8, through one `names` crate** (decided 2026-11-17: a small `no_std`, `forbid(unsafe_code)` crate owns the rule, `validate` first; `proto::file::valid_name` becomes a wrapper; invalid names are refused with `EILSEQ`, names are stored as given; normalised comparison is a later slice, T141, with no flags word). **F step 0b: names are UTF-8** (FS-SPEC F6, check 14; specified, mechanical; TIDY-UP T146 lists what has to follow): `proto::file::valid_name`,
   goldens, `memfs`, `doofs`, `rt::fs`, the libc.
5. [x] (done: findings fixed in PR 229, the rest in T148; policy items (1), (3) and (5) are for the owner) **An independent review of this session's work** (fresh context, against `INVARIANTS.md` and `SYSCALLS.md`): the spawn broker and
   the signal and terminal paths in `init/src/bin/sh.rs` and `consoled.rs`, the libc signal/vfork code, `memfsd` attributes. Fix what it
   finds; the slice I (AIA) review is also still owed.
6. [x] (finding, no code: the fetched tcc 0.9.27 has **no RISC-V back end**; see "Decisions to discuss in the morning") **`tcc` spike** (time-boxed; fetched, pinned): compile and link C on the OS with the libc and run it; needs the batch in 3. If it
   needs executable memory, say so and stop at "compiles to a file".
7. [ ] If time is left: the next slice in `ROADMAP.md` order (SD step 4, the remaining `ish` steps (SH6: TOML and YAML after Q2; SH7 is the hush port, no shell to write: T139 lists what is left), or L-task work, which are the
   owner's to choose: do not start the kernel locking tasks unasked).

## Long-range list (not scheduled)

- **A complete libc**: the C standard library as the gap survey can measure it, then POSIX base interfaces (locale and wide
  characters, `regex`, `iconv`, `mmap` and `fork` shims, `dlopen` refused), with the host's libc as the oracle wherever the behaviour
  is defined. Phase 1 is item 3 above; the measure is "every fetched program in `scripts/libc_gap_survey.py` reports no undeclared
  names, and the portable test corpus passes".
- GNU tar and GNU coreutils (both need gnulib on a libc with the interfaces above; T145), a real `/bin/sh` for interactive use, `tcc`.
- The remaining `ish` steps (SH1 to SH5 are done: `sh` is `ish`; SH6 is left; SH7 is the hush port), S10 namespaces and attributes, S11 compatibility, the S-series locking peels (L tasks 11 to 28).

## Plan 1 (history)

Order (the owner approved it; sizes are guesses, and tend to be over-estimates):

1. [x] **CI runs the shell test with real BusyBox** (PR 211, merged: a `kernel (shell test)` job and the soak workflow fetch BusyBox,
   cached by the manifest hash). Running it on CI found: the Ctrl-C ordering race, the spawn-channel key bug, and symbol tables
   making the image 50 MB (now `--strip-all`, 5 MB; T138).
2. [x] **Spawn-rate measurement** (UV8's open criterion): done in PR (branch spawn-rate): the shell test times 100 `xargs` spawns;
   found and fixed a duplicate-port-key bug in the shell's wait loops (12.9 s to 2.3 s for 100 spawns, about 44 per second in the
   debug build under QEMU). Recorded in TIDY-UP T136(9).
3. [~] **Host tests for the libc core** (done for `fnmatch`, `getopt`, `strverscmp`: `cfmt/tests/portable.rs`, a bug found; `sscanf` and the signal table are left) (TIDY-UP T134(3)): a host build of the pure parts (`getopt`, `fnmatch`, `sscanf`, `realpath`,
   the signal table) with their own tests.
4. [~] **Small signal and spawn leftovers** (done: `poll`/`select` interruptible, the signal table saved around `vfork`, bytes through the shell's start path; left: a profile field to turn the spawn grant off)  (T136/T137): `poll`/`select` interruptible; save and restore the signal table around
   `vfork`; bytes (not lossy UTF-8) through the shell's start path; a profile field to turn the spawn grant off.
5. [x] (done as a spike: hush runs scripts, files redirect, `system()`/`popen()` work, 25 scripts checked against the host sh; left in T139: here-documents, a `/bin/sh` name, interactive mode, a `configure` corpus) **(corrected: BusyBox `hush`, not `ash`: ash refuses to build without `fork`) `/bin/sh` is a port, not `ish` (SHELL-SPEC lines 23 and 465, SH7): a spike with BusyBox `ash` in its no-MMU mode** (not dash:
   dash needs a real `fork`). Needs `pipe()`, `dup2` and `fcntl` beyond fds 0 to 2, job control off. Acceptance (SH7): five real
   `configure` scripts run; dash's results are the second oracle on a shared corpus. Do the spawn rate first.
6. [x] **Ctrl-C as SIGINT** (T47, done) **and `termios` for programs** (T124, done for the foreground job: modes, size, `VMIN`/`VTIME`): the shell sends SIGINT to a program that has a handler for it and keeps the kill otherwise
   (BASIC's BREAK key needs it); then **`termios` for programs** (T124: raw mode, for `kilo` and friends).
7. [x] (done: PR 223; the library half, `pthread_cancel` and `alarm` in a CPU loop, is still open in T56) **Kernel `thread_interrupt`** (SCHED-SMP-SPEC task 26, T56): signals into a program in a CPU loop. **Owner's go-ahead
   needed**; its own PR; soak at `-smp 2` and `-smp 4`; Fable review.

Then: a first real port from `third_party/fetched` (**`kilo`, `sqlite3` and GNU `gzip` done** (PRs 220, 222, 224); `tar` is BusyBox's (PR 225) and GNU tar is deferred: T145), and the design-review slice queue before S8 (`ROADMAP.md`,
`reviews/`).

Working rules that apply throughout (CLAUDE.md, the owner's earlier instructions): a runnable acceptance check first; one mutation
check per new test; `make fmt-check lint docs-check` and `make check` before a push; `make os-serial-test` and the shell test at
`-smp 4` when the shell, libc or protocols change; a fresh-context Fable review for authority and kernel slices (they have found real
bugs twice: PR 208 and 209); shortcuts into TIDY-UP.md; tick ROADMAP boxes only when verified in QEMU; say plainly what was and was
not run.

## Decisions to discuss in the morning

(Add one line per decision taken without the owner: what, the alternative, why, where it is recorded.)

- **`tcc` cannot be our C compiler as pinned** (2026-11-17 night): `third_party/fetched/tcc` is tcc 0.9.27, whose source has back ends for i386, x86_64, arm, arm64, c67 and il, not RISC-V (`riscv64-gen.c` arrived after 0.9.27, in the development branch). So there was nothing to spike. Alternatives for "compile C on the OS", none started: tcc from its development branch (a git snapshot, not a release tarball: a pin and a hash from a commit archive), or QBE with `cproc` (QBE has a riscv64 back end; two small programs, no self-hosting of the libc headers), or neither (the OS is cross-built on the host, which is how everything works today). The pinned tcc entry in `third_party/fetch-manifest.txt` is therefore unused.
- **GNU tar is not the next port; the libc is** (owner's own call, recorded): after the libc batch only gnulib's own `localcharset.h` remains undeclared for it.
- **Review finding that needs a policy (T148 (1))**: a program that says it handles SIGINT and never waits can keep Ctrl-C from ever ending it. DECIDED 2026-11-18: SIGQUIT (Ctrl-\\) and SIGTSTP (Ctrl-Z) as the uncatchable escape, T149 (Ctrl-C stays cooperative). Options considered: a second Ctrl-C within a second, or a deadline on the hold. Not changed tonight.
- **Mount intersection by name (T148 (3))**: tighten `parse_trust` (ban `/tmp`, `/dev`, `/proc`) and intersect by volume; small, left for you because it changes `/etc/trusted` rules.
- **The smoke gate is at its 10-minute budget** (TESTING.md and `scripts/check_docs.py` cap a gate job at 10): the run on PR 229 was cancelled at 10 minutes after the boot test had passed (306 tests), during the release build; the suite and the image have grown and that runner was slow. I did not raise the cap (the docs check forbids it) and re-ran the job. The proper fix is yours to pick: move the release smoke (build + boot) into its own job, as the harness fail-demo already is, or trim the smoke boot.
- (Add any further night decisions here.)
