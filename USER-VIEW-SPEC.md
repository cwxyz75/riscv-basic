# USER-VIEW-SPEC: the command-line view of the system (specification, draft v0, 2026-11-15)

Status: **draft written from the 2026-11-15 design discussion with the owner. Nothing in it is built.** It covers what
the system looks like from a command line, whether the one looking is a person at a console or software: the POSIX
surface and which existing programs should work, the filesystem as a program sees it, the terminal, sessions and
login, and the shell (detailed in `SHELL-SPEC.md`, which this file does not repeat).

Labels used throughout. **Decided**: the owner answered yes in the discussion. **Proposal**: the drafter's, needs a
yes. **Open**: deliberately undecided (collected in section 15). Nothing here is in the decision log
(`OS-ARCHITECTURE.md` section 17) until section 14's entries are accepted. Where this file amends an existing
decision it says which (OD11 and OD20, mainly).

Basis: `OS-ARCHITECTURE.md` sections 6.3, 10, 12 (namespaces, `fork` room, the file protocol, symlinks, unions),
OD8, OD11, OD19, OD20, OD67, OD73, OD97 (the service directory, `/svc`), OD84 (accounts), `FS-SPEC.md` (doofs: no
hard links, no users or modes on disk, attributes wanted), `S7-SPEC.md` (the C libc), and the current `consoled`,
`proto::tty` and `init/src/bin/sh.rs`.

## 1. Principles

* **P1 (Decided).** The native interface is **handles plus service protocols**. The filesystem tree holds
  **adapters** over it: `/proc`, `/net`, the sockets in `/run`, parts of `/dev`, and the libc's POSIX calls
  (`*at`, `fork`/`exec`) are renderings of native things for byte-oriented software. A program without an adapter still
  works natively. No operation may be reachable through an adapter that is not reachable through the handle.
* **P2 (Decided).** **POSIX is a porting surface, not a personality.** We provide what a named corpus of real
  programs needs, measure it, and write down what we refuse. "Works" means a gate in section 3, not a feeling.
  Small, documented patches to a port are acceptable; highly tuned software may not work, and that is fine.
* **P3 (Decided).** **Every process has its own view.** What a program sees is its **namespace table** (section 5),
  built by whoever launched it. There is no global root that programs truly see. The Docker and Plan 9 model.
* **P4 (Decided).** **Servers never see identities** (INVARIANTS H5). A "user" is a profile (section 10). Sharing is by
  handle. Advisory uid, gid and mode exist for POSIX compatibility and enforce nothing.
* **P5 (Decided).** **Pipelines carry bytes.** Structure lives in the shell's values and in service protocols, and is
  converted at the edges (`SHELL-SPEC.md`).

## 2. Two runtimes (OD19, refined; **Proposal**)

OD19 has a native Rust runtime (`rt`, `no_std`, capability-native) and a C libc. We add: **a Rust `std` for our target as
a Unix-family target on top of the C libc** (the Redox pattern), because the ecosystem's crates (`libc`, `nix`,
`rustix`, `uucore`) assume a Unix target. Native programs written for this OS stay on `rt`. This makes the libc a
load-bearing layer, not a thin shim, and is a precondition for uutils, cargo and rustc. A native `std` that is not
Unix-family would not compile uutils.

## 3. Porting targets (Decided as direction; the ladder is a **Proposal**)

| Tier | Targets | What it forces |
|---|---|---|
| A | Lua, SQLite, BusyBox (NOMMU build), tcc, a small editor (kilo) | mostly S7; terminal basics; `vfork`/`posix_spawn`; honest file semantics |
| B | GNU utilities (tar, gzip, grep, sed, coreutils, findutils, diffutils, make), dash, autoconf `configure` | file metadata, the `*at` family, `fork` shim or `vfork`, spawn speed |
| C | nginx | `poll`/`select` over many handles, sockets, shared `mmap`, a signals subset; `master_process off` first |
| D | clang/LLVM, then rustc and cargo (full self-hosting) | ELF TLS (`__thread`), a C++ runtime (libc++, libc++abi, an unwinder), multi-GB memory and big `mmap`, process spawn, dynamic loading (`dlopen`) for rustc proc macros, a Rust `std` port, `flock` and sockets for cargo |
| + | uutils/coreutils alongside BusyBox, once the Rust `std` exists | a Unix-family `std` on our libc; the GNU coreutils tests as an oracle |

Notes (**Decided** unless marked):
* **BusyBox NOMMU first.** It uses `vfork`+`exec` and avoids `fork`, so it is the cheapest first real userland (ash
  plus the basic tools) and needs only the S7 libc plus spawn. **dash** needs `fork` for subshells: the S11 shim, or a
  patch to `vfork`; it is the probe of the "small patches" line.
* **uutils is added, not substituted.** It has no shell, and `tar`, `gzip`, `sed`, `grep` and `find` are separate or less
  mature projects. Over time prefer uutils for the core utilities where it passes; keep GNU or BusyBox `tar` and `gzip`
  until a Rust equivalent matches. Redox is the precedent for a non-Linux OS with its own libc shipping uutils.
* **tcc** is the early self-hosting win and the feasibility test for tier D. **git** is the stress test and a "might
  never" target; it is useful for finding the line.
* **Cross-built bring-up** for clang and rustc (build on the host, run natively) is the **Proposal**. Open Q2 covers
  dynamic linking.
* **Gates** (each tier names programs; "works" is these pass): a program's own test suite where one exists
  (SQLite's, Lua's, uutils' with the GNU coreutils tests, `toml-test`-style corpora elsewhere), plus runs of real
  `configure` scripts for tier B. Any program failing a gate that cannot be fixed within P2 is recorded as refused.

## 4. The POSIX surface policy (**Proposal**; every row is confirmed by the gap survey in step UV0)

| Area | Policy |
|---|---|
| Process creation | `posix_spawn` and `vfork` first (cheap, no COW); `fork` composed in the libc from COW VMOs in S11 (OD8); no kernel `fork` or `exec` |
| Signals | a subset in the libc over protocol events (interrupt, hangup, resize, child exit, pipe broken); no async signal-safe guarantees beyond what the corpus needs; masks and `pthread_cancel` stay unimplemented (OD73) |
| Terminal | `isatty`, `termios` (cooked and raw, `VMIN`/`VTIME`), window size, `/dev/tty`, a pty as a terminal backend (section 9) |
| Waiting | `poll` and `select` as shims over waiting on many handles; they must work across sockets, pipes and terminals |
| Files | the `*at` family maps onto directory handles; `rename` atomic over; `fsync` a real commit (F4); `pread`/`pwrite`/`ftruncate`; `fcntl` and `flock` advisory locking, honest enough for SQLite |
| Metadata | `stat` fully populated (section 6); nanosecond times; symlinks; **no hard links** |
| Identity | uid, gid, `getpwuid`, `getgrgid`, `umask` synthesized from the session's profile; `chmod` and `chown` record advisory attributes |
| Memory | `mmap` private and shared (S11); `MAP_NORESERVE`-style reserve and commit for tier D |
| Threads | pthreads as a shim over cheap one-to-one kernel threads (OD73); ELF TLS (`__thread`) added before tier D, and pthreads wants it too (OD67 excluded it from S7) |
| Locale and time | UTF-8 with a **C locale only** (**Proposal**); a timezone database in `/share`; `date` and `localtime` |
| Refused (so far) | hard links; setuid and real privilege changes; `ptrace` as Linux does it; `/proc/sys`, `kcore`; SysV IPC; the rest of the refusals are accumulated in `TIDY-UP.md` |

## 5. Namespaces and paths (**Decided**; amends OD11)

* A process's view is its **namespace table**, prefix to handle, in the libc (OD11 stands). The kernel has no paths.
* **`..` is defined by the table, not by handles, servers or disk.** The protocol still forbids `..` (the server never
  interprets it), so the sandbox is the handles, exactly as before. The **libc resolves `..` lexically** against the
  process's own table, with a **clamp at the table's root** (the chroot rule): `/..` is `/`, and `/work/..` is `/`
  because the table says `/work` hangs off `/`. A handle with no table context has no `..` (and a file handle never
  did); `openat(fd, "../x")` works only if the libc recorded the path when it opened `fd` (**path-annotated fds** in the
  fd table), otherwise it fails.
* **The cwd is a root-relative path string plus its handle**, inherited through the startup message; `chdir` validates
  by opening the directory; `getcwd` returns the string. A relative path that never climbs above the cwd starts from
  the cwd handle (the fast case); one that climbs re-walks from the matching table prefix.
* **Symlinks:** the server returns the target text and **how much of the path it consumed**; the libc resolves a
  relative target from the link's own directory path (lexically), an absolute one against the table. A loop limit
  gives `ELOOP`.
* **The root is a skeleton filesystem the launcher builds** (a small memfs holding the real directories, the
  symlinks and the empty mount points), with table entries mounted over it. So `ls /`, `ls -l` and `readlink` give
  honest answers and a launcher can build a different skeleton for a different kind of program. Intermediate
  directories above mounts therefore exist as real entries, not as a libc fiction.
* **Known deviations (go in `TIDY-UP.md`):** lexical `..` differs from physical `..` across symlinks; a cwd whose
  parent is renamed keeps the old name; `shell-core::resolve` currently returns an error above the root where the
  chroot rule clamps (to change in step UV1).
* **9P stays an adapter** (OD11, section 10 of the architecture); unions, `/svc` and the rest are unchanged.

## 6. File metadata (**Proposal**, consistent with `FS-SPEC.md`)

doofs v1 keeps its non-goals (users, modes and ACLs on disk; hard links, F3). POSIX metadata is carried instead as
**advisory extended attributes** in the attribute system we already want (BeOS-style, indexed, format-independent,
built in memfs first):

* `posix.mode`, `posix.uid`, `posix.gid` attributes. The libc **synthesizes defaults** when absent (directories
  `0755`, files `0644`, executables `0755`, uid and gid from the profile). `chmod`, `chown`, `umask` and `install -m`
  write the attributes. **Enforcement is by handle rights, not by these bits.** The execute bit matters to `PATH`
  lookup, `test -x` and `make install`, so the launcher and the libc honour it as data, not as a check.
* **Hard links are refused** (`EPERM`); `st_nlink` is 1. `tar` extraction falls back to a copy; `cp -l` and `ln` fail.
* **`st_dev` and `st_ino` are opaque tokens** (the server's identity, composed by the libc, plus an object token),
  never doofs ids, so the on-disk format stays hidden behind `proto::file`. Stable within a mount, unique enough for the
  loop detection that `find`, `tar` and `cp` do.
* Times are nanosecond; `mtime` is what `make` sees. Names are UTF-8 and case-sensitive; limits are per volume.
* Open Q8: the exact attribute names and whether `posix.*` may be indexed for `find`-style queries.

## 7. The canonical layout (**Proposal**; this is the default table an interactive session builds)

A convention, not a fixed tree: every process gets only the entries its launcher grants. A service may see just `/tmp`,
`/dev/null` and its own data.

```
/                  skeleton memfs built by the launcher
├── bin            union: system, then the user's, then the project's (OS-ARCHITECTURE section 10)
├── lib            libraries
├── include        headers
├── share          data files, terminfo, zoneinfo
├── etc            system configuration; read-only to most programs
│   ├── passwd, group    small, synthesized from profiles
│   ├── hosts, resolv.conf, profile, trusted
├── dev            synthesized: null, zero, urandom, tty, fd/N, console
├── proc           synthesized, minimal (section 8)
├── svc            the service directory: a registry VIEW (OD97); only the names this program may use
├── run            a WRITABLE registry area: bind() and connect() for AF_UNIX (section 8)
├── tmp            memfs, private per job (quota domain), gone with the job
├── home           the user's volume (doofs); /home/<user> is $HOME
├── mnt            volumes granted to this program, one directory each: /mnt/<name> (names from etc/mounts, see below)
├── boot           the boot image, read-only (what bootfs is today)
├── net            optional: a file-shaped adapter over /svc/net, only if the program holds that view
└── usr            compatibility only: bin, lib, include, share are symlinks to ../bin, ../lib, ...
```

* **Volumes are granted at `/mnt/<name>`** (decided with the owner, 2026-11-17; amends the earlier `/vol/<label>` draft). The
  name is the one the mount table (`/etc/mounts`, `FS-SPEC.md` section 6) gives the volume's uuid, never the disk's own label.
  `/svc/vol/...` lists the volumes that exist (a view the program may not have); `/mnt/...` is what this program was given:
  the two never share a prefix. Built in the filesystem slice (step 5e): `/etc/trusted` lines name the volumes a program gets;
  the session manager's profiles (UV5) and the manifest (S10) replace that list. `/home` is the user's own volume, granted the
  same way.
* **`/usr` is a real directory of four or five symlinks**, not a self-link (`/usr -> /` would make `find -L` and
  `tar -h` walk `/usr/usr/...` to the loop limit). `/usr/local` is not provided until the install-layout question is
  settled.
* `/lib` is the real directory and `/usr/lib` the link.
* **Install layout is the flat merged layout above (Q4, resolved 2026-11-17).** The alternative, kept as a later option: `/pkg/<name>/<version>/{bin,lib,share}` with
  `/bin` and `/lib` built as unions from manifests (a fit for per-process views; more machinery; `--prefix` covers most
  build systems). System `/bin` read-only, user tools in `/home/<user>/bin` unioned in, would make "trusted programs"
  a visible layer instead of a config file.
* `/svc` replaces any need for a separate `/srv`; there is no `/srv`.
* Queries (BeOS-style) stay a command-line tool for now; a `/query/<expr>` virtual directory is Open Q9.

## 8. Adapters (**Decided** as a principle, P1; contents are a **Proposal**)

Every file in an adapter is a commitment, so each is added only when the corpus needs it and is documented as a
"compat rendering of X".

**`/proc`** is a synthesized server in front of the process server, scoped by the process handles the holder has:

* `self`, plus an entry per process the holder has a handle to (children, descendants, and the members of a job it
  holds); the system-wide view is what a handle to the **root job** shows, so there is no special-case privilege.
* Per process, with the rights on the handle deciding what appears: `status` and `stat` (what `ps`, `top` and `pidof`
  read), `cmdline`, `environ` (only with the right), `fd/` (the open handles; `self` at least, because some programs
  enumerate it), `task/` (threads), `maps` (`self` only), and **`ns`** (the namespace table, one line per prefix and
  server; the data behind the shell's `ns`; `/proc/mounts` is a rendering of it).
* System files **scoped to the caller's job**, in the manner of lxcfs: `cpuinfo`, `meminfo`, `uptime`, `loadavg`
  report the job's quota and allowed CPUs, not the machine. The libc's `sysconf` answers the same questions directly;
  the files are for unmodified programs.
* **Skipped or faked:** `exe` and `cwd` as links (a target is meaningful only in the reader's namespace), `/proc/sys`,
  `kcore`, `net/*` (that belongs to `/net`).
* `kill(pid)` stays in the libc, talking to the process server. A Plan 9 `ctl` file is possible later.
* **PIDs: local to a job or global** is Open Q5.

**`/dev`:** `null`, `zero`, `urandom`, `tty`, `fd/N`, `console` are small synthesized servers the launcher mounts;
`/dev/console` can be a window onto `consoled`. Hardware devices stay services (`devmgr`), not `/dev` nodes, until a
port needs one.

**`/svc` versus `/dev`:** `/svc/<name>` gives a *connection to a service that speaks its own protocol* (network,
launcher, process server); `/dev/<name>` gives a *file-like byte stream* with a POSIX flavour.

**`/run` and Unix sockets:** the registry already has `Publish` (with a listener channel and a `PUBLISH` right) and
an open that forwards to the listener, which is what `bind` and `connect` on an `AF_UNIX` path mean. `/run` is a
**writable area of the registry**: `bind("/run/x.sock")` publishes, `connect()` opens, the libc hides it. A job gets its
own `/run` subtree, and the `PUBLISH` right is the permission to bind. `SCM_RIGHTS`-style fd passing is handle
passing; passing a service connection to another program, and revocation, are the registry's Q10 and stay open.
(**Proposal**; Open Q6 asks whether to build it now or wait for a port that needs it.)

**`/net`:** optional, Plan 9 style, over `/svc/net`, so a shell script can use a socket with redirection. Not needed
by the sockets API, which goes through the libc.

## 9. The terminal (**Decided**: items 1 to 4 and the detach feature; the rest **Proposal**)

Today `consoled` is a console server that does **cooked** line editing itself (the pure `console::LineEditor`),
echoes, and on ^C kills the foreground job (it was given the `KILL` right, OD51) and tells the shell. There is no raw
mode, no window size, no terminal types, and no notion of a session.

1. **A terminal is a server-side object with a pluggable backend**: the UART, a virtio console, a framebuffer
   console, a **channel held by another process (a pty)**, or the network. A pty is a use of the design, not a
   feature; an ssh server, `script`, a terminal emulator and an expect-style test harness are all backend providers.
2. **Two modes.** **Cooked** for simple programs that read lines (editing in the terminal server, using `console`).
   **Raw** for the shell and editors, with escape sequences passed through, where `console` becomes a library the
   shell uses itself (history, namespace-aware completion, multi-line).
3. **Control events are protocol events, not signals**: interrupt, suspend, hangup, resize, delivered to the
   terminal's **foreground job**. Native programs receive messages; the libc maps them to `SIGINT`, `SIGTSTP`,
   `SIGHUP`, `SIGWINCH` for ported software.
4. **The foreground job is a handle** the shell gives the terminal (a job handle with `KILL`, later a suspend right).
   Job control then needs no process groups or sessions in the kernel; the libc **emulates** them for ported shells.
5. **Terminal types:** a small `TERM` set (vt100 and an xterm-compatible one), a `terminfo` database in `/share`,
   UTF-8 with character-width tables, and the **window size as a property**, queried from the host terminal over
   serial. A framebuffer console (a VT emulator) is a separate server, later.
6. **Software-facing behaviour:** a program tells a terminal from a pipe by `isatty` (the connection's kind). A pty
   backend lets software drive an interactive program. There is also a control channel for events, so software does not
   have to inject escape sequences.
7. **Multiple terminals** (virtual consoles) map to sessions; switching needs a keyboard source (framebuffer era).
8. **Detach and reattach are a designed feature (Decided).** A session's job outlives its backend, a new backend can
   attach to the same terminal object, and the terminal keeps enough state (screen or scrollback, size, mode) to
   redraw. Hangup and detach are distinguishable events, since a closing backend can mean either.

## 10. Sessions and login (**Decided**: the framing; mechanism **Proposal**)

* A **session** is: a **job** (the quota and kill domain), a **namespace table** built from a **profile**, an optional
  **terminal**, an environment, and an identity record. Closing it ends its job (unless detached).
* A **profile** is a table template (which prefixes, which `/svc` views), a home volume, quotas, a shell, and the values
  the libc synthesizes for uid, gid and `getpwuid`. **A user is a profile.** Nothing in the kernel or any server knows
  about users. A user's home is private because only that user's sessions hold a handle to it.
* A **session manager** service builds sessions. **Authentication is only what permits it to build one.** Autologin on
  the console is the development default; a remote login is a terminal backend that authenticates, asks for a session
  and connects a pty-like terminal. How a party is authenticated, how the first profile comes to exist and how access
  returns after a lost credential are in section 10.1.
* Daemons run in sessions with no terminal (`supervise` is the starter). A registry of live sessions (`/svc/session`,
  seen through `/proc`) answers `who`, `w` and `last`.
* **Re-authentication for more authority** (the `su` and `sudo` equivalent): ask the manager for a child session with a
  different profile. The design of that is the authority-UX topic (section 12).
* **Sharing between profiles is by handle** (Decided); the mechanism is deferred. A sketch (not a decision): one
  session sends an attenuated handle to a named inbox in the session manager and the recipient's next session receives it
  in its table.
* **Multi-user identity is not built** (Decided): no ACLs, no groups, no identity in servers. Revisit only if a real
  sharing need shows up; the session manager is where it could be added without touching servers.

### 10.1 Authentication, enrolment and recovery (**Decided** in the 2026-11-16 discussion unless marked)

Authentication answers one question: **may this party obtain this capability?** It is a gate in front of the session
manager, never a name that another server checks (P4, UVI6). Surveyed for this (from memory, not fresh research):
Unix and PAM (a uid checked everywhere: the ambient model P4 rejects), Windows NT (a token minted by the LSA, plus the
secure attention sequence as a trusted path), Plan 9 (`factotum`, a per-user agent that holds secrets and runs the auth
protocols so programs never see them), Fuchsia (no uid; a userspace account manager authenticates and starts a session),
EROS, KeyKOS and Genode (authentication is a small trusted component outside the kernel), and Android, systemd-homed and
macOS (the credential releases the key that decrypts the user's storage). The shape below is Plan 9 and Fuchsia, with the
trusted path from Windows and the key release from the last group.

1. **`authd` is a service of its own, separate from the session manager** (Decided). It holds the credential verifiers
   (a password hash to begin with; SSH public keys next; WebAuthn later) and nothing else. Its output is **the right to
   ask the session manager for a session from profile X**, as an attenuated handle; it is never a uid or a name. The
   session manager therefore holds no secrets and does not check passwords, and `authd` builds no sessions.
2. **Secrets stay in the agent** (Decided; the factotum and ssh-agent pattern). A program that must authenticate to
   something (an `ssh` or HTTPS client, a `git` remote) asks `authd` or its per-session agent to run the exchange and
   never holds the secret. The agent's protocol is the authority-UX topic's (section 12) first concrete piece.
3. **The trusted path mostly comes free** (Decided as a property to keep). Only a process holding the console handle can
   draw on it, so no other program can fake the console login. The weak spot is a **fake re-authentication prompt
   inside a session** (the `sudo` case); `authd` must prompt through a channel the session's programs do not hold, as
   the Windows secure desktop does. The design of that channel belongs with the authority-UX topic.
4. **A remote login is a terminal backend** (Decided): sshd (or a login service) authenticates through `authd`, then asks
   the session manager for a session and connects a pty-like terminal. No other path changes.
5. **A password-hash gate is enough until storage encryption exists** (Decided). The hash is a memory-hard one (argon2
   or scrypt: a small utility crate needing its own decision-log entry), stored by `authd` on its own volume.
6. **Later (Proposal, not scheduled): derive the home volume's key from the credential** (the homed and Android
   pattern), so a wrong password yields no data instead of a refusal that a bypass could skip. It needs storage
   encryption in `fsd` first and is the point where a lost credential stops being resettable (see recovery below).
7. **Program identity is separate from user identity** (Decided as a direction): what a program may do comes from its
   manifest (S10), as with Fuchsia component URLs, never from the user who ran it.

**Enrolment**: how the first profile and its credential come to exist. Three ways, each tied to a scenario, and all
three are the same act (giving the session manager its first "may build profile X" grant), so they add no mechanism:

* **Owner claim at first boot** (the real-use default). With no profile on the volume, the console starts a **setup
  session**: an unauthenticated profile that can do exactly one thing, create the first profile and set its credential.
  When that is done the setup profile ceases to exist. The physical console is the root of trust, as on a phone or a
  new Mac.
* **A seed credential in the image or manifest** (tests and headless boots). The build, or the S10 manifest, supplies
  a public key or a hash. The credential sits in the image, which is acceptable for QEMU tests and development and for
  nothing else.
* **Autologin** (the development default, and what the system does today). A named profile (`autologin`) that the
  manifest enables; it is **off in every non-development manifest**, and its use is logged.

**Recovery**: how access returns after a lost credential. Authority has to come from somewhere other than the lost
credential, and **how much to promise is a per-system policy, not one built-in answer** (owner, 2026-11-16: physical
access is fine for most uses, something more elaborate for a handful). The policy is `authd` configuration read at boot
(its format is Q13). The mechanisms, from weakest to strongest:

* **Physical console** (the default and the first one built): booting with a recovery flag, or from install media,
  gives the console session the right to reset credentials. Physical access equals full authority, which is right for a
  single-owner machine whose disk is not encrypted.
* **A second enrolled credential** (a spare SSH key or hardware key) authenticates a reset. No console is needed.
* **A recovery key**, generated at enrolment and kept by the owner elsewhere, accepted in place of the credential. It is
  **mandatory once the home volume's key derives from the credential** (item 6): then a reset cannot recover the data,
  so without the recovery key a lost credential means lost data, and the policy must say so out loud.
* **No recovery** (a policy a system may choose): console access does not reset anything.

Which mechanisms a system enables, and whether any needs a second party or a delay, is configuration; the mechanisms
themselves are built one at a time, in the order above.

## 11. The shell

See **`SHELL-SPEC.md`**. In one paragraph: the native shell, **ish** ("improved shell"), is es-style (closures, exceptions, hook functions), with the
JSON data model plus closures, byte-stream pipelines, capability verbs (`ns`, `bind`, `mount`, `confine`), spawn-native
process starts, and a raw-mode interactive layer; dash or BusyBox ash is `/bin/sh` for ported software. It amends OD20.

## 12. Topics not yet designed (the backlog; each wants its own discussion)

Authority UX (what replaces `sudo`; how grants and revocation feel; avoiding prompt fatigue); software installation,
update and rollback; in-system documentation (`man`, `--help`, completion definitions); boot, shutdown and service
management from the CLI (a `svcctl`, startup scripts in the new shell); logging and diagnostics (a log service, a
`dmesg` equivalent, crash reports, `strace`-like tracing over process handles); time, timezone and locale; resource
control from the CLI (`ulimit`, `nice`, `time`, accounting over jobs and quotas); scheduled work (`cron`, `at`);
named pipes, special file types and locking; pagers and a terminal multiplexer; the host workflow (the 9p share,
getting files in and out of QEMU).

## 13. Steps, each with an acceptance check written before the code (**Proposal**)

Ordering follows dependencies on the S-series (S9 network, S10 filesystem, S11 compat) and on the review queue.
Acceptance for anything that touches SMP is said plainly as `-smp 2` and `-smp 4`.

* **UV0: the gap survey.** Table each tier-A and tier-B program against what the libc and `rt` provide today.
  *Acceptance:* the table is committed (program, the calls it needs, present, absent, small or structural), and the
  policy table in section 4 is corrected from it. No code.
* **UV1: `..`, the cwd string and path-annotated fds.** libc changes plus the `shell-core::resolve` clamp.
  *Acceptance:* host tests (clamp at root; `..` across a mount boundary; relative symlink with `..`; `openat` on a
  path-annotated fd; `openat` with `..` on an unannotated handle fails); the suite checks that the **server still
  refuses a raw `..`**; one mutation check per new test.
* **UV2: the skeleton root and the canonical layout.** Launcher builds the skeleton, `/usr` links, `/dev` servers
  (`null`, `zero`, `urandom`, `tty`, `fd`), a private `/tmp` per job. *Acceptance:* a suite test lists `/`, reads
  `/usr/bin` through the link, and reads and writes `/dev/null`; a confined program sees only its table.
* **UV3: advisory POSIX attributes.** `posix.mode/uid/gid`, defaults, `stat` populated, opaque `st_dev`/`st_ino`, hard
  links refused. *Acceptance:* host tests against memfs; a differential run of `ls -l`-style output against the
  expected on a corpus; `chmod` then `stat` round trips.
* **UV4: the terminal's raw mode, events and window size.** *Acceptance:* a scripted session over the serial console
  (the `proto::tty` host tests extended, then QEMU): raw reads with `VMIN`/`VTIME`, resize delivered, interrupt and
  hangup distinguishable; the existing cooked behaviour is unchanged.
* **UV5: the session manager, profiles and autologin.** *Acceptance:* boot goes to a session built from a profile; a
  second profile cannot open the first's home; closing a session ends its job; `/proc/self/ns` matches the table.
* **UV5a: `authd`, enrolment and the physical-console recovery** (after UV5; section 10.1). *Acceptance:* a boot with an
  empty volume starts only the setup session, which can create one profile and then no longer exists; a wrong password
  yields no session and a right one yields a session for that profile only; `authd` and the session manager are separate
  processes and the manager's table holds no verifier; a seed-credential boot (the test manifest) logs in headlessly; the
  recovery flag resets a credential and an ordinary boot refuses it; autologin is refused by a manifest that does not
  enable it. A negative control per property (a mutation that lets the wrong password through, or the setup session
  survive, must fail a test).
* **UV6: `/proc`.** The adapter, driven by the corpus (`ps`, `free`, `nproc`, `top`). *Acceptance:* each file
  has a test from the program that reads it; a process cannot see processes it holds no handle to.
* **UV7: detach and reattach.** *Acceptance:* a long job keeps running when the backend closes; a new backend
  attaches and sees the screen state; hangup still ends the job.
* **UV8: BusyBox NOMMU as the first real userland.** *Acceptance:* ash plus the basic applets run the tier-A gates; its
  own tests where it has them; spawn rate measured (the `configure` case).
* **UV9: `/run` and `AF_UNIX`** when a port needs it; **UV10: the Rust `std` on our libc and uutils** (with S11).

## 14. Decision-log entries to add on acceptance (draft text; numbers provisional)

* **OD-UV1 (amends OD11).** The protocol forbids `..`; the libc resolves it lexically against the process's own table,
  clamped at the table's root; the cwd is a root-relative string plus a handle; fds opened through the table carry
  their path; handles received elsewhere have no `..`.
* **OD-UV2 (amends OD20).** The native shell is es-style with the JSON data model plus closures; dash or BusyBox ash is
  `/bin/sh` for ported software only (`SHELL-SPEC.md`).
* **OD-UV3.** POSIX compatibility is a porting surface judged by a named corpus (section 3); refusals are recorded.
* **OD-UV4.** Adapters (`/proc`, `/net`, `/run` sockets, parts of `/dev`) render handles and service protocols for
  byte-oriented software; nothing reachable through an adapter is unreachable by its handle.
* **OD-UV5.** The canonical layout (section 7), with a launcher-built skeleton root and a symlinked `/usr`.
* **OD-UV6.** POSIX mode, uid and gid are advisory attributes; hard links are refused; `st_dev` and `st_ino` are opaque.
* **OD-UV7.** A terminal is a server object with a pluggable backend (a pty is a channel backend); cooked and raw
  modes; control events are protocol events delivered to a foreground-job handle; sessions are detachable.
* **OD-UV8.** A session is a job, a table built from a profile, an optional terminal and an environment; a user is a
  profile; servers never see identities (restates H5); sharing is by handle.
* **OD-UV9.** The Rust `std` for the target is a Unix-family target over the C libc; native programs stay on `rt`
  (refines OD19).
* **OD-UV10.** Authentication is a capability-granting gate: an `authd` service, separate from the session manager,
  holds the credential verifiers and yields only the right to ask for a session from a profile, never a name. Secrets
  stay in `authd` or a per-session agent; a re-authentication prompt uses a channel the session's programs do not hold. A
  salted memory-hard password hash is enough until storage encryption exists.
* **OD-UV11.** Enrolment is the first "may build profile X" grant, by one of three scenario-tied routes: an owner claim
  through a one-shot setup session (real use), a seed credential in the image or manifest (tests, headless), or the
  logged `autologin` profile (development only, off in other manifests). Recovery is a per-system policy configured
  in `authd`, with physical-console reset as the default and a second credential, a recovery key (mandatory once the
  home key derives from the credential) and no recovery as stronger options.

## 15. Open questions (stop and ask)

1. **Q1 (resolved 2026-11-15):** `{ }` is a JSON map literal, `[ ]` a list literal, and shell code blocks are
   `do ... end` (`SHELL-SPEC.md` Q1).
2. **Q2: dynamic linking.** A deliberate later project (a `.so` loader, `dlopen`), or avoid it by keeping rustc
   proc macros out of scope at first?
3. **Q3: handles as shell values** (`SHELL-SPEC.md` Q3).
4. **Q4 (resolved 2026-11-17): install layout** is the flat merged one of section 7 (`/bin`, `/lib`, `/include`, `/share`). `/pkg/<name>/<version>` with unions needs the union server (S10) and stays a later option; flat does not prevent it.
5. **Q5: PIDs**: local to a job, or global numbers from the process server.
6. **Q6: `/run` and `AF_UNIX`**: build with the registry now, or wait for a port that needs it.
7. **Q7: system configuration format**: TOML, or shell data literals (`SHELL-SPEC.md` Q2).
8. **Q8: advisory attribute names** and whether they may be indexed.
9. **Q9: queries in the tree** (`/query/<expr>`) or only a command.
10. **Q10 (the registry's, open already): passing a service connection to another program, and revocation.**
11. **Q11: the self-hosting bring-up path**: cross-build and run natively (Proposal), or bootstrap from tcc.
12. **Q12 (resolved 2026-11-17): default namespace for a program the shell launches** is the confined one: the current directory's subtree, standard input, output and error, and a few `/dev` entries (`null`, `zero`, `urandom`, `tty`). Anything wider (the skeleton root, volumes, `/svc` views) is an explicit grant: `/etc/trusted` lines until the session manager's profiles (UV5) and the manifest (S10) replace them. (OD20 stands.)

13. **Q13: the recovery policy's format and menu** (section 10.1): the configuration keys, whether a mechanism can
    require a second party or a delay, and whether the policy is changeable only by an authenticated session or also
    from the console. The first built mechanism is the physical console only.
14. **Q14: deriving the home volume's key from the credential** (section 10.1, item 6): when, with which key
    hierarchy, and what `fsd` encryption it needs. Not scheduled; it turns recovery from a reset into a recovery-key
    question and so must be decided before it is built, not after.
15. **Q15: the multi-factor and hardware-key story** (WebAuthn, a TOTP, a smartcard): which `authd` verifier kinds, and
    in what order. Later; the verifier interface should not assume a password.

## 16. Invariants to add when built

* **UVI1** `..` is resolved only by the libc against the process's own table; a server never sees one; no path a
  process can name reaches a handle outside its table.
* **UVI2** An adapter never offers an operation the underlying handle's rights do not allow.
* **UVI3** `/proc` shows only processes the holder has handles to; `meminfo`, `cpuinfo` and the like report the
  caller's job, never more.
* **UVI4** A terminal's foreground job is a handle the shell gave it, and an event reaches only that job.
* **UVI5** Closing the last backend of a detachable session never kills its job; hangup does.
* **UVI6** No server stores or checks an identity; uid, gid and mode are data.
* **UVI7** `authd`'s output is a handle to ask for a session from one profile, never a name; the session manager holds
  no credential verifier, and no program other than `authd` and its agent ever holds a secret.
* **UVI8** The setup session exists only while no profile does, and can only create one; `autologin` exists only where
  the manifest enables it.

## 17. Shortcuts to record in `TIDY-UP.md` when built

`/etc/passwd` and `/etc/group` are static one-entry files from the boot image until the session manager (UV5) synthesizes them; authentication and the profile format are decided at UV5 (owner, 2026-11-17); lexical `..` deviation and stale cwd after a parent rename; advisory metadata that enforces nothing; no hard links;
`/proc` kept minimal and scoped; a C locale only; no `/usr/local`; the `shell-core::resolve` error above the root until
UV1; a one-user, single-owner session model with guest and service profiles only; **recovery is physical-console reset
only** (a system that needs a second credential, a recovery key or no recovery waits for the configurable policy, Q13), and
**home volumes are not encrypted** (Q14), so a password gate is the whole defence.

## 18. Risks

* **The libc becomes load-bearing** (section 2). Every POSIX call we add is a permanent commitment; the corpus and the
  refusals list are what keep it bounded.
* **Lexical `..` and symlinks.** The deviation is small and rarely matters, but a build system that depends on physical
  `..` across a symlinked directory would break; the gate runs would find it.
* **Adapters drifting into the real interface.** Mitigated by P1 and UVI2, and by describing each file as a
  rendering of something native.
* **Terminal and session scope.** Detach, pty and remote login are three features that look small and are not;
  UV4 to UV7 are sliced so each is useful alone.
* **A custom shell language.** Fewer ecosystem scripts, and models know POSIX sh better; mitigated by a small,
  specified grammar, golden transcripts and a differential check against upstream `es` on the shared subset.
