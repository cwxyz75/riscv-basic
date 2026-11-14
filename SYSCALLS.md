# Syscalls (OS track)

The contract between the kernel and userland, and the one kernel artifact meant to
be readable. Syscalls are specified here *before* they are implemented; the target is
about 50. Each entry has the form below, and every error listed has a test.

Status of this file: **S3a** specifies the ABI, status codes, rights, signals and the
handle and channel/event calls (their pure logic is built and model-tested in
`kcore`). The rest of the S3 calls are listed in the index as *planned* and get full
entries before their implementation (S3b, S3c).

## ABI (decision OD36)

- A syscall is `ecall` from user mode (U-mode). The number is in `a7`; arguments are in
  `a0` to `a5`; nothing else is read. On return `a0` is a **status** (0 or a negative
  code, below) and `a1` onwards carry results where an entry says so. All other
  registers are preserved.
- A **handle** is a 32-bit value. 0 is never valid. The low 13 bits are the table slot
  plus 1 (at most 8191 slots); the high 19 bits are a generation that changes every time the slot is freed,
  so a stale value fails (INVARIANTS H2). Values are meaningful only in the process
  that holds them.
- **Pointers** are user virtual addresses; the kernel validates them (inside the
  caller's own address space, mapped with the needed permission) and copies. It never
  reads user memory through `sstatus.SUM`. A bad pointer is `INVALID_ARGS`, never a
  fault in the kernel.
- There is **no errno** and no partial success: a call either does everything it says
  or changes nothing (the exception, `channel_read` with a too-small buffer, is
  stated where it happens).
- Unknown syscall numbers return `BAD_SYSCALL`.

### Status codes

| Name | Code | Meaning |
|---|---|---|
| OK | 0 | success |
| BAD_HANDLE | -1 | the value is not a live handle in this process (0, never issued, closed, or stale) |
| WRONG_TYPE | -2 | the handle is live but names the wrong kind of object |
| ACCESS_DENIED | -3 | the handle lacks a right the call needs, or the call would widen rights |
| INVALID_ARGS | -4 | an argument is malformed (bad pointer, bad flag, duplicate handle in a list, ...) |
| NO_MEMORY | -5 | the kernel cannot allocate, or this process's handle table is full |
| SHOULD_WAIT | -6 | the operation would block and was asked not to (e.g. reading an empty channel) |
| PEER_CLOSED | -7 | the other end is gone |
| BUFFER_TOO_SMALL | -8 | the caller's buffer cannot hold the result (sizes are returned) |
| TOO_BIG | -9 | a message or list is over a limit |
| BAD_STATE | -10 | the object is not in a state that allows this |
| NOT_SUPPORTED | -11 | valid request the kernel will not do (e.g. a channel endpoint sent over itself) |
| TIMED_OUT | -12 | a wait reached its deadline |
| BAD_SYSCALL | -13 | no such syscall number |

### Rights

A right is a bit in a handle's rights mask. A handle can hold any subset of the rights
its object kind defines; rights can only be removed, never added (INVARIANTS H3).

| Right | Bit | Allows |
|---|---|---|
| DUPLICATE | 0 | `handle_duplicate` of this handle |
| TRANSFER | 1 | sending this handle through a channel |
| READ | 2 | reading (channel messages, VMO contents) |
| WRITE | 3 | writing |
| EXECUTE | 4 | mapping executable (VMO) |
| MAP | 5 | mapping into an address space (VMO) |
| SIGNAL | 6 | changing user signals (event) |
| WAIT | 7 | waiting on the object |
| MANAGE | 8 | controlling a process or thread (map, start, create threads) |
| INSPECT | 9 | reading its properties |
| KILL | 10 | ending a process with `process_kill` (so a service can be given the power to stop a job without being able to map memory into it or add threads) |
| SET_LIMITS | 11 | changing a job's limits (`job_set_limits`); separate from MANAGE so that making things inside a job does not allow lifting its limits |
| DMA | 12 | learning a DMA VMO's physical address (`vmo_phys_addr`) |
| COMMIT | 13 | committing a VMO's pages now (`vmo_commit`); the pages are charged to the VMO's creating job |

`SAME_RIGHTS` (bit 31) is not a right: as the `rights` argument of `handle_duplicate` or
`handle_replace` it means "the same as the source handle".

### Signals

Each waitable object has a 32-bit signal word. Bits 0 to 7 are set by the kernel; bits
24 to 31 are *user signals* that only the holder of an event handle with SIGNAL can
change.

| Signal | Bit | Raised when |
|---|---|---|
| READABLE | 0 | a channel has a queued message |
| WRITABLE | 1 | a channel's peer exists and its queue has room (fewer than `MAX_QUEUED_MSGS` messages and `MAX_QUEUED_BYTES` queued) |
| PEER_CLOSED | 2 | the other end of a channel is gone |
| TERMINATED | 3 | a process or a thread has ended |
| INTERRUPT | 5 | an interrupt object's source fired and has not been acknowledged |
| WRITE_CLOSED | 4 | the peer socket end called `socket_shutdown`: no data beyond what is queued will arrive |
| USER_0 .. USER_7 | 24..31 | set and cleared by `event_signal` |

## Limits

`MAX_HANDLES` 4096 per process (the handle encoding allows 65535); `MAX_MSG_BYTES`
65536 and `MAX_MSG_HANDLES` 64 per message; `MAX_QUEUED_MSGS` 128 and
`MAX_QUEUED_BYTES` 256 KiB per channel endpoint; `MAX_OBJECTS` 16384 kernel objects
alive at once (creating more is NO_MEMORY); `MAX_IPC_BYTES` 2 MiB of queued channel and socket
data (message bytes, headers, queued handles, port bindings) in the whole system;
`MAX_REGIONS` 1024 per address space; `MAX_THREADS_PER_PROCESS` 8 and `MAX_USER_THREADS` 128;
`MAX_PROCESSES` 64.

These keep the kernel from panicking and from running out of its fixed heap (OD50); they are
not per-process limits. A real bound needs per-job quotas (OS-ARCHITECTURE Q9, `TIDY-UP.md`
T1). Per-job limits exist since S4.5 (processes, threads, memory pages, child jobs, objects and queued IPC
bytes, enforced through the job tree: OD54-OD59), and every program the launcher starts runs in a job of its own;
the system-wide caps above remain as the safety net for the kernel's own memory (kernel stacks and kernel
heap use are not yet charged to a job, T1). Stated plainly so nobody relies on more than this.

The syscall layer must reject `nbytes > MAX_MSG_BYTES` and `nhandles > MAX_MSG_HANDLES`
with TOO_BIG *before* copying anything from user memory, so a user-chosen length never
drives unbounded work.

## User address space and process start (S3b)

User addresses are the Sv39 lower half, `0x0000_0000_0000_1000` up to (not including)
`0x0000_0040_0000_0000`; page 0 is never mapped. A process starts with `sp` at
`0x0000_0020_0000_0000`, a user stack of 64 pages (256 KiB) mapped below it and
committed lazily, an unmapped guard page under that, `a0 = 0`, and every other
register 0. The program image is a static ELF64 for RISC-V (`ET_EXEC`, little endian,
one `PT_LOAD` per segment): segments become lazily committed memory objects mapped with
their own permissions (R, RW or RX; a segment that is both writable and executable is
refused, INVARIANTS M2); `PT_GNU_STACK` asking for an executable stack is refused. The
kernel half of the address space is shared by every process and is not reachable from
user mode (no U bit).

The kernel half's top-level page-table slots are fixed once the first process exists (a
process copies them when it is created); the kernel never creates or frees one afterwards
(OD41), and refuses to start a process if one changed.

A user thread that faults on an address no region covers, on a permission its region
does not give, or that executes an illegal instruction, ends its process (exit code
`-1000 - scause`); the kernel never trusts a user fault.

## Syscall numbers and registers (S3b)

`a7` = number, `a0`..`a5` = arguments, results: `a0` = status, `a1`, `a2` = values.

| # | Call | Arguments | Results |
|---|---|---|---|
| 0 | debug_write | `a0` ptr, `a1` len | |
| 1 | process_exit | `a0` code | does not return |
| 2 | thread_yield | | |
| 3 | clock_get | | `a1` ticks since boot (1 ms each) |
| 4 | debug_stat | `a0` which | `a1` the kernel counter |
| 10 | handle_close | `a0` handle | |
| 11 | handle_duplicate | `a0` handle, `a1` rights | `a1` new handle |
| 12 | handle_replace | `a0` handle, `a1` rights | `a1` new handle |
| 20 | channel_create | | `a1`, `a2` the two handles |
| 21 | channel_write | `a0` channel, `a1` bytes, `a2` nbytes, `a3` handles, `a4` nhandles | |
| 22 | channel_read | `a0` channel, `a1` bytes buf, `a2` max_bytes, `a3` handles buf, `a4` max_handles | `a1` nbytes, `a2` nhandles (set on OK and on BUFFER_TOO_SMALL) |
| 23 | channel_read_txid | `a0` channel, `a1` txid, `a2` bytes buf, `a3` max_bytes, `a4` handles buf, `a5` max_handles | `a1`, `a2` as 22 |
| 30 | event_create | | `a1` handle |
| 31 | event_signal | `a0` event, `a1` clear, `a2` set | |
| 32 | object_signals | `a0` handle | `a1` the signal word |
| 33 | object_wait | `a0` handle, `a1` mask, `a2` deadline | `a1` observed signals |
| 34 | object_info | `a0` handle | `a1` koid, `a2` peer koid |
| 40 | vmo_create | `a0` pages | `a1` handle |
| 41 | vmo_read | `a0` vmo, `a1` offset, `a2` buffer, `a3` length | |
| 42 | vmo_write | `a0` vmo, `a1` offset, `a2` buffer, `a3` length | |
| 43 | vmo_get_size | `a0` vmo | `a1` bytes |
| 44 | vmo_create_physical | `a0` resource, `a1` physical address, `a2` bytes | `a1` handle |
| 45 | vmo_create_dma | `a0` resource, `a1` pages | `a1` handle |
| 46 | vmo_phys_addr | `a0` vmo | `a1` physical address |
| 47 | vmo_commit | `a0` vmo, `a1` offset, `a2` length | |
| 50 | process_create | | `a1` handle |
| 51 | process_self | | `a1` handle |
| 52 | process_map | `a0` process, `a1` vmo, `a2` vaddr, `a3` vmo_offset, `a4` length, `a5` flags | |
| 53 | process_unmap | `a0` process, `a1` vaddr | |
| 54 | process_start | `a0` process, `a1` entry, `a2` sp, `a3` bootstrap channel, `a4` arg | |
| 55 | process_kill | `a0` process, `a1` code | |
| 56 | process_info | `a0` process | `a1` exit code |
| 70 | socket_create | | `a1`, `a2` the two handles |
| 71 | socket_write | `a0` socket, `a1` bytes, `a2` length | `a1` written |
| 72 | socket_read | `a0` socket, `a1` buffer, `a2` max | `a1` read |
| 80 | port_create | | `a1` handle |
| 81 | port_bind | `a0` port, `a1` object, `a2` mask, `a3` key | |
| 82 | port_unbind | `a0` port, `a1` key | |
| 83 | port_wait | `a0` port, `a1` deadline | `a1` key, `a2` observed |
| 60 | thread_create | `a0` process, `a1` entry, `a2` sp, `a3` arg | `a1` handle |
| 61 | thread_exit | `a0` code | does not return |
| 62 | thread_info | `a0` thread | `a1` exit code |
| 24 | channel_call | `a0` channel, `a1` args pointer | `a1` nbytes, `a2` nhandles |
| 90 | job_create | `a0` parent job, `a1` pointer to the limits | `a1` handle |
| 91 | job_set_limits | `a0` job, `a1` pointer to the limits | |
| 92 | job_info | `a0` job, `a1` which (0 processes, 1 threads, 2 pages, 3 jobs, 4 objects, 5 ipc bytes) | `a1` used, `a2` limit |
| 93 | job_kill | `a0` job, `a1` exit code | |
| 94 | job_root | | `a1` handle |
| 95 | resource_root | | `a1` handle |
| 100 | interrupt_create | `a0` resource, `a1` source | `a1` handle |
| 101 | interrupt_ack | `a0` interrupt | `a1` claims so far |
| 110 | futex_wait | `a0` address, `a1` expected, `a2` deadline | |
| 111 | futex_wake | `a0` address, `a1` count | `a1` woken |
| 102 | resource_dtb | `a0` resource | `a1` handle, `a2` length |

A handles array is `nhandles` 32-bit values; a handles buffer receives that many.

### Execution model of a syscall (S3b)

A syscall runs with interrupts off on the calling thread's kernel stack, so the timer never
preempts it. It is atomic with respect to every other thread **except at its documented
blocking and wake-up points**: `object_wait`, `channel_call` and the wake-up that follows any
call that can change a signal may switch to another thread in the middle of the call (the
kernel never keeps a reference to shared state across such a point). That keeps S3b simple and is *not* a promise for later: S5 (SMP) replaces
it with locks, and long syscalls (a 64 KiB message copy, the object sweep) are a latency
cost the real-time band will feel. The kernel does not touch the user's `sp`, `gp` or `tp`
(it has its own stack and uses no thread pointer); S5 must reload `gp`/`tp` on entry.
Arguments declared 32-bit (handles, rights, signal masks, `txid`) must fit in 32 bits: a
value with higher bits set is BAD_HANDLE (a handle), ACCESS_DENIED (rights) or INVALID_ARGS
(a signal mask or `txid`), never silently truncated.

### Copying from and to user memory

For every pointer argument the kernel (1) checks `ptr + len` does not wrap and stays in
the user half, (2) walks the process's page tables page by page, committing a lazily
backed page through the fault path if it is not yet mapped, (3) requires the page to be
user-accessible and to allow the access (read for a source, write for a destination),
and copies through the direct map. Anything else is `INVALID_ARGS`; a zero length never
touches memory, whatever the pointer. A call that fails on a pointer changes nothing.
`debug_write` is limited to 4096 bytes per call (TOO_BIG beyond). It is a debugging aid for
the trusted boot-time suite: nothing ties a `##TEST` line to the program that wrote it.

### debug_stat(which) -> value

A measuring aid for the boot-time suite (like `debug_write`; no rights, no effect on the system): `a1` is
a kernel counter, `which` 0 the number of times a thread blocked in a wait, 1 how many of those blocks
were by a thread that had been woken without being satisfied (a *spurious wake*), 2 how many threads
were woken. Any other index reads 0. The counters only grow. They exist to measure the wake path
(roadmap slice W): today a bystander blocked on an unrelated object is woken and goes back to sleep after
nearly every syscall.

### debug_write(ptr, len) -> status

Sends bytes to the serial console. Rights: none (a debugging aid for the boot-time
suite; real processes get a console channel in S4). Output is shown a line at a time: every call is flushed when it ends (a call that does not finish with a newline gets one), lines are cut at 512 bytes, and non-printable bytes are shown as `.`. A line that starts with `##` is prefixed `user: ` unless it is a well-formed `##TEST name PASS|FAIL` result (which the boot-time suite uses), so user code cannot forge the kernel's verdict lines.
Errors: INVALID_ARGS (bad pointer);
TOO_BIG (len over 4096).

### process_exit(code) -> does not return

Ends the calling process: its threads stop, its handle table is closed (INVARIANTS H6),
its address space is torn down.

### thread_yield() -> status, clock_get() -> status, ticks, object_signals(handle) -> status, signals

`thread_yield` gives up the rest of the time slice. `clock_get` returns ticks of 1 ms
since boot. `object_signals` reads the signal word of the object a handle names
(BAD_HANDLE for a bad handle; no right needed, the signals are not secret to a holder).

## Waiting, calling and the bootstrap handle (S3c-1)

### object_info(handle) -> status, koid, peer_koid

The identity of the object a handle names (`koid`: unique among live objects, never reused) and
of its peer (`peer_koid`; 0 for an object with no peer, or whose peer is gone). No right is
needed. It lets a server see that two handles it holds are the two ends of one channel (a client
can hand a server both ends of a channel it made, so that neither end ever sees PEER_CLOSED).
Errors: BAD_HANDLE (also a handle value wider than 32 bits). Tests: `object_info_names_objects_and_peers`.

### object_wait(handle, mask, deadline) -> status, observed

Blocks the calling thread until `signals(handle) & mask != 0` or the clock reaches
`deadline` (absolute ticks, as `clock_get` counts; `0` means "do not wait", `u64::MAX`
"forever"). The result `observed` is the signal word when the call returned (also on
TIMED_OUT).
Rights: WAIT on `handle`. Blocking: until a signal in `mask` is set, the deadline, or the
process is killed.
Effects: none (waiting changes no signal).
Errors, in order: BAD_HANDLE (also if the handle stops naming anything while waiting);
ACCESS_DENIED (no WAIT right); INVALID_ARGS (`mask` is 0 or has
a bit that is not a signal); TIMED_OUT (the deadline arrived first, or `deadline` was 0 and
nothing in `mask` was set). A condition that already holds returns OK at once, even with
`deadline` 0. Only the signals in `mask` end a wait: to stop waiting for a message when the
peer dies, include PEER_CLOSED (`READABLE | PEER_CLOSED`).
Invariants: C3 (no lost wakeup: the check and the sleep are one atomic step).
Tests: `wait_*` in `init`.

### channel_call(channel, args) -> status, nbytes, nhandles

Sends one request and waits for the reply with the same transaction id, in one call.
`args` points to a 56-byte record in user memory (little endian):

| offset | field |
|---|---|
| 0 | `u64` request bytes pointer |
| 8 | `u64` request handles pointer |
| 16 | `u32` request byte count (at least 4: the first 4 bytes are the transaction id) |
| 20 | `u32` request handle count |
| 24 | `u64` reply bytes buffer |
| 32 | `u64` reply handles buffer |
| 40 | `u32` reply buffer capacity (bytes) |
| 44 | `u32` reply buffer capacity (handles) |
| 48 | `u64` deadline (absolute ticks, `u64::MAX` for none) |

Rights: WRITE and READ on `channel`; TRANSFER on the request handles.
Effects: as `channel_write` then `channel_read_txid` with the request's transaction id.
Messages with other transaction ids stay queued, for other readers or later reads.
Blocking: until the reply arrives, the peer closes, or the deadline.
Errors before anything is sent, in order: INVALID_ARGS (a bad `args` pointer); BAD_HANDLE
(also: any `a0` over 32 bits); TOO_BIG (request over the limits); INVALID_ARGS (a request under
4 bytes); WRONG_TYPE, then ACCESS_DENIED (no READ or no WRITE on the channel); INVALID_ARGS
(a bad request pointer); then those of `channel_write` for the request (BAD_HANDLE for a listed
handle, ACCESS_DENIED for one without TRANSFER, INVALID_ARGS for a duplicate, NOT_SUPPORTED,
PEER_CLOSED, SHOULD_WAIT for a full queue). **Errors after the request was sent** (it is
not taken back, and its handles have gone): PEER_CLOSED (the peer closed before replying),
TIMED_OUT (the deadline passed first; the reply may still arrive and can be read with
`channel_read_txid`), BUFFER_TOO_SMALL (the reply is too big for the capacity; it stays queued
and the sizes come back in `nbytes`, `nhandles`), INVALID_ARGS (the reply buffer or reply handles
buffer is not writable; the reply stays queued), NO_MEMORY (the reply carries handles and the
table has no room; it stays queued).
The kernel does not donate priority to the server yet (OS-ARCHITECTURE 5.3): the call is
where that attaches.
Tests: `channel_call_*` in `init`.

### The bootstrap handle

Every process starts with exactly one handle, **handle value 1**: a channel endpoint, the
bootstrap channel, whose other end the creator keeps (OS-ARCHITECTURE section 9). It has
the default channel rights. Everything else a program is given arrives as a message on it.

## Memory objects and processes (S3c-2a)

### Objects

A **VMO** (memory object) is a lazily committed run of pages (1 to `MAX_VMO_PAGES`, 16384 =
64 MiB), zero until written, that can be read and written by handle and mapped into
processes. Rights: DUPLICATE, TRANSFER, READ, WRITE, EXECUTE, MAP. A **process** object
is a handle to an address space, a handle table and (once started) a thread. Rights:
DUPLICATE, TRANSFER, MANAGE (map, start, create threads), KILL, INSPECT (exit code), WAIT. A process raises
the signal **TERMINATED** (bit 3) when it has ended. **Closing the last handle to a process
kills it** (so a creator that drops its handle cannot leave an unmanageable process behind);
closing the last handle to a VMO frees it once nothing maps it.

### vmo_create(pages, job) -> status, handle

New zero-filled VMO whose pages are charged to a job as they are first touched (S4.5): `job` is
a job handle with MANAGE, or 0 for the caller's own job. Errors: INVALID_ARGS (`pages` is 0 or
over the limit); BAD_HANDLE, WRONG_TYPE, ACCESS_DENIED (`job`); NO_MEMORY (object cap or kernel
memory). A page that cannot be charged is refused when it is first touched, with NO_MEMORY from
`vmo_write`, or as a fault that ends the process. Tests: `vmo_*` in `init`.

### vmo_read(vmo, offset, buffer, length) / vmo_write(vmo, offset, buffer, length) -> status

Copy between a VMO and user memory. Rights: READ / WRITE. Errors, in order: BAD_HANDLE;
WRONG_TYPE; ACCESS_DENIED; INVALID_ARGS (a bad user pointer, or the range does not lie
inside the VMO: `offset + length` wraps or exceeds its size; a zero `length` is allowed
anywhere inside or at the end). `length` is limited to `MAX_MSG_BYTES` per call (TOO_BIG).
Nothing is written on an error.

### vmo_commit(vmo, offset, length) -> status

Commit the pages `[offset, offset + length)` of a VMO **now** (slice V): the VMO's *creating job* is charged for
each page that is not already committed and the frames are taken, so that a later touch can neither fail nor
charge anyone. All or nothing: if the job's page limit or the frames run out, everything this call committed is
given back and the call returns NO_MEMORY with the VMO and the job ledger exactly as before. Rights: COMMIT
(a new right; creating a VMO gives its handle COMMIT, and a creator narrows it away before handing the VMO to
someone else, since a holder with COMMIT spends the creator's quota). `offset` and `length` must be page
aligned (INVALID_ARGS), inside the VMO (INVALID_ARGS) and at most 1024 pages (TOO_BIG); a device or DMA VMO's
handle has no COMMIT (ACCESS_DENIED: those VMOs are fixed memory, already "committed"). Pages that fit neither
the job's room nor the free frames are refused up front (NO_MEMORY, nothing changed). Errors: BAD_HANDLE;
WRONG_TYPE; ACCESS_DENIED (no COMMIT); INVALID_ARGS; TOO_BIG; NO_MEMORY.

### vmo_get_size(vmo) -> status, bytes

The size in bytes. Errors: BAD_HANDLE; WRONG_TYPE.

### Device capabilities (S6a; S6-SPEC.md section 3)

The **root resource** is an object whose handle (rights DUPLICATE, TRANSFER, MANAGE) is the
authority to mint device memory and DMA buffers. `resource_root()` returns one, **only to the
process the kernel started** (as `job_root`: ACCESS_DENIED to everyone else); the holder passes
it on deliberately. A process without it cannot reach device memory.

### vmo_create_physical(resource, pa, bytes) -> status, handle

A VMO over device memory (MMIO). `resource` needs MANAGE. The range must be page aligned,
non-empty, a whole number of pages and inside one *device window* (`abi::layout::DEVICE_WINDOWS`:
the eight QEMU `virt` virtio-mmio slots, `0x1000_1000..0x1000_9000`); anything else, RAM
included, is INVALID_ARGS. Rights: READ, WRITE, MAP, DUPLICATE, TRANSFER (never EXECUTE, never
DMA). The frames are not in the frame database: they are never freed, zeroed or charged as pages,
and `vmo_read`/`vmo_write` on it are NOT_SUPPORTED. **The kernel never copies through device
memory**: a syscall buffer pointer that lands in an MMIO mapping is INVALID_ARGS. Errors, in
order: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED; INVALID_ARGS; NO_MEMORY (handle table or object
quota).

### vmo_create_dma(resource, pages) -> status, handle

A VMO of `pages` physically contiguous, zeroed frames, committed now and charged to the caller's
job as pages. At most `MAX_DMA_PAGES` (64) per object and `MAX_DMA_TOTAL_PAGES` (1024) in the
system (INVALID_ARGS past either, or for 0); NO_MEMORY if the job is out of pages or no
contiguous run is free. The returned handle has VMO rights without EXECUTE plus **DMA**.

### vmo_phys_addr(vmo) -> status, address

The physical address of a DMA VMO's first byte. Needs the DMA right (ACCESS_DENIED without it, so
a duplicate made without DMA cannot learn addresses); WRONG_TYPE for a VMO that is not a DMA VMO.

### resource_dtb(resource) -> status, handle, length

A read-only VMO (rights READ, MAP, DUPLICATE, TRANSFER) holding a copy of the device tree OpenSBI
handed to the kernel; `length` is its size in bytes. `resource` needs MANAGE. NOT_SUPPORTED if the
kernel found no tree (or one over `MAX_DTB_BYTES`, 256 KiB); NO_MEMORY if the caller's job is out of
pages. The tree is data from the firmware: the reader (`kcore::fdt::parse`) is hostile-input safe.

### futex_wait(address, expected, deadline) -> status

Sleep if the 32-bit word at `address` (in the caller's address space) still equals `expected`. The
check and the enqueue are one step under the big kernel lock, so a wake that follows a user-mode store
to the word is never lost; the word is read with one aligned atomic load. `deadline` is an absolute
tick: `0` polls (equal: TIMED_OUT at once, no sleep), `u64::MAX` waits forever. OK when woken (the
sleeper left the table because of a `futex_wake`); spurious wakeups are allowed, so callers re-check.
Errors, in order: INVALID_ARGS (`address` not 4-aligned, outside user memory, not readable or not
mappable; `expected` wider than 32 bits); SHOULD_WAIT (the word differs from `expected`: nothing
slept); TIMED_OUT (the deadline passed, never earlier, or a poll of an equal word). A kill ends the
sleep; the sleeper leaves the table whichever way it ends. No rights are involved: a futex is a
(process, address) pair and naming one grants nothing the process could not do with its own memory.

### futex_wake(address, count) -> status, woken

Wake up to `count` sleepers on `address` **in the caller's process**, oldest first (FIFO); `woken` is
how many. Another process sleeping on the same address number is not touched. `count` 0 wakes
nobody. INVALID_ARGS if `address` is not 4-aligned or outside user memory. (The address need not be
mapped: nobody can be sleeping on an address the sleeper could not read.)

### interrupt_create(resource, source) -> status, handle

An interrupt object for PLIC source `source` (1..=`MAX_IRQ`, 8: the QEMU `virt` virtio-mmio
slots; slot *n* is source *n*+1). `resource` needs MANAGE. Rights: DUPLICATE, TRANSFER, WAIT,
INSPECT, MANAGE (acknowledge). The source is enabled (priority 1) for the boot hart while the object
lives and masked when it is destroyed. When it fires the kernel claims it, **masks it**, holds the
claim open and raises INTERRUPT; waiters wake. A source may also be pending from before the object
existed (a device that raised its line while nobody listened): **drivers must tolerate a spurious
interrupt** right after `interrupt_create` and check their device's own status. Errors, in order: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED;
INVALID_ARGS (source 0 or above the range); NOT_SUPPORTED (a machine without a PLIC: AIA; drivers
poll); BAD_STATE (a live object already owns the source); NO_MEMORY.

### interrupt_ack(interrupt) -> status, claims

Clear INTERRUPT, complete the held claim and unmask the source; the device condition must be cleared first or a level-triggered
device fires again. `claims` is how many times the kernel has claimed this source for the object
(a storm shows here). Rights: MANAGE. BAD_STATE if the source has not fired since the last ack.

### process_create(job) -> status, handle

A new, empty, not yet started process: an address space with nothing mapped and an empty handle
table, charged to a job (S4.5): `job` is a job handle with MANAGE, or 0 for the caller's own
job. Errors: BAD_HANDLE, WRONG_TYPE, ACCESS_DENIED (`job`); NO_MEMORY (the job or an ancestor is
at its process limit, kernel memory, or `MAX_PROCESSES` reached).

### process_self() -> status, handle

A handle to the calling process, with MANAGE, INSPECT, WAIT and DUPLICATE (not TRANSFER, not KILL: a process ends itself with `process_exit`).

### process_map(process, vmo, vaddr, vmo_offset, length, flags) -> status

Map `length` bytes of `vmo` starting at byte `vmo_offset` into `process` at `vaddr`, with
permissions `flags` (R = 1, W = 2, X = 4; read is always implied; W and X together is
INVALID_ARGS, INVARIANTS M2). Pages are committed on first touch, **except** that a VMO charged to a job that is
neither the caller's nor the target process's must already be committed over the mapped range (BAD_STATE
otherwise; slice V, INVARIANTS V1): a process cannot be killed, or a stranger charged, by memory it was handed.
Rights: MANAGE on `process`; MAP on `vmo`, plus READ for any mapping, WRITE if W, EXECUTE if X.
Errors, in order: BAD_HANDLE (process, then vmo); WRONG_TYPE; ACCESS_DENIED; BAD_STATE (the
process has ended); INVALID_ARGS (`vaddr`, `vmo_offset` or `length` not page aligned or
`length` 0; unknown flag bits; W with X; the range outside the user half (or including page zero) or outside the
VMO; overlap with an existing mapping; a process picks its own stack, so no address range is
reserved); NO_MEMORY. Mapping into a running process takes effect at once.
Tests: `process_map_*` in `init`.

### process_unmap(process, vaddr) -> status

Remove the mapping that starts at `vaddr`. Rights: MANAGE. Errors: BAD_HANDLE; WRONG_TYPE;
ACCESS_DENIED; BAD_STATE; INVALID_ARGS (no mapping starts there).

### process_start(process, entry, sp, bootstrap, arg) -> status

Start the process's first thread at `entry` with stack pointer `sp` and `a0 = arg`. The
channel endpoint `bootstrap` leaves the caller's table and becomes the new process's handle 1
(the bootstrap channel). Rights: MANAGE on `process`; TRANSFER on `bootstrap`. The process must
not have been started or ended, `entry` must lie in a mapping that is executable, `sp` must be
16-byte aligned and in the user half (what is mapped below it is the caller's business).
Errors, in order: BAD_HANDLE (process, then bootstrap); WRONG_TYPE (not a process / not a
channel); ACCESS_DENIED; BAD_STATE (already started, or ended); INVALID_ARGS (`entry` or `sp`); NO_MEMORY (the system already has `MAX_USER_THREADS` threads, or the process's job or an ancestor
is at its thread limit; the refusal comes before anything changes).
On any error nothing changes (the handle stays with the caller). Tests: `process_start_*`,
`a_refused_process_start_changes_nothing_and_can_be_retried`.

### process_kill(process, code) -> status

End the process with exit code `code` (as if it had called `process_exit(code)`): its threads
stop, its handle table is closed, its address space is torn down, TERMINATED is raised.
Rights: KILL (MANAGE alone is not enough). Errors: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED; BAD_STATE (already ended).
A thread blocked in a wait is woken to die. Tests: `process_kill_*`.

### process_info(process) -> status, exit_code

The exit code of an ended process (`-1000 - scause` after a fault). Rights: INSPECT.
Errors: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED; BAD_STATE (not ended yet).

## Jobs and limits (S4.5, OD56-OD58)

A **job** is an account that processes, threads and memory are charged to; jobs form a tree with
the kernel's **root job** (no limits) on top. A job has limits on six resources: live
**processes**, live **threads**, **pages** of memory (committed VMO pages and page-table frames),
**jobs** (how many may exist below it, at any depth), **objects** (kernel objects alive that its
processes created) and **ipc bytes** (data queued in the channels and sockets its processes
created) (`u64::MAX` means no limit). The limits travel as six little-endian `u64`s in user
memory, in this order: processes, threads, pages, jobs, objects, ipc bytes. A charge is applied to the job and **every ancestor**, so a job can never use more than
any ancestor allows, whatever its own limits say. Every process belongs to the job it was created
in; the kernel's first process (`init`) is in the root job.

The job rights are DUPLICATE, TRANSFER, MANAGE (create jobs, processes and VMOs in it; set limits),
KILL (`job_kill`) and INSPECT (`job_info`). **No program can name a job it was not given**: there
is no call that returns the caller's own job, and `job_root` is for the process the kernel
started. Limits are exhausted with NO_MEMORY.

What is charged: a process from `process_create` to its teardown; a thread from the moment it is
created (`process_start` or `thread_create`) until it ends; a VMO page from the moment it is first
committed (a write, a read through a mapping, a fault) until the VMO is freed; **page tables** as
a worst-case estimate: one page for a process's root table and, per mapped region, one page per
512 pages it spans plus four, charged to the process's job when the region is mapped and credited
when it is unmapped or the process ends (the kernel's own objects are bounded by the global caps,
not charged). **Objects** are charged to the job they are created for: the creating process's job, except that a
process object and a VMO made with a job handle (`process_create(job)`, `vmo_create(pages, job)`)
are charged to that job (a channel or socket pair counts two) and credited when they are destroyed. **Queued data** is charged to the job
that created the *endpoint holding the queue* (so the creator of a conversation pays for both
buffers, whoever writes into them), credited when read or when the endpoint is destroyed; a
socket write takes only as many bytes as the job has room for. A port's bindings are charged like queued data (32 bytes each) to the
port's job. A job
record lives while any process, VMO, job object or child job holds it.

### job_create(parent, limits) -> status, handle

A child of `parent` (MANAGE) with the limits at `a1`. Errors, in order: BAD_HANDLE; WRONG_TYPE;
ACCESS_DENIED; INVALID_ARGS (the limits cannot be read: a bad pointer); NO_MEMORY (object cap, 256 jobs exist, or the parent or an ancestor is at its
limit on jobs). The handle carries every job right.

### job_set_limits(job, limits) -> status

Needs SET_LIMITS (MANAGE is not enough: a holder of MANAGE can make children, processes and VMOs in
the job but cannot lift its limits). Usage above a new limit is not undone; later charges fail
until usage is back under it. Errors, in order: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED; INVALID_ARGS (the limits at `a1` cannot be
read). The rights are checked before the pointer is touched.

### job_info(job, which) -> status, used, limit

Needs INSPECT. `which`: 0 processes, 1 threads, 2 pages, 3 jobs, 4 objects, 5 ipc bytes; `used` counts the job and everything
below it. Errors: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED; INVALID_ARGS (`which`).

### job_kill(job, code) -> status

Needs KILL. Sets the job's process and thread limits to 0 (nothing new may start in it or below
it) and ends every process in the job and its descendants with exit code `code`, as `process_kill`
does (a caller inside the job ends too, last). Errors: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED.

### job_root() -> status, handle

A handle with every job right to the root job. Only a process started by the kernel may ask
(`init`); everything else gets ACCESS_DENIED. Tests: `job_*` in `init`.

## Threads (S3c-2b)

A process runs one or more user threads, all sharing its address space and handle table.
The first thread starts at `process_start` (or at load, for a kernel-started process); more
are made with `thread_create`. A thread is a kernel object (kind THREAD): a handle to it can
be waited on (TERMINATED) and asked for its exit code. Rights: DUPLICATE, TRANSFER, MANAGE,
INSPECT, WAIT. There are at most `MAX_THREADS_PER_PROCESS` (8) live threads per process.

Ending. `thread_exit(code)` ends the calling thread only. When the last live thread of a
process ends, the process ends, with exit code `code` of that last thread unless the
process had already been given one (`process_exit`, `process_kill`, a fault). `process_exit`,
`process_kill` and any fault in any thread end the **whole** process: every other thread
stops at its next return to user mode (a blocked one is woken to die), and the process object
is TERMINATED only after the last of its threads has gone and its memory is freed. A thread
that was killed before it ever ran never runs user code.

### thread_create(process, entry, sp, arg) -> status, handle

Start a new thread in a running process: it begins at `entry` in U-mode with `a0 = arg`, the
stack pointer `sp`, and every other register zero. The new thread's stack is the caller's to
provide (map a VMO, pass the top). Rights: MANAGE on `process`. Errors, in order: BAD_HANDLE;
WRONG_TYPE; ACCESS_DENIED; BAD_STATE (the process has not been started, or has ended or is
dying); INVALID_ARGS (`entry` not in an executable mapping of the process; `sp` not 16-byte
aligned or outside `BASE..=TOP`); NO_MEMORY (the process already has `MAX_THREADS_PER_PROCESS`
threads, the system already has `MAX_USER_THREADS` (128; each thread costs a kernel stack), the
caller's handle table is full, the system-wide object cap `MAX_OBJECTS` is reached, or the process's
job or an ancestor is at its thread limit). The process handle may be the caller's
`process_self` handle. On error nothing changes. Tests: `thread_create_*`.

### thread_exit(code) -> does not return

End the calling thread (see above). Its thread object is TERMINATED with exit code `code`.

### thread_info(thread) -> status, exit_code

The exit code of an ended thread (`-1000 - scause` is *not* used: a fault ends the process
and the thread's code is the same as the process's). Rights: INSPECT. Errors: BAD_HANDLE;
WRONG_TYPE; ACCESS_DENIED; BAD_STATE (not ended yet).

Syscalls run under the big kernel lock (OD60, S5): a thread holds it from the trap until it returns to user
mode or goes idle, and the lock follows the thread across a context switch (INVARIANTS K1-K5, lock order).
With several harts, syscalls on different harts serialise on it (TIDY-UP T11).

## Sockets and ports (S3c-3)

### Sockets

A socket is a connected pair of byte-stream endpoints (pipes, stdio, network socket endpoints
build on it). Each endpoint owns an inbound buffer of at most `SOCKET_BUFFER` (65536) bytes.
Rights: DUPLICATE, TRANSFER, READ, WRITE, WAIT. Signals: READABLE (own buffer not empty),
WRITABLE (peer exists and its buffer has free space), PEER_CLOSED (the peer endpoint is gone;
READABLE stays set while bytes remain, so a reader drains them and then sees the end).

### socket_create() -> status, handle0, handle1

NO_MEMORY if the handle table has no room for two handles or the object cap is reached.

### socket_write(socket, bytes, length) -> status, written

Append up to `length` bytes to the peer's buffer; a write that does not fit completely writes
what fits (a short write) and reports it. Rights: WRITE. `length` at most `SOCKET_BUFFER`.
Errors, in order: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED; TOO_BIG; INVALID_ARGS (the source is
not readable user memory); PEER_CLOSED (the peer is gone; nothing written); SHOULD_WAIT (the
peer's buffer is full and `length` > 0; nothing written). `length` 0 is OK with `written` 0.

### socket_read(socket, buffer, max) -> status, read

Remove up to `max` bytes from the own buffer into `buffer`. Rights: READ. `max` at most
`SOCKET_BUFFER`. Errors, in order: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED; TOO_BIG; then, if
`max` > 0 and the buffer is empty: PEER_CLOSED (the peer is gone: end of stream) or
SHOULD_WAIT; INVALID_ARGS (the destination is not writable user memory; nothing is removed).
`max` 0 is OK with `read` 0.

### socket_shutdown(socket) -> status

Half-close: this endpoint writes no more. The peer reads what is queued, then gets end of stream
(`socket_read` returns PEER_CLOSED) and sees the signal WRITE_CLOSED; PEER_CLOSED itself stays
clear, because the peer exists and may still write, and this endpoint can still read. After it,
this endpoint's `socket_write` fails with BAD_STATE and it no longer has WRITABLE. Irreversible
and idempotent (a second call is OK). Rights: WRITE. Errors, in order: BAD_HANDLE; WRONG_TYPE;
ACCESS_DENIED. A reader that waits for end of stream waits for READABLE | PEER_CLOSED |
WRITE_CLOSED.

### Ports

A port is a wait set: it watches signals on other objects and reports which are asserted.
Rights: DUPLICATE, TRANSFER, MANAGE (bind, unbind), WAIT (port_wait, object_wait). Its own
signal READABLE is set while any binding is ready, so a port can be waited on with
`object_wait`; a port cannot be bound to a port. A binding is *level-triggered*: it is ready
exactly while `signals(object) & mask != 0`, and reporting it consumes nothing. A binding
holds no reference: when the object is destroyed the binding disappears.

### port_create() -> status, handle

### port_bind(port, object, mask, key) -> status

Watch `object` for `mask`; report it under `key` (chosen by the caller, unique within the
port). Rights: MANAGE on `port`, WAIT on `object`. `mask` must be non-empty and within the
waitable signals. Errors, in order: BAD_HANDLE (port, then object); WRONG_TYPE (`port` is not
a port); ACCESS_DENIED; INVALID_ARGS (bad mask; `object` is a port; `key` already bound);
NO_MEMORY (`MAX_PORT_BINDINGS` (256) bindings).

### port_unbind(port, key) -> status

Rights: MANAGE. Errors: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED; INVALID_ARGS (no such key).

### port_wait(port, deadline) -> status, key, observed

Wait until some binding is ready or the absolute tick `deadline` passes (0: do not block;
`u64::MAX`: no deadline), then return one ready binding's `key` and `observed` (`signals &
mask`). Successive calls rotate through the ready bindings, so none starves. Rights: WAIT.
Errors: BAD_HANDLE (also if the handle stops naming anything while waiting); WRONG_TYPE;
ACCESS_DENIED; TIMED_OUT. A killed process stops waiting at once.

## Entry template

```text
### sys_<name>(args) -> result

What it does, in one or two sentences.

Preconditions:  what must hold about the arguments and the caller.
Rights:         the rights each handle argument must carry (or "none").
Effects:        what changes (objects created, handles moved, signals raised).
Blocking:       never | may block until <condition> | may be interrupted by <x>.
Errors:         each status the call can return and exactly when.
Invariants:     which rules in INVARIANTS.md this call touches.
Tests:          the tests covering success and every error above.
```

## Handles

### handle_close(handle) -> status

Closes a handle: the slot is freed and the object loses one reference.
Preconditions: none beyond a valid handle. Rights: none. Blocking: never.
Effects: the slot's generation changes. If that was the last reference the object is
destroyed (see Object lifetime below).
Errors: BAD_HANDLE.
Invariants: H2, H4, H6. Tests: `handle_model`, `registry_model`.

### handle_duplicate(handle, rights) -> status, new_handle

Makes a second handle to the same object with `rights` (or SAME_RIGHTS).
Rights: the source needs DUPLICATE. Effects: one more reference to the object; the
table gains one entry. Blocking: never.
Errors: BAD_HANDLE; ACCESS_DENIED (source lacks DUPLICATE, or `rights` has a bit the
source lacks, or `rights` has a bit no right uses); NO_MEMORY (table full; nothing
changes).
Invariants: H3. Tests: `handle_model`, `rights_escalation_is_refused`.

### handle_replace(handle, rights) -> status, new_handle

Atomically closes `handle` and returns a new handle to the same object with `rights`
(or SAME_RIGHTS). The old value is invalid afterwards, even if the call fails after
validation. Used to drop rights.
Rights: none needed on the source (replacing cannot widen). Effects: reference count
unchanged.
Errors: BAD_HANDLE; ACCESS_DENIED (`rights` is not a subset of the source's, or has an
unassigned bit). On error the handle is unchanged and still valid.
Invariants: H2, H3. Tests: `handle_model`, `rights_escalation_is_refused`.

## Channels

A channel is a pair of endpoints. A message is up to MAX_MSG_BYTES bytes and up to
MAX_MSG_HANDLES handles, delivered in order to the *other* endpoint's queue. The
first 4 bytes of a message are by convention a transaction id (`channel_call`).

### channel_create() -> status, handle0, handle1

Creates a connected pair. Both handles have DUPLICATE, TRANSFER, READ, WRITE, SIGNAL
and WAIT. Errors: NO_MEMORY (fewer than two free slots, `MAX_OBJECTS` reached, or no memory; nothing created).
Tests: `channel_basics`, `channel_model`.

### channel_write(channel, bytes, nbytes, handles, nhandles) -> status

Queues a message on the peer. The listed handles leave the caller's table and travel in
the message; they appear in the receiver's table when it reads.
Rights: `channel` needs WRITE; every listed handle needs TRANSFER.
Effects: the handles are removed from the caller's table *only if the whole call
succeeds*; the message holds one reference to each handle's object.
Blocking: never.
Errors, checked in this order: TOO_BIG (over MAX_MSG_BYTES or MAX_MSG_HANDLES; first,
because it bounds the work of every later check); BAD_HANDLE (the channel, then each
listed handle); WRONG_TYPE (not a channel); ACCESS_DENIED (no WRITE on the channel, then
a listed handle without TRANSFER); INVALID_ARGS (a handle listed twice); NOT_SUPPORTED
(either endpoint *of this channel*, by object identity, is in the list: that would put
an endpoint in its own queue); PEER_CLOSED; SHOULD_WAIT (the peer's queue holds
MAX_QUEUED_MSGS, or MAX_QUEUED_BYTES with this message); NO_MEMORY (the kernel cannot
allocate the message; nothing changed). On any error the caller's table is unchanged.
Invariants: H1, H3, H4. Tests: `channel_basics`, `channel_errors`, `channel_model`.

### channel_read(channel, max_bytes, max_handles) -> status, bytes, handles

Dequeues the oldest message. Its handles are inserted into the caller's table (rights
unchanged) and returned.
Rights: `channel` needs READ. Blocking: never (the syscall layer adds a blocking
variant with a wait; see `object_wait`).
Errors: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED; SHOULD_WAIT (queue empty and the peer is
open); PEER_CLOSED (queue empty and the peer is gone: messages queued before the peer
closed are still delivered first); BUFFER_TOO_SMALL (the front message does not fit
`max_bytes` or `max_handles`; **the message stays queued** and the actual sizes are
returned); NO_MEMORY (the caller's table lacks room for the message's handles; the
message stays queued).
Invariants: H4. Tests: `channel_basics`, `channel_errors`, `channel_model`.

The user's destination pointers are checked against the **message actually delivered**, not
against the declared capacity: a 5-byte message needs 5 writable bytes however large
`max_bytes` is (a capacity over the limits counts as the limit). They are checked *after*
the status above is known (so BUFFER_TOO_SMALL and SHOULD_WAIT win over a bad pointer) and
*before* the message is dequeued, so a bad pointer is INVALID_ARGS and never loses a message.

### channel_read_txid(channel, txid, max_bytes, max_handles) -> status, bytes, handles

As `channel_read` but takes the oldest message whose first 4 bytes (little endian) equal
`txid`, skipping others. Used by `channel_call` to pick its reply out of a busy channel.
Errors as `channel_read`, with one difference: if no queued message matches, the result is
PEER_CLOSED when the peer is gone (nothing more can ever arrive, even though other
messages may still be queued) and SHOULD_WAIT otherwise.
Tests: `channel_txid`, `a_txid_read_on_a_closed_channel_does_not_wait_forever`.

### channel_call (planned, S3b)

`channel_write` then wait for a reply with the same transaction id, in one call. The
point of a distinct call is priority donation (OS-ARCHITECTURE 5.3): not implemented, the
hook is.

## Events

### event_create() -> status, handle

An object with 8 user signals. Rights: DUPLICATE, TRANSFER, SIGNAL, WAIT.
Errors: NO_MEMORY (table full, `MAX_OBJECTS` reached, or no memory). Tests: `event_basics`.

### event_signal(event, clear_mask, set_mask) -> status

Clears then sets user signal bits.
Rights: SIGNAL. Errors: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED; INVALID_ARGS (a mask has
a bit outside the user signals).
Tests: `event_basics`, `event_model`.

## Object lifetime

Every handle and every in-flight message entry holds one reference to its object. When
the last reference goes, the object is destroyed: a channel endpoint's queued messages
are dropped (their handles released in turn, iteratively, so nesting depth is no
problem), and its peer gets PEER_CLOSED. A **cycle** (two endpoints each queued in the
other's channel, with no handle to either) cannot be freed by counting; the kernel
reclaims such garbage by reachability from every process's handle table when a process
dies, and also **under pressure**: before creating a kernel object the kernel asks
`Registry::needs_sweep` (the object count has doubled since the last sweep) and, if so,
sweeps from every process's table. Without that, one process making cycles in a loop
could grow kernel memory without bound while it is still alive; with it the garbage stays
within a constant factor of the live set (decision OD37). A sweep returns the references
that destroyed objects held on survivors, so survivors are still freed correctly.
Tests: `cycles_are_swept`, `a_sweep_gives_back_the_references_doomed_objects_held`,
`a_process_cannot_grow_kernel_memory_by_making_cycles`, `random_channels_agree_with_the_model`.

## Index

| Call | Status |
|---|---|
| handle_close, handle_duplicate, handle_replace | specified (S3a); implemented in S3b |
| channel_create, channel_write, channel_read, channel_read_txid | specified (S3a); implemented in S3b |
| event_create, event_signal | specified (S3a); implemented in S3b |
| channel_call | specified (S3c-1) |
| object_wait (wait on a signal with a deadline) | specified (S3c-1) |
| port_create, port_bind, port_unbind, port_wait | specified (S3c-3) |
| debug_write, process_exit, thread_yield, clock_get, object_signals | specified (S3b numbers above); implemented in S3b |
| vmo_create, vmo_read, vmo_write, vmo_get_size | specified (S3c-2a) |
| vmo_set_size, vmo_create_child | planned, later |
| process_map, process_unmap (stand in for vmar_*; protect is later) | specified (S3c-2a) |
| process_create, process_self, process_start, process_kill, process_info | specified (S3c-2a) |
| thread_create, thread_exit, thread_info | specified (S3c-2b) |
| job_create, job_set_limits, job_info, job_kill, job_root | specified and implemented (S4.5) |
| socket_create, socket_write, socket_read | specified (S3c-3) |
| socket_shutdown | specified and implemented (tidy-up, T17) |
| vmo_create_physical, vmo_create_dma, vmo_phys_addr, resource_root | specified and implemented (S6a) |
| interrupt_create, interrupt_ack | specified and implemented (S6a-2) |
| resource_dtb | specified and implemented (S6c) |
| futex_wait, futex_wake | specified and implemented (S7b) |
