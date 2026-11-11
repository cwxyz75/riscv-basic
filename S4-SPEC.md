# S4: the first interactive system (specification)

Status: DRAFT for review, written before any S4 code (OS track rule: spec, then acceptance
checks, then code, then independent review). Parent documents: `OS-ARCHITECTURE.md`
(sections 9, 10, 12, 14), `SYSCALLS.md`, `INVARIANTS.md`. New decisions here become OD51+
once accepted.

## 1. Goal and acceptance

After boot the machine prints a startup message and a prompt on the serial console. A user
(or a script) types shell commands; programs run as separate user processes started by a
userspace launcher, find files in a memory-backed filesystem server, and are connected to
the terminal and to each other with sockets.

**Acceptance (from ROADMAP S4):** a scripted shell session passes (`ls`, `cat`, `echo`, a
pipeline), and Ctrl-C kills the foreground job. The scripted session is driven from the
host by an independent Python harness over the serial port, against a transcript derived by
hand from this document (never from the implementation's output).

Out of scope for S4 (named so nobody assumes them): persistence (disk), network, graphics,
POSIX libc, `fork`/`exec`, job control beyond one foreground pipeline, terminal escape
sequences beyond echo/backspace/Ctrl-U/Ctrl-C/Ctrl-D, indexed attributes, symlinks, union
directories, per-job quotas (OS-ARCHITECTURE Q9), SMP.

## 2. Slices (one PR each, each leaves a bootable, green system)

| Slice | Content | Acceptance check, written first |
|---|---|---|
| S4a | `proto` crate (message formats, pure); user runtime growth (`rt` heap, channel/socket helpers, startup-message parser); boot image + kernel-provided serial socket; `init` becomes a supervisor | host tests of `proto` (round trips, malformed input); QEMU: init starts a child through the new startup message and the child echoes a line through the serial socket |
| S4b | `launcher` service (ELF parse in userland, process construction, startup message) | QEMU: spawn each program of the boot image through the launcher; corrupt ELF refused with no leak (existing leak checks) |
| S4c | `memfs` crate (pure tree, quotas) and `memfsd` server (file protocol) | host model tests against an independent reference model; QEMU: a client suite over channels covers every protocol error |
| S4d | `consoled` (line discipline, foreground job, Ctrl-C) | QEMU + Python harness: typed text is echoed and delivered as lines; byte 0x03 kills a registered job |
| S4e | `sh` (parser crate + shell process), `ls`, `cat`, `echo`, pipes | the acceptance session |

## 3. Boot and the first processes

1. The kernel embeds a **boot image**: a flat archive (`magic`, count, then entries
   `name_len:u16, name, size:u32, bytes`, all little endian, 4-byte aligned) holding every
   user program (`init`, `launcher`, `memfsd`, `consoled`, `sh`, `ls`, `cat`, `echo`, plus
   test programs). It is built by `build.rs` from the user ELFs. The kernel makes one VMO
   from it (read-only handle) and starts `init` (the only executable it parses itself).
2. `init` receives, in its startup message: the boot image VMO, and a **serial socket**
   (below). It starts services in this order, each given exactly the handles it needs:
   `memfsd` (then unpacks `/bin/*` from the boot image into it), `launcher`, `consoled`,
   `sh`. `init` supervises: if a service dies it logs a `##` line on the serial socket and,
   for `memfsd`/`launcher`/`consoled`, restarts it (state of `memfsd` is lost: documented).
3. **Serial socket.** Today any process can call `debug_write` (ambient authority: kept
   for test programs and for the kernel's `##` structured output). For the console, the
   kernel provides a *kernel-side bridge*: a kernel thread connecting the UART to one end
   of a socket pair; the other end goes to `init`, which hands it only to `consoled`. The
   bridge copies bytes both ways; it never blocks the UART interrupt path (input is
   buffered in a bounded ring; overflow drops bytes and counts them). `##`-prefixed lines
   that kernel tests print are unaffected. (Hardware access by user drivers, S6, will
   replace the bridge with an interrupt object plus MMIO VMO.)
4. `debug_write` from user programs is *not* used by S4 services except `init`'s `##` lines.

## 4. Startup message ("process args")

Exactly one message on the bootstrap channel (handle 1), written by the launcher/`init`
before the child runs and read by `rt` before `main`. Layout (little endian):

```
u32 magic = 0x53544152 ("STAR"), u32 version = 1
u32 argc, u32 envc, u32 nsc, u32 nhandles
then argc+envc strings (u16 length, bytes, no NUL), then nsc namespace entries
(u16 length, prefix bytes, u32 handle_index), then nhandles role words (u32 role)
```

The message's handle array parallels the role words. Roles: `STDIN` and `STDOUT` (sockets;
stderr is `STDOUT`; for the terminal both are the same tty socket, duplicated), `CWD` (directory handle), `ROOT` (directory
handle, only for trusted programs), `PROCESS_SELF`, `LAUNCHER` (service channel),
`FS` (service channel to memfs), `CONSOLE_CTL`, `BOOT_IMAGE`, `SERIAL`. Limits: the whole
message at most 16 KiB, at most 32 handles (kernel limit is 64), strings at most 1024
bytes, argc at most 64. A parser rejects every violation (wrong magic/version, counts that
disagree with the bytes, indices out of range, duplicate or unknown required roles,
trailing bytes) with an error and the child exits with code `-2001`; nothing panics. The
parser is in the pure `proto` crate with a mutation fuzz (as the ELF loader has).

## 5. Launcher

A userspace service. Request on its service channel: `Spawn{ path | image VMO handle,
args, env, namespace, handles with roles }`. It (1) obtains the image (reads the file
through the file protocol using a directory handle it was given, or takes the VMO),
(2) parses the ELF with `kcore::elf` (the same validating parser the kernel uses; pure, so
linked into the launcher), (3) creates a process, one VMO per PT_LOAD segment (copying
file bytes, rest zero), a stack VMO (default 64 KiB plus guard), maps them, (4) creates the
bootstrap channel, sends the startup message, `process_start`s it, (5) replies with the
**process handle** (rights: wait, inspect, manage, transfer) to the caller. Errors are
replies (`BAD_ELF`, `NO_MEMORY`, `NOT_FOUND`, `TOO_BIG`); on any error everything created
so far is released (leak checks must show it). The launcher validates every size against
limits before allocating (image at most 4 MiB, segments at most 16, total mapped at most
16 MiB), and a caller can only pass handles it owns (the kernel guarantees that: they
arrive in the request message). The launcher never grants a child any handle the request did
not contain (no ambient authority).

## 6. File protocol (memfs)

Authority is directory handles: a **connection** is a channel whose server end is bound to
one node with a rights mask `{READ, WRITE, ENUMERATE, CREATE}`. There are no path names in
the kernel and no global namespace.

Requests are messages `txid:u32, op:u16, ...`; replies echo `txid` and carry `status:i32`.
Ops on a **directory** connection: `Open(path, flags, rights, new_endpoint_handle)`,
`ReadDir(cookie) -> entries (bounded batch) + next cookie`, `Mkdir(name)`, `Unlink(name)`,
`Stat(path)`. Ops on a **file** connection: `Read(offset, len<=32 KiB)`, `Write(offset,
data<=32 KiB)`, `GetSize`, `SetSize`, `Stat`, `GetVmo` (later; reserved). `Open` carries a
fresh channel endpoint that the server binds to the opened node, so later traffic does not
pass through the directory (OS-ARCHITECTURE section 10). Rules:

- A path is a sequence of components separated by `/`; each component is 1..255 bytes, not
  `.` or `..`, contains no NUL or `/`; at most 32 components and 1024 bytes in all. Leading
  `/` is not allowed (paths are relative to the directory handle). Violations are
  `INVALID_ARGS`; `..` can never appear, so a directory handle is a sandbox boundary.
- The opened node's rights are the **intersection** of the requested rights and the
  directory connection's rights; asking for more is `ACCESS_DENIED`, never silently
  clamped.
- Limits (quotas, per server, enforced and tested): 8192 nodes, 16 MiB total file bytes,
  64 connections per client channel group, 16 outstanding replies per connection, file
  size at most 4 MiB. Exceeding any is `NO_MEMORY`/`TOO_BIG`; never a server panic.
- A server never blocks on a client: replies are written with `channel_write`; if a client's
  queue is full (`SHOULD_WAIT`) the connection is closed (the client is not reading).
  Clients that close their endpoint free their connection (PEER_CLOSED).
- Single-threaded event loop on one **port** (`port_wait`), every connection bound for
  READABLE|PEER_CLOSED. A malformed request (short message, unknown op, bad length fields)
  gets an error reply when a `txid` can be read, else the connection is closed.
- `pure` crate `memfs`: the tree (directories as ordered maps, files as chunked byte
  vectors), name validation, quota accounting, all operations; no I/O. The server is a
  thin shell over it.

## 7. Console server and terminal discipline

`consoled` owns the serial socket. It runs the existing `console::LineEditor` (echo,
backspace, Ctrl-U, Ctrl-C) in cooked mode. For each **session** it exposes one **tty
socket** (client end handed out over its service channel; one session is enough for S4):

- bytes the client writes go to the serial socket with `\n` expanded to `\r\n`;
- completed lines (including the trailing `\n`) are written to the client end; Ctrl-D on an
  empty line delivers end of stream (closes the socket write side by closing consoled's
  end only if the client asked; for S4 the shell treats it as `exit`);
- **Ctrl-C (0x03)**: the line being edited is discarded, `^C\r\n` is echoed, and every
  **foreground job process handle** registered by the shell is killed (`process_kill`, exit
  code `-130`). The shell registers/clears the set through the console control channel
  (`SetForeground(handles)`, `ClearForeground`) with duplicated process handles (rights:
  `MANAGE`, `WAIT` only). Consoled holds no other authority over jobs.
- A shell crash closes the tty socket; consoled keeps serving and accepts a new session.

## 8. Shell and programs

`sh` reads lines from its stdio socket (the tty). Grammar (pure crate `shell-core`, host
tested): a line is `stage ('|' stage)*`; a stage is words; words are separated by blanks,
single or double quotes group (no escapes, no expansion, no variables, no globbing);
`#` starts a comment; at most 16 stages, 64 words per stage, 1024 bytes per line.
Builtins: `cd`, `pwd`, `exit`, `help`. Anything else is an **external command**: resolved
as `/bin/<name>` in the shell's own `FS` namespace (no PATH), spawned through the launcher.

**Powerbox (confined by default, Q8).** A spawned program receives exactly: `STDIN` and `STDOUT`
sockets (the tty or a pipe end, per stage), a `CWD` directory handle
(a duplicate of the shell's cwd with the same rights), args, and an empty environment. It
does *not* get `ROOT`, the launcher, `FS` or console channels. Programs named in
`/etc/trusted` (plain text) additionally get `ROOT` (read-only). `/bin` is mounted
read-only.

**Pipelines.** For `a | b | c` the shell creates `n-1` socket pairs. A socket end is
bidirectional, but a pipe is one-way, so each stage gets two handles with roles `STDIN` and
`STDOUT`: stage *i*'s `STDOUT` is one end of pair *i* and stage *i+1*'s `STDIN` is the other
end (the first stage's `STDIN` and the last stage's `STDOUT` are the tty socket, duplicated;
a stage that only writes never reads its `STDIN`, and rights on pipe ends are narrowed to
write-only and read-only respectively). `rt` provides `stdin()`/`stdout()` over sockets.
The shell closes its own copies of every
pipe end immediately after spawning so end-of-stream propagates (a reader sees
`PEER_CLOSED` after draining). The shell waits for every stage's `TERMINATED` signal
(through one port) and prints nothing about exit codes except for a nonzero one
(`[exit N]`), after which it prompts again.

Programs: `echo args...`, `cat [file...]` (no files: copy stdin to stdout), `ls [dir]`,
all using only `rt` (stdio, directory operations over the file protocol, a heap).
`ls` output is sorted by name, one per line, directories suffixed `/`.

## 9. Runtime (`rt`) additions

A user heap (a `#[global_allocator]` over `linked_list_allocator`, growing by mapping fresh
VMOs into `process_self`, capped at 16 MiB and by `MAX_REGIONS`); a typed wrapper for the
startup message; blocking helpers (`read_exact`, `write_all` that handle `SHOULD_WAIT` by
waiting on the right signal with a deadline); `stdin/stdout`; a file client over the file
protocol (`open`, `read`, `write`, `readdir`, `mkdir`, `unlink`); a service-client helper
for request/reply with `channel_call`. Every wrapper returns `Result` (no panics on
syscall errors); the panic handler prints a `##PANIC user program` line and exits `101`.

## 10. Security and robustness requirements (reviewer checklist)

- No program gets a handle it was not explicitly given; `..` and absolute paths cannot
  escape a directory handle; rights only shrink along every path.
- Every size/count read from a message is validated before allocation; every service has a
  fixed ceiling on memory per client and in total; a hostile client can make a server
  return errors or drop that client, never panic or exhaust the heap.
- No server blocks forever on one client; no unbounded queues (sockets and channels are
  bounded by the kernel; the kernel's global IPC budget is 2 MiB).
- Process death of any service or client leaves no leaked objects (the existing leak
  checks run at the end of the suite) and no stuck peer (everyone waits on
  `PEER_CLOSED`).
- Every message parser is a pure function with a fuzz test; the pure crates are
  `forbid(unsafe_code)`.

## 11. Verification plan (independent oracles first)

- `proto`, `memfs`, `shell-core`: host unit tests, **model tests** against an independent
  reference (for `memfs`: a trivially simple `BTreeMap<String, Vec<u8>>` path model written
  separately), property tests, and mutation fuzz of parsers.
- ELF launching: the existing ELF fuzz corpus plus real programs.
- QEMU suites: the `init` user test suite grows service client tests per slice; the boot test
  script's REQUIRED list grows with them.
- **Scripted shell session** (`scripts/s4_session_test.py`): independent Python harness
  that types a script over the serial port and compares the output with a hand-derived
  golden file (with timeouts and a mutation check that a wrong golden fails). It covers
  `ls`, `cat`, `echo`, `echo hi | cat`, a three-stage pipeline, a quoted argument, an unknown command, a long
  line, `cd` and relative `ls`, Ctrl-C of a program that never ends (a test program in the
  boot image), and the shell still responding afterwards.
- Leak and liveness: after the session the harness asks `init` (via a debug command on a
  test-only channel) for object/process/VMO counts, which must return to the post-boot
  baseline after the foreground jobs are gone.

## 12. Decisions to record (proposed)

OD51 boot image as an archive VMO; OD52 serial socket bridge in the kernel for S4 (user
drivers in S6); OD53 startup message format and roles (`STDIN`/`STDOUT`); OD54 launcher parses ELF in userland with `kcore::elf`; OD55 memfs file protocol
(connection-per-node, `Open` carries the new endpoint, no `..`, rights intersect); OD56
confined-by-default powerbox with an `/etc/trusted` list; OD57 console server owns Ctrl-C
via duplicated job handles; OD58 pipes are socket pairs with two handles per stage.

## 13. Open points (assumed unless the product owner objects)

- One interactive session (no multiple terminals).
- Environment variables exist in the startup message but the shell sets none.
- Exit codes: `0` success; Ctrl-C kill `-130`; launcher errors are negative statuses.
- Redirection (`>`, `<`) is not in S4 (a later slice can add it via a pump in the shell).
- A process can currently create processes without any right (no quotas): the launcher is
  the only S4 user, but this is ambient authority to be tightened with jobs (Q9).
