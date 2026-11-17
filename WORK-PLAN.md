# Work plan (agreed with the owner, 2026-11-17)

**Standing instruction from the owner: carry on with this plan autonomously until it is complete or the owner interrupts, whichever
comes first. Do not stop to ask; decide, log (OS decision log, TIDY-UP) and go on. Merge green PRs. Do not vendor third-party
sources (they are fetched, pinned by sha256, never committed). The kernel step (7) needs the owner's go-ahead first.** If the
conversation was compacted, read this file and `git log` / `gh pr list` to see where things stand, and tick items here as they land.

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
7. [ ] **Kernel `thread_interrupt`** (SCHED-SMP-SPEC task 26, T56): signals into a program in a CPU loop. **Owner's go-ahead
   needed**; its own PR; soak at `-smp 2` and `-smp 4`; Fable review.

Then: a first real port from `third_party/fetched` (**`kilo` done**: PR kilo; next `gzip`, `gnutar`, `sqlite`), and the design-review slice queue before S8 (`ROADMAP.md`,
`reviews/`).

Working rules that apply throughout (CLAUDE.md, the owner's earlier instructions): a runnable acceptance check first; one mutation
check per new test; `make fmt-check lint docs-check` and `make check` before a push; `make os-serial-test` and the shell test at
`-smp 4` when the shell, libc or protocols change; a fresh-context Fable review for authority and kernel slices (they have found real
bugs twice: PR 208 and 209); shortcuts into TIDY-UP.md; tick ROADMAP boxes only when verified in QEMU; say plainly what was and was
not run.
