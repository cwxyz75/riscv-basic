# S4: the first interactive system (specification, v2)

Status: v2, revised after an independent spec review (Fable, 2026-11-11) that found the draft
contradicted the real syscall contract in several places. Written before any S4 code (OS track
rule: spec, then acceptance checks, then code, then independent review). Parent documents:
`OS-ARCHITECTURE.md` (sections 9, 10, 12, 14), `SYSCALLS.md`, `INVARIANTS.md`. Decisions
become OD51+ once the slices land. Section 14 lists what changed and the defaults I chose for
the review's product-owner questions.

## 1. Goal and acceptance

After boot the machine prints a startup message and a prompt on the serial console. A user (or
a script) types shell commands; programs run as separate user processes started by a userspace
launcher, find files in a memory-backed filesystem server, and are connected to the terminal and
to each other with sockets.

**Acceptance (ROADMAP S4):** a scripted shell session passes (`ls`, `cat`, `echo`, a pipeline),
and Ctrl-C kills the foreground job. The session is driven from the host by an independent
Python harness over the serial port, against a golden transcript derived by hand from section 8
(never from the implementation's output).

Out of scope for S4: persistence, network, graphics, POSIX libc, `fork`/`exec`, job control
beyond one foreground pipeline, terminal escapes beyond echo/backspace/Ctrl-U/Ctrl-C/Ctrl-D,
file creation from the shell (no redirection; a later slice), indexed attributes, symlinks,
union directories, per-job quotas (Q9), SMP.

## 2. Slices (one PR each; each leaves a bootable, green system)

| Slice | Content | Acceptance check, written first |
|---|---|---|
| S4a | kernel: `KILL` process right, boot image VMO, serial socket bridge (with `##` escaping); pure `proto` crate (all wire formats of this document); `rt` growth (heap, startup-message reader, helpers); `init` becomes supervisor | host tests of `proto` (round trips, malformed input, mutation fuzz); QEMU: kernel suite unchanged and green; a child started with a startup message echoes a line through the serial socket |
| S4b | `launcher` service (`Spawn` by VMO) | QEMU: spawn each boot-image program; corrupt ELF and oversized requests refused with no leak |
| S4c | `memfs` crate (pure) + `memfsd` (file protocol, `/bin` as boot-image-backed read-only nodes, `GetVmo`) | host model tests vs an independent reference; QEMU client suite covers every protocol error and every quota |
| S4d | `consoled` (line discipline, foreground set, Ctrl-C) | QEMU + Python harness: echo, line delivery, Ctrl-C kills a registered job |
| S4e | `sh` (parser crate + process), `ls`, `cat`, `echo`, `probe`, `spin`, pipes | the acceptance session, plus the confinement probe |

## 3. Kernel-side changes (all in S4a)

1. **`KILL` right.** A new right bit (`KILL`, bit 10; `Rights::ALL` grows to 0x7ff). `process_kill`
   requires `KILL` instead of `MANAGE`. `Rights::PROCESS` includes it; `process_self` handles
   do not. Rationale: `MANAGE` also permits `process_map`, `thread_create` and `process_start`,
   so handing it to the console server would let it inject code into jobs (OS-ARCH section 9
   already wants start/kill/memory as separate rights). Closing the last process handle
   still kills a started process (unchanged).
2. **Boot image.** `build.rs` builds a flat archive (`"BIMG"`, count:u32, entries
   `name_len:u16, name, offset:u32, size:u32`, 16-byte aligned data) of every user program and
   the fixture files of section 8. The kernel makes one VMO from it (read-only handle) and
   passes it to `init` in its startup message. `init` remains the only program the kernel
   parses.
3. **Serial socket bridge.** A kernel thread connects the UART to one end of a socket pair;
   the other end goes to `init`. UART input is buffered in a bounded ring in the interrupt path
   (overflow drops and counts); the thread copies both ways and never blocks the interrupt
   path or the kernel's `##` output. **All UART output goes through one shared line state (`kcore::linestate`, OD52): a `##` at the
   start of a line a user program writes becomes `# #`, and kernel verdict text (`##...`) is
   preceded by a line break when the physical line is not at its start**, so no user program,
   and no file `cat`ed to the terminal, can forge a verdict line (`##TEST`, `##PANIC`,
   `##DONE`) or swallow a real one. Bytes pass through unchanged (no text conversion). The
   bridge is a polling thread (`TIDY-UP.md` T14); input it cannot deliver yet is kept (4 KiB)
   and retried every tick, and bytes lost beyond that are counted and checked. `debug_write` stays for test programs (OS-ARCH `##` protocol); S4 services use it only for
   `init`'s own `##` lines and the `rt` panic handler.
4. **`SYSCALLS.md` fixes:** `MAX_OBJECTS` is 16384 (document drift); the launcher's default
   stack is 256 KiB (OD42), not 64 KiB.

## 4. Roles, handles and rights (the authority table)

Every S4 process, every handle it holds, and with what kernel rights (K) and protocol rights
(P). Anything not listed is not held (no ambient authority beyond the syscalls of section 13).

| Process | Handles | K rights | P rights |
|---|---|---|---|
| `init` | boot image VMO; serial socket end; process handles of services; service channels | VMO read/map; socket R/W; process all | n/a |
| `memfsd` | boot image VMO (read-only); its service channel from `init`; client connections (it is server end) | VMO read | owns the tree |
| `launcher` | service channel; (per request) the image VMO, STDIN/STDOUT sockets, directory connections | per request | per request |
| `consoled` | serial socket end (R/W/WAIT); control channel; `KILL|WAIT|TRANSFER`-only duplicates of foreground job handles | as listed | none |
| `sh` | tty socket (as STDIN and STDOUT, duplicated); root connection to memfs (read-only); `/tmp` connection (read-write); launcher channel; console control channel; job process handles | job handles: all but `MANAGE` is not needed: `WAIT|KILL|DUPLICATE|TRANSFER|INSPECT` | root: READ+ENUMERATE; `/tmp`: all |
| `ls`, `cat`, `echo` (confined) | STDIN, STDOUT; `CWD` (fresh read-only connection) | socket R or W only | READ+ENUMERATE |
| trusted program (`/etc/trusted`) | as confined plus `ROOT` (fresh read-only root connection) | | READ+ENUMERATE |

Rules that make this table true:
- A handle from a service is never a *duplicate of the service's endpoint*: duplicating a
  channel endpoint makes two holders share one connection (each can steal the other's replies
  or stall it by not reading). **Every client gets its own connection**: the file protocol has
  a `Clone(rights, endpoint)` op (and `Open` with an empty path) that binds a fresh endpoint to
  the same node with equal or fewer rights. A child's `CWD` is always such a fresh connection.
- Pipe ends passed to stages are narrowed with `handle_replace` to read-only / write-only but
  **keep `WAIT`** (a blocked read or write waits on its own end; without WAIT the runtime fails
  instead of spinning).
- The launcher closes its end of the bootstrap channel after writing the startup message (the
  message stays readable; the child cannot park IPC budget on that channel).
- Every handle forwarded to a child must carry `TRANSFER`; the launcher forwards exactly the
  handles in the request and strips nothing implicitly.

## 5. Startup message (`proto::startup`)

Exactly one message on the bootstrap channel (handle 1), written by `init`/the launcher before
the child runs, read by `rt` before `main`. Little endian:

```
u32 magic = 0x53544152, u32 version = 1,
u32 argc, u32 envc, u32 nsc, u32 nstdio
argc strings, envc strings              (u16 length, bytes; no NUL)
nsc namespace entries                   (u16 length, prefix bytes, u32 handle_index)
nstdio (role u32, handle_index u32) pairs
```

The message's handle array holds the handles; indices refer to it. Roles: `STDIN=1`, `STDOUT=2`
(stderr is `STDOUT`), `SERVICE_FS=3`, `SERVICE_LAUNCH=4`, `SERVICE_CONSOLE=5`, `BOOT_IMAGE=6`,
`SERIAL=7`, `SERVICE_INIT=8`. **Programs read the message themselves** with `rt::startup::read`
(not before `main`); a message that does not parse, or that arrives with a handle nothing
refers to, exits `-2001`; no message at all exits `-2002`. Strings contain no NUL; namespace
prefixes are only `"."` and `"/"`, each at most once. Namespace prefixes: `"."` (the working directory) and `"/"`
(the root: only for trusted programs). Limits: message at most 16 KiB; at most 32 handles; each
string at most 1024 bytes; `argc` at most 64, `envc` at most 16, `nsc` at most 4. The parser
(pure, in `proto`, `forbid(unsafe_code)`, fuzzed) rejects: wrong magic/version, counts that
disagree with the bytes, indices out of range, a handle referenced twice, an unknown or
duplicate role, trailing bytes. On rejection the child exits `-2001`. Nothing panics.

## 6. Launcher

A userspace service on one channel (from `init`, handed to `sh` only). It holds **no filesystem
authority**: the caller supplies the program.

`Spawn` request (`proto::launch`, normative byte layout in its golden test): `txid:u32, op:u16=1,
pad:u16=0, image_offset:u64, image_len:u64, startup_len:u32`, then the startup-message bytes to
deliver (section 5 format), and in the message handles: element 0 is a VMO handle (the image,
or the boot image with offset/length; READ|MAP rights), then the handles the startup message
references (at most 33 handles in all). The reply is `txid:u32, status:i32` plus, on success,
the process handle. It (1) validates every size before allocating (image
at most 4 MiB, at most 16 segments, total mapped at most 16 MiB, startup message at most 16
KiB), (2) parses the ELF with `kcore::elf` (pure, linked in), (3) creates a process, one VMO per
PT_LOAD segment (bytes copied from the image VMO with `vmo_read`/`vmo_write`, no large channel
copies), a 256 KiB stack VMO, maps them, (4) creates the bootstrap channel, writes the startup
message with the forwarded handles, closes its end, (5) `process_start`s, (6) replies with
status and the **process handle** with rights `WAIT|INSPECT|KILL|DUPLICATE|TRANSFER` (not
`MANAGE`, so the caller cannot map into or add threads to the child). On any error everything
created is released (leak checks must show it). It reads requests with exactly
`max_handles = 33` (the image VMO plus at most the startup message's 32) and `max_bytes` = the
28-byte header plus at most a 16 KiB startup message; a message that does not fit
(`BUFFER_TOO_SMALL`) is a protocol violation: the launcher closes that client connection (it
would otherwise block the queue head forever) and closes every handle that arrived with a bad
request. The launcher serves **one client**, so closing that connection ends the launcher (and
the supervisor rebuilds the stack; `TIDY-UP.md` T30).

## 7. File protocol (memfs)

Authority is directory connections: a connection is a channel whose server end is bound to one
node with a rights mask `{READ, WRITE, ENUMERATE, CREATE}`.

**Wire format.** Request: `txid:u32, op:u16, flags:u16, rights:u32, a:u64, b:u64, len:u32,
payload[len]`; reply: `txid:u32, status:i32, a:u64, len:u32, payload[len]`. At most 33 KiB per
message; `Read`/`Write` data at most 32 KiB. A request carries the number of handles its op
defines (0, except `Open`/`Clone` which carry exactly 1); anything else is a protocol violation.

Ops on a **directory** connection: `1 Open(path, flags, rights)+endpoint`, `2 Clone(rights)+
endpoint`, `3 ReadDir(cookie = last name seen, empty = start)`, `4 Mkdir(name)`, `5 Unlink(name)`,
`6 Stat(path)`; `2 Clone` is also valid on a file connection. Ops on a **file** connection: `10 Read(offset, len)`, `11 Write(offset, data)`,
`12 GetSize`, `13 SetSize(n)`, `14 Stat`, `15 GetVmo` (no arguments; for every boot-image file, `/bin` programs included: the reply carries a
read-only handle to the *whole* boot image VMO, `a` = the file's offset and the 8-byte payload
its length, so no program is ever copied through channels; any other file is `BAD_REQUEST`). Flags for `Open`: `CREATE`, `EXCLUSIVE`, `TRUNCATE`,
`DIRECTORY`.

**Statuses** (protocol statuses are kernel statuses plus these, negative `i32`): `-100 NOT_FOUND`,
`-101 EXISTS`, `-102 NOT_EMPTY`, `-103 IS_DIRECTORY`, `-104 NOT_DIRECTORY`, `-105 NAME_TOO_LONG`,
`-106 QUOTA`, `-107 BAD_ELF`, `-108 BAD_REQUEST`.

Rules:
- A path is components separated by `/`; each component is 1..255 bytes, not `.` or `..`, with
  no NUL or `/`; at most 32 components and 1024 bytes. A leading `/` is `BAD_REQUEST`.
  Relative to the directory connection, with no way to name a parent: a connection is a
  sandbox boundary.
- Requested rights must be a **subset** of the connection's (otherwise `ACCESS_DENIED`).
  `Open` with `CREATE` needs `CREATE` on the connection.
- On `Open`/`Clone` failure the passed endpoint is closed by the server (the client sees
  `PEER_CLOSED` after the error reply on the old connection).
- `Unlink` of a non-empty directory is `NOT_EMPTY`; unlinking a node with open connections
  succeeds (the connections keep a reference and read the old contents; the node's bytes are
  freed with the last connection). `SetSize` charges the quota for growth (holes count as
  bytes). Offsets and lengths are checked with overflow-safe arithmetic.
- Quotas (server totals, enforced, each with a test): 8192 nodes; 8 MiB of file bytes; file
  size at most 4 MiB; 2 MiB of names and per-node overhead (each node is charged its name plus
  96 bytes: 8192 nodes with 255-byte names would otherwise be megabytes the byte quota never
  sees); 2 MiB of replies held for clients with no room (a connection that would exceed it is
  dropped); every allocation a client can drive is fallible, so the worst case (8 + 2 + 2 MiB
  plus message copies) fits the server's 16 MiB heap; **at most 200 live connections in all** (one port holds 256 bindings,
  leaving room for the service channel); excess `Open`/`Clone` is `QUOTA`. Servers cannot tell
  clients apart (INVARIANTS H5), so per-client limits are not specified.
- `Open` needs READ on the directory connection (it traverses), `Mkdir`/`Unlink`/`SetSize`/`Write`
  need WRITE, `ReadDir` needs ENUMERATE, everything else READ. The server also refuses an endpoint
  whose peer end it already serves (`object_info`): both ends of one channel handed over would
  make connections nobody could close and a reply/request echo that never ends. A request with the wrong number
  of handles for its op is answered `BAD_REQUEST` and its handles are closed; only a structurally
  broken one (too short to carry a txid, more handles or bytes than any request may have) closes
  the connection. The endpoint of an `Open`/`Clone` must be a channel end with READ, WRITE, WAIT
  and TRANSFER; anything else is `BAD_REQUEST`. A refused `Open` has no side effect (the
  connection limit is checked before anything is created or truncated; `Clone` ignores flags).
- Event loop: one port, every connection bound for `READABLE|PEER_CLOSED`. A reply that
  cannot be written (`SHOULD_WAIT`, queue full; or `NO_MEMORY`, the kernel's global IPC budget)
  is held as that connection's single pending reply and the connection is polled on a 10 ms
  timer instead of being bound to the port (a channel's WRITABLE means room for *some* message,
  so a binding would spin: `TIDY-UP.md` T28); a client that never reads therefore stalls only
  itself, and the connection is closed after 30 s. Requests are read with exact `max_handles` for their op; `BUFFER_TOO_SMALL` or any
  unexpected handle is a protocol violation (close the connection, close every received
  handle). A malformed request gets `BAD_REQUEST` when a `txid` can be read, else the connection
  is closed.
- The pure `memfs` crate holds the tree, name validation, quota accounting and all
  operations; `memfsd` is a thin loop over it. File data is stored in fixed 16 KiB chunks in the server's heap (16 MiB cap, so the 8 MiB
  file quota fits with room to spare): a file holds about the bytes it counts, never twice.
- An image name is the absolute path of the file without its leading slash (`bin/hello`,
  `etc/trusted`, `home/sub/inner.txt`; `kernel/build.rs` makes them from the `bootfs/` tree and the
  built programs). `/bin` and `/etc` are **boot-image-backed read-only nodes** (no write path exists); `/tmp` is
  the only writable subtree; `/home` is the shell's starting directory (read-only fixtures of
  section 8). `init` unpacks nothing: `memfsd` builds the directory entries from the boot
  image table.

## 8. Console, shell and programs (observable behaviour: this section is the golden)

**Console server.** `consoled` owns the serial socket and is fully event-driven (it never blocks
in a serial write: output is queued, bounded, oldest-first dropped with a counter; echo is
batched per read). It runs `console::LineEditor` unchanged (echo, backspace, Ctrl-U, Ctrl-C),
which caps lines at 255 bytes (extra bytes are ignored) and ignores 0x04 and tab: consoled
intercepts 0x04 itself. One tty socket per session; bytes the client writes go to the serial
socket with `\n` expanded to `\r\n`; each completed line is delivered with its `\n`.
**Ctrl-D** at an empty line delivers the single byte `0x04` (no newline); the shell treats it as
`exit`. (Sockets have no half-close, so there is no end of stream on the terminal for
programs: `cat` with no arguments on the terminal ends only by Ctrl-C. Documented, tested.)

**S4d decisions (implemented).** The terminal socket reaches `consoled` as role `TTY` (9) next to
`SERIAL` and `SERVICE_CONSOLE`. A control message is answered with `txid:u32, status:i32`
(0 = done), so the shell can wait for the registration before it releases its children. The
"one-byte `0x03` line" after Ctrl-C is exactly one byte, 0x03, with no newline (like the
Ctrl-D byte). Output the serial line cannot take is queued up to 16 KiB; beyond that the oldest
bytes are dropped and, once the queue drains, `\r\n[console: N bytes dropped]\r\n` is printed
(N = the bytes dropped since the last notice). Lines waiting for a client that is not reading are
held up to 4 KiB, then dropped and counted. The session ends (consoled exits 0 after flushing the
serial line for up to a second) when the client closes its terminal socket or the serial line
goes away. Ctrl-C type-ahead is discarded only within the chunk that carried the Ctrl-C (a
serial read is at most 256 bytes), which is what the editor's own contract allows.

**Ctrl-C (0x03).** consoled (1) calls `process_kill(h, -130)` on every handle of the foreground
set (no IPC needed, so it works when the IPC budget is exhausted; `BAD_STATE` from an
already-dead job is fine), (2) discards the line being edited and type-ahead, (3) echoes
`^C\r\n`, (4) sends the shell a one-byte `0x03` line on the tty socket so the shell reprints its
prompt even when no job was running. The foreground set is registered by the shell on the
control channel (`SetForeground(n <= 16 handles)`, `ClearForeground`; handles carry only
`KILL|WAIT|TRANSFER`); a Ctrl-C between spawn and `SetForeground` is lost (documented race:
the shell registers before releasing the children, see below).

**Shell (`sh`, grammar in the pure `shell-core` crate).** Banner on boot: `riscv-os S4 ready`
then a newline; prompt: `$ ` (dollar, space). A line is `stage ('|' stage)*`; words are split
on blanks; single or double quotes group (no escapes, expansion, variables, globbing); `#` starts a
comment; lines longer than 255 bytes cannot be typed (editor limit); at most 16 stages, 64 words
per stage. Empty line: just a new prompt. Builtins: `cd`, `pwd`, `exit`, `help`, `halt`.
- `help` prints exactly: `builtins: cd pwd exit help halt` then `programs: ls cat echo probe spin`.
- `pwd` prints the shell's lexical path (`/home` at start). `cd` takes one argument, tracks the
  path lexically and reopens it from the shell's own root connection (the wire has no `..`);
  `..` in a `cd` argument is resolved by the shell's lexical path and is refused above `/`.
- Unknown command: `sh: <name>: not found`. A name containing `/` is refused: `sh: <name>: bad
  command name`. `cd` to a missing directory: `cd: <arg>: not found`.
- `halt` (test builds and the acceptance session): asks `init` to end the session; `init` kills
  the stack and exits 0, which lets the kernel run its leak checks and power off through the
  QEMU test finisher. `exit`/Ctrl-D: the shell exits and `init` restarts it (new banner).

**Spawning and powerbox (confined by default, Q8).** For each stage the shell sends `Spawn` with
the image VMO from `GetVmo` of `/bin/<name>` (it never gives the launcher a path), `STDIN` and
`STDOUT` (sockets), a fresh read-only `CWD` connection (`"."`), the args, an empty
environment, and nothing else. Programs listed in `/etc/trusted` (`ls`, `cat`; a boot-image
file, unwritable) additionally get `"/"`, a fresh read-only root connection, so absolute paths
resolve against it; others (`echo`, `probe`, `spin`) cannot name anything outside `.`. Before
releasing any stage the shell registers every stage's process handle with `consoled`; stages
are started suspended on their first `STDIN` read (they begin only once the shell has closed
its own copies of the pipe ends and sent a one-byte "go" on a private start channel), so the
registration race cannot lose a Ctrl-C.

**Pipelines.** For `a | b | c` the shell creates `n-1` socket pairs; each stage gets `STDIN` and
`STDOUT`: stage *i*'s `STDOUT` is one end of pair *i* (narrowed to write-only), stage *i+1*'s
`STDIN` the other (read-only); the first `STDIN` and last `STDOUT` are the tty socket
(duplicated). The shell closes its own copies of every pipe end after spawning so that end of
stream propagates (the reader sees `PEER_CLOSED` after draining). A writer to a closed pipe gets
`PEER_CLOSED`; `rt` turns that into exit code `141` for the program and the shell does not
mention it. If a stage cannot be spawned, the shell kills the already-spawned stages, prints
the error and prompts. The shell waits for all stages' `TERMINATED` (one port) and then prints,
in stage order, `[exit N]` for each stage whose exit code is nonzero and not 141, then the
prompt. After Ctrl-C a killed stage prints `[exit -130]`.

**Programs** (using only `rt`: stdio, directory operations, a heap; they print `name: error`
messages to `STDOUT` and exit 1 on error):
- `echo args...`: arguments joined by one space plus `\n`.
- `cat [file...]`: files or, with none, STDIN to STDOUT; unreadable file: `cat: <file>: not
  found` (or `not a file`).
- `ls [dir]`: entries sorted bytewise, one per line, directories suffixed `/`; `ls` with no
  argument lists `.`; missing: `ls: <dir>: not found`.
- `probe`: (confinement test) tries, and prints one result line each: `open /etc/trusted: <status>`,
  `open ../x: <status>`, `write ./f: <status>`, `clone: ok|<status>`, `spawn: <status>`;
  expected `NOT_FOUND`/`BAD_REQUEST`/`ACCESS_DENIED` as derived in the golden.
- `spin`: never ends (for Ctrl-C).

**Fixtures** (in the boot image as `fixtures/<path>`, which `memfsd` mounts under `/home`; listed by `ls /home`): `/home/hello.txt` containing
`hello, world\n`; `/home/lines.txt` containing `one\ntwo\nthree\n`; directory `/home/sub/` with
`/home/sub/inner.txt` containing `inner\n`. A fixed fixture set keeps the golden stable when
programs are added to `/bin`.

## 9. Runtime (`rt`) additions

A user heap (`#[global_allocator]` over `linked_list_allocator`; growth by mapping fresh VMOs
into a **reserved contiguous virtual range** of 16 MiB, so the allocator sees one region;
capped by that range); a typed startup-message reader; `read_exact`/`write_all` that wait on the
right signal (no deadline for stdio, so a slow pipe reader does not break a pipeline; deadlines
only for service calls; `NO_MEMORY` from the global IPC budget is retried every 10 ms for up
to 1 s and then returned: documented limit, the budget is shared by all processes until Q9);
`stdin()`/`stdout()` over sockets; a file client over the file protocol; a service client helper
over `channel_call`. Wrappers return `Result`; the panic handler prints `##PANIC user program`
and exits 101 (through `debug_write`).

## 10. `init` as supervisor

`init` contains its own process builder (shared with the launcher in `rt::spawn`) to start
`memfsd` and `launcher` (the first two processes cannot use a launcher). Boot sequence:
`memfsd`, `launcher`, `consoled`, `sh`. **Death policy for S4:** if `memfsd`, `launcher` or
`consoled` dies, `init` logs a `##` line, tears down and rebuilds the whole stack in order (all
client channels would be dead anyway and no rediscovery exists); `sh` dying (exit, Ctrl-D,
crash) just restarts `sh` (and consoled starts a new session). `init` keeps its own duplicate
of the serial socket end so that a restarted `consoled` can receive it. In **test builds**
(the existing 145-test suite and all slice suites) a service death is a test failure, not a
restart. Boot mode is a start argument: `0` normal suite (today), `2` interactive. `init`
exits on `halt` (section 8).

## 11. Security and robustness requirements (reviewer checklist)

- No program gets a handle it was not explicitly given (section 4 table); no child shares a
  connection with its parent; `..` and absolute paths cannot escape a directory connection;
  rights only shrink along every path; the launcher has no filesystem authority; consoled can
  only `KILL`.
- Every size/count read from a message is validated before allocation; every service has a
  fixed ceiling in memory and connections; a hostile client can make a server return errors,
  stall itself, or be dropped, never panic or exhaust the server heap. **Known limit:** the
  kernel's global IPC budget (2 MiB) is shared and ambient: a confined program can fill it
  with sockets and make writes fail system-wide until Q9; services treat `NO_MEMORY` as in
  sections 7 and 9. Other ambient syscalls kept for S4 (recorded in an OD): `process_create`,
  `vmo_create`, `socket_create`, `debug_write` (with `##` escaping on the console path).
- No server blocks forever on one client; no unbounded queues.
- Death of any service or client leaves no leaked objects (the leak checks run at the end of
  the session via `halt`) and no stuck peer.
- Every message parser is a pure function with a fuzz test; pure crates are
  `forbid(unsafe_code)`.

## 12. Verification plan (independent oracles first)

- `proto`, `memfs`, `shell-core`: host unit tests, **model tests** against an independent
  reference (`memfs` against a trivial `BTreeMap<Vec<String>, Node>` path model written
  separately), property tests, mutation fuzz of parsers, a byte-level golden for every wire
  format in this document (hand-written hex).
- QEMU suites: per slice, client tests over the real services (error and quota coverage), with
  REQUIRED names added to `scripts/os_boot_test.py`.
- **Scripted session** (`scripts/s4_session_test.py`, independent Python): drives the serial
  port, compares with `scripts/s4_session.golden` hand-derived from section 8, with timeouts and
  a mutation check (a wrong golden must fail). It covers: the banner and prompt; `help`;
  `ls`; `ls sub`; `cat hello.txt`; `cat lines.txt sub/inner.txt`; `echo hi there`; `echo "a  b"`;
  `echo hello | cat`; `cat lines.txt | cat | cat` (three stages); `cd sub` then `pwd` and `ls`;
  `cd ..`; `cd nope`; an unknown command; a name with `/`; `cat nope`; `ls /bin` (a trusted
  program sees the root) and `probe` (confined: results as derived); `spin` then Ctrl-C then a
  following command proving the shell still works; `spin | cat` and Ctrl-C (all stages killed);
  and finally `halt`.
- **Leak and liveness** are checked by the kernel after `halt` with the existing
  `process_teardown_returns_every_frame` / `no_process_or_vmo_survives_init` /
  `process_exit_releases_all_objects` checks, plus (S4a) a kernel-thread count check. The
  `rt` heap's mapped VMOs do not "shrink", so no baseline comparison is attempted between
  rounds inside a process.

## 13. Syscall authority S4 leaves ambient (to tighten with jobs, Q9; see `TIDY-UP.md` T1, T2, T4)

`process_create`, `vmo_create`, `socket_create`, `channel_create`, `event_create`,
`debug_write`, thread creation in one's own process. A confined program can therefore create
processes (up to 64), consume frames (no quota), and fill the IPC budget. This is recorded
risk, not a goal.

## 14. Changes from the draft and defaults chosen for the review's questions

Fixed from the review: confinement hole (fresh connections via `Clone`, read-only `/bin` and
`/etc`, read-only child `CWD`, writable `/tmp` only for the shell); consoled authority (a new
kernel `KILL` right, rights masks corrected, kill before echo, interrupt line to the shell);
supervision (whole-stack rebuild, `sh` restart, `init` keeps the serial end, process builder
in `rt`, boot modes, `halt`); numbers (memfs quota 8 MiB, 200 connections, backpressure
instead of close-on-full, exact `max_handles` and handle hygiene, Ctrl-D, editor limits,
256 KiB stack, `MAX_OBJECTS` drift); acceptance transcript (all texts defined, fixtures,
`[exit N]` rules, byte-level wire formats, leak check through `halt`); launcher confused
deputy (no paths: `GetVmo` plus VMO slices, no channel copies of programs); `##` forgery on the
serial bridge; stale statements.

Defaults chosen for the product-owner questions (change any by saying so):
1. `ls` and `cat` are in `/etc/trusted` (they get a read-only root; recorded as `TIDY-UP.md`
   T3); other programs are confined. A fuller powerbox (the shell opens path arguments) waits for the manifest-driven
   shell milestone.
2. A child's `CWD` is read-only.
3. A kill-only `KILL` right is added to the kernel in S4a.
4. `process_create`, VMO creation and `debug_write` stay ambient for S4 (section 13).
5. Whole-stack rebuild on a core service death is enough for S4.
6. No terminal end-of-file for programs in S4 (Ctrl-C only); no socket half-close.
7. Serial access through the kernel bridge thread plus a socket (OD52), with `##` escaping.
