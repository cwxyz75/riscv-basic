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
- A **handle** is a 64-bit value (AB1). 0 is never valid. The low 24 bits are the table slot
  plus 1 (at most 2^24 - 1 slots; the table grows on demand); the high 40 bits are a generation that changes every time the slot is freed,
  so a stale value fails (INVARIANTS H2). A slot that has used all 2^40 generations is retired. Values are meaningful only in the process
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
| INTERRUPTED | -14 | a blocking call that `thread_interrupt` told to stop (`object_wait`, `port_wait`, `futex_wait`, `channel_call`) |
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

A process holds as many handles as its job's object quota and memory allow (up to 2^24 - 1; there is no fixed `MAX_HANDLES`); `MAX_MSG_BYTES`
65536 and `MAX_MSG_HANDLES` 64 per message; `MAX_QUEUED_MSGS` 128 and
`MAX_QUEUED_BYTES` 256 KiB per channel endpoint; `MAX_OBJECTS` 16384 kernel objects
alive at once (creating more is NO_MEMORY); `MAX_IPC_BYTES` 2 MiB of queued channel and socket
data (message bytes, headers, queued handles, port bindings) in the whole system;
`MAX_REGIONS` 16384 per address space; `MAX_THREADS_PER_PROCESS` 2048 and `MAX_USER_THREADS` 4096;
`MAX_PROCESSES` 64.

These keep the kernel from panicking and from running out of heap (OD50; the heap grows at safe points, slice T step 2, so the caps also bound its growth); they are
not per-process limits. A real bound needs per-job quotas (work items Q-OS9
and T1). Per-job limits exist since S4.5 (processes, threads, memory pages, child jobs, objects and queued IPC
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
`-1000 - kind`, where `kind` is one of `abi::arch::fault`: 1 PAGE_FAULT, 2 ILLEGAL, 3 BREAKPOINT, 4 MISALIGNED,
5 ACCESS, 6 PAGER; every page fault is kind 1 whatever the access); the kernel never trusts a user fault.

## Syscall numbers and registers (S3b)

`a7` = number, `a0`..`a5` = arguments, results: `a0` = status, `a1`, `a2` = values.

| # | Call | Arguments | Results |
|---|---|---|---|
| 0 | debug_write | `a0` ptr, `a1` len | |
| 1 | process_exit | `a0` code | does not return |
| 2 | thread_yield | | |
| 3 | clock_get | | `a1` nanoseconds since boot |
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
| 48 | vmo_decommit | `a0` vmo, `a1` offset, `a2` length | |
| 49 | vmo_set_size | `a0` vmo, `a1` bytes | |
| 50 | process_create | | `a1` handle |
| 51 | process_self | | `a1` handle |
| 52 | process_map | `a0` process, `a1` vmo, `a2` vaddr, `a3` vmo_offset, `a4` length, `a5` flags | |
| 53 | process_unmap | `a0` process, `a1` vaddr | |
| 54 | process_start | `a0` process, `a1` entry, `a2` sp, `a3` bootstrap channel, `a4` arg | |
| 55 | process_kill | `a0` process, `a1` code | |
| 56 | process_info | `a0` process | `a1` exit code |
| 57 | process_suspend | `a0` process | |
| 58 | process_resume | `a0` process | |
| 70 | socket_create | | `a1`, `a2` the two handles |
| 71 | socket_write | `a0` socket, `a1` bytes, `a2` length | `a1` written |
| 72 | socket_read | `a0` socket, `a1` buffer, `a2` max | `a1` read |
| 80 | port_create | | `a1` handle |
| 81 | port_bind | `a0` port, `a1` object, `a2` mask, `a3` key, `a4` flags | |
| 82 | port_unbind | `a0` port, `a1` key | |
| 83 | port_wait | `a0` port, `a1` deadline | `a1` key, `a2` observed |
| 84 | port_rearm | `a0` port, `a1` key | |
| 85 | port_wait_many | `a0` port, `a1` buffer, `a2` capacity, `a3` deadline | `a1` count |
| 61 | thread_exit | `a0` code | does not return |
| 62 | thread_info | `a0` thread | `a1` exit code |
| 63 | thread_interrupt | `a0` thread (MANAGE) | |
| 65 | thread_set_sched | `a0` thread (0: the caller), `a1` priority 1-63, `a2` flags | |
| 66 | thread_get_sched | `a0` thread (0: the caller) | `a1` packed priorities, `a2` tid |
| 24 | channel_call | `a0` channel, `a1` args pointer | `a1` nbytes, `a2` nhandles |
| 90 | job_create | `a0` parent job, `a1` pointer to the limits | `a1` handle |
| 91 | job_set_limits | `a0` job, `a1` pointer to the limits | |
| 92 | job_info | `a0` job, `a1` which (0 processes, 1 threads, 2 pages, 3 jobs, 4 objects, 5 ipc bytes, 6 scheduling limits, 7 processors) | `a1` used, `a2` limit (for 6: `max_prio`, `rt_budget` ns; for 7: the number of online harts in both) |
| 96 | job_set_sched | `a0` job, `a1` max_prio 0-63, `a2` rt_budget ns per 100 ms, `a3` cpu_weight 1-65536 | |
| 93 | job_kill | `a0` job, `a1` exit code | |
| 94 | job_root | | `a1` handle |
| 95 | resource_root | | `a1` handle |
| 100 | interrupt_create | `a0` resource, `a1` source | `a1` handle |
| 101 | interrupt_ack | `a0` interrupt | `a1` claims so far |
| 110 | futex_wait | `a0` address, `a1` expected, `a2` deadline | |
| 111 | futex_wake | `a0` address, `a1` count | `a1` woken |
| 102 | resource_dtb | `a0` resource | `a1` handle, `a2` length |

A handles array is `nhandles` 64-bit values (8 bytes each, little endian); a handles buffer receives that many.

### Execution model of a syscall (S3b)

A syscall runs with interrupts off on the calling thread's kernel stack, so the timer never
preempts it. It is atomic with respect to every other thread **except at its documented
blocking and wake-up points**: `object_wait`, `channel_call` and the wake-up that follows any
call that can change a signal may switch to another thread in the middle of the call (the
kernel never keeps a reference to shared state across such a point). That keeps S3b simple and is *not* a promise for later: S5 (SMP) replaces
it with locks, and long syscalls (a 64 KiB message copy, the object sweep) are a latency
cost the real-time band will feel. The kernel does not touch the user's `sp`, `gp` or `tp`
(it has its own stack and uses no thread pointer); S5 must reload `gp`/`tp` on entry.
Handles are full 64-bit registers (a value that names nothing is BAD_HANDLE from the table). Arguments declared 32-bit (rights, signal masks,
`txid`) must fit in 32 bits: a value with higher bits set is ACCESS_DENIED (rights) or INVALID_ARGS (a signal mask or `txid`), never
silently truncated.

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
were woken; 3 device interrupts taken on the AIA, 4 how many of those on a hart other than the boot hart, 5 how many on a hart they were not aimed at (always 0). 6 the `time` counter (10 MHz on QEMU `virt`: the benchmarks' clock), 7 whether the kernel command line has `bench=full` (1) or not (0), 13 which part of the suite the kernel line asks for (0 all, 1 `shard=a`, 2 `shard=b`; the smoke tier runs the two as parallel boots, OD121). Any other index reads 0. The counters only grow. They exist to measure the wake path
(roadmap slice W): today a bystander blocked on an unrelated object is woken and goes back to sleep after
nearly every syscall.

`which` 110 and 111 (PL7) are the executable pages mapped by faults so far and the times the instruction caches were synchronised for them (once per fault that mapped any: `fence.i` here and SBI `remote_fence_i` on every other online hart); a page that was already mapped adds nothing.

`which` 109 is the kernel heap's leak oracle (a wrapping figure: taken before and after something, the difference is the heap bytes it kept; MM5's acceptance test uses it).

`which` 100 to 105 (PL4) are not counters: the lowest byte address of RAM, the address after the highest, the frames of RAM the device tree describes, the frames the frame allocator manages, the physical address where the kernel image starts, and the frequency of the `time` counter (selector 6) in hertz. They come from the device tree and the kernel's layout, are fixed after boot, and need no rights; the suite uses them for the bounds of DMA addresses and the physical-VMO refusals. `which` 106 (PL6) counts the calls the kernel made to the SBI inter-processor-interrupt extension: always 0, since inter-hart interrupts are messages to the harts' IMSIC files (message id 1; device sources are ids 2 and up).

### debug_write(ptr, len) -> status

Sends bytes to the serial console. Rights: none (a debugging aid for the boot-time
suite; real processes get a console channel in S4). Output is shown a line at a time: every call is flushed when it ends (a call that does not finish with a newline gets one), lines are cut at 512 bytes, and non-printable bytes are shown as `.`. A line that starts with `##` is prefixed `user: ` unless it is a well-formed `##TEST name PASS|FAIL` result (which the boot-time suite uses), so user code cannot forge the kernel's verdict lines.
Errors: INVALID_ARGS (bad pointer);
TOO_BIG (len over 4096).

### process_exit(code) -> does not return

Ends the calling process: its threads stop, its handle table is closed (INVARIANTS H6),
its address space is torn down.

### thread_yield() -> status, clock_get() -> status, nanoseconds, object_signals(handle) -> status, signals

`thread_yield` gives up the rest of the time slice. `clock_get` returns the **nanoseconds since boot**, from the `time` counter and its
frequency (AB2: one flag-day change from 1 ms ticks; every deadline below is an absolute time on this clock, in nanoseconds; the system
library `rt` offers millisecond conveniences, `clock_get()` and deadlines in ms, and converts at the boundary). `object_signals` reads the signal word of the object a handle names
(BAD_HANDLE for a bad handle; no right needed, the signals are not secret to a holder).

## Waiting, calling and the bootstrap handle (S3c-1)

### object_info(handle) -> status, koid, peer_koid

The identity of the object a handle names (`koid`: unique among live objects, never reused) and
of its peer (`peer_koid`; 0 for an object with no peer, or whose peer is gone). No right is
needed. It lets a server see that two handles it holds are the two ends of one channel (a client
can hand a server both ends of a channel it made, so that neither end ever sees PEER_CLOSED).
Errors: BAD_HANDLE. Tests: `object_info_names_objects_and_peers`.

### object_wait(handle, mask, deadline) -> status, observed

Blocks the calling thread until `signals(handle) & mask != 0` or the clock reaches
`deadline` (absolute nanoseconds, as `clock_get` counts; `0` means "do not wait", `u64::MAX`
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
| 48 | `u64` deadline (absolute nanoseconds, `u64::MAX` for none) |

Rights: WRITE and READ on `channel`; TRANSFER on the request handles.
Effects: as `channel_write` then `channel_read_txid` with the request's transaction id.
Messages with other transaction ids stay queued, for other readers or later reads.
Blocking: until the reply arrives, the peer closes, or the deadline.
Errors before anything is sent, in order: INVALID_ARGS (a bad `args` pointer); BAD_HANDLE;
TOO_BIG (request over the limits); INVALID_ARGS (a request under
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

### vmo_create(pages, job, flags) -> status, handle

New zero-filled VMO whose pages are charged to a job as they are first touched (S4.5): `job` is
a job handle with MANAGE, or 0 for the caller's own job. `flags` (MM6): bit 0 `RESIZABLE` makes the VMO's size changeable (`vmo_set_size`); any other bit is INVALID_ARGS. Errors: INVALID_ARGS (`pages` is 0 or
over the limit); BAD_HANDLE, WRONG_TYPE, ACCESS_DENIED (`job`); NO_MEMORY (object cap or kernel
memory). A page that cannot be charged is refused when it is first touched, with NO_MEMORY from
`vmo_write`, or as a fault that ends the process. Tests: `vmo_*` in `init`.

### vmo_read(vmo, offset, buffer, length) / vmo_write(vmo, offset, buffer, length) -> status

Copy between a VMO and user memory. Rights: READ / WRITE. Errors, in order: BAD_HANDLE;
WRONG_TYPE; ACCESS_DENIED; INVALID_ARGS (a bad user pointer, or the range does not lie
inside the VMO: `offset + length` wraps or exceeds its size; a zero `length` is allowed
anywhere inside or at the end). `length` is limited to `MAX_MSG_BYTES` per call (TOO_BIG).
Nothing is written on an error.

### vmo_decommit(vmo, offset, length) -> status

Give back the pages `[offset, offset + length)` of a VMO (MM6): every mapping of them, in every address space, goes and every hart drops its translations **before** any frame is freed, then the VMO drops its references, the frames return to the allocator and the VMO's creating job is credited. The range reads as zeros afterwards, through a mapping (the page faults in again as a fresh zero page, charged then) and through `vmo_read`. Pages that were never committed cost nothing. Rights: WRITE. `offset` and `length` must be page aligned and `length` not 0 (INVALID_ARGS), the range inside the VMO (INVALID_ARGS). Errors, in order: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED; INVALID_ARGS; NOT_SUPPORTED (a device or DMA VMO: their frames are fixed). The mappings stay (the regions keep their length); only the pages go. Tests: `vmo_decommit_and_resize_from_user_mode`, `kcore/tests/vmo_ops_model.rs`.

### vmo_set_size(vmo, bytes) -> status

Change the size of a VMO made with `RESIZABLE` (MM6); `bytes` must be a whole number of pages, not 0, and at most the VMO limit (INVALID_ARGS). Growing changes the size and nothing else (no page is committed). Shrinking decommits the pages past the new end exactly as `vmo_decommit` does (mappings and translations first, then the frames) and the size follows; a mapping that reaches past the new end keeps its length, and **touching its part past the end is a page fault that ends the process** (the address is in no backed region). Rights: WRITE. Errors, in order: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED (no WRITE, or the VMO was not made RESIZABLE); INVALID_ARGS; NOT_SUPPORTED (a device or DMA VMO). Sizes are whole pages until the byte-sized VMOs of MM6's full text (work/T207.md). Tests: `vmo_decommit_and_resize_from_user_mode`, `touching_past_a_shrunk_vmo_ends_the_process_cleanly`.

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
non-empty, a whole number of pages and inside one *device window*: the kernel builds the windows at boot from the device tree (`kcore::devwin`) and the rule fails closed. A node
contributes its `reg` ranges, widened to whole pages, only if its `compatible` is on the list of devices the system drives (`virtio,mmio`
and `google,goldfish-rtc`), its `status` is not disabled and every bus above it passes addresses through unchanged (`ranges;`); from the
windows are cut RAM and everything reserved, the console, and the ranges of every interrupt controller, timer, test finisher and power or
reset node. On QEMU `virt` that is the eight virtio-mmio slots and the RTC page. Anything else, RAM and every other device included (fw-cfg,
the flash banks, the PCI ECAM window), is INVALID_ARGS. Rights: READ, WRITE, MAP, DUPLICATE, TRANSFER (never EXECUTE, never
DMA). The frames are not in the frame database: they are never freed, zeroed or charged as pages,
and `vmo_read`/`vmo_write` on it are NOT_SUPPORTED. **The kernel never copies through device
memory**: a syscall buffer pointer that lands in an MMIO mapping is INVALID_ARGS. Errors, in
order: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED; INVALID_ARGS; NO_MEMORY (handle table or object
quota).

### vmo_create_dma(resource, pages) -> status, handle

A VMO of `pages` physically contiguous, zeroed frames, committed now and charged to the caller's
job as pages. At most `MAX_DMA_PAGES` (64) per object and `MAX_DMA_TOTAL_PAGES` (1024) in the
system (INVALID_ARGS past either, or for 0); NO_MEMORY if the job is out of pages or no
free aligned block of the size rounded up to a power of two exists (a 33-page request needs a free aligned 64-page block; the frame
allocator is a buddy allocator, MM2). The returned handle has VMO rights without EXECUTE plus **DMA**.

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
time in nanoseconds: `0` polls (equal: TIMED_OUT at once, no sleep), `u64::MAX` waits forever. OK when woken (the
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

An interrupt object for AIA source `source`: one of the sources the admitted devices of the device tree name in `interrupts` (`kcore::aiatree`: on
QEMU `virt` the virtio-mmio slots, sources 1 to 8, and the RTC, 11; slot *n* is source *n*+1), and at most 62 (a source is delivered as IMSIC message id source + 1; message 1 is the inter-hart interrupt); every other source, the console's
included, is refused. `resource` needs MANAGE. Rights: DUPLICATE, TRANSFER, WAIT,
INSPECT, MANAGE (acknowledge). The source is delivered (as an MSI to the IMSIC) to **the hart that called `interrupt_create`**, and is masked when the object is destroyed. When it fires the kernel takes the message, **masks the source** and raises INTERRUPT; waiters wake. `interrupt_ack` re-aims the source at the hart that acknowledges, so delivery follows the driver thread. A source may also be pending from before the object
existed (a device that raised its line while nobody listened): **drivers must tolerate a spurious
interrupt** right after `interrupt_create` and check their device's own status. Errors, in order: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED;
INVALID_ARGS (source 0, not a source of an admitted device, or past 32 bits); BAD_STATE (a live object already owns the source); NO_MEMORY.

### interrupt_ack(interrupt) -> status, claims

Clear INTERRUPT and unmask the source (a level still asserted fires again at once); the device condition must be cleared first or a level-triggered
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

### process_suspend(process) / process_resume(process) -> status

Stop every thread of the process, or let them go on (SIG2, OD123). Rights: KILL (the right to end a process is the right
to stop it; MANAGE alone is not enough). Errors: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED; BAD_STATE (the process ended, or was killed and is ending).
Both are idempotent (stopping a stopped process, or resuming one that runs, is OK), and a process may stop itself: its
calling thread parks as the call returns. A stopped thread **parks at its next return to user mode**, the point where a
kill takes effect: a thread running on another hart is kicked and parks at once, a thread blocked in a call stays blocked and
parks when the call returns (its timeout still runs, its signals still arrive), and a thread created in a stopped process
runs no user code until the process is resumed. A kill ends a stopped process as usual. The process's other state (handles,
memory, channels) is untouched and servers keep answering it. While stopped, the process object has the signal `STOPPED` (bit 6; waitable, cleared by `process_resume`; a process that ended has TERMINATED only), so a parent can wait for a stop. `process_info` still answers BAD_STATE. Tests: `process_suspend_*`.

### process_info(process) -> status, exit_code

The exit code of an ended process (`-1000 - kind` after a fault). Rights: INSPECT.
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
created (`process_start` or `thread_spawn`) until it ends; a VMO page from the moment it is first
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

Needs INSPECT. `which`: 0 processes, 1 threads, 2 pages, 3 jobs, 4 objects, 5 ipc bytes, 6 the job's scheduling limits (`a1` its `max_prio`, `a2` its `rt_budget` in ns per 100 ms window; see `job_set_sched`), 7 the processors its threads may run on (UV6; the number of online harts, in `a1` and `a2`: there is no CPU affinity yet), 8 kernel memory in bytes (MM5: the memory the kernel holds because of the job's programs, other than pages they map; its limit is an eighth of the job's page limit in bytes, none without one); `used` counts the job and everything
below it. Errors: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED; INVALID_ARGS (`which`).

**Kernel bytes (`which` 8)** are charged for object bodies (every call that creates an object: `channel_create`, `event_create`, `socket_create`, `port_create`, `vmo_create`, `thread_spawn`, `process_create`, `job_create`, ... returns NO_MEMORY when the job or an ancestor would pass its kernel-byte limit) and for the growth of a process's handle table (charged when the call that grew it returns: a job that cannot pay leaves the table unable to grow, so the calls that insert a handle return NO_MEMORY, while closing and reusing handles still work; the table stays unable to grow until the next call the process makes after its job has room again, so the first insert after room returns can still be refused once) and for a thread's floating-point and vector register state, charged at the thread's first use of the unit. A job at its limit that makes a thread use floating point or vector instructions for the first time ends the process with the illegal-instruction exit code: the instruction faults like any illegal one (a program built for RVA23 whose start-up code is vectorised dies this way in a job whose kernel-byte limit is too small to run a single thread's register state).

**Pages (`which` 2) count the pages a job's address spaces have prepaid** for their faults as well as the pages its VMOs hold: in a job with a limit, an address space prepays up to 64 pages at a time (an eighth of the job's room at most, one at least) and gives the rest back when it ends, so `used` can read a little above the committed pages while a process of the job runs. A job with no limit prepays nothing.

### job_set_sched(job, max_prio, rt_budget_ns, cpu_weight) -> status

Set a job's scheduling limits (slice SC, SCHED-SMP-SPEC 4.9). Needs SET_LIMITS. `max_prio` (0 to 63) is the highest **base**
priority any thread of the job may have (the real-time band, 32 to 63, therefore only exists in a job whose maximum reaches
it); `rt_budget_ns` is real-time CPU per 100 ms window (accounted, **not yet enforced**); `cpu_weight` (1 to 65536) is stored
for a later fair class. No value may exceed the parent job's (PR2: ACCESS_DENIED); a new job starts with its parent's values;
**lowering `max_prio` lowers the base priority of every live thread in the job and below it that is above it, at once**
(PR1: a job with maximum 0 still runs its threads at priority 1, the lowest a user thread has) and lowers every descendant job's values; a thread created but not yet run is clamped when it first runs. `rt_budget_ns` above the 100 ms window is INVALID_ARGS. Errors: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED (no SET_LIMITS, or a value above
the parent's); INVALID_ARGS (`max_prio` above 63, `cpu_weight` outside 1 to 65536). Tests: `thread_priority_is_bounded_by_the_job`,
`kcore/tests/job_sched.rs`.

### thread_set_sched(thread, priority, flags) -> status

Set a thread's base priority (1 to 63) and flags (bit 0: round robin within its level; the flag is stored and reported but not yet
passed to the scheduler, so a real-time thread (32 to 63) never rotates whatever the flag says, TIDY-UP T107; the normal band always rotates). `thread` 0 names the caller (no right needed and still bounded). Otherwise MANAGE on the thread
handle. ACCESS_DENIED if `priority` exceeds the thread's job's `max_prio`. A new user thread inherits its creator's base
priority clamped to its job's maximum; the first thread of a process starts at 10, clamped. Errors: BAD_HANDLE; WRONG_TYPE;
ACCESS_DENIED; INVALID_ARGS (priority 0 or above 63, a reserved flag bit); BAD_STATE (the thread has ended or has not started).

### thread_get_sched(thread) -> status, sched, tid

`sched` packs the base priority (bits 0 to 7), the flags (8 to 15) and the effective priority (16 to 23; equal to the base until
donation exists, task 22); `tid` is the kernel thread id. `thread` 0 names the caller. Needs INSPECT on a handle. Errors:
BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED; BAD_STATE.

### job_kill(job, code) -> status

Needs KILL. Sets the job's process and thread limits to 0 (nothing new may start in it or below
it) and ends every process in the job and its descendants with exit code `code`, as `process_kill`
does (a caller inside the job ends too, last). Errors: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED.

### job_root() -> status, handle

A handle with every job right to the root job. Only a process started by the kernel may ask
(`init`); everything else gets ACCESS_DENIED. Tests: `job_*` in `init`.

### Retired numbers

A retired number is never reused: a call to it is BAD_SYSCALL.

| number | was | retired |
|---|---|---|
| 60 | `thread_create` (replaced by `thread_spawn` with CALLER_STACK) | AB5 |

## Threads (S3c-2b)

A process runs one or more user threads, all sharing its address space and handle table.
The first thread starts at `process_start` (or at load, for a kernel-started process); more
are made with `thread_spawn` (the kernel provisions the stack, or the caller brings one: CALLER_STACK). A thread is a kernel object (kind THREAD): a handle to it can
be waited on (TERMINATED) and asked for its exit code. Rights: DUPLICATE, TRANSFER, MANAGE,
INSPECT, WAIT. There are at most `MAX_THREADS_PER_PROCESS` (2048) live threads per process. A thread's kernel stack (four frames) is **billed to the process's job as four pages, for as long as the thread lives** (and the thread itself counts against the job's thread limit); the job's page limit can therefore refuse a thread.

Ending. `thread_exit(code)` ends the calling thread only. When the last live thread of a
process ends, the process ends, with exit code `code` of that last thread unless the
process had already been given one (`process_exit`, `process_kill`, a fault). `process_exit`,
`process_kill` and any fault in any thread end the **whole** process: every other thread
stops at its next return to user mode (a blocked one is woken to die), and the process object
is TERMINATED only after the last of its threads has gone and its memory is freed. A thread
that was killed before it ever ran never runs user code.

### thread_exit(code) -> does not return

End the calling thread (see above). Its thread object is TERMINATED with exit code `code`.

### thread_info(thread) -> status, exit_code

The exit code of an ended thread (`-1000 - kind` is *not* used: a fault ends the process
and the thread's code is the same as the process's). Rights: INSPECT. Errors: BAD_HANDLE;
WRONG_TYPE; ACCESS_DENIED; BAD_STATE (not ended yet).

Syscalls run under the big kernel lock (OD60, S5): a thread holds it from the trap until it returns to user
mode or goes idle, and the lock follows the thread across a context switch (INVARIANTS K1-K5, lock order).
With several harts, syscalls on different harts serialise on it (TIDY-UP T11).

## Sockets and ports (S3c-3)

### thread_interrupt(thread) -> status

Built (T56, SCHED-SMP-SPEC task 26). Sets the target thread's interrupt flag and wakes it. The next blocking call of the
target that would sleep (`object_wait`, `port_wait`, `futex_wait`, `channel_call`, with a deadline other than 0) returns
INTERRUPTED (-14) and takes the flag. A call that is satisfied without sleeping leaves the flag (so an interrupt is not lost
between two waits), and a poll (deadline 0) neither sees nor takes it. One interrupt ends one wait. A `channel_call`
interrupted while waiting leaves the request queued, as a timeout does; one interrupted before it sent anything (the flag was already pending) sends nothing, so a retry sends one request. The call ends waits only: a thread
running user code is not stopped. Rights: MANAGE on a thread handle (0 names the caller). Errors: BAD_HANDLE; WRONG_TYPE;
ACCESS_DENIED; BAD_STATE (the thread has ended). Callers treat INTERRUPTED as a retry or a cancellation point. Tests:
`thread_interrupt_*` in the suite (nine).

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
Errors, in the order the code checks them: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED; TOO_BIG;
INVALID_ARGS (the source is not readable user memory); BAD_STATE (this endpoint has shut down
its write side, `socket_shutdown`); PEER_CLOSED (the peer is gone; nothing written; this holds for
`length` 0 too, so a zero-length write is a cheap probe for a dead peer); then, for `length` 0
on a live socket, OK with `written` 0; SHOULD_WAIT (the peer's buffer is full and `length` > 0;
nothing written); NO_MEMORY (the system-wide queued-data budget is spent, or the quota of the
job that owns the *receiving* endpoint is: its creator pays for what is queued, and no reader
of another socket will make room, so this is not SHOULD_WAIT; also if the buffer cannot grow).
A write that is not refused writes at least one byte.

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
exactly while `signals(object) & mask != 0`, and reporting it consumes nothing; a binding made with `ONESHOT` is *disarmed* when `port_wait` reports it and stays quiet, whatever the level, until `port_rearm`. A binding
holds no reference: when the object is destroyed the binding disappears.

### port_create() -> status, handle

### port_bind(port, object, mask, key, flags) -> status

Watch `object` for `mask`; report it under `key` (chosen by the caller, unique within the
port). Rights: MANAGE on `port`, WAIT on `object`. `mask` must be non-empty and within the
waitable signals. Errors, in order: BAD_HANDLE (port, then object); WRONG_TYPE (`port` is not
a port); ACCESS_DENIED; INVALID_ARGS (bad mask; `object` is a port; `key` already bound);
NO_MEMORY (no room for the binding: it costs 32 bytes of the port's job queued-data quota and of the system-wide budget; there is no fixed count). `flags` is `0` or `ONESHOT` (`abi::port_flags`, 1); any other bit is INVALID_ARGS (checked with the mask).

### port_rearm(port, key) -> status

Arm binding `key` again after a `ONESHOT` delivery (harmless on one that is armed). It is ready at once if its object's signals match now, and a thread blocked in `port_wait` is woken for it. Rights: MANAGE. Errors: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED; INVALID_ARGS (no such key).

### port_unbind(port, key) -> status

Rights: MANAGE. Errors: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED; INVALID_ARGS (no such key).

### port_wait(port, deadline) -> status, key, observed

Wait until some binding is ready or the absolute time `deadline` (nanoseconds) passes (0: do not block;
`u64::MAX`: no deadline), then return one ready binding's `key` and `observed` (`signals &
mask`). Successive calls rotate through the ready bindings, so none starves. Rights: WAIT.
Errors: BAD_HANDLE (also if the handle stops naming anything while waiting); WRONG_TYPE;
ACCESS_DENIED; TIMED_OUT. A killed process stops waiting at once.

### port_wait_many(port, buffer, capacity, deadline) -> status, count

`port_wait` for a batch (AB4). `capacity` is 1 to 64; each record in `buffer` is 16 bytes: `key` (`u64`), `observed` (`u32`, `signals & mask`) and four bytes of zero. Waits as `port_wait` does (`deadline` in nanoseconds, 0: do not
block, `u64::MAX`: none) until at least one binding is ready, then takes every ready binding up to `capacity`, each at most once, in the order that many
successive `port_wait` calls would have given them (the port's rotation moves past the last one taken, and a delivered one-shot binding disarms exactly as it
does for `port_wait`). `count` is how many records were written. Rights: WAIT. Errors: INVALID_ARGS (`capacity` 0 or above 64, or `buffer` not writable user
memory for `capacity` records; checked before anything is taken, and again in the step that takes the packets, so a packet is never taken and lost); BAD_HANDLE
(also if the handle stops naming anything while waiting); WRONG_TYPE; ACCESS_DENIED; INTERRUPTED (as `port_wait`); TIMED_OUT. `port_wait` stays until the system
library wraps it (AB7). Tests: `port_wait_many_*`; `a_batch_wait_equals_that_many_single_waits` (host model).

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
because it bounds the work of every later check); BAD_HANDLE (the channel, before
any user memory is read); INVALID_ARGS (`bytes` or the handle list is not readable user
memory); BAD_HANDLE (each listed handle); WRONG_TYPE (not a channel); ACCESS_DENIED (no WRITE on the channel, then
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
| process_suspend, process_resume | specified and implemented (SIG2) |
| thread_spawn, thread_exit, thread_info | specified (S3c-2b) |
| job_create, job_set_limits, job_info, job_kill, job_root | specified and implemented (S4.5) |
| socket_create, socket_write, socket_read | specified (S3c-3) |
| socket_shutdown | specified and implemented (tidy-up, T17) |
| vmo_create_physical, vmo_create_dma, vmo_phys_addr, resource_root | specified and implemented (S6a) |
| interrupt_create, interrupt_ack | specified and implemented (S6a-2) |
| resource_dtb | specified and implemented (S6c) |
| futex_wait, futex_wake | specified and implemented (S7b) |

## Planned changes (SCHED-SMP-SPEC.md; **none of this is implemented**)

These calls and amendments are specified, not built. A call here answers BAD_SYSCALL until the task that adds it has
merged (task 8 for the priority calls, 9 for `thread_spawn`, 22 and 23 for donation and the PI futex, 26 for
`thread_interrupt`); the reserved numbers are in `SCHED-SMP-SPEC.md` section 5. The time unit of every deadline and of
`clock_get` changes from 1 ms ticks to nanoseconds in one flag-day change (task 6).

**Limits** (will replace the thread sentence): "`MAX_REGIONS` 16384 per address space; live threads are limited by the
job's thread and page limits, with a system-wide backstop of `RAM_SIZE / 64 KiB` (4096)."

**Execution model** (will replace the section when task 20 lands): "A syscall runs on
the calling thread's kernel stack. Calls marked *legacy* in the Locks column run with interrupts off under the legacy
kernel lock; the others run with interrupts on, take the locks named for short sections, and can be preempted between
them. A call is atomic with respect to other threads only where its entry says so."

**Copying** (will be added): "A copy of more than 16 KiB is done in sections. If another thread of the same process unmaps the
destination buffer of a read while the read is copying, the read fails with INVALID_ARGS and the message or bytes it
had taken are lost: that race is the program's."

### thread_spawn(process, args) -> status, handle, tid

Create and start a thread in a running process, with its stack. `args` points to a 64-byte record (little endian):

| offset | field |
|---|---|
| 0 | `u64` entry |
| 8 | `u64` argument (the thread's `a0`) |
| 16 | `u64` `tp` value (opaque to the kernel) |
| 24 | `u64` stack bytes (0: 256 KiB; rounded up to a page; at most 64 MiB) |
| 32 | `u64` guard bytes (0: one page; rounded up to a page) |
| 40 | `u32` priority (0: the creator's base priority, clamped to the new thread's job) |
| 44 | `u32` flags: bit 0 round robin; bit 1 detached (no handle is returned); bit 2 caller stack (use offset 48, provision nothing) |
| 48 | `u64` stack pointer (caller stack only; otherwise 0) |
| 56 | `u64` reserved, 0 |

The thread starts at `entry` with `sp` at the top of its stack, `a0` the argument, `a1` its tid, `tp` as given, every
other register zero. A kernel-provisioned stack is mapped in the process's stack arena, is charged to the process's
job as it is touched, and is unmapped and freed when the thread has ended. Rights: MANAGE on `process`. Errors, in
order: INVALID_ARGS (bad `args` pointer); BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED (no MANAGE; or the priority asked is
above the job's maximum); BAD_STATE (the process is not running); INVALID_ARGS (entry not executable; sizes out of
range; reserved bits or fields set; a caller stack pointer not 16-aligned or outside user memory); NO_MEMORY (a job
limit: threads, pages, regions, objects; the caller's handle table; the system backstop; no room in the stack arena).
On error nothing changes. Locks: legacy until task 19.

**Built (task 9).** `a1` of the reply is the handle (0 for a detached thread), `a2` the tid. The stack arena is
`0x30_0000_0000` to `0x38_0000_0000`; the kernel looks for the lowest gap that holds the guard and the stack at or after
a per-process hint (just above the last stack it gave), wrapping once, and maps an anonymous VMO (charged to the
target's job as it is touched, with the page-table estimate charged at once like `process_map`) read-write above the
guard. A priority above the job's maximum is ACCESS_DENIED, one above 63 is INVALID_ARGS. When the thread ends the
region is unmapped, other harts' translations are shot down and the VMO is freed before the frames can be reused; a
whole-process exit leaves the stacks to the teardown. `MAX_THREADS_PER_PROCESS` (2048) and `MAX_USER_THREADS` (4096)
still apply (the per-process one goes with the job-quota work, T83). Tests: `thread_spawn_*`,
`two_thousand_threads_live`, `ten_thousand_threads_leak_nothing`, `find_gap_agrees_with_a_scan` (host).

### thread_set_sched(thread, priority, flags) -> status

Set a thread's base priority (1 to 63) and flags (bit 0: round robin within its level; other bits reserved, zero).
`thread` 0 names the caller. Rights: MANAGE on the handle (none for 0). Errors: BAD_HANDLE; WRONG_TYPE;
ACCESS_DENIED (no MANAGE; or `priority` above the thread's job's `max_prio`); INVALID_ARGS (priority 0 or above 63,
reserved flags); BAD_STATE (the thread has ended). The effective priority may be higher while other threads donate.

### thread_get_sched(thread) -> status, sched, tid

`sched` packs the base priority (bits 0-7), the flags (bits 8-15) and the effective priority (bits 16-23): a syscall
returns two values besides the status. Rights: INSPECT (none for 0, the caller). Errors: BAD_HANDLE; WRONG_TYPE;
ACCESS_DENIED.

### thread_suspend(thread) / thread_resume(thread) -> status

**Reserved.** Validate the handle (MANAGE) and answer NOT_SUPPORTED.

### thread_interrupt(thread) -> status   (replaces the reserved entry at task 26)

Make the target's current blocking call, or its next one, return INTERRUPTED. Rights: MANAGE. Errors: BAD_HANDLE;
WRONG_TYPE; ACCESS_DENIED; BAD_STATE (ended). Every blocking call lists INTERRUPTED among its errors; a
`channel_call` interrupted after its request was sent leaves the request in flight, as a timeout does.

### job_set_sched(job, max_prio, rt_budget_ns, cpu_weight) -> status

Set a job's scheduling limits. Rights: SET_LIMITS. `max_prio` 0 to 63 and `rt_budget_ns` (per 100 ms window) must not
exceed the parent job's; `cpu_weight` 1 to 65536 (reserved for the fair class; stored). Lowering `max_prio` lowers the
base priority of every thread in the job and its descendants that is above it. Errors: BAD_HANDLE; WRONG_TYPE;
ACCESS_DENIED (no SET_LIMITS; or a value above the parent's); INVALID_ARGS. `job_info` gains selectors 6 (`max_prio`),
7 (RT time used this window, budget), 8 (CPU nanoseconds used by the job's subtree, 0).

### futex_lock_pi(address, deadline) -> status   and   futex_unlock_pi(address) -> status

The word at `address` holds 0 (free) or the owner's tid in bits 0-29, with bit 31 set while threads wait in the
kernel and bit 30 if an owner died holding it. A thread takes a free lock in user mode by compare-and-swap from 0 to
its tid and releases an uncontended one by compare-and-swap back to 0; it calls the kernel only when either fails.
`futex_lock_pi` returns OK when the caller owns the lock; while it waits, the owner runs at least at the caller's
effective priority. Errors: INVALID_ARGS (as `futex_wait`); BAD_STATE (the word names no live thread of this process;
the lock is poisoned; or waiting would close a cycle of waiting threads: a deadlock); TIMED_OUT; INTERRUPTED.
`futex_unlock_pi` hands the lock to the highest-priority waiter (first come among equals) or frees it. Errors:
INVALID_ARGS; ACCESS_DENIED (the caller does not own it). `futex_wait` and `futex_wake` gain a flags argument whose
bit 0 (shared key) is reserved: NOT_SUPPORTED.

### channel_call (will amend the last paragraph, task 22)

"From task 22 the caller lends its effective priority to the thread that reads its request, until a message with the
same transaction id is written back, the call ends for any reason, or that thread ends."

## Planned changes (KERNEL-V1-SPEC.md, OD126 to OD141)

The v1 kernel changes this contract in the ways listed here; the entries above describe the kernel as it is and are
rewritten call by call as each change lands. Full semantics: `docs/specs/KERNEL-V1-SPEC.md` section 3.

* **The boundary moves.** Programs call the functions of the system library the kernel maps into every process
  (`sys_<name>`); syscall numbers and registers become private, and this file becomes the reference of that library.
* **Handles are 64 bits** (24 slot bits, 40 generation bits); handle arrays are 8 bytes per handle. (Built: AB1.)
* **Times are nanoseconds**; `clock_get` is computed in user mode by the system library.
* **Identifiers** (koid, thread id) are 64-bit and never reused; no counter wraps.
* **Memory:** `vmo_create` by bytes with RESIZABLE, `vmo_set_size`, `vmo_decommit`, `vmo_create_child` (SNAPSHOT,
  PRIVATE), `process_map` with ANYWHERE returning the address, `process_unmap` and `process_protect` over ranges,
  `process_map_info`, `process_clone_region`, and the pager calls.
* **Threads and exceptions:** `thread_spawn` is the only way to make a thread; it gains SUSPENDED; `thread_suspend`,
  `thread_resume`, `thread_read_state`, `thread_write_state`; `process_exception_channel`, `job_exception_channel`,
  `exception_resume`.
* **Waiting:** `port_wait_many`.
* **Harts and power** (root resource): `hart_set_online`, `hart_info`, `system_suspend`, `system_set_placement`.
* **Debug calls** need a debug resource handle.
* **Limits:** the system-wide caps become backstops computed from RAM; per-process ceilings are job quotas.
