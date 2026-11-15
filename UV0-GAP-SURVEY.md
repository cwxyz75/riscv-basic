# UV0: the libc gap survey (first pass 2026-11-15; ground truth from real sources added in section 10)

Status: **a measurement, not a decision; first pass.** `USER-VIEW-SPEC.md` step UV0 asks for a table of the tier A and B
programs against what our libc and `rt` provide today. This is that table, built from **evidence on this machine**, with
the limits of the method stated in section 1. The acceptance check ("the table is committed and the policy table in
section 4 of the spec is corrected from it") is **met in part**: the table exists and section 7 lists the corrections, but
the ground-truth pass in section 8 (linking the real ports against our headers) has not been done, so the numbers below
are a strong indication and not yet a proof. Nothing here was built or booted; no OS code was touched.

## 1. Method and its limits

* **Our side.** `libc/src` exports (`extern "C"` functions, the assembly `setjmp` and variadic stubs) plus every function
  declared in `libc/include/*.h`: **198 names**. (Header declarations include things the toolchain supplies, such as
  `memcpy`, and the maths functions.)
* **Their side.** The libc symbols that **27 real programs import**, read with `nm -u` from the x86_64 slice of the
  binaries that ship on this Mac: `dash`, `sqlite3`, `tar`, `gzip`, `awk`, `sed`, `grep`, `less`, `vi`, `curl`, `ls`,
  `cp`, `mv`, `rm`, `chmod`, `cat`, `head`, `tail`, `wc`, `tr`, `cut`, `env`, `sort`, `diff`, `find`, `xargs`, `ps`, `bc`.
  Names were normalised (`stat$INODE64` to `stat`, `__error` to `errno`, `fstat64` to `fstat`, and so on) and
  macOS-specific, BSD-only and framework symbols were removed (`malloc_type_*`, `xo_*`, `acl_*`, `fts_*`, `libarchive`,
  `curses`/`termcap`, `readline`/`libedit`, `dl*`, `kqueue`, Mach calls).
* **What this is not.** These are macOS and BSD builds, not the programs we would port: BusyBox is not installed here
  (and its NOMMU build avoids `fork`), GNU `tar` and coreutils differ from `bsdtar` and BSD coreutils, `make`, `m4` and
  `sh` are small launcher stubs on this machine (five or six imports each) and were dropped. Directory walking in BSD (and gnulib) tools goes through
  `fts`, which was filtered as noise, so **directory-reading needs are understated**. `bsdtar` hides its real work in
  `libarchive`. Treat the per-program percentages as relative indications, and the *shape* of the gap as the finding.

## 2. What we have today

* **stdio, strings, stdlib, ctype, maths, `time.h`**: the `FILE` layer (`fopen`, `fclose`, `fread`, `fwrite`, `fgets`,
  `fputs`, `fseek`, `ftell`, `setvbuf`, `tmpfile`), the `printf` family (over `cfmt`), the common `str*` and `mem*`
  functions, `malloc`/`realloc`/`free`, `qsort`, `bsearch`, `strtod` and the `strto*` family, `localtime`, `gmtime`,
  `mktime`, `strftime`, `clock_gettime`.
* **A small fd layer** over the file protocol: `open`, `close`, `read`, `write`, `lseek`, `unlink`, `mkdir`, `rmdir`,
  `rename`, `isatty`, and `stat` returning **size and kind only** (`struct Stat { st_size, st_mode }`).
* **Threads**: pthreads over futexes (create, join, detach, mutex, cond, keys, once), `sched_yield`, `nanosleep`, `sleep`.
* **Odds**: `setjmp`/`longjmp`, `signal` and `raise`, `system`, `getenv`, `atexit`, `abort`, `exit`.
* **Below the libc**: the file protocol has `OPEN`, `CLONE`, `READDIR`, `MKDIR`, `UNLINK`, `RENAME`, `STAT`, `FSTAT`,
  `READ`, `WRITE`, `GETSIZE`, `SETSIZE`, `GETVMO`; `rt` can spawn through the launcher (`rt::launch::spawn_via`), wait on
  handles (`object_wait`, `port_wait`), make channels, and use futexes. The protocol has **no times, modes, owners,
  symlink, readlink, link, sync or locking**.

## 3. Per-program coverage (core symbols only)

| Proxy for | Program | Needs | Missing | Covered |
|---|---|---|---|---|
| shell | dash | 74 | 42 | 43% |
| database | sqlite3 | 138 | 56 | 59% |
| editor, pager | vi, less | 179, 64 | 88, 23 | 50%, 64% |
| archive | tar, gzip | 48, 57 | 12, 20 | 75%, 64% |
| text | awk, sed, grep, sort, diff, cut, tr, wc, head, tail | 10 to 98 | 4 to 41 | 55% to 78% |
| file tools | ls, cp, mv, rm, chmod, find, xargs, cat | 27 to 67 | 8 to 32 | 43% to 70% |
| system | ps, env, bc | 52, 27, 32 | 19, 13, 4 | 63%, 51%, 87% |
| network | curl | 70 | 17 | 75% |

**Altogether 224 distinct libc symbols are missing**, 114 of them needed by two or more programs, 38 by five or more and
8 by ten or more. The most widely needed (of 27): `optind` (20), `errx` (18), `err` (17), `warn` (16), `warnx` (14),
`fstat` (14), `optarg` (13), `getopt` (12), `access` (9), `sysconf`, `regcomp`, `regexec`, `nl_langinfo`, `lstat`,
`getopt_long`, `fcntl` (8 each).

The shape of the need, among these 27:

* **Process creation** (`fork`, `vfork`, `posix_spawn`, `exec*`): 7 programs (dash, vi, find, xargs, diff, mv, env).
  Most tools never start another program; **the shell, the launcher-style tools and the editor do.**
* **Signals** (`sigaction`, `sigprocmask`, `kill`, `alarm`, `sigsuspend`): 9 programs, mostly to catch `SIGINT`, `SIGPIPE`
  or `SIGWINCH`.
* **`mmap`**: 5 (sqlite3, grep, sort, diff, tail), mostly an optimisation with a fallback.
* **Terminal control** (`ioctl`, `tcgetattr`, `tcsetattr`): the editor, the pager and `ls`/`ps` for width.
* **Directory reading and file metadata** are the most widespread need of all (after allowing for the `fts` understatement).

## 4. The gaps by area, with a size and where each lands

Size: **small** (libc-only, or a thin wrapper over something that already exists), **medium** (needs a protocol change or a
new service, but no new kernel mechanism), **structural** (kernel, a design decision, or a large piece of software).

| Area | Missing symbols (examples; programs needing it) | What exists below | Size | Lands in |
|---|---|---|---|---|
| Arguments and diagnostics | `getopt`, `getopt_long`, `optind`, `optarg`, `err`/`errx`/`warn`/`warnx`, `setenv`, `unsetenv`, `basename`, `dirname` (20 programs) | nothing needed | **small**; pure code, host-testable against the host libc | UV0a |
| Strings and misc stdlib | `strcasecmp`, `strncasecmp`, `strtok`, `strsep`, `strlcpy`, `strlcat`, `strndup`, `stpncpy`, `strtoimax`, `asprintf`, `getline`, `fdopen`, `fseeko`, `ftello`, `random`, `atoll`, `strerror_r`, `isblank`, **`sscanf`** | `cfmt` is the print half | **small**, except `sscanf` (a scan core, **medium**) | UV0a |
| Directories | `opendir`, `readdir`, `closedir`, `dirfd`, `fnmatch`, `glob`, `getcwd`, `chdir`, `fchdir`, `realpath` | `READDIR` op, `rt::fs`; the cwd design is UV1 | **small** for iteration and `fnmatch`; `getcwd`/`chdir`/`realpath` wait for **UV1** (cwd string, lexical `..`) | UV0b, UV1 |
| File operations | `fstat`, `ftruncate`, `truncate`, `pread`, `pwrite`, `dup`, `dup2`, `pipe`, `fcntl` (flags, `F_DUPFD`), `mkstemp`, `mkdtemp`, `unlinkat`, `openat` | `FSTAT`, `SETSIZE`, the fd table, kernel channels | **small**; the `*at` calls wait for path-annotated fds in UV1 | UV0b |
| Sync and locks | `fsync`, `fdatasync`, `flock`, `lockf`, `fcntl` locks | no protocol op | **medium**: a `SYNC` op (F4 already means a real commit) and an advisory lock service | UV0b / S10 |
| Metadata and links | `lstat`, `access`, `chmod`, `fchmod`, `chown`, `fchown`, `umask`, `utimes`, `futimens`, `utime`, `symlink`, `readlink`, `link` | `Stat` is size and kind only; no time, mode, owner or symlink ops | **medium**: needs the advisory-attribute design (**UV3**) and symlink ops in the protocol (S10) | UV3 |
| Identity | `getuid`, `geteuid`, `getgid`, `getegid`, `getpwuid`, `getpwnam`, `getgrnam`, `getgrgid` | none | **small**, synthesized from the profile once sessions exist | UV5 |
| Process | `getpid`, `getppid`, `getrlimit`, `setrlimit`, `getrusage`, `times` | startup message, job quotas | **small** (stubs and data the OS already has) | UV0b |
| Process | `posix_spawn*`, `execve`, `execvp`, `execl`, `vfork`, `waitpid`, `wait`, `popen`, `pclose` | launcher client, handle waits, the process server (PID rule is Q5) | **medium**: fd and table inheritance mapping, PID design | UV8 |
| Process | `fork`, `getpgrp`, `setpgid`, `setsid`, `tcgetpgrp` | COW VMOs are only *room left* (OD8) | **structural**: the S11 `fork` shim; process groups emulated over UV4 | S11 |
| Signals | `sigaction`, `sigprocmask`, `sigsuspend`, `kill`, `killpg`, `alarm`, `sigsetjmp`, `siglongjmp`, `strsignal`, `sigaltstack` | `signal` and `raise` only | **structural**: the signals subset needs protocol events delivered to a thread, plus the kernel hook | S11 / UV4 |
| Terminal | `ioctl` (`TIOCGWINSZ`), `tcgetattr`, `tcsetattr`, `tcflush`, `cf*`, `ttyname`, `openpty`, `grantpt`, `ptsname` | cooked-only `consoled` | **medium** for termios over a raw-mode terminal, **structural** for the pty backend | UV4 |
| Terminal library | `tgetent`, `tputs`, `tgoto`, curses (the editor and pager both use it) | none | a **port** (a minimal terminfo reader or ncurses) | with UV4 |
| Waiting | `select`, `poll` | `object_wait`, `port_wait` | **medium**: a shim over waiting on many handles, across files, pipes, terminals and sockets | UV4 |
| Memory | `mmap`, `munmap`, `mprotect`, `madvise`, `sysconf` (`_SC_PAGESIZE`, CPU count), `getpagesize` | VMOs, `GETVMO` | `sysconf` and `getpagesize` are **small**; `mmap` is **structural** (S11 VM work) | S11 |
| Time | `gettimeofday`, `tzset`, `strptime`, `ctime` | `clock_gettime`, `localtime` | **small** (the timezone database arrives later) | UV0a |
| Regex and locale | `regcomp`, `regexec`, `regfree`, `regerror`, `nl_langinfo`, `mbrtowc`, `mbtowc`, `mblen`, `wcwidth`, `rpmatch` | none | **medium**: a POSIX regex engine (write or port, with an independent test suite), and UTF-8-only multibyte stubs for the C locale | UV0a |
| Network | `socket`, `connect`, `getaddrinfo`, `getsockopt`, ... | `/svc/net` UDP and echo; no TCP yet | wait for S9 | S9 |
| Dynamic loading | `dlopen`, `dlsym` | nothing | **structural**; the open question about dynamic linking (Q2) | later |
| Threads | beyond what we have: `pthread_rwlock_*`, `pthread_barrier_*`, `sem_*`, `pthread_sigmask`; thread-local storage (`__thread`) | futexes | **medium** (locks, barriers); TLS is **structural** | tier D |

## 5. What this says

1. **Most of the gap is small.** Roughly half of the 224 missing symbols are pure library code (arguments, diagnostics,
   strings, `sscanf`, time, regex) or thin wrappers over operations the file protocol and `rt` already have. This is one
   large but low-risk pass, testable on the host against the host's own libc.
2. **The medium gaps are protocol work, not kernel work.** File times, modes, symlinks, sync and locking all need the
   file protocol to grow (and the attribute design, UV3). They are the same work the FS-SPEC and the metadata decision
   already point at.
3. **The structural gaps are few and well-known**: `fork`, signals, `mmap`, pty/process groups, dynamic loading, TLS.
   None is needed by the sample's *ordinary tools*; they are needed by shells, the editor, and database or compiler
   software. That matches the ladder: tier A without them, tiers C and D with them.
4. **A shell is the demanding client.** dash alone needs `fork`/`vfork`, `execve`, `pipe`, `dup2`, `waitpid`,
   `sigaction`, `sigprocmask`, `sigsuspend`, `kill`, process groups, `tcgetpgrp`, `getrlimit`, `opendir`, `fnmatch` and the
   identity calls. **This is why BusyBox NOMMU first is the right bet:** its `vfork`-based design removes `fork` from the
   list, leaving signals and process groups.
5. **`fstat` is the most widely needed single call** (14 of 27) and the protocol already has `FSTAT`; that is the cheapest
   large win. `getopt` and the `err` family are next (13 to 20 programs) and cost almost nothing.

## 6. Suggested order (each a small PR with an acceptance check written first)

* **UV0a: pure libc additions.** `getopt` family, `err` family, the string and stdlib misc above, `sscanf`, `gettimeofday`
  and friends, the C-locale multibyte stubs, `fnmatch`, and a POSIX regex engine. *Acceptance:* host tests against the
  host libc as the independent oracle (for `getopt`, `regcomp`, `sscanf`, `strptime`, `fnmatch`) and an upstream
  conformance suite for regex; the suite grows a test per function family.
* **UV0b: fd, directory and process-lite calls.** `fstat`, `opendir`/`readdir`/`closedir`, `ftruncate`, `pread`/`pwrite`,
  `dup`/`dup2`/`pipe`, `fcntl` flags, `mkstemp`, `getpid`/`getppid`, `getrlimit`, `sysconf`. *Acceptance:* suite tests in
  QEMU at `-smp 2` and `-smp 4`, said plainly.
* **Then the slices already in the spec:** UV1 (`..`, cwd, `*at`), UV3 (metadata), UV4 (terminal, `ioctl`, `poll`),
  UV5 (identity), UV8 (spawn, `waitpid`, BusyBox), and S11 for `fork`, signals and `mmap`.

## 7. Corrections to `USER-VIEW-SPEC.md` section 4 (not yet applied)

* Add **`getopt`/`getopt_long`, the `err` family, `regcomp`/`regexec`, `sscanf`, `sysconf`, `fcntl`, `access`, `fstat`,
  `lstat`** as explicit "provided" rows; the survey shows they are among the most needed and none is in the table.
* **`fstat` is not metadata work.** It is needed now and the protocol has `FSTAT`; only the *fields* wait for UV3.
* The "Terminal" row should add the **terminal library** (a minimal terminfo reader or a port) as its own item; the editor
  and pager need it, and it is separate from `termios`.
* The "Process creation" row is accurate, but note that **only the shell-class programs and a few tools** need it in this
  sample; most tools never spawn.
* Record **`sscanf` and `strptime`** as library work that is missing today.

## 8. Next: ground truth (done at the header level: see section 10)

The numbers above come from proxies. The reliable check is to **compile the real targets against our headers and list their
undefined symbols**: `llvm-nm -u` on objects built for `riscv64` with `-I libc/include`. The targets: BusyBox (NOMMU
config), dash, GNU coreutils and tar and gzip, the SQLite amalgamation, kilo, tcc, and (Lua already runs). That needs the
sources, which I have not downloaded; downloading each one needs your go-ahead (name, source, size). The result would
replace section 3 with exact per-program lists and turn this document into the acceptance table.

## 9. Appendix: how the data was produced

The first pass used throwaway scripts over `nm -u -arch x86_64` (not kept); the second pass (section 10) is `scripts/libc_gap_survey.py`;
the provided list is `libc/src` exports plus header declarations (198). Raw numbers are reproducible on any Mac with the
same binaries; they will differ on another OS version.

## 10. Ground truth, second pass (2026-11-15): the real sources against our headers

**Method.** `make fetch-third-party` downloads eight sources into `third_party/fetched/` (ignored by git; the hashes are
pinned in `third_party/fetch-manifest.txt`; BusyBox's own published checksum matches). Each source file is compiled
**syntax-only** with the project's riscv64 clang against `libc/include`: nothing is linked, run, configured or generated.
A missing header is replaced by an empty stand-in so the compile continues, and clang's diagnostics then list what is
**undeclared by our headers**: functions, constants and variables, types, and whole system headers. Names that are not
system interfaces (a program's own functions, undeclared only because their generated headers are stand-ins) are dropped
by keeping only names the macOS SDK declares, and names our headers already declare are dropped.

**Limits.**
(a) Configuration macros are absent, so code behind `HAVE_*` (GNU) and `ENABLE_*` (BusyBox) is not compiled: counts are
**lower bounds** for fully configured builds.
(b) coreutils, tar and gzip use gnulib, which replaces some functions at build time; a few listed names would be supplied
by gnulib on our system.
(c) SQLite is built with `SQLITE_OMIT_LOAD_EXTENSION` and without readline; BusyBox is 242 files (libbb, ash, the
coreutils and findutils applets, tar, gzip, sed, awk, vi, diff, less, ps) under default feature macros.
(d) "Declared by our headers" is not "implemented": a name we declare but do not export still fails at link time, and
that is not measured here. A few common words that are not libc names (`copy`, `to`, `major`) slip through the filter.

| Program (files) | Functions | Constants and variables | Types | System headers missing |
|---|---|---|---|---|
| kilo (1) | 6 | 18 | 0 | 4 |
| tcc (7) | 5 | 15 | 6 | 3 |
| gzip (12) | 13 | 28 | 3 | 3 |
| SQLite, library and CLI (2) | 21 | 33 | 6 | 9 |
| dash (28) | 42 | 57 | 4 | 10 |
| GNU tar (24) | 60 | 85 | 13 | 12 |
| GNU coreutils, 30 programs (30) | 72 | 129 | 11 | 14 |
| BusyBox subset (242) | 169 | 232 | 24 | 29 |
| **distinct across all eight** | **224** | **395** | **30** | **39** |

**Exact lists for the smallest programs (the most trustworthy rows):**

* **kilo** (an editor): `ftruncate`, `getline`, `ioctl`, `sscanf`, `tcgetattr`, `tcsetattr`; headers `sys/types.h`,
  `sys/ioctl.h`, `sys/time.h`, `termios.h`; structs `termios` and `winsize`.
* **tcc** (a C compiler): `execvp`, `fdopen`, `getcwd`, `gettimeofday`, `sscanf`; headers `inttypes.h`, `sys/time.h`,
  `dlfcn.h`; types `int32_t` and friends.
* **gzip**: `opendir`, `closedir`, `fdopendir`, `openat`, `unlinkat`, `fstat`, `fsync`, `chmod`, `getopt_long`,
  `sigaction`, `sigemptyset`, `sigaddset`, `sigismember`, `sigprocmask`.
* **SQLite** (library and CLI, extensions off): `access`, `chdir`, `chmod`, `closedir`, `fsync`, `getpid`, `getpwuid`,
  `getrusage`, `gettimeofday`, `lstat`, `opendir`, `readdir`, `readlink`, `sscanf`, `symlink`, `sysconf`, `utimes`; the file
  locking constants (`F_GETLK`, `F_SETLK`, `F_RDLCK`, `F_WRLCK`, `F_UNLCK`), `mmap` constants and `mmap`/`munmap`.
* **dash**: `fork`, `vfork`, `wait3`, `execve`, `pipe`, `dup2`, `fcntl`, the signal set (`sigaction`, `sigprocmask`,
  `sigsuspend`, `sigemptyset`, `sigfillset`), process groups (`getpgrp`, `setpgid`, `tcgetpgrp`, `tcsetpgrp`), the identity
  calls, `opendir`/`closedir`, `umask`, `kill`, `getopt`, `mkstemp`, `strcasecmp`, `strtok`, and the `WIF*` macros.

**What this adds to the first pass.**

1. **Headers, types and constants are the largest gap, and the symbol survey could not see it.** 39 system headers are
   missing: `sys/types.h` (7 of 8 programs), `sys/ioctl.h` and `sys/time.h` (6), `dirent.h`, `inttypes.h` and `termios.h`
   (5), `pwd.h` and `sys/resource.h` (4), `fnmatch.h` and `getopt.h` (3), then `grp.h`, `regex.h`, `sys/mman.h`,
   `sys/param.h`, `sys/utsname.h`, `sys/wait.h`, `poll.h`, `strings.h`, `utime.h`, `libgen.h`, the socket headers and
   more. Without `sys/types.h` the whole family of POSIX types (`pid_t`, `mode_t`, `uid_t`, `gid_t`, `dev_t`, `ino_t`,
   `nlink_t`) does not exist, and neither do `DIR` and `sigset_t`.
2. **395 constants and variables, mostly header-only:** the signal numbers (`SIGCHLD`, `SIGHUP`, `SIGPIPE`, `SIGQUIT`,
   `SIGTSTP`), the `O_*` flags (`O_NONBLOCK`, `O_NOCTTY`, `O_CLOEXEC`, `O_NOFOLLOW`, `O_SEARCH`), the `S_I*` mode bits and
   `S_IS*` macros, errno values (`EINTR`, `ELOOP`, `EXDEV`, `ENOLCK`), `F_*`, `AT_*`, `R_OK`/`W_OK`, `STDIN_FILENO`,
   `optind`, `optarg`. Small to write, but each one **commits us to a value and a meaning** that the libc and any
   servers must honour, so they are design work, not typing.
3. **The two surveys disagree usefully.** Of 224 functions in each, 117 are in both. The proxy list had BSD and macOS
   names the real sources never use (`err`/`warn`, `strlcpy`, wide-character calls); the real sources add what the BSD
   builds hid (the `S_IS*` and `W*` macros, the signal-set calls, `wait`/`wait3`, the `set*id` family, `mkfifo`, `mknod`,
   `chown`, `fdopendir` and the other `*at` calls, `statvfs`, and the sockets BusyBox's network applets use). **Plan from
   the union.**
4. **kilo and tcc are within reach.** Both need roughly five or six functions and four headers; with UV0a and UV0b plus a
   raw-mode terminal (for `tcgetattr`, `tcsetattr`, `ioctl` and window size) they would compile clean. **SQLite** adds file
   locking and an `mmap` fallback; **dash** needs the process, signal and process-group machinery described in section 4.
5. **The numbers are lower bounds** (limit a above); the real full-configuration lists will be longer, mainly for BusyBox,
   coreutils and tar.

**A mechanical acceptance check falls out of this.** The harness compiles a chosen set of real sources syntax-only against
`libc/include` and prints what is undeclared. A step is done when the harness prints **zero undeclared system names** for
its target set: for example, kilo, tcc and the SQLite CLI for UV0a plus UV0b and the header set, then dash and the BusyBox
shell once the process and signal pieces exist. That is an independent oracle (real programs and a real compiler), as the
project asks for. The harness is `scripts/libc_gap_survey.py` (`make libc-gap`, output in `target/libc-gap/`);
`make libc-gap LIBC_GAP_ARGS="kilo tcc --check"` runs a subset as a gate and exits non-zero while anything is undeclared.

**Suggested addition to the order in section 6:** a step **UV0a0, the header set**, before UV0a: `sys/types.h`,
`sys/time.h`, `sys/ioctl.h`, `sys/resource.h`, `sys/param.h`, `sys/wait.h`, `sys/mman.h`, `sys/utsname.h`, `dirent.h`,
`termios.h`, `inttypes.h`, `pwd.h`, `grp.h`, `fnmatch.h`, `getopt.h`, `regex.h`, `strings.h`, `utime.h`, `poll.h`,
`libgen.h`, with the types and constants they define. Acceptance: kilo, tcc and the SQLite CLI show no undeclared header,
type or constant.
