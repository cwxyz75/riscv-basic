# LIBC-GAPS: what the programs we want still need from the libc

Measured 2026-11-17 with `scripts/libc_gap_survey.py` over every program in `third_party/fetched` (syntax-only compiles against
`libc/include`; the survey's limits are in its header and in `UV0-GAP-SURVEY.md` section 10: configuration macros are absent, so the
counts are lower bounds; "declared" is not "implemented"). This is the work list for "a complete libc, phase 1" (`WORK-PLAN.md`
plan 2, item 3); the owner's long-range goal is the whole C standard library as the survey can measure it, then the POSIX base.

| Program | Files | Undeclared functions | Constants | Headers | Notes |
|---|---|---|---|---|---|
| kilo, sqlite | 3 | 0 | 0 | 0 | built and running |
| tcc | 7 | 0 | 0 | `dlfcn.h` | `dlopen` is refused by design (no dynamic linking: UV Q2) |
| gzip | 12 | `getopt_long`, `unlinkat` | `O_SEARCH` | `getopt.h` | built with shims in `third_party/compat` |
| dash | 28 | `stat64` family | 20 | `getopt.h`, `histedit.h` | needs `fork` (S11); `histedit.h` is the optional line editor |
| busybox | 242 | `crypt`, the `utmpx` calls, `tzset` | 30 | none | built (about 90 applets) |
| coreutils | 30 | 26 (below) | 48 | `alloca.h`, `getopt.h` | gnulib: needs the `*at` family and wide characters |
| gnutar | 24 | 22 (below) | 27 | `alloca.h`, `strings.h`, `sys/file.h`, `sysexits.h`, `localcharset.h` | gnulib; see TIDY-UP T145 |

## The functions, ranked by how many programs want them

1. **The `*at` family** (`fstatat`, `unlinkat`, `faccessat`, `fchmodat`, `fchownat`, `linkat` (refused), `mkdirat`, `mkfifoat`
   (refused), `mknodat` (refused), `readlinkat`, `renameat`, `symlinkat`; `O_SEARCH`): needed by coreutils, gnutar, gzip. The
   libc already has `openat` over the path code, so each is a thin wrapper.
2. **`getopt_long`** (and `getopt.h`): gzip, coreutils, dash, tar. Written for the gzip port in `third_party/compat/getopt_long.c`;
   it belongs in the libc.
3. **`error`, `error_at_line`** (`error.h`): coreutils, tar.
4. **`execlp`/`execvp`/`execl...`**: coreutils, tar. Over the spawn grant.
5. **Small string and stdio items**: `bcmp`, `bcopy`, `index`, `rindex` (`strings.h`), `creat`, `ftello`, `fscanf`, `isblank`,
   `strtold`, `strxfrm`, `fpurge`, `mkostemp`, `sigwait`, `pthread_sigmask`, `tzset`.
6. **Headers**: `alloca.h`, `strings.h`, `sys/file.h` (`flock`: refused or advisory no-op), `sysexits.h` (constants only),
   `getopt.h`.
7. **Wide characters** (`wchar.h`, `wctype.h`, `uchar.h`): `mbrtowc`, `wcrtomb`, `mbsinit`, `iswspace` and friends, `wcwidth`; `wint_t`,
   `mbstate_t`. UTF-8 only (the system's one encoding, F6).
8. **Not planned**: `crypt`, `utmpx`, `dlopen` (no dynamic linking), `libedit` (an optional line editor), the 64-bit `stat64` names (a
   header alias is enough: `stat64` is `stat` here).

## Phase 1 (the batch for 2026-11-17 night)

Items 1 to 6 above and the UTF-8 core of 7, each with a C test (`libc/tests/c`, host libc as oracle where it can be) or a host test in
`cfmt`, and an entry in TIDY-UP for anything refused. Then re-run the survey (the counts above must drop), re-measure GNU tar and
record it in T145.

## Beyond phase 1 (the goal)

The rest of the C standard library as measured by a conformance corpus, not only by what our ports reference: `locale`, `regex`
(`pregex` exists), `iconv` (UTF-8 and ASCII/Latin-1), `setlocale`, `wordexp`, `mmap` for files and `fork` (S11, a separate decision),
and a portable test corpus (a public libc test suite is the oracle; which one is a decision for when phase 1 is done).

## Result of phase 1 (2026-11-17, same survey after the batch)

| Program | Undeclared functions before, after | Headers before, after |
|---|---|---|
| gzip | 2, 0 | 1, 0 |
| dash | 3, 0 | 2, 1 (`histedit.h`, optional) |
| coreutils | 26, 4 (`copy` and `S_TYPEIS*` are macros or its own, `strtold`) | 2, 0 |
| gnutar | 22, 0 (`fscanf` and `sigwait` were added after the first measurement) | 5, 1 (`localcharset.h` is gnulib's) |
| busybox | 5, 4 (`crypt` and the `utmpx` calls: not planned) | 0, 0 |

Built: the `*at` family (on the volume; `symlinkat` needs a file server with symlinks, which `memfsd` does not have yet), `getopt_long`
(`cfmt::getopt_long`, with the host's as oracle), `error`, `execl`/`execlp`/`execle`, `strings.h`, `creat`, `ftello`, `mkostemp`,
`pthread_sigmask`, `isblank`, `fpurge`, a no-op `flock`, `alloca.h`, `sysexits.h`, `stdbool.h`, `O_SEARCH`, the `stat64` names, and the
UTF-8 wide-character core (`cfmt::mb`, tested against `std`: `mbrtowc`, `wcrtomb`, `mbstowcs`, `wcstombs`, `wcs*`/`wmem*`, the `isw*`
classes, `towupper`/`towlower`, `wcwidth`, `uchar.h`), `fscanf`/`scanf` (over `cfmt::scan`, which now reports how much it used) and
`sigwait`. Not built: `strtold` (`long double` is a 128-bit float on this ABI), `crypt`, `utmpx`, `dlopen`, `libedit`.

## Phase 2, batch 1 (2026-11-18)

Survey re-run after phase 1 and before this batch: functions 8 (`crypt`, the three `utmpx` calls, `strtold`, and gnulib's `copy` and
`S_TYPEIS*`, which are its own or macros), constants and variables 104 distinct, headers 3 (`dlfcn.h`, `histedit.h`,
`localcharset.h`, none to be built). Most of the 104 are not libc: a program's own local names the survey cannot tell apart, and
configuration macros (`PACKAGE`, `VERSION`). The ones that are libc names, taken as this batch:

- the rest of POSIX's errno list with Linux's numbers and glibc's messages (`EROFS`, `EMLINK`, `EDQUOT`, `ETXTBSY`, `EFBIG`, `EPROTO`,
  `ESTALE`, `ECANCELED`, `EOWNERDEAD`, ... 23 names; the message table also gained the entries it was missing for 6, 8, 15, 16, 18, 19, 23,
  35, 37, 40 and the socket errors);
- `MB_CUR_MAX` (4: UTF-8), `LC_MESSAGES`, `AT_EACCESS`, `FNM_FILE_NAME`, `FNM_LEADING_DIR`;
- the termios names `VSTART`, `VSTOP`, `VSUSP`, `VEOL`, `CS5` to `CS7`, `FLUSHO`; `tcgetattr` now reports `VSTART` ^Q, `VSTOP` ^S and
  `VSUSP` ^Z (the console server's Ctrl-Z, OD124).

Checked on the OS in `ctest posix2` (the numbers against Linux's, every message present); the numbers and texts come from the Linux
headers and glibc, which the macOS development host cannot confirm, so they are not checked against a host run here. After: constants
and variables 87 distinct (busybox 30 to 20, coreutils 39 to 32, gnutar 23 to 20).

Not taken: `optreset` and `sys_siglist` (BSD and glibc-private names; dash and busybox have fallbacks), `ARPHRD_ETHER` and
`USER_PROCESS` (network device ioctls and `utmpx`, not planned), `strtold` (128-bit `long double`). The conformance corpus that
would measure the rest of the C library is still the owner's choice (which public libc test suite is the oracle); the plan does not
answer it.
