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
| 40 | vmo_create | `a0` bytes, `a1` job (0: the caller's), `a2` flags | `a1` handle |
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
| 52 | process_map | `a0` process, `a1` vmo, `a2` vaddr (0 with ANYWHERE), `a3` vmo_offset, `a4` length, `a5` flags (R 1, W 2, X 4, ANYWHERE 8) | `a1` the address, for ANYWHERE |
| 53 | process_unmap | `a0` process, `a1` vaddr, `a2` length | |
| 54 | process_start | `a0` process, `a1` entry, `a2` sp, `a3` bootstrap channel, `a4` arg | |
| 55 | process_kill | `a0` process, `a1` code | |
| 56 | process_info | `a0` process, `a1` which (0 exit code, 1 CPU time) | `a1` exit code, or CPU nanoseconds |
| 57 | process_suspend | `a0` process | |
| 59 | process_protect | `a0` process, `a1` vaddr, `a2` length, `a3` flags | |
| 58 | process_resume | `a0` process | |
| 67 | process_map_info | `a0` process, `a1` vaddr, `a2` buffer | `a1` the next region's base, or 0 |
| 68 | vmo_create_child | `a0` vmo, `a1` offset, `a2` bytes, `a3` job (0: the caller's), `a4` flags | `a1` handle |
| 69 | process_clone_region | `a0` source process, `a1` target process, `a2` vaddr, `a3` mode | |
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
| 62 | thread_info | `a0` thread (0: the caller, for `which` 1), `a1` which (0 exit code, 1 CPU time) | `a1` exit code, or CPU nanoseconds |
| 63 | thread_interrupt | `a0` thread (MANAGE) | |
| 74 | thread_suspend | `a0` thread (MANAGE) | |
| 75 | thread_resume | `a0` thread (MANAGE) | |
| 76 | thread_read_state | `a0` thread (MANAGE), `a1` kind, `a2` buffer, `a3` length | |
| 78 | process_exception_channel | `a0` process (MANAGE) | `a1` channel |
| 79 | job_exception_channel | `a0` job (MANAGE) | `a1` channel |
| 97 | exception_resume | `a0` exception (MANAGE), `a1` how | |
| 103 | pager_create | | `a1` pager, `a2` the user end of its request channel |
| 104 | pager_create_vmo | `a0` pager (MANAGE), `a1` key, `a2` bytes | `a1` VMO |
| 105 | pager_supply | `a0` pager (MANAGE), `a1` VMO, `a2` offset, `a3` length, `a4` source VMO (WRITE), `a5` source offset | |
| 106 | pager_fail | `a0` pager (MANAGE), `a1` VMO, `a2` offset, `a3` length, `a4` status | |
| 107 | pager_query_dirty | | NOT_SUPPORTED (reserved) |
| 108 | pager_writeback_done | | NOT_SUPPORTED (reserved) |
| 77 | thread_write_state | `a0` thread (MANAGE), `a1` kind, `a2` buffer, `a3` length | |
| 65 | thread_set_sched | `a0` thread (0: the caller), `a1` priority 1-63, `a2` flags | |
| 66 | thread_get_sched | `a0` thread (0: the caller) | `a1` packed priorities, `a2` tid |
| 24 | channel_call | `a0` channel, `a1` args pointer | `a1` nbytes, `a2` nhandles |
| 90 | job_create | `a0` parent job, `a1` pointer to the limits | `a1` handle |
| 91 | job_set_limits | `a0` job, `a1` pointer to the limits | |
| 92 | job_info | `a0` job, `a1` which (0 processes, 1 threads, 2 pages, 3 jobs, 4 objects, 5 ipc bytes, 6 scheduling limits, 7 processors, 8 kernel bytes, 9 real-time time, 10 CPU time) | `a1` used, `a2` limit (for 6: `max_prio`, `rt_budget` ns; for 7: the number of online harts in both) |
| 96 | job_set_sched | `a0` job, `a1` max_prio 0-63, `a2` rt_budget ns per 100 ms, `a3` cpu_weight 1-65536 | |
| 93 | job_kill | `a0` job, `a1` exit code | |
| 94 | job_root | | `a1` handle |
| 95 | resource_root | | `a1` handle |
| 100 | interrupt_create | `a0` resource, `a1` source | `a1` handle |
| 101 | interrupt_ack | `a0` interrupt | `a1` claims so far |
| 110 | futex_wait | `a0` address, `a1` expected, `a2` deadline | |
| 111 | futex_wake | `a0` address, `a1` count | `a1` woken |
| 112 | futex_lock_pi | `a0` address, `a1` deadline | |
| 113 | futex_unlock_pi | `a0` address | |
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

### vmo_create(bytes, job, flags) -> status, handle

New zero-filled VMO whose pages are charged to a job as they are first touched (S4.5): `job` is
a job handle with MANAGE, or 0 for the caller's own job. `flags` (MM6): bit 0 `RESIZABLE` makes the VMO's size changeable (`vmo_set_size`); any other bit is INVALID_ARGS. The size is in bytes, rounded up to whole pages (`vmo_get_size` returns the rounded size). Errors: INVALID_ARGS (`bytes` is 0 or
over the limit); BAD_HANDLE, WRONG_TYPE, ACCESS_DENIED (`job`); NO_MEMORY (object cap or kernel
memory). A page that cannot be charged is refused when it is first touched, with NO_MEMORY from
`vmo_write`, or as a fault that ends the process. Tests: `vmo_*` in `init`.

### vmo_read(vmo, offset, buffer, length) / vmo_write(vmo, offset, buffer, length) -> status

Copy between a VMO and user memory. Rights: READ / WRITE. Errors, in order: BAD_HANDLE;
WRONG_TYPE; ACCESS_DENIED; INVALID_ARGS (a bad user pointer, or the range does not lie
inside the VMO: `offset + length` wraps or exceeds its size; a zero `length` is allowed
anywhere inside or at the end). `length` is limited to `MAX_MSG_BYTES` per call (TOO_BIG).
Nothing is written on an error. **A page of a pager's VMO that nobody has supplied** (MM9): the kernel asks the pager, waits with no lock held until the page is supplied (or the pager fails or dies, or the thread is killed or interrupted), and the call then returns SHOULD_WAIT (the page is there now: call again) or, when the pager failed it, the pager's status. The same holds for a user pointer into such a page, in every call that copies from or to user memory (`channel_call` waits for a page of its reply buffer inside the call, so the request is never sent twice). A call that was interrupted (`thread_interrupt`) during that wait reports the interruption as any blocking call does and takes the flag; a **fault** ignores `thread_interrupt` and leaves the flag for the next blocking call.

### vmo_decommit(vmo, offset, length) -> status

Give back the pages `[offset, offset + length)` of a VMO (MM6): every mapping of them, in every address space, goes and every hart drops its translations **before** any frame is freed, then the VMO drops its references, the frames return to the allocator and the VMO's creating job is credited. The range reads as zeros afterwards, through a mapping (the page faults in again as a fresh zero page, charged then (a member of a family with a parent maps the shared zero frame for a read and is charged at its first write)) and through `vmo_read`. Pages that were never committed cost nothing. Rights: WRITE. `offset` and `length` must be page aligned and `length` not 0 (INVALID_ARGS), the range inside the VMO (INVALID_ARGS). Errors, in order: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED; NOT_SUPPORTED (a device or DMA VMO: their frames are fixed); INVALID_ARGS; BAD_STATE (a process of another job than the VMO's has a mapping that covers part of the range: V1, INVARIANTS: such a process holds its mapping on the promise that touching it never fails on the VMO's job; nothing changes). For a member of a family (MM8) a page inside the window it inherits becomes an explicit hole: it reads zeros whatever its parent shows now or later, and a read maps the shared zero frame, uncharged; the hole's slot (8 bytes) is kernel bookkeeping not charged to the job, as the page index of any VMO is. Outside that window the pages simply go. The mappings stay (the regions keep their length); only the pages go. A decommit that finds nothing mapped anywhere costs no shootdown. Tests: `vmo_decommit_and_resize_from_user_mode`, `kcore/tests/vmo_ops_model.rs`.

### vmo_set_size(vmo, bytes) -> status

Change the size of a VMO made with `RESIZABLE` (MM6); `bytes` must be a whole number of pages, not 0, and at most the VMO limit (INVALID_ARGS). Growing changes the size and nothing else (no page is committed). Shrinking decommits the pages past the new end exactly as `vmo_decommit` does (mappings and translations first, then the frames) and the size follows; a mapping that reaches past the new end keeps its length, and **touching its part past the end is a page fault that ends the process** (the address is in no backed region). Rights: WRITE. Errors, in order: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED (no WRITE, or the VMO was not made RESIZABLE: every device and DMA VMO is not, and is refused here, before the size is looked at); INVALID_ARGS; BAD_STATE (a shrink, when a process of another job than the VMO's maps part of the tail: as for `vmo_decommit`; or the VMO has children, MM8). Growing never fails on a mapping. Sizes are whole pages until the byte-sized VMOs of MM6's full text (work/T207.md). Tests: `vmo_decommit_and_resize_from_user_mode`, `touching_past_a_shrunk_vmo_ends_the_process_cleanly`.

### vmo_create_child(vmo, offset, bytes, job, flags) -> status, handle

Make a copy-on-write child of the bytes `[offset, offset + bytes)` of a VMO (MM8, KERNEL-V1-SPEC 4.3). `flags` is exactly one of `child_flags::SNAPSHOT` (1) or `PRIVATE` (2). Rights: READ on the source; the new handle carries the full VMO rights. `job` is a job handle with MANAGE, or 0 for the caller's own. **SNAPSHOT**: afterwards the source and the child each see the source's contents as they were, and each one's later writes are private to the writer. The child is charged to the source's job (`job` must be it: ACCESS_DENIED otherwise), so a page never changes payer (COW2). The source's committed pages move to a hidden node that both hang under (or, when the source holds nothing of its own since the last SNAPSHOT, the child hangs under the node that is already there: a `fork` that clones every region of one VMO makes one node); every writable translation of the source is made read-only and dropped from every hart's TLB before the call returns (COW1); the first write to a page the writer only inherits copies it into the writer (charged to the writer's job), or takes the frame over when no other node can see it. When a leaf goes and a hidden node is left with one child, the node is merged into it. **PRIVATE**: the child sees the source's pages, including later changes to pages it has not written (a page nobody has committed reads as zeros through the shared zero frame, read-only, and a later write by the source shows); the child's writes copy into the child and are charged to its job; the source never sees them. Allowed across jobs. No chain of children, counted from the deepest descendant of the source, is more than 64 links (BAD_STATE). `offset` and `bytes` must be page aligned and `bytes` not 0 (INVALID_ARGS), the range inside the source (INVALID_ARGS). Errors, in order: INVALID_ARGS (flags, alignment, zero length); BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED (READ; MANAGE on the job; a SNAPSHOT into another job); BAD_STATE (**a SNAPSHOT while a process of another job than the VMO's maps the source**: V1, such a process holds its mapping on the promise that a write never charges the VMO's job, and a SNAPSHOT would turn its next write into a copy); then NOT_SUPPORTED (a device or DMA VMO; a SNAPSHOT of a pager's VMO, MM9), INVALID_ARGS (the range), BAD_STATE (too deep), NO_MEMORY (including no room for the handle: the source is not touched until the handle exists). A source that is written between two SNAPSHOTs while the earlier children live adds a hidden node each time, so one written VMO can have about 64 live SNAPSHOT children before the next is BAD_STATE (the spec's depth limit; children of an unwritten source share one node). Nothing changes on an error. Everything that holds a page of a family (the member's own mappings, the mappings of everything below it, in every address space) is kept coherent: a copy, or a page that becomes visible where a read had mapped the shared zero frame, drops the stale translations of every address space and every hart before the thread that caused it runs on. `vmo_write` and `vmo_commit` work on members; `vmo_decommit` of a member's range inside the window it inherits leaves an explicit hole (zeros whatever the parent shows, no frame, no charge); `vmo_set_size` is refused (BAD_STATE) for a VMO with children and otherwise works (a grown member reads zeros for the new pages). Tests: `copy_on_write_children_from_user_mode` (with its negative control, the `fault-skip-cow-protect` kernel), `kcore/tests/vmofamily_model.rs` (random trees against a reference, through `vmo_read`/`vmo_write` and two address spaces, with protect cycles, decommit and resize; mutation-checked), `kcore/tests/vmofamily_concurrency.rs` (shuttle), `ksys/tests/proc_calls.rs`.

### process_clone_region(source, target, vaddr, mode) -> status

Map the region of `source` that **starts at** `vaddr` into `target` at the same address with the same permissions and ceiling (MM8); no VMO handle is exposed, so a `fork` shim can loop over `process_map_info` and clone every region. `mode` `clone_mode::SHARE` (1) maps the same VMO; `SNAPSHOT` (2) maps a SNAPSHOT child of the source's VMO range (as `vmo_create_child` does, with its rules), which must be charged to the target's job (ACCESS_DENIED otherwise: a fork child runs in its parent's job). Rights: MANAGE on both processes (and so authority over everything either maps: a holder of MANAGE on two processes can read and write the source's memory through a SHARE without any VMO handle, as the spec intends). The target must have no mapping in the range. Errors, in order: INVALID_ARGS (mode, alignment); BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED; INVALID_ARGS (no region starts at `vaddr`, the target has a mapping there); BAD_STATE (either process has ended; a SNAPSHOT while a process of another job maps the VMO; too deep; a stranger's VMO that is not committed, as for `process_map`); NOT_SUPPORTED (a SNAPSHOT of a device or DMA region); NO_MEMORY (the target is at the region limit; no memory). The page tables are charged to the target's job as for `process_map`. Nothing changes on an error. Tests: `process_clone_region_shares_or_snapshots_a_region_into_another_process`.

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

### pager_create() -> status, pager, channel

(MM9, KERNEL-V1-SPEC 4.4.) A **pager** is the authority to back VMOs with a user-mode server. The call returns a pager handle (DUPLICATE, TRANSFER, MANAGE) and the user end of a channel on which the kernel writes **page requests**, each `abi::pager::REQUEST_BYTES` (40) bytes of five little-endian words: the VMO's key, the byte offset, the byte length (one page, 4096), the kind (`abi::pager::READ` = 1; 2 is reserved for writeback), and 0. The kernel keeps the other end of the channel; the messages are charged to the caller's job like any channel's. A page is requested **once** however many threads wait for it; a request that finds the channel full stays pending (and its waiters waiting) and is written when there is room (retried every 10 ms by the waiters). A pager whose channel has no reader left is dead (checked when the user end's last handle goes, and at every new wait): every wait for its pages fails (PEER_CLOSED). The pager object ends with its last handle (its waits fail, the kernel's end of the channel is closed). Errors: NO_MEMORY (the caller's handle table, the job's object limit).

### pager_create_vmo(pager, key, bytes) -> status, handle

A VMO of `bytes` bytes (page aligned, not 0, at most `MAX_VMO_PAGES`) whose pages the pager supplies: none is committed, and a touch of a page nobody has supplied (a fault in a mapping, a `vmo_read`, a `vmo_write`, a kernel copy to or from a mapping) **asks the pager and waits**; the VMO reads as zeros nowhere. The handle has every VMO right but COMMIT (a commit would put zeros where the pager's data belongs). The VMO is charged to the caller's job as its pages arrive; `key` is the number the requests for it carry (the pager's own name for the file or object). It can be mapped, read, written, decommitted (its pages are asked for again), and given PRIVATE children (a child asks the parent's pager for the pages it inherits; its own writes copy as for any PRIVATE child); a SNAPSHOT of it is NOT_SUPPORTED. Errors, in order: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED (MANAGE on the pager); INVALID_ARGS (`bytes`); NO_MEMORY.

### pager_supply(pager, vmo, offset, length, source, source_offset) -> status

Give pages to a pager's VMO and wake the threads waiting for them. The pages **move**: the frames of `source` (an anonymous VMO of the caller's, with WRITE, with no parent, no children and no pager, whose pages in the range are all committed and **mapped by no one**) become the pager VMO's pages at `offset`, nothing is copied, and the charge moves with them from the source's job to the pager VMO's. The source's mappings of the range are removed first, in every address space, with the TLBs of every hart (refused, BAD_STATE, when a process of another job than the source's maps the range: V4). A destination page that is already there is left as it is (and the source keeps its page). `offset`, `length` and `source_offset` are page aligned (INVALID_ARGS), `length` not 0, both ranges inside their VMOs. Errors, in order: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED (MANAGE on the pager; WRITE on the source; a VMO of another pager); INVALID_ARGS (alignment, ranges, a VMO that is not a pager's, source and destination the same); BAD_STATE (a source page that is not committed or is still held by a mapping elsewhere; a stranger's mapping); NO_MEMORY (the destination's job has no room: the pages that did move stay moved and their waiters wake). Tests: `ksys/tests/pager_calls.rs`, `kcore/tests/pager_vmo.rs`, `a_pager_serves_a_vmo_to_a_reader_in_another_process`, `pager_calls_check_their_arguments`.

### pager_fail(pager, vmo, offset, length, status) -> status

The pager cannot supply these pages: the threads waiting for them fail with `status` (a negative status code other than OK, SHOULD_WAIT and INTERRUPTED: INVALID_ARGS otherwise). A thread that was **faulting** takes a PAGER exception (`abi::arch::fault::PAGER`, the status as its cause, then the channels of EX2; with no handler the process ends with `-1000 - 6`); a kernel copy returns the status. A page that was failed is asked for again the next time a thread needs it. Errors, in order: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED (MANAGE; a VMO of another pager); INVALID_ARGS (alignment, range, a VMO that is not a pager's, `status`). Tests: `a_pager_failure_is_a_pager_exception_and_a_supplied_page_lets_it_retry`.

### pager_query_dirty() / pager_writeback_done() -> NOT_SUPPORTED

Reserved for dirty tracking (request kind 2); the names exist and answer NOT_SUPPORTED. **PG1** (INVARIANTS): a thread waiting for a pager holds no kernel lock and can always be killed; a reader killed while it waits leaves nothing behind (`killing_a_reader_waiting_for_a_pager_leaks_nothing`), a pager that dies fails every waiter (`a_pager_that_dies_fails_the_reader_with_a_pager_fault`).

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
check and the enqueue are one step under the key's bucket lock (no big kernel lock is held while it sleeps, and `futex_wake` takes it only for a switch its wake asked for, or when a kill, stop or dirty FP state needs the full path: L14), so a wake that follows a user-mode store
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
`length` 0; unknown flag bits; W with X; the range outside the user half (page zero is never mapped; address 0 is refused unless the flag ANYWHERE is set) or outside the
VMO; overlap with an existing mapping; a process picks its own stack, so no address range is
reserved); NO_MEMORY. Mapping into a running process takes effect at once.

**ANYWHERE (MM7):** the flag `ANYWHERE` (8; `vaddr` must be 0, else INVALID_ARGS) asks the kernel to choose (every argument error is found first, so a call refused for its arguments draws nothing from the entropy pool; one that fails later, for V1 or for want of memory, has drawn up to nine values): a page-aligned place in the mapping arena (`0x9_0000_0000`, above the runtime heap's range, up to the stack arena), picked at random (the kernel entropy pool: HD1): eight random positions are tried, then the first fit from a random start, wrapping once; the address comes back in `a1`. NO_MEMORY when nothing fits. A mapping that touches another of the same VMO at the next offset with the same permissions becomes part of it. The mapping remembers what it may later be raised to (`process_protect`): read, write if the VMO handle had WRITE, execute if it had EXECUTE.
Tests: `process_map_*` in `init`.

### process_unmap(process, vaddr, length) -> status

With `length` 0 (the original call): remove the mapping that starts at `vaddr`, whole. (A mapping is the region: two `process_map` calls of the same VMO at adjacent addresses, contiguous offsets and the same permissions are one mapping, so removing it by its start removes both; the range form below addresses pages and does not care.) With a `length` (MM7): remove every page of `[vaddr, vaddr + length)`; holes are fine, and a mapping that reaches past either edge is split there and the rest stays. Every hart drops its translations before a freed frame can be reused (TLB2). Rights: MANAGE. Errors: BAD_HANDLE; WRONG_TYPE;
ACCESS_DENIED; BAD_STATE; INVALID_ARGS (`length` 0: no mapping starts there; otherwise `vaddr` or `length` not page aligned, or the range is not inside the user half); NO_MEMORY (the call needs to split a mapping and the address space is at its region limit: a range that lands on mapping boundaries needs no split and never fails for this).

### process_protect(process, vaddr, length, flags) -> status

Change the permissions of `[vaddr, vaddr + length)` to `flags` (R = 1, W = 2, X = 4; read is always implied; MM7). The **whole range must be mapped** (mappings that touch count as one), and `flags` must be within what each mapping may be raised to (read; write if the VMO handle that mapped it had WRITE; execute if it had EXECUTE), and never W with X. Mappings are split at the edges, the page-table entries change in place (no frame moves), and neighbours that now agree merge. A permission taken away is dropped by every hart before the call returns; pages that became executable are synchronised for instruction fetch on every hart (PL7). Rights: MANAGE. Errors, in order: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED (MANAGE; or `flags` beyond the ceiling of part of the range, checked after the range is known to be mapped); INVALID_ARGS (unknown flag bits or W with X; `vaddr` or `length` not page aligned, `length` 0, or the range not inside the user half; part of the range not mapped); BAD_STATE (the process has ended); NO_MEMORY (the split would pass the region limit). Nothing changes on an error. Tests: `process_protect_*` in `init`, `kcore/tests/region_ops_model.rs`.

### process_map_info(process, vaddr, buffer) -> status, next

Describe the mapping that contains `vaddr`, or the first one above it (MM7): six 64-bit words are written to `buffer` in the caller's memory: base, length in bytes, permissions, the permissions it may be raised to (bit 0 read, bit 1 write, bit 2 execute), the VMO's id, and the byte offset into the VMO. `a1` is the base of the next mapping above this one, or 0. With no mapping at or above `vaddr` the record is all zeros and `a1` is 0. Rights: INSPECT on `process`. Errors: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED; BAD_STATE; INVALID_ARGS (a bad buffer). Tests: `process_protect_*` in `init`.

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

### process_info(process, which) -> status, value

`which` 0 (the default): the exit code of an ended process (`-1000 - kind` after a fault). `which` 1 (AB6): the CPU nanoseconds the process's threads have run, the ended ones and the live ones (works while it runs; a process that has ended gives its total). Rights: INSPECT.
Errors: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED; BAD_STATE (`which` 0, not ended yet); INVALID_ARGS (`which`).

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
process object and a VMO made with a job handle (`process_create(job)`, `vmo_create(bytes, job)`)
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

Needs INSPECT. `which`: 0 processes, 1 threads, 2 pages, 3 jobs, 4 objects, 5 ipc bytes, 6 the job's scheduling limits (`a1` its `max_prio`, `a2` its `rt_budget` in ns per 100 ms window; see `job_set_sched`), 7 the processors its threads may run on (UV6; the number of online harts, in `a1` and `a2`: there is no CPU affinity yet), 9 the real-time CPU time (SCHED-SMP-SPEC 4.5, task 8): `a1` the nanoseconds the job's own threads have run **at a real-time base priority** in the current 100 ms window (the windows are aligned to multiples of 100 ms since boot; the figure restarts at zero with each), `a2` the job's `rt_budget` (enforced: a job that has used it has its real-time threads demoted to priority 31, round robin, until the window ends), 10 the CPU time (`a1`: the nanoseconds run by the threads of the job and of the jobs below it since each was created; the root job also holds the kernel's own threads, which are charged to it; a job that has been freed is no longer counted; `a2` 0), 8 kernel memory in bytes (MM5: the memory the kernel holds because of the job's programs, other than pages they map; its limit is an eighth of the job's page limit in bytes, none without one); `used` counts the job and everything
below it. Errors: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED; INVALID_ARGS (`which`).

**Kernel bytes (`which` 8)** are charged for object bodies (every call that creates an object: `channel_create`, `event_create`, `socket_create`, `port_create`, `vmo_create`, `thread_spawn`, `process_create`, `job_create`, ... returns NO_MEMORY when the job or an ancestor would pass its kernel-byte limit) and for the growth of a process's handle table (charged when the call that grew it returns: a job that cannot pay leaves the table unable to grow, so the calls that insert a handle return NO_MEMORY, while closing and reusing handles still work; the table stays unable to grow until the next call the process makes after its job has room again, so the first insert after room returns can still be refused once) and for a thread's floating-point and vector register state, charged at the thread's first use of the unit. A job at its limit that makes a thread use floating point or vector instructions for the first time ends the process with the illegal-instruction exit code: the instruction faults like any illegal one (a program built for RVA23 whose start-up code is vectorised dies this way in a job whose kernel-byte limit is too small to run a single thread's register state).

**Pages (`which` 2) count the pages a job's address spaces have prepaid** for their faults as well as the pages its VMOs hold: in a job with a limit, an address space prepays up to 64 pages at a time (an eighth of the job's room at most, one at least) and gives the rest back when it ends, so `used` can read a little above the committed pages while a process of the job runs. A job with no limit prepays nothing.

### job_set_sched(job, max_prio, rt_budget_ns, cpu_weight) -> status

Set a job's scheduling limits (slice SC, SCHED-SMP-SPEC 4.9). Needs SET_LIMITS. `max_prio` (0 to 63) is the highest **base**
priority any thread of the job may have (the real-time band, 32 to 63, therefore only exists in a job whose maximum reaches
it); `rt_budget_ns` is real-time CPU per 100 ms window, enforced: when the job's threads of real-time base priority have run that long in a window they count as priority 31, round robin, until it ends, and a hart that has run real-time threads for 80 ms of a window gives the normal band the rest of it when the normal band has work; `cpu_weight` (1 to 65536) is stored
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

`sched` packs the base priority (bits 0 to 7), the flags (8 to 15) and the effective priority (16 to 23: the base raised by what is
donated to the thread, L23); `tid` is the kernel thread id. `thread` 0 names the caller. Needs INSPECT on a handle. Errors:
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

### thread_info(thread, which) -> status, value

`which` 1 (AB6) is the CPU nanoseconds the thread has run (works while it runs, and gives the total once it has ended; handle 0 names the calling thread and needs no right); INVALID_ARGS for any `which` above 1. `which` 0, the default: the exit code of an ended thread (`-1000 - kind` is *not* used: a fault ends the process
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
| vmo_set_size, vmo_create_child | specified and implemented (MM6, MM8) |
| process_clone_region | specified and implemented (MM8) |
| process_map, process_unmap, process_protect, process_map_info | specified (S3c-2a, MM7) |
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
| 44 | `u32` flags: bit 0 round robin; bit 1 detached (no handle is returned); bit 2 caller stack (use offset 48, provision nothing); bit 3 SUSPENDED (EX1: the thread exists, with its registers as below, but does not run until `thread_resume`; with detached it is INVALID_ARGS, since nothing could resume it) |
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

### thread_suspend(thread) -> status, thread_resume(thread) -> status

(EX1, KERNEL-V1-SPEC 3.6.) A thread has a **suspend count**. `thread_suspend` (MANAGE) raises it and returns at once: the thread stops at its next return to user mode (a thread running user code on another hart is reached by the same inter-hart interrupt a kill uses), or where it is if it is **blocked in a call** (a thread that enters a blocking call after the suspend stops there too). A thread whose count is above 0 never returns to user mode: it parks as it would return. When it has stopped its object raises the signal **SUSPENDED** (bit 7; waitable; a level: it falls when the count returns to 0; never set with TERMINATED). `thread_resume` (MANAGE) lowers the count; at 0 the thread runs again (a parked one is woken, one still inside its call just goes on). A thread may suspend itself (it parks as the call returns). Suspending is independent of `process_suspend`. The count is at most 2^20 (NO_MEMORY beyond). Errors, in order: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED (MANAGE); BAD_STATE (the thread has ended; for `thread_resume` also: its count is 0). A kill of a process ends a parked thread as it does any other. Tests: `thread_suspend_stops_a_thread_and_its_registers_can_be_read_and_written`, `suspend_and_state_calls_check_their_arguments_and_rights`, `kcore/tests/suspend_model.rs` (the count against a model: no lost wake, the signal exactly when stopped; mutation-checked).

### process_exception_channel(process) -> channel, job_exception_channel(job) -> channel, exception_resume(exception, how) -> status

(EX2, KERNEL-V1-SPEC 3.6.) A fault the kernel cannot resolve (a page fault it refuses, an illegal instruction, a breakpoint, a misaligned or refused access: `abi::arch::fault`) no longer ends the process at once if someone has asked to handle it. `process_exception_channel` and `job_exception_channel` (MANAGE on the process or job) return the user end of a new channel; the kernel keeps the other. A second call while the first channel's user end is open is BAD_STATE; one whose user end has closed replaces it. When a thread takes such a fault the kernel **stops the thread** (it is in the exception state: stopped, SUSPENDED raised, `thread_read_state` and `thread_write_state` allowed) and offers the exception to the process's channel; if there is none, its user end is closed, or the write fails (no room, the job's object limit), to the process's job's channel, then each ancestor job's. With none, the process ends as it always did with `-1000 - kind`. The message is 40 bytes (five little-endian words: kind, tid, pc, address, cause, where cause is the architecture's raw cause: on RISC-V 12, 13 or 15 for a page fault on a fetch, a load or a store) with two handles: the thread (INSPECT, MANAGE, WAIT, as `thread_spawn` gives) and the **exception** object (TRANSFER, MANAGE). `exception_resume(exception, how)` (MANAGE) says what becomes of the thread: RETRY (1) returns it to user mode from its state, which the handler may have rewritten (the SIGSEGV pattern: fix the page with `process_protect`, then RETRY), NEXT (2) offers it to the next level, KILL (3) ends the process with the usual code. An exception is resolved once (BAD_STATE after), and a process that has ended discards its exceptions (BAD_STATE). **Closing the last handle to an exception is NEXT.** Errors: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED; INVALID_ARGS (`how`; for `exception_resume`); BAD_STATE (an exception already resolved, or one that is no longer the thread's current one, or a process that has ended; for the channel calls a second open channel or an ended process); NO_MEMORY (the caller's table, the target's job's object limit). The thread is stopped *for the exception*: `thread_resume` does not release it (BAD_STATE: its suspend count is 0) and only the exception's resolution does; a `thread_suspend` on top keeps it stopped after a RETRY until it is resumed too. The thread handle in the message carries a reference of its own: closing it never destroys the thread's object. **A handler that takes an exception itself**, in the thread that reads the channel, is not special: the message waits and nobody reads it; the supervisor's job-level channel is the remedy. The kernel's end of a job's channel is released when its user end is closed (at that handle close or at that process's end), and a process's own with the process; a later call after the user end closed makes a new one. `fault::PAGER` is produced by a pager failure (MM9, `pager_fail` or a pager that died): the cause is the pager's status. Tests: `a_handler_thread_can_make_a_faulting_store_succeed_and_retry`, `a_parent_handler_can_kill_a_child_that_took_an_illegal_instruction`, `next_reaches_the_job_channel_and_closing_the_handle_is_next`, `a_process_killed_with_a_pending_exception_leaks_nothing_and_a_closed_channel_is_skipped`, `ksys/tests/exception_calls.rs`.

### thread_read_state(thread, kind, buffer, length) -> status, thread_write_state(thread, kind, buffer, length) -> status

(EX1.) Read or write the user registers of a thread that has **stopped**: SUSPENDED is up (parked at the return to user mode or blocked in a call), or it was made SUSPENDED and has not run. Rights: MANAGE on a thread handle (0, the caller itself, is BAD_STATE: it is running). `kind` and the layouts (RISC-V; `abi::thread_state`): GENERAL (1) is 32 little-endian words, x1 to x31 then pc (256 bytes); FP (2) is f0 to f31 then fcsr (264 bytes; fcsr's 8 defined bits); VECTOR (3) is vtype, vl, vstart, vcsr, then v0 to v31 of `vlenb` bytes each (`32 + 32 * vlenb` bytes; NOT_SUPPORTED on a machine without a vector unit). `length` must be exactly the layout's size (INVALID_ARGS). A thread that never used the FP or vector unit reads zeros; a write gives it the state (charged to its job's kernel bytes, NO_MEMORY at the limit) and it runs with the unit on. A write to a thread that has run is **left for the thread to apply to its own frame when it wakes** (a read in between shows the pending value); the layout of every kind is applied before the thread runs another instruction. A write is refused (BAD_STATE) while the thread is **inside a blocking call** (a read is allowed: its user registers are as they were when it entered); a write to a GENERAL pc that is not a user address, or not 2-aligned, is INVALID_ARGS and changes nothing. Errors, in order: INVALID_ARGS (kind, length); BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED; BAD_STATE (not stopped; a write inside a blocking call; the thread itself); INVALID_ARGS (a bad buffer; the pc). Tests: `a_stopped_thread_can_be_sent_to_a_handler_and_back` (the signal-injection sequence), `a_thread_started_suspended_runs_only_after_resume_with_registers_written_first` (the `fork` child sequence), `fp_and_vector_state_round_trip_through_thread_state`.

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

### futex_lock_pi(address, deadline) -> status

The word at `address` holds 0 (free) or the owner's tid in bits 0-29, with bit 31 set while threads wait in the
kernel and bit 30 if an owner died holding it. A thread takes a free lock in user mode by compare-and-swap from 0 to
its tid and releases an uncontended one by compare-and-swap back to 0; it calls the kernel only when either fails.
`futex_lock_pi` returns OK when the caller owns the lock; while it waits, the owner runs at least at the caller's
effective priority. A deadline of 0 polls (TIMED_OUT if it would wait). **Built (L23).** The tid is the kernel thread id (`thread_get_sched` with thread 0 names the caller's); a thread whose tid does not fit in 30 bits cannot use PI futexes (NOT_SUPPORTED). A waiter that gives up leaves the `WAITERS` bit set; the next unlock finds no queue and writes 0. Errors: INVALID_ARGS (as `futex_wait`); BAD_STATE (the word names no live thread of this process, and the kernel then poisons it; the lock is poisoned; the caller owns it already; or waiting would close a cycle of waiting threads: a deadlock; a waiter whose owner dies holding it, with the word poisoned, gets this too); NOT_SUPPORTED (the caller's tid does not fit); TIMED_OUT; INTERRUPTED.
(A chain of waiters deeper than 8 is BAD_STATE too, and a lock word in a page of a pager's VMO that is not there is SHOULD_WAIT: try again.)

### futex_unlock_pi(address) -> status

Hands the lock to the waiter with the best effective priority at that moment (first come among equals) or frees it. Errors:
INVALID_ARGS; ACCESS_DENIED (the caller does not own it, or the word is poisoned); NOT_SUPPORTED (the caller's tid does not fit in 30 bits); SHOULD_WAIT (a pager's page); BAD_STATE (the caller's process is gone). `futex_wait` and `futex_wake` gain a flags argument whose
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
