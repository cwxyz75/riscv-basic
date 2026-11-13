# S7: libc, threads on futexes, and ported software (spec, v2)

Status: v2, after a Fable review of v1 against the real code (findings folded in below). S7a, S7b
and all of S7c (the `cfmt` cores, the C ABI library with the first C program, files and time, threads) is built (see their sections for what differs from the plan); the rest is not. One question for the product owner is open (section 10): the C toolchain. This
file is the written acceptance contract for S7 (CLAUDE.md: every milestone needs a runnable
acceptance check, written before the code). Decisions are logged as OD67 to OD70 in
`OS-ARCHITECTURE.md` section 17 when the spec is accepted.

## 1. Goal and non-goals

**Goal.** Ordinary software runs on the OS without being rewritten for it. Two programs are the
proof: the `basic` interpreter crate (the home computer's language) as a userland program, and
**Lua 5.4** (plain C, the architecture's chosen "good libc test"), built against a C library written
in this repository. Along the way userland gets real threads: a futex primitive, and a mutex,
condition variable and `pthread` subset on top of it.

**Acceptance (ROADMAP S7, made runnable in section 8).** The BASIC golden corpus passes as a
userland program on QEMU, byte for byte; a Lua test script passes, with its expected output
produced by an independent build of the same Lua on the host.

**Non-goals (S7).** No `fork`, `exec`, `mmap`, signals or process server (S11); no sockets in the
libc (S8/S9); no `dlopen`; no `rename` (the file protocol has none: it returns `ENOSYS`, see T46);
no locale beyond `"C"`; no wide characters or `long double` (the `L` length modifier is rejected);
no `__thread` / `PT_TLS`, no `.init_array` constructors (the loader and libc do not run them; C code
must not rely on them); no floating-point hardware use (the user target stays `riscv64imac`, floats
are soft-float, which is bit-exact IEEE); no cross-process futexes; no Rust `std` port. The home
computer's *contract* (`home/BASIC.md`, the golden `.out` files, the boot menu) is untouched; its
crates may be refactored to share code (section 3).

## 2. Slices, in order

Each slice is one PR, green on `make check`, tests and acceptance written first, with an independent
(Fable) review for slices that add kernel authority or parse hostile input.

| Slice | What | Needs a C compiler? |
|---|---|---|
| **S7a** | `basic` as a userland program: interactive on the console, and a golden mode that runs the whole corpus on the target; stderr and terminal roles | no |
| **S7b** | Futex syscalls, `kcore::futex`, `rt::sync` (Mutex, Condvar, Once) | no |
| **S7c** | The C library: pure Rust cores (host-tested against the host's libc), the C-ABI layer with assembly variadic stubs, headers, startup, fd table, pthreads | for the C-side tests |
| **S7d** | Lua 5.4: vendored source, build, boot-image program, acceptance script | yes |
| **S7e** | The upstream Lua test suite's pure-language files under the same program | yes |

S7a and S7b, and the Rust cores of S7c, do not need the toolchain.

## 3. S7a: BASIC as a userland program

**Stdio plumbing first (B4).** Today a started program has `Stdin` and `Stdout` roles and nothing
else, and cannot tell a terminal from a pipe. S7a adds: a `Role::Stderr` (the launcher and shell pass
the same socket as stdout when there is no separate one), and the environment entry `STDIO=tty` set by
the shell when stdin/stdout are the console's terminal socket. On a terminal the console server
delivers whole lines plus in-band `0x04` (end of input at an empty line); on a pipe those bytes are
data. `isatty(fd)` is therefore `STDIO=tty && fd < 3`, and a line reader treats `0x04` at the start
of a line on a tty as end of file.

**The program `basic`** (boot image `bin/basic`) is the `basic` crate behind a stdio `Io`:
`write_str` to stdout, `read_line` assembles lines from the stdin *byte stream* (a line is bytes up to
`\n`; `0x04` or end of stream is `Line::Eof`), `millis` from `clock_get`, `break_pressed` always false
(the console server kills the foreground job on Ctrl-C unconditionally, so a runaway program can
only be stopped by losing it: a product decision recorded as T47, a console "raw break" mode is the
fix), `reboot` exits 0, screen control by the ANSI escapes the trait defaults emit, PEEK/POKE on a
private sparse byte map. No disk in interactive mode: `SAVE`/`LOAD` report the disk-absent errors
the language defines.

**Golden mode** (`basic --golden NAME`) runs one script of the corpus with exactly the semantics of
`home/basic/tests/golden.rs`: script lines are typed input, `<BREAK AFTER n>`, `<LOAD file>`, the
disk directives and `<FILE ...>` (a `cartfs` `MemDisk`), the sparse memory and the 250 ms-per-read
clock. To make that one implementation the scripted console moves out of the host test into a
`no_std` + `alloc` crate `basic-script` (`forbid(unsafe_code)`, `BTreeMap` instead of `HashMap`,
`<LOAD>` through a file-reading callback instead of `CARGO_MANIFEST_DIR`), used by both the host
golden test and the userland program. The `.out` files stay the only oracle and are not regenerated.

**As built (S7a).** The suite test `basic_golden_corpus_on_the_target` runs all 68 scripts and reports
*one* `##TEST` (it lists the first failing names), not 68; its negative control checks that two scripts'
transcripts are told apart, and a mutation (the clock step 250 to 251) was caught by the `timer` golden.
The shell passes `Role::Stderr` (a duplicate of stdout, so stdout handles now carry DUPLICATE) and sets
`STDIN=tty` / `STDOUT=tty` for the stages that are the terminal; `basic` honours `STDIN=tty` for Ctrl-D.
The interactive session is tested in `s4e_shell_test.py` (start, immediate statement, program, `RUN`,
Ctrl-D, `BYE`, back at the shell). The `basic` crates build at opt-level 2 for the user build.

**Who judges.** The program does not judge itself: it prints the transcript on stdout and the
**suite compares** it to `golden/NAME.out`, one `##TEST` per script (68 scripts). A negative
control runs one script against a deliberately corrupted expected output and must fail.

**Image and namespace.** `kernel/build.rs` copies the corpus into the boot image under `etc/basic/golden/`
(scripts and `.out` files) and `etc/basic/examples/` (about 160 entries, under the 256-entry cap; under
`etc/` and not `home/` so that `ls /home` and the top level stay as they were; the init banner omits them). The suite starts
`memfsd` as it does today and hands `basic --golden` a namespace with `/golden` and `/examples`
read-only; the shell is not involved (the shell gives programs only `.` and, for trusted ones, a
read-only `/`).

**Build profile.** The `home/*` crates build at opt-level 0 in the dev profile; `basic`, `cartfs`,
`libm` and `console` get an `opt-level = 2` override for the user build (soft-float on TCG is slow
and unoptimised code is stack-hungry).

**Risks, corrected.** Deeply nested expressions have no depth limit in `expr.rs` (only `GOSUB` depth
is bounded, in a heap `Vec`): a very deep parenthesis tower overflows the 256 KiB stack and kills the
process with a fault, where the host has a bigger stack. If a golden does that, the program gets a
larger stack (see S7c's 1 MiB main stack) rather than the golden changing. The largest golden arrays
are 40,000 elements: heap is not a risk (the per-program job allows 4096 pages in all, image and stack
included: about 15 MiB of heap). The corpus runs in **one** configuration (normal); SMP and AIA add
cost and no coverage for a pure program.

## 4. S7b: futexes and threads

**Syscalls** (`abi::syscall`, `SYSCALLS.md`, errors in order):

- `futex_wait(addr, expected, deadline)`: under the big kernel lock, read the 32-bit word at user
  address `addr` with **one aligned atomic load through the direct map** (a bytewise `copy_from_user`
  can tear); if it equals `expected`, sleep until woken or `deadline` (`0`: poll, `u64::MAX`: forever);
  otherwise `SHOULD_WAIT`. `addr` must be 4-aligned, mapped in the caller's address space and
  readable (`INVALID_ARGS`; a lazily backed page is committed first). The wake condition of the
  sleeper is "removed from the table", not "the value changed". `TIMED_OUT` at the deadline, never
  earlier. Spurious wakeups are allowed; callers re-check.
- `futex_wake(addr, count)`: wake up to `count` waiters on `addr` in the caller's process, FIFO;
  returns how many. The key is the pair (process, virtual address); a wake from another process on
  the same address wakes nobody. No rights are involved: naming a futex grants nothing a thread of
  the process could not do with its own memory.

**State.** `kcore::futex`: a pure table with FIFO queues and `wait`, `wake`, `forget(thread)` (a
killed, timed-out or torn-down waiter leaves at once). Host-tested against an independent reference
(a plain list) with a random model, and under loom/shuttle for the wait/wake race (the value check
and the enqueue are one step under the lock, a wake between a user store and a wait is not lost).
Kill, process teardown and SMP wake-up reuse the existing paths (the wake IPI of S5).

**Userland.** `rt::sync` gets `Mutex` (three-state futex mutex), `Condvar`, `Once`, `Semaphore`,
replacing the spin-and-yield locks in `rt` (the heap lock, T26) where a thread may sleep.

**Threads and limits.** A launcher-started program is limited to 4 threads in all (`PROGRAM_THREADS`,
the main thread counts) and a process to 8 (`MAX_THREADS_PER_PROCESS`). Threaded tests therefore run
under a **job the suite creates with room for 8**; raising the launcher's limit is a deliberate later
decision, not part of S7. A thread's stack is a 64 KiB mapping plus an unmapped guard page made by
`pthread_create`; `pthread_join` waits for the thread object's TERMINATED signal, then unmaps the
stack (a thread cannot unmap its own); a detached thread's stack is released when the process ends
(documented leak).

**As built (S7b).** `kcore::futex` (table, random model against a plain list, loom/shuttle test of
the check-then-enqueue step with a mutant that makes it fail), `futex_wait`/`futex_wake` (syscalls 110
and 111; the value is read by `usercopy::read_user_u32`, one aligned 64-bit load), a backstop that
forgets a process's sleepers at teardown, and a kernel check that no futex sleeper survives the test
run. `rt::sync` has `Mutex`, `Condvar`, `Once` and `Semaphore`, each with a deadline form; replacing
the spin lock in the `rt` heap (T26) is not done. The tests below pass at `-smp 1`, `2`, `4` and in
single-hart icount mode; a mutant that never sleeps fails three of them. Differences from the list below:
`futex_wake_counts` and `futex_wakes_fifo` are one test, `futex_wake_counts_and_fifo` (counts and the
other-process case first, where only the *set* of the two oldest woken is checked because two harts
record in either order, then FIFO one wake at a time); `mutex_excludes_across_harts` runs 3 contending
workers plus the main thread, not 4; `condvar_producer_consumer` moves 2,500 items, not 10,000;
`kill_a_futex_waiter` has the child say it is about to sleep, and checks the job's ledger plus the
kernel's end-of-run check that no sleeper survives (the "a later wake returns 0" check is covered by
`kcore::futex`'s unit tests). The loom/shuttle test models the kernel's check-then-enqueue step with a
lock; during development a mutant that read the word *before* taking the lock failed it, but it checks
the model, not `kernel/src/user.rs` (the QEMU tests do that). Review found and fixed a lost-wake bug
in `Mutex::lock_until` (it gave up on an OK wake past its deadline); the interleaving cannot be forced
in a test, so the fix is argued in a comment.

**Acceptance (QEMU).** Each proves *blocking*, not just completion (a futex that returns at once
would pass a spin loop):
`futex_argument_errors`; `futex_times_out_not_early` (no waker: `TIMED_OUT`, elapsed >= deadline);
`futex_wake_counts` (no waiters: 0; 3 waiters and count 2: returns 2 and the third is still blocked;
a wake from another process on the same address: 0); `futex_wakes_fifo` (one wake at a time, each
observed before the next); `kill_a_futex_waiter` (no leak, a later wake returns 0, frames/objects/job
exact); `mutex_excludes_across_harts` (4 threads, exact total) and `condvar_producer_consumer` (10,000
items, no lost wakeup) both with **timed** waits so a lost wakeup fails instead of hanging until the
240 s boot timeout. Run at `-smp 1`, `2`, `4` and in **icount mode on one hart** (the gate CLAUDE.md
sets for SMP work; multi-hart icount stays out, T21).

## 5. S7c: the C library

**Written in Rust, exported with the C ABI** (*decision OD67*), as a crate `libc/` whose pure logic
lives in small `no_std`, `forbid(unsafe_code)` core crates (host-tested) and whose `extern "C"` layer
is thin, with a `// SAFETY:` comment on every `unsafe`. The ABI crate is `#![no_builtins]` (otherwise
LLVM turns a Rust `strlen` or `memset` body into a call to itself) and does not define `mem*`
(`compiler_builtins` already exports them).

**As built (S7c-1).** The cores are the crate `cfmt/`: `format` (the printf engine, over an `Args`
trait the C layer implements on `va_list`), `strtod` (decimal by Rust's correctly rounded parser,
hexadecimal by our own half-to-even rounding), `strtol`, `strtoul`. `tests/differential.rs` compares
them with the host's libc over ~78,000 generated `snprintf` conversions and 60,000 generated
strings each for `strtod` and `strtol`/`strtoul`, all agreeing; two mutants (a dropped sticky bit in
hex rounding, a wrong `%g` exponent precision) are caught. `%a` with a precision that must round is
compared only on Linux (Apple's libc truncates where glibc rounds), so CI's glibc checks that corner.

**As built (S7c-2).** The crate `libc/` (package `oslibc`, user target only) exports the C ABI:
`string.h`, `ctype.h`, `errno.h`, `assert.h`, `setjmp.h`, `stdlib.h` (malloc over the runtime heap with a
16-byte size header, strto*, qsort as heapsort, bsearch, rand, atexit, exit, getenv), `math.h` (over
`libm`) and the first part of `stdio.h`: the printf family over `cfmt` and *unbuffered* output to
`stdout`/`stderr`. The variadic entry points are the assembly stubs described below (no C compiler
involved in the library itself); `setjmp`/`longjmp` are assembly. Headers are in `libc/include/`.
The C build: `init/build.rs` compiles `libc/tests/c/transcript.c` with `clang --target=riscv64-unknown-elf
-march=rv64imac -mabi=lp64 -ffreestanding -nostdlibinc -fno-pic -fno-stack-protector -O2`, archives it
(llvm-ar, else `ar`) and passes the archive to the final cargo/rust-lld link of the `ctest` binary: no
separate linker, as planned (OD69). The compiler comes from `CC_RISCV` or the usual places; without one
`ctest` is a stub that exits 99 and the suite says so. The acceptance test is
`c_transcript_matches_the_host`: the guest prints the program's output as `c|` lines and the harness
builds the *same* C file with the host's `cc` and libc, runs it, and compares line by line (86 lines
covering printf, strings, ctype, stdlib, malloc, math, setjmp and stdio). A mutant that inverts the
`qsort` comparison is caught at once. Not yet: `FILE` with buffering and files, the fd layer, time,
pthreads, per-thread `errno` (S7c-3).

**As built (S7c-3a).** `FILE` with real buffering (stdout line buffered on a terminal and fully
buffered otherwise, stderr unbuffered, files fully buffered, `setvbuf`, one push-back for `ungetc`,
`fseek`/`ftell`/`rewind` that account for read-ahead and pending output, flush of every stream at
`exit`), `fopen` with all the modes, `freopen`, `tmpfile`, `tmpnam`, `remove`, `perror`; the fd layer
(`open`, `read`, `write`, `lseek`, `close`, `unlink`, `rmdir`, `mkdir`, `stat`, `isatty`, 64 fds, EACCES
and friends from the file protocol's statuses; `rename` is ENOSYS, T46); `<time.h>` over a new
`cfmt::time` (gmtime, timegm/mktime, strftime in the C locale, UTC only) which is differential-tested
against the host's `gmtime_r`, `timegm` and `strftime`; `signal`/`raise` and `localeconv`/`setlocale`.
Headers added: `stdio.h` (full), `time.h`, `signal.h`, `locale.h`, `fcntl.h`, `unistd.h`,
`sys/stat.h`; all of them compile together under the freestanding RISC-V flags. The transcript test
now gives the program a namespace (`/` the read-only memfsd root, `.` its `/tmp`) and prints 120+
lines including file round trips, seeks, append, `r+`, `tmpfile`, unbuffered mode and time; the host
oracle runs in a scratch working directory. Deviations: files are relative to `.` (a program has no
ambient `/tmp`: `tmpfile` creates its scratch file in the working directory, T51); `struct stat` is
ours (size and kind); `sscanf` is not provided (nothing in Lua's import list needs it, to be confirmed
at S7d). Review (Fable) fixes folded in: `stat`/`unlink`/`mkdir` open parents with READ|WRITE only (and `stat`
uses the server's whole-path STAT), `freopen` reuses the stream object, a terminal's `0x04` is found
anywhere in a read (bytes before it first, the rest kept), append streams start at the end, each view of
a namespace connection numbers its requests from its own range, a failed `fseek` changes nothing, paths
are normalised (`./x`, `a//b`, trailing slash; an empty path is ENOENT), push-back survives a switch to
writing, and `bkl_is_released_on_user_entry_and_idle` (flaky under load, T49) now only requires the lock
to be free at least as often as held. Still to do in S7c-3b: pthreads, per-thread `errno`, the 1 MiB
main stack.

**As built (S7c-3b).** `pthread_create`/`join`/`detach`/`exit`/`self`/`equal` and attributes, mutexes
(normal, recursive, error-checking) and condition variables (with `pthread_cond_timedwait` against
`CLOCK_REALTIME`, which is time since boot) on the S7b futexes, `pthread_once`, thread-specific keys
(no destructors run), `sched_yield`, `nanosleep`, `sleep`, `usleep`, `clock_gettime`. A thread's state
(its `errno`, key values, join state) is a thread control block that `tp` points to: `run_main` sets it
for the main thread, the thread trampoline for the others (the kernel starts threads with `tp` = 0 and
keeps a thread's `tp` across traps); a library call from a thread this library did not create uses a
process-wide fallback `errno`. A thread's stack is 256 KiB mapped lazily at the top of a 512 KiB slot
(the lower half is an unmapped guard); `pthread_join` waits for the thread to announce its exit and for
its thread object to terminate, then unmaps the stack. `main` runs on a 1 MiB stack the library maps
itself. `<pthread.h>`, `<sched.h>` and the `time.h`/`unistd.h` additions are in `libc/include/`. The
transcript's threads section (a mutex-protected counter, joins with return values, per-thread `errno`,
a condition-variable queue with two consumers, `pthread_once`, keys, recursive mutex and `trylock`
from another thread, a detached thread, and a 700 KiB recursion that needs the big stack) matches the
host program under `-pthread` at one, two and four harts; mutants that make `errno` process-wide or
drop the big stack are caught. Not done (T52): stdio is not thread safe per stream, detached threads
and their stacks are reclaimed only at process end, key destructors, `pthread_cancel`, barriers, read-write
locks, thread stacks other than the default size.

**Variadics (B1).** Stable Rust cannot define a variadic function. `printf`, `fprintf`, `snprintf`,
`sprintf`, `sscanf` (only if Lua's import list needs it) are entered through **assembly stubs** written
with `global_asm!` (no C compiler needed): on lp64 the variadic arguments arrive in `a0`-`a7` then on
the caller's stack; the stub stores `a0`-`a7` in a save area directly below the entry stack pointer so
that the save area and the caller's stack arguments form one contiguous run of 8-byte slots, and tail-calls
the Rust `v*` function with a pointer into that run as its `va_list` (RISC-V's `va_list` is exactly
such a pointer). `vprintf` and friends take a C `va_list` the same way. Tested with C callers once the
toolchain is available and, before that, with Rust callers that build the same frames.

**Subset.** Mechanical, not recalled: the import list of the S7d objects (`llvm-nm -u`) must be
resolved completely, with no stubs beyond those named here. Planned content:

- `string.h`: `mem*` (from `compiler_builtins`), `str*` family, `strcoll`, `strerror`.
- `ctype.h`: the `"C"` tables. `stdlib.h`: `malloc`/`calloc`/`realloc`/`free` over the `rt` heap (size
  header, alignment 16; if Lua's allocation pattern is too slow under the first-fit allocator,
  size classes go in `malloc`, measured first), `strtol`/`strtoul`, `strtod` (decimal by Rust's
  correctly rounded parser, **hex floats by our own exact parser**, since Lua relies on them), `atoi`,
  `qsort`, `bsearch`, `rand`/`srand`, `exit`, `abort`, `atexit`, `getenv`, `system` (returns -1).
- `stdio.h`: `FILE` over the fd table with buffering (`fopen`, `freopen`, `fclose`, `fread`, `fwrite`,
  `fgets`, `fputs`, `fputc`, `getc`, `ungetc`, `fflush`, `fseek`, `ftell`, `feof`, `ferror`, `clearerr`,
  `setvbuf`, `tmpfile`, `tmpnam`, `remove`); `printf` family with `%d %i %u %x %X %o %c %s %p %f %F %e
  %E %g %G %a`, flags `-+ #0`, width, precision, `*`, and `hh h l ll z j t`.
- `math.h`: the `libm` crate (OD27) behind C names. `time.h`: `time`, `clock`, `difftime`, `gmtime`,
  `localtime` (UTC), `mktime`, `strftime` (C locale); wall time is `clock_get` plus an offset of 0
  (no RTC yet, documented). `locale.h`: `setlocale` accepts `"C"`/`""`, `localeconv`.
- `setjmp.h`: `setjmp`/`longjmp` in assembly (`ra`, `sp`, `s0`-`s11`; no FP state: no FPU).
  `signal.h` (`signal` records and returns `SIG_DFL`, `raise` aborts), `errno.h`, `assert.h`.
- Threads: `pthread_create/join/detach/self`, `pthread_mutex_*`, `pthread_cond_*`, `pthread_once`,
  `pthread_key_*` on S7b. Thread state (errno, the key table) is a block addressed by `tp`: the
  kernel saves and restores a user `tp` per trap and zeroes it at thread start, so `_start` and the
  thread trampoline set it, and libc calls from threads not made by `pthread_create` are unsupported.
- The fd layer (OS-ARCHITECTURE section 10): `open`, `close`, `read`, `write`, `lseek`, `unlink`,
  `mkdir`, `stat` (size, kind), `isatty`, over the file protocol and the namespace table (longest
  prefix, `..` rejected), fds 0-2 from the startup roles (S7a), a table of 64 (well inside the
  per-program object limit of 256). `rename` fails with `ENOSYS`.

**Startup and stacks.** `_start` reads the startup message (as `rt::startup` does), builds `argv` and
`environ`, installs the tables, switches to a **1 MiB main stack** it maps itself (Lua allows 200 nested
C calls and C buffers are large; the loader's fixed 256 KiB is too tight), runs `main`, calls `exit`.
The loader (`kcore::elf`) takes the existing three-segment layout unchanged.

**Link model (H1).** The C sources are compiled to objects with
`clang --target=riscv64-unknown-elf -march=rv64imac -mabi=lp64 -ffreestanding -nostdlibinc -fno-pic
-fno-stack-protector -O2 -c`, archived with `llvm-ar` (already in the Rust toolchain's llvm-tools),
and the **final link is the existing cargo/`rust-lld` link through `init/link.ld`**, as for every other
program: no separate linker, no `staticlib`, and `compiler_builtins` (soft-float, `mem*`), the
entry point and the allocator are present exactly once. `gp` is unused (`init/build.rs` passes
`--no-relax`); C keeps it that way. `-nostdinc` is *not* used: it would drop clang's own `stdarg.h`.

**Verification: the host libc is the oracle.** The formatting, parsing and string cores are
differential-tested on the host against the host's own libc through FFI: tens of thousands of
generated `snprintf` cases (every conversion, flag, width, precision, special value, `%a`),
`strtod`/`strtol` over generated and boundary strings, `qsort` against `std` sorting, `strftime` over
generated times. The host is **two** libcs (Apple's locally, glibc in CI): a case is a failure unless
*both* agree with ours, and platform-defined cases (`%p`, NULL `%s`, `strerror` text, some
`strftime`) are excluded by name in the test and listed in the libc's documentation. On the target, a C
test program compiled with the host `cc` and with the cross clang prints a transcript; the two must
be identical.

## 6. S7d: Lua

**Source.** Upstream Lua 5.4.x (MIT), unmodified, vendored under `third_party/lua/` with its licence
and the exact version and checksum recorded. Our only edits live in an override header (`luaconf`
settings: no POSIX, no `popen`, no `dlopen`, default `double`), used by both builds below.

**Build.** The cross clang builds `lua` and its objects against `libc/include/`; the Rust link makes
`bin/lua`, which joins the boot image. The host oracle is built from the same sources and override
header with the system `cc` (not the cross clang).

**Acceptance.** `tests/lua/s7.lua`, a script of ours covering the language (closures, metatables,
coroutines, goto, integer and float semantics, the string library including `format`, `pack` and
patterns), `math`, `table.sort`, `utf8`, `os.time`/`os.clock`/`os.getenv`, and `io` on files under
`/tmp` (the suite gives the program a namespace with `/tmp` read-write: the shell cannot, T33). The
**expected transcript is generated at test time by the host build** running the same script; the
two must match byte for byte (clock-dependent lines are normalised by the script). The Lua test also
checks the exit status and that the per-program job's ledger is back to zero.

**As built (S7d).** Lua 5.4.8 is vendored unmodified in `third_party/lua/` (`SOURCE.md` has the URL and the
SHA-256, matching lua.org's published value; `LICENSE` is the MIT text). `os_lua.h` (`-include`d by both builds)
is the only configuration: plain ISO C, no POSIX, so `lua_stdin_is_tty` answers 1 and `popen`/`dlopen` are
absent. `init/build.rs` compiles every source but `luac.c` with the cross clang into an archive linked into
`bin/lua` (a stub exiting 99 without a RISC-V clang, like `ctest`); the whole libc import list resolved on the
first try (`llvm-nm -u`), the soft-float and `mem*` helpers coming from `compiler_builtins`. The suite test
`lua_script_matches_the_host_build` runs `lua /etc/lua/s7.lua` with `/` read-only and `.` the writable `/tmp`,
prints the output as `lua|` lines (bytes outside printable ASCII as `\xNN`, the console shows nothing else),
and `scripts/os_boot_test.py` builds Lua from the same sources with the host `cc`, runs the script and compares
line by line. `tests/lua/s7.lua` covers arithmetic, strings (`format`, `pack`, patterns, `utf8`), tables and
`sort`, closures, `goto`, metatables, errors, coroutines, `math`, `os`, `io` on files, the garbage collector
and `load`; it prints nothing that differs between machines (no addresses, sorted `pairs`, `NaN` signs, clock
values, `rename` is left out: T46). The interactive REPL is checked by `s4e_shell_test.py` (start, a statement,
an expression, an error with its traceback, state kept, Ctrl-D, back at the shell). Findings: Lua's allocation
pattern made the runtime's first-fit heap quadratic (16,000 small tables took 1.4 s, 100,000 would take about a
minute), so `malloc` now keeps size-class free lists for blocks up to 512 bytes (16-byte classes, never returned to
the heap); that exposed a bug in the first version (`malloc(0)` blocks too small for the free-list link), caught
by a mutation check and now pinned by the C transcript (`classes`, `malloc(0)` neighbours). A write-only file
connection cannot ask the file server for its size, so `seek("end")` on a file opened for writing only fails
(T51). A program's memory stays capped at 16 MiB, which Lua notices (T53). The kernel gained a `fault-log`
feature that prints the pc, address and return address of a fatal user fault, which found the `malloc(0)` bug in
minutes.

## 7. S7e: the upstream Lua test suite

The upstream test files are a separate tarball and need `_port`/`_soft` set with `-e` and a few
adaptations, so this is its own slice: the pure-language files (`calls`, `closures`, `constructs`,
`events`, `goto`, `literals`, `locals`, `math`, `nextvar`, `pm`, `sort`, `strings`, `utf8`, `vararg`)
run under `bin/lua` with the documented flags, each its own `##TEST`. It is the strongest libc
oracle there is and may expose libc bugs S7d did not; its findings are fixed in the libc, not
patched around in the tests.

**As built (S7e).** The upstream test suite for 5.4.8 (`lua-5.4.8-tests.tar.gz`, checksum in `third_party/lua-tests/SOURCE.md`)
is vendored unmodified (the `.lua` files; its C libraries are not) and carried in the boot image under
`/etc/lua-tests`. Each file is its own `##TEST` (`lua_upstream_NAME`): the suite runs `lua -e "package.path=...;
_port=true _soft=true _nomsg=true" /etc/lua-tests/NAME.lua` with `/` read-only and `.` the writable /tmp, and the
file's own `assert`s are the judge (exit 0). 23 files run: calls, closure, constructs, events, goto, literals,
locals, math, nextvar, pm, sort, strings, utf8, vararg, bitwise, tpack, coroutine, errors, db, gc, gengc, cstack,
attrib. 22 passed the first time; `locals.lua` found the memory ceiling (T53 became R14, OD72): its stack-overflow
test needs about 70 MiB, so the machine is now 256 MiB, a program's heap 128 MiB and its job 136 MiB, and the shell
test's hog check follows the new numbers. Left out: `files.lua` (needs `rename` and `popen`, T46), `main.lua` (runs
the `lua` binary through `os.execute`), `api.lua`, `big.lua`, `heavy.lua`, `verybig.lua`, `tracegc.lua` and
`ltests/` (need Lua's internal test build or are only for memory-rich hosts), and `all.lua` (the driver the suite
replaces).

## 8. Acceptance (runnable checks, written before the code)

*S7a:* `basic_golden_corpus_on_the_target` (68 `##TEST`s, judged by the suite); `basic_golden_negative_control`;
`basic_interactive_session` (the serial shell test types a short program at `basic` and checks the
transcript, `BYE`, and Ctrl-D at an empty line); the host golden test still passes through
`basic-script`; stdio roles tests (stderr, `STDIO=tty`).
*S7b:* section 4.
*S7c:* the differential host tests; `libc_transcript_matches_the_host` on QEMU; fd-layer tests on a
`/tmp` namespace the suite builds (open/read/write/seek/unlink, `..` refused, the 64-fd limit,
`rename` is `ENOSYS`); `setjmp`/`longjmp` across a deep stack; variadic stubs against a Rust-built frame
and, with the toolchain, real C callers; pthread tests under the 8-thread job at `-smp 4`.
*S7d:* `lua_script_matches_the_host_build`. *S7e:* the upstream files.
No program may leak: frames, objects and the per-program job's ledger are checked exact after each
exits. The BASIC corpus and Lua run in the normal configuration; thread tests add SMP and icount.

## 9. Risks and what we accept

- **Size.** The boot image is about 4.9 MiB today; Lua adds a few hundred KiB and the corpus a few
  hundred KiB more. The 4 MiB per-program cap is not at risk; harness boot time is watched.
- **Soft-float speed.** Number-crunching tests are slow; budgets are per script.
- **`%a` and `strtod`** are where libcs differ; the two-host oracle settles them.
- **One big lock.** Threaded tests contend on the BKL (T11); correctness, not scaling, is claimed.
- **A libc is a large surface:** its C callers share the address space, so the risk is correctness, not
  confinement; the differential tests and exact-leak checks are the mitigation.

## 10. Open question for the product owner: the C toolchain

Lua is C, so S7c's C-side tests and S7d need a compiler that emits RV64 code. This machine has none:
Apple clang here has no RISC-V target, the Rust toolchain ships `rust-lld` and `llvm-ar` but no C
compiler, and neither `gcc` cross, `zig` nor `tcc` is installed. Options:

1. **clang with the RISC-V backend** (recommended): with the link model above only `clang` is needed,
   no `lld`. Locally `brew install llvm` (keg-only, so the Makefile takes a `CC_RISCV` path variable;
   installed size is over a gigabyte). In CI the Ubuntu runner already has clang; the version is
   pinned in the workflow. Every job that builds the boot image needs it once `bin/lua` is in the
   image (all six kernel jobs and local `make check`), and the Lua test is in the harness's required
   list so it cannot be skipped silently.
2. **`riscv64-unknown-elf-gcc`** (a cross GCC): works, and brings an independent soft-float `libgcc`,
   but also newlib headers that must be kept out of the include path; heavier to install.
3. **`zig cc`**: a single-download cross compiler, a new tool in the repo's story.
4. ~~`tcc`~~: its RISC-V backend is hard-float only and the kernel keeps the FPU off: unusable.

*Recommendation: option 1.* Installing it is a change to this machine, so it is the one action I will
ask you to approve or perform when work reaches the C side of S7c; S7a, S7b and the Rust cores of S7c
do not need it.
