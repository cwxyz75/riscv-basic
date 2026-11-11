# OS Architecture (proposal)

A capability-based microkernel and userland in Rust for QEMU's RISC-V `virt`
board: multicore, with a Fuchsia-style object model, Plan 9-style per-process
namespaces, our own network stack, and a POSIX-ish compatibility layer on top.
Built for amusement and as an experiment: **every line of code is written by AI;
the human is the product owner.**

**Status: design proposal v0.1 (2026-11-10). Nothing here is implemented.**
"Decided" means the product owner said so. "Proposed" means it is a
recommendation that has not been confirmed. Anything under "Open questions" is
genuinely open: stop and ask rather than guessing.

---

## 0. Read this first: relationship to the existing machine

This repository began as a finished BASIC/LOGO home computer. Its `kernel/` lists
**multitasking, processes, user mode, paging, SMP, networking and POSIX
compatibility as non-goals**. This document proposes the opposite on purpose, for
a different kernel, so the two cannot share a kernel binary.

**Settled (2026-11-10, OD25):** the OS lives in this repository as a **new track**
(the S-series in `ROADMAP.md`). `CLAUDE.md`, `DESIGN.md` (D23, D24) and `ROADMAP.md`
have been amended accordingly.

- The home computer is finished and **keeps its non-goals**. Do not grow `kernel/`
  toward this design.
- New work goes in **new crates and a new kernel binary**. The existing kernel
  keeps booting BASIC and LOGO exactly as it does now.
- The existing pure crates are reused, not forked (see section 1).

### 0.1 Conventions we adopt unchanged from this repo

The repo already embodies the method this project needs. All of these carry over:

- Pure logic in `no_std`, `#![forbid(unsafe_code)]` crates, tested on the host.
  Hardware access only through traits. Every `unsafe` block has a `// SAFETY:`
  comment saying why it holds.
- Golden and scripted tests pin behaviour; write the test first, watch it fail.
- One milestone (or slice) per branch/PR. `main` always builds and boots.
- A roadmap with acceptance criteria; tick a box only when the criterion has been
  **verified** (for kernel work: booted in QEMU, not merely compiled).
- Real design choices go in a decision log. If something cannot be verified in the
  environment (no QEMU, say), say so plainly.
- `make check` is the gate before calling anything done.

---

## 1. Goals, non-goals, and what we reuse

### Goals

- A real (toy) operating system with the moving parts: virtual memory, preemptive
  multitasking on **multiple cores**, user/kernel separation, IPC, drivers,
  a filesystem, a network stack, a shell and userland, ported real software.
- A design in the Fuchsia/Zircon lineage (handles, rights, channels, userspace
  servers) because it is a good design and well represented in what the builders
  know, with a POSIX-ish layer for compatibility.
- **Verifiable by construction.** The primary artifact of the project is not the
  kernel; it is the harness that lets an AI build a kernel without a human reading
  every line (section 3).
- Demo-able milestones. Each stage ends in something you can run in QEMU and judge.

### Non-goals (for now)

- Running real workloads, real hardware, or anything business-critical.
- Self-hosting (compiling the OS on itself). "It will come, or it won't." Do not
  make choices that make it impossible, but do not pay for it.
- NUMA, heterogeneous cores, CPU hotplug, or more than a handful of identical harts.
- A novel research idea. The design is deliberately conventional-for-microkernels.
- Binary size (same as the existing project).

### What we reuse from the existing crates

| Existing crate | Reuse in the new OS |
|---|---|
| `basic` (`Io` trait) | Runs as a userland program: `Io` implemented over stdio handles. Golden transcripts become the userland test. |
| `logo`, `turtle`, `rv64` | LOGO as a userland program once a framebuffer service exists; `Canvas` over a framebuffer memory object. The JIT needs executable mappings (a VM feature to specify). |
| `console` | Line editor and ANSI helpers for the console server and shell. |
| `cartfs` (`BlockDevice` trait) | The block-device trait shape and the independent host-side disk tool pattern (`scripts/disk.py`). The floppy format itself is not the planned filesystem. |
| `kernel/src/virtio_blk.rs` | Reference for the modern virtio-mmio transport (QEMU needs `-global virtio-mmio.force-legacy=false`). The new driver is a userspace server. |
| `scripts/*_test.py` | The QEMU harness pattern: serial scripting, monitor over loopback TCP, `screendump`. |

---

## 2. Decision summary

The full log is in section 17. The shape of the system:

- **Kernel:** Rust microkernel, RV64, S-mode on OpenSBI, Sv39 paging, **multicore
  from the start** (boots on one hart first, with a big lock as a stepping stone).
- **Object model:** typed kernel objects accessed only through handles carrying
  rights. No ambient authority, no uids, no paths in the kernel.
- **Everything else is userspace servers** speaking protocols over channels:
  drivers, filesystems, the network stack, the console.
- **Namespaces:** per-process, a table of path prefix to directory handle,
  assembled by the launcher from a manifest and implemented in the libc.
- **Scheduler:** fixed-priority preemptive with a real-time band; policies are
  pluggable classes **selected at boot** (not live-swappable).
- **Network:** our own stack, written here. External tools and RFCs are fine as
  references and test oracles; third-party stack code is not.
- **Storage first step:** a memory-backed filesystem with BeOS-style indexed
  attributes. A real on-disk filesystem is a longer-term goal.
- **Compatibility:** `fork`, `mmap`, `posix_spawn`, signals are provided by the
  libc and a process server on top of kernel primitives. Room is left in the kernel
  design; they are not first-milestone work.

```text
 user mode   +---------+ +--------+ +--------+ +---------+ +-----------------+
             | shell & | | BASIC, | | memfs  | | net     | | console, driver |
             | programs| | Lua .. | | server | | stack   | | manager, launch |
             +----+----+ +---+----+ +---+----+ +----+----+ +--------+--------+
                  |  libc / native runtime (namespace table, fd table, POSIX shim)
                  +------------------ channels carrying bytes + handles --------+
 -----------------------------------------------------------------------------
 kernel      handles+rights | channels, sockets, ports | VMOs + address spaces
 (small)     processes, threads, jobs | scheduler classes | interrupt objects
             traps, timer, IPIs, TLB shootdown | debug console (panics)
 -----------------------------------------------------------------------------
 firmware    OpenSBI (M-mode): timer, IPI, remote fence, hart start, reset
 hardware    QEMU virt: RV64GC x N harts, PLIC (or AIA), virtio-mmio, fw_cfg
```

---

## 3. How the project is built (the process is part of the design)

### 3.1 Roles

- **Product owner (human):** defines what "works" in terms observable by running
  the system; chooses milestones; runs demos; makes the calls recorded as open
  questions. Reads code out of curiosity but is not a required reviewer.
- **Builder (AI):** implements a milestone.
- **Reviewer/adversary (AI, fresh context):** attacks the builder's work against
  the specification, not against the builder's intent. Writes torture tests and
  fuzzers; hunts invariant violations. Has no stake in the code.
- **Referee (not an agent):** oracles that neither agent controls (section 15).

### 3.2 Rules

1. **Acceptance tests exist before the code.** A milestone's acceptance criterion
   is a runnable check, written first.
2. **Independent oracles beat self-written tests.** If the same model writes code
   and tests, they can share a misunderstanding. Prefer tests whose expected
   output comes from outside: upstream suites, host tools, real clients, other
   implementations, an independent reference model.
3. **Core logic lives in host-testable `no_std` crates** behind narrow traits, so
   it can be tested with model-based/property tests, concurrency model checkers
   and Miri without an emulator.
4. **Always green.** A milestone is committed only when bootable and passing.
   No big-bang rewrites; every step is revertable to a working system.
5. **Decision log.** Every design fork the builder resolves is logged with
   alternatives rejected, so the product owner can skim, veto and redirect.
6. **Written invariants.** `INVARIANTS.md` (to be created in S0) states the rules
   every change must preserve: handle unforgeability, rights monotonicity, lock
   ordering, ownership of memory. Reviewers check against it.
7. **A readable syscall table.** `SYSCALLS.md` (S0) lists every syscall with
   preconditions, required rights, and error cases. It is both the contract and
   the one kernel artifact the product owner might actually read. Target about 50
   syscalls.
8. **Observable without a debugger.** Everything must be diagnosable from the
   serial console: structured panic output, structured test lines, QEMU's exit
   device for pass/fail. A fully headless boot-test loop, seconds per change.
9. **Say what could not be verified** (no QEMU, flaky, ran only in one mode).

### 3.2.1 Structured test output

Every OS-track kernel and test program reports through the serial port in lines that
start at column 0 with `##`; anything else on the port is free-form noise and is
ignored. The format (S0, OD28):

```text
##BOOT <name> <version>
##TEST <name> PASS
##TEST <name> FAIL <detail>
##PANIC <message>
##DONE pass=<n> fail=<n>
```

The verdict is QEMU's exit status, from the test finisher device at `0x100000`
(write `0x5555` to pass; `(code << 16) | 0x3333` to fail with status `code`), and the
lines must agree with it. The kernel formats the lines with the pure `kcore::testlog`;
`scripts/os_boot_test.py` parses them with its own independent code, and a
`fail-demo` build proves the harness really sees a failure.

### 3.3 Anticipated failure modes

- Shared blind spots between code and tests (rule 2).
- Heisenbugs: races, interrupt-masking errors, memory-ordering errors. Counter
  with deterministic replay, loom/shuttle on extracted logic, a lock-order
  checker, and stress under `-smp N` in both modes.
- Context drift on a codebase larger than any window. Counter with the docs above,
  small modules behind narrow interfaces, and a decision log.
- Environment stalls (QEMU virtio quirks, nondeterminism) eating days.

---

## 4. Platform

- QEMU `virt`, RV64GC, **N harts** (`-smp N`). OpenSBI (`-bios default`) boots the
  kernel in S-mode. Secondary harts are started through the SBI HSM extension.
- **Sv39** paging (512 GiB virtual, three levels). Kernel in the upper half. Use
  the two software (RSW) PTE bits for kernel bookkeeping such as COW. Use ASIDs.
- Interrupts: **AIA** (APLIC + IMSIC; QEMU `-machine virt,aia=aplic-imsic`, OD35), not the
  PLIC the home computer's memory map describes. Timer and IPIs through SBI first
  (OpenSBI already routes IPIs through the IMSIC); Sstc later if wanted. External device
  interrupts first matter in S6.
- Remote TLB flushes through the SBI RFENCE extension at first.
- Devices: **virtio-mmio** first (no PCI enumeration). PCI/virtio-pci later if
  realism is wanted.
- Discovery: the device tree OpenSBI hands over (`a1`). A small parser in a
  userspace device manager; the kernel needs only what it must (memory, harts,
  interrupt controller).
- Memory model: RVWMO is weak. Atomics and fences are confined to a small audited
  layer (section 7).

---

## 5. Kernel object model

### 5.1 Handles and rights

- A process owns a **handle table**: opaque handle value to (object reference,
  rights mask). Handle values include a generation counter so a stale or reused
  value fails instead of pointing at something new. Value 0 is invalid.
- Operations: close; duplicate (needs the duplicate right; rights can only
  shrink); replace with fewer rights; transfer through a channel.
- Objects are reference-counted with a type tag and destroyed when the last handle
  and the last kernel reference are gone. Each carries a signal mask threads can
  wait on.
- **No ambient authority.** Authority is holding a handle. Syscalls that touch
  hardware or other processes require a handle. Object-creating syscalls are
  permitted but charged to the caller's job (quota, policy). (Proposed; see Q9 on
  quotas.)

### 5.2 Object set (minimal, deliberately small)

| Object | Purpose |
|---|---|
| Channel | Bidirectional message pipe carrying bytes and handles. The one IPC primitive. |
| Socket | Byte-stream pipe (pipes, stdio, network socket endpoints). |
| VMO (memory object) | A chunk of memory that can be mapped, shared, snapshotted. The central memory abstraction. |
| Address space (VMAR) | A process's mappings of VMOs. |
| Process, Thread, Job | Containers; jobs form a tree for cleanup and limits. |
| Port | Wait set (epoll-like): register (object, signals, key), wait for packets. |
| Event / timer | Signalling and time. |
| Interrupt | A device interrupt line as an object (section 11). |
| Resource (root) | Privileged minting authority for device memory and interrupts; held only by early boot code. |

Plus a futex-style user wait/wake primitive so userspace locks avoid a syscall on
the fast path.

### 5.3 Channels

- Message-based (not a stream), bounded in size and queue depth, so one process
  cannot exhaust kernel memory. A message is bytes plus handles; the sender gives
  up the handles and they appear in the receiver's table.
- Closing one endpoint raises **peer-closed** on the other.
- **Bulk data does not go through channel messages.** It goes through shared VMOs;
  channels carry small requests and pass handles to larger buffers.
- A distinct **call** operation (write, then wait for the matching reply) exists
  from the start. It is the hook for priority donation to servers (section 8).

### 5.4 Signals and waiting

Each object exposes signals (readable, writable, peer-closed, plus user bits).
Waiting on one object is layered on the same mechanism as ports, so there is **one
carefully written block/wake path** and one place to get lost wakeups right.

### 5.5 Edge cases that need explicit tests

- Handles inside queued messages when a channel is destroyed (must be closed, not
  leaked).
- A channel endpoint sent over its own channel.
- A process dying with handles open: every object it held is released (leak check
  against allocator counts).
- Stale handle values after close/reuse (generation check).
- Rights escalation attempts through duplicate, replace, transfer and open.

---

## 6. Memory management

**Rule: every memory operation goes through the memory-object (VMO) interface,
never directly to frames.** That one rule keeps `fork`, `mmap` and pagers possible.

### 6.1 Layers

1. **Physical frames.** Frame allocator (buddy or free lists, per-hart caches) plus
   a **frame database** with per-frame ownership and reference counts. Refcounts
   exist from day one so sharing and COW are not a redesign.
2. **Kernel heap.** Slab-style allocator over frames, plugged in as Rust's global
   allocator. (The existing project uses the `linked_list_allocator` crate; see Q2.)
3. **Address spaces.** Sv39; a tree of regions mapping VMOs; enumerable and
   clonable.
4. **VMOs.** Lazily committed (allocate on first touch). Kinds via a **page-source
   slot**: anonymous, physical (device memory), and later pager-backed.

### 6.2 Fault path

The fault handler asks one question: *"memory object, give me a page for this
offset, for read or write."* Lazy commit, COW and (later) userspace pagers all
plug in behind that interface. Invariants checked in tests: no frame mapped by two
unrelated owners; W^X never both; refcounts conserved.

### 6.3 Room left for `fork` and `mmap` (not first-milestone work)

For `fork`: refcounted frames; COW snapshot children of VMOs (shared pages until
one side writes); a software PTE bit for COW; enumerate-and-clone address spaces;
create a process from a cloned space; thread creation with arbitrary initial
registers (so the child returns 0); handle-table duplication with rights masking.
Note: write-protecting the parent's pages for COW needs a **remote TLB shootdown**,
tying it to the SMP design.

For `mmap`: file-backed mappings come from the filesystem server as a VMO.
`MAP_PRIVATE` maps a COW child; `MAP_SHARED` maps it directly. The server's cache
is that same VMO so `read`/`write`/`mmap` are coherent. Early: read-only private
mappings (executables). Later: a pager protocol with dirty-page tracking and
writeback (`msync`).

There is no kernel `fork` and no kernel `exec` initially. Native programs use
explicit spawn; the libc shim composes `fork` from primitives for POSIX programs
(section 14).

---

## 7. SMP and concurrency rules

Multicore is a **firm goal**. Commitments made now because retrofitting them is
painful:

- **Per-hart everything:** run queues, timers, current-thread pointer, scratch
  state, reached through a register. Global state takes explicit locks.
- **A written lock discipline** in `INVARIANTS.md`: a global lock ordering,
  interrupts disabled while a spinlock is held, and a **debug-build lock-order
  checker** (lockdep-style).
- **IPIs and TLB shootdowns from the start** (SBI IPI and RFENCE).
- **One wait/wake primitive**, written once and tested hard (lost wakeups).
- Atomics and fences in a small audited layer; elsewhere Rust's `Send`/`Sync`
  rule out many data races at compile time.
- **Bring-up ladder:** (1) correct locking on one hart; (2) secondary harts with
  **one big kernel lock**; (3) per-object locks on hot paths (scheduler,
  channels); (4) finer only where measurements justify it. Each rung leaves the
  system green. A microkernel's small kernel makes a coarse lock viable for long.
- **Skip:** NUMA, hotplug, heterogeneous cores.

### Testing concurrency

Structure core logic (scheduler queues, channel implementation, handle tables,
wait queues) as `no_std` crates that also build on the host, where they are
explored with **loom/shuttle** (exhaustive or randomised interleavings) and
**Miri**. In QEMU, run `-smp 4` under stress in **both** normal mode and
deterministic **icount** mode. Icount serialises vCPUs in a fixed interleaving,
which reproduces bugs but can hide truly parallel races, so run both.

---

## 8. Threads and scheduling

- **Thread object:** user register context, **its own kernel stack** (so kernel
  code can simply block mid-function; easier to write and review than
  continuation-passing), state, priority, owning process. A context switch is a
  few dozen lines of assembly (save callee-saved registers, swap stacks).
- **Policy (decided):** fixed-priority preemptive, round-robin within a level,
  with a **real-time band** above the normal band. This is predictable, easy to
  test, and gives a BeOS-like responsive feel.
- **Pluggable, selected at boot (decided):** the kernel keeps mechanism (context
  switch, locking, IPIs, migration, timers, block/wake); policy is a **scheduling
  class** behind a narrow interface (enqueue, dequeue, pick next for this hart,
  tick, choose a hart for a waking thread, yield, priority change, exit). Classes
  have strict precedence (real-time, then normal, then idle). The class is
  chosen at boot via the command line or a build feature. **Live swapping is
  explicitly out of scope.** A userspace policy service gated by a privileged
  scheduler handle is a possible later extension; leave room, do not build it.
- **Guardrails:** each runnable thread is in exactly one place (debug assertions);
  the kernel validates that the policy's choice is a thread it actually holds; a
  watchdog falls back to the default policy if a runnable thread is starved.
- **SMP shape:** per-hart run queues each with their own lock; wakeups pick a hart
  by affinity and load and poke idle harts with an IPI; migration locks two queues
  in a fixed order; idle harts `wfi`; periodic tick first, tickless later if wanted.
- **Priority inversion:** a high-priority client blocked on a low-priority server.
  The `call` primitive (section 5.3) is where priority donation will attach.
  Not implemented at first; the hook is.
- **Easy to get wrong:** killing a thread running on another hart (needs an IPI);
  thread-object lifetime (reference counting); lost wakeups.
- **Verification:** the scheduler core is a host-testable crate driven by a
  simulated clock. Properties: the highest-priority runnable thread runs (single
  hart); no starvation within a level; threads are never lost across random
  wake/sleep/priority sequences. A **conformance suite** that every policy must
  pass lets an alternative policy be dropped in safely.
- **Demo:** a high-priority thread stays responsive while low-priority CPU hogs run.

---

## 9. Processes and program loading

A process is a container: address space + handle table + threads. Creation is
**explicit construction**:

1. Create an empty process (get handles to the process and its address space).
2. Create VMOs, fill them with the program image, map them.
3. Create a thread with an entry point and stack.
4. Start it, passing exactly **one** handle: a **bootstrap channel**. Everything
   else arrives as a message.

- **The kernel does not parse executables** except for a minimal built-in loader
  for the first process. A userspace launcher parses ELF and builds processes, so
  the executable format stays out of the kernel. Static binaries first; dynamic
  linking later.
- **Boot:** the bootloader/firmware path provides a boot image (an initial
  ramdisk) as a VMO. The kernel starts `init` from it. `init` receives root-level
  handles (boot image, root job, debug console, the root resource for device memory
  and interrupts) and starts the first services: a ramdisk filesystem, then the
  launcher, then the rest.
- **Startup message** (the "process args"): arguments, environment, a namespace
  table (path prefix to directory handle), and other handles each tagged with a
  role (stdout, main thread, ...). The libc consumes it at startup. This is where
  least privilege becomes real: a child gets exactly what its launcher chose.
- **Jobs:** a tree of processes for killing subtrees and applying limits. A minimal
  version early, mostly for cleanup.
- **Death:** all handles close, the address space is torn down, a **terminated**
  signal fires on the process object with the exit code.
- **Rights on a process handle:** start, kill, and read/write memory are separate
  rights. Debugger-level access to another process's memory is a distinct capability.
- **POSIX compatibility (later; see section 14):** `fork` is composed in the libc
  (create a process, snapshot the parent's mappings, duplicate chosen handles,
  start a thread with the parent's registers); `exec` is either spawn-and-replace
  or a kernel operation that clears a process's mappings and threads but keeps the
  process object (keep this open); PIDs, `wait` and signals live in a userspace
  **process server** mapping PIDs to handles. POSIX signals are the ugly part
  (interrupting a thread arbitrarily); defer them.

---

## 10. Namespaces

Plan 9-like per-process namespaces, but capability-based.

- Each process has a **namespace table**: prefix to directory handle (or, later,
  an ordered list of handles; see unions below). It lives in the **userspace libc**,
  not the kernel. The kernel has no notion of paths. Authority is the handles, so a
  holder of a handle can use it whether or not it appears in the table.
- The libc implements POSIX `open()` by longest-prefix match, then sending
  `Open(relative-path, flags/rights)` to that directory's server. The server walks
  the components. Crossing a mount forwards the remainder to the other server. The
  client supplies a fresh channel endpoint with the request; the final server binds
  it, so data flows directly between client and owner. Requests can be issued on the
  new endpoint before the open completes (pipelining).
- The new handle can have at most the rights of the directory handle used. **`..` is
  forbidden** in paths, so a directory handle is a true sandbox boundary.
- **Differences from Plan 9:** the namespace is not kernel state; authority is
  handles, not identity and permission bits; a process can only build namespaces
  from handles it holds; the protocol is channel-based and can carry handles (9P
  cannot).
- **Union directories (decided: later, wanted):** several directories presented as
  one (so `/bin` can combine system, personal and project tools). Implement as an
  ordinary **union server** that fronts several directory handles, so it composes
  with rights attenuation and the powerbox shell and needs no kernel or libc
  special case. Semantics: lookups try layers in order and only "not found" falls
  through; listings merge with the first occurrence winning; creates go to a
  designated writable layer; deleting a name that exists in a lower layer is
  unsupported (as in Plan 9). Requirement on the core design now: the file protocol
  treats directories as first-class forwardable objects.
- **9P is an adapter, not the native protocol.** It assumes identity-based attach
  and cannot pass handles. A 9P bridge is still valuable: QEMU can share a host
  directory into the guest over virtio-9p (to be verified on the RISC-V `virt`
  board), and Linux's 9P client can test our file server.
- **Optional extra:** a synthesized `/net` file server on top of the stack, so shell
  scripts can use TCP with plain redirection.

---

## 11. Interrupts and the driver model

- **In the kernel:** the trap handler, timer, IPIs and interrupt routing. Plus a
  minimal in-kernel debug console that works even if every userspace driver is dead.
- **Interrupt objects:** each interrupt line is an object. When it fires, the kernel
  masks it at the PLIC and raises a signal; a userspace driver thread wakes,
  services the device, then acknowledges (which unmasks). Masking until
  acknowledgement prevents interrupt storms from level-triggered devices. Driver
  interrupt threads run in the real-time band for low latency.
- **Device memory objects:** the root resource can mint a VMO backed by a specific
  physical range (a device's MMIO registers). A driver's whole authority is its MMIO
  VMO, its interrupt object and its DMA buffers.
- **DMA is the honest weak spot.** QEMU's virtio devices access guest physical
  memory directly and there is no IOMMU by default. Userspace drivers contain
  crashes (a stray pointer cannot corrupt the kernel) but not malice. DMA buffers are
  **pinned VMOs** that expose physical addresses only to holders of a specific
  handle. A virtual IOMMU is a possible later tightening.
- **Driver manager** (userspace): parses the device tree, creates device nodes,
  launches the matching driver with only its handles. Drivers publish services over
  channels (a block protocol, a network protocol). Killing and restarting a block
  driver makes a good demo of why the architecture exists.
- **Moving data:** control over channels; bulk data through shared buffers and
  queues registered with the driver, not copied through messages.
- **Cost accepted:** extra context switches per I/O compared with in-kernel drivers.
- **Verification:** the interrupt object's state machine is host-testable. Devices:
  write a pattern to the virtual disk, read it back, compare checksums; ping for
  networking. Milestones: userspace serial echo, block round trip, ping reply.

---

## 12. Filesystem and storage

- **A userspace server** speaking a file protocol over channels: open, read, write,
  attributes, directory listing, link, unlink, rename, sync, and "give me the
  backing memory object". Internally three layers: block cache, on-disk format,
  protocol front end. The format logic is a host-testable crate against a
  block-device trait.
- **Decided for now: a memory-backed filesystem.** It serves the boot image, `/tmp`
  and everything else until a disk filesystem exists. No persistence initially.
- **BeOS-style indexed attributes (decided: wanted).** Extended attributes on files
  plus indexes that support database-style queries, a `query` command, and
  attribute-aware tools. Built in the memory filesystem first and format-independent;
  a later disk format must persist them.
- **Symlinks (decided):** stored as the format allows, but the server **never
  follows them itself**. It returns the target text and the libc resolves it against
  the process's own namespace, preserving the sandbox property.
- **Longer-term: a real on-disk filesystem.** Candidate ladder: an ext2-compatible
  format first (oracles: `mke2fs -d` to build images, `e2fsck` to verify, Linux can
  mount the image; weaker as a journal, strong on semantics), and possibly a
  copy-on-write or log-structured design of our own later. Even then, test **crash
  consistency** by simulating power loss at every write boundary in the host
  harness and running the checker on what is left.
- **File cache and `mmap`:** the server's cache is made of VMOs, handed out through
  the backing-memory request (section 6.3).
- **Concurrency:** a single-threaded event loop per mount first; locking inside a
  multithreaded server is added later and tested hard.

---

## 13. Networking

- **Our own stack, written here (decided).** RFCs, packet captures, Wireshark,
  packetdrill-style tools and real clients are welcome references and oracles.
  Copying or vendoring a third-party stack is not. (smoltcp may be used as a test
  peer on the host.)
- **Shape:** virtio-net driver (userspace) to a network stack server (Ethernet,
  ARP, IPv4, ICMP, UDP, TCP, a DHCP client, a DNS resolver) to applications through
  a sockets interface in the libc. Network access is a **handle to the stack's
  socket service**; a process without it cannot name the network at all.
- **Sockets are handles.** Each socket is an object you talk to. Byte streams use
  the kernel socket object. Start with data copied through it; move to shared ring
  buffers when throughput matters.
- **QEMU networking caveat:** user-mode networking provides DHCP and DNS and NAT out
  and `hostfwd` for inbound TCP/UDP. **Inbound ping from the host does not work in
  user mode** (guest-to-gateway ping does). Capture traffic with
  `-object filter-dump` for inspection in Wireshark.
- **TCP is the hard part:** the state machine, retransmission timers, congestion
  control (start with slow start and AIMD), window scaling, delayed ACKs. Tests in
  layers: fuzzed parsers; scripted packet tests (inject exact segments, assert exact
  responses); a **simulated lossy/reordering/duplicating link** with the property
  "bytes in equals bytes out, no stalls"; interop through QEMU with `curl`
  transfers and checksums.
- **Milestones:** frame echo; ARP and guest pings its gateway; UDP echo, then DHCP;
  TCP client fetches a file from a host `python3 -m http.server` and the hash
  matches; TCP server answers `curl localhost:8080` from the host through `hostfwd`,
  ideally serving files from the memory filesystem; the same under a lossy link;
  DNS.

---

## 14. Userland

- **Two runtime layers.** A **native runtime** in Rust (`no_std` plus an allocator)
  wrapping syscalls, channel protocols and the startup message, used by services
  and tools. A **C libc** (stdio, strings, malloc, a file-descriptor table over the
  file protocol, sockets, pthreads on futexes, the `fork`/`exec` shim) for porting
  existing software. A Rust `std` port is possible but fiddly: defer.
- **The shell:** POSIX-ish syntax with a few builtins, acting as a **powerbox**.
  Running a program grants it stdio and a working directory plus whatever you pass
  explicitly (e.g. `cmd --with /data/project`). **Confined by default** (decided, Q8); trusted programs listed in a
  config file get broader namespaces.
- **Console server:** line editing, echo and key input (reusing the `console`
  crate). Ctrl-C simply terminates the foreground job; real signal semantics later.
- **Component manifest:** `init` reads a plain-text manifest declaring what each
  service uses and offers and builds each process's namespace from it. The Fuchsia
  component idea at toy scale.
- **Introspection:** a `/proc`-like service of synthesized files so `ps` and `top`
  are plain file reads.
- **Utilities:** `ls`, `cat`, `cp`, `mv`, `rm`, `mkdir`, `ps`, a tiny editor and
  pager, a network fetch tool, `query` for attribute indexes.
- **The BASIC machine** returns as an ordinary userland program (the `basic` crate
  over stdio-backed `Io`); LOGO when a framebuffer service exists.
- **Graphics** (virtio-gpu and input devices, a compositor): a separate later topic.
- **Ported software as oracles**, in rising difficulty: Lua (tiny, plain C, a good
  libc test), a small HTTP server, SQLite (a brutal test of file semantics and
  `mmap`), one day a small C compiler.

---

## 15. Verification strategy

| Layer | What | Notes |
|---|---|---|
| 1. Host unit and property tests | Pure `no_std` crates: frame allocator, VM bookkeeping, handle table, channel state machine, scheduler core, filesystem format, TCP state machine, parsers | Model-based: random operation sequences compared against a trivially simple reference model |
| 2. Concurrency checking | loom/shuttle, Miri, on extracted core logic | Interleaving exploration |
| 3. Golden and scripted tests | Shell sessions (expect-style), protocol transcripts, BASIC goldens running as a userland program | Reuses the repo's golden-test habits |
| 4. QEMU boot tests | Serial scripting, structured test lines, QEMU exit device for pass/fail | Extend `scripts/*_test.py` |
| 5. Stress | Random channel/VMO/process churn, TCP under a hostile link, `-smp 4` in normal and icount modes | Debug lock-order checker on |
| 6. External oracles | `curl`, packet captures, upstream test suites, ported software, host tools (`e2fsck` later) | Not written by the builder |
| 7. Fuzzing | ELF loader, packet parsers, device-tree parser, message decoding | Parsers must not panic on hostile input |

Invariants worth encoding directly as property tests: handles cannot be forged;
rights never increase through duplicate, replace, transfer or open; `..` cannot
escape a directory handle; no frame mapped by two unrelated owners; W^X; refcounts
conserved; killing a process leaves no leaked objects.

---

## 16. Milestone ladder

Provisional crate names: `kcore` (pure kernel logic), `abi` (syscall numbers and
message formats shared by kernel and userland), the hardware-facing kernel binary,
`rt` (native runtime), `libc`, and one crate per server (`svc-memfs`, `svc-console`,
`svc-blk`, `svc-net`), plus `launcher`, `init`, `shell`. Names are provisional until S0 fixes them. The OS kernel gets its own directory with its own `.cargo/config.toml`, like `kernel/` (proposed; S0 confirms). Milestones are prefixed **S** to avoid colliding with the existing
M and L series. Each milestone ends in a demo and has a runnable acceptance check.

- **S0: Foundation.** Workspace for the new crates; host test setup including
  loom/shuttle/Miri; a stub kernel in QEMU that prints structured test lines and
  exits through the test finisher with a pass/fail code; `INVARIANTS.md` and
  `SYSCALLS.md` skeletons. *Acceptance:* a host test run and a headless boot test
  both gate `make` targets.
- **S1: Memory.** Sv39 on, frame allocator with refcounts, kernel heap, page-fault
  handler. *Acceptance:* model-based tests agree with the reference on the host;
  in QEMU, map/unmap/fault sequences return allocation counts to baseline.
- **S2: Threads and scheduling (one hart).** Context switch, timer preemption,
  fixed-priority plus real-time band, the single block/wake path. *Acceptance:* a
  high-priority thread stays within a latency bound while CPU hogs run; the
  scheduler conformance suite passes.
- **S3: User mode, syscalls, handles, channels, VMOs.** Minimal kernel ELF loader
  starts `init` from the boot image. *Acceptance:* a user test suite passes; each
  syscall's error cases are covered per `SYSCALLS.md`; killing a process leaks no
  objects.
- **S4: First interactive system.** Launcher, startup message, memory filesystem
  server, console server, shell with builtins and external commands, pipes via the
  socket object. *Acceptance:* a scripted shell session passes (`ls`, `cat`, `echo`,
  a pipeline); Ctrl-C kills the foreground job.
- **S5: SMP.** Secondary harts via SBI HSM, per-hart queues, IPIs, TLB shootdown;
  big lock, then fine-grained on hot paths. *Acceptance:* every earlier suite passes
  with `-smp 4` in normal and icount modes; a long stress run reports no lock-order
  violations and no leaks.
- **S6: Storage drivers.** Driver manager, userspace virtio-blk driver, block
  protocol. *Acceptance:* write/read/compare pattern; killing and restarting the
  driver is survivable.
- **S7: libc and ported software.** C libc subset, threads on futexes, BASIC as a
  program, Lua port. *Acceptance:* BASIC goldens pass as a userland program; a Lua
  test script passes.
- **S8: Network, lower layers.** virtio-net, ARP, ICMP, UDP, DHCP. *Acceptance:*
  guest pings its gateway; UDP echo through `hostfwd`.
- **S9: TCP and HTTP.** *Acceptance:* hash-verified fetch from a host HTTP server;
  host `curl` against an in-OS HTTP server serving memory-filesystem files; the
  lossy-link suite passes.
- **S10: Namespaces and attributes.** Indexed attributes and `query` in the memory
  filesystem; union server; manifest-driven confined-by-default shell. *Acceptance:*
  scripted attribute queries; a union-backed `/bin` demo.
- **S11: Compatibility.** `fork` shim with COW snapshot VMOs, `mmap` private and
  shared, `posix_spawn`, a signals subset, pager-backed VMOs. *Acceptance:* ported
  software that needs `fork` runs.

**Longer-term backlog:** a real on-disk filesystem (ext2-compatible, crash-injection
tested), the 9P bridge, graphics and input, DNS, scheduler alternatives against the
conformance suite, capability revocation, quotas, live-image niceties, self-hosting.

If you give up early, each rung is still a finished artifact: rung 4 is a working
shell on a preemptive kernel; rung 9 is a machine that serves a web page.

---

## 17. Decision log

| # | Decision | Status |
|---|---|---|
| OD1 | All code is AI-written; the human is product owner (defines "works", picks milestones, runs demos) | Decided |
| OD2 | Rust, `no_std`, RV64 on QEMU `virt`, OpenSBI, S-mode | Decided (carried over from the repo) |
| OD3 | Fuchsia/Zircon-style capability microkernel: typed objects, handles with rights, channels, userspace servers | Decided (product owner: "leaning to a Fuchsia-like model") |
| OD4 | Multicore is a firm goal; design SMP-ready from day one; bring up on one hart first with a big lock | Decided |
| OD5 | Scheduler: fixed-priority preemptive, real-time band; classes pluggable but **selected at boot**, no live swap | Decided |
| OD6 | Per-thread kernel stacks | Proposed |
| OD7 | Sv39, refcounted frame database, lazy-commit VMOs, page-source slot, all memory through VMOs | Proposed |
| OD8 | Leave room for `fork` (COW snapshots, clonable address spaces) and `mmap` (backing-memory VMOs, later pager), but do not build them first | Decided |
| OD9 | Kernel does not parse executables except a minimal loader for the first process; a userspace launcher loads programs | Proposed |
| OD10 | One bootstrap handle plus a startup message; manifest-built namespaces | Proposed |
| OD11 | Namespaces are Plan 9-like but live in the libc; authority is handles; `..` forbidden | Proposed (discussed and accepted in principle) |
| OD12 | Union directories wanted, implemented later as a union server; protocol must not preclude it | Decided |
| OD13 | 9P as an adapter/bridge, not the native file protocol | Proposed |
| OD14 | Userspace drivers over interrupt objects and device-memory VMOs; virtio-mmio first; DMA is trusted-driver | Proposed |
| OD15 | Storage first step: a memory-backed filesystem; a real filesystem is longer-term | Decided |
| OD16 | BeOS-style indexed attributes with a `query` tool | Decided |
| OD17 | Symlinks stored on disk but resolved by the client library against its own namespace; servers never follow them | Decided |
| OD18 | Our own network stack; references and test tools welcome; no third-party stack code | Decided |
| OD19 | Two userland runtimes: native Rust runtime and a C libc; Rust `std` port deferred | Proposed |
| OD20 | Shell is a POSIX-ish powerbox, **confined by default** with a trusted-program config | Decided |
| OD21 | Self-hosting is not a goal for now | Decided |
| OD22 | Skip NUMA, hotplug, heterogeneous cores | Decided |
| OD23 | A kernel socket (byte stream) object is in the minimal object set | Proposed |
| OD24 | Independent adversarial reviewer and external oracles are part of the process | Proposed |
| OD25 | The OS lives in this repo as a new track (S-series) with its own kernel binary and new crates; the home computer keeps its non-goals; `CLAUDE.md`, `DESIGN.md` (D23, D24) and `ROADMAP.md` amended | Decided |
| OD26 | The OS track is launched as a separate kernel binary with its own `make` targets, not as a boot-menu entry on the home-computer kernel | Decided |
| OD27 | Small third-party utility crates (such as an allocator or `libm`) are allowed in the kernel and userland, each with its own decision-log entry; the kernel, network stack, filesystem and libc are written here. Test-time crates (loom, shuttle, proptest, Miri) assumed fine | Decided |
| OD28 | Structured serial test output (`##BOOT/##TEST/##DONE/##PANIC`) plus QEMU's test finisher for the exit status; a deliberately failing build proves the harness detects failure (section 3.2.1) | Decided |
| OD29 | S1 kernel heap is a fixed `.bss` region under the `linked_list_allocator` crate (as the home computer's kernel); user-visible memory comes from the refcounted frame database, not from it. A slab over frames can replace it later | Decided |
| OD30 | S1 kernel layout: linked in the Sv39 upper half at `0xffff_ffff_8020_0000`, loaded at `0x8020_0000`; a throwaway 1 GiB-leaf boot table in `_start` enables paging, then `kmem::init` builds the real table with the pure `kcore` code (4 KiB leaves, W^X per section) and drops the boot table. The direct map (`0xffff_ffc0_0000_0000 + pa`) covers only frame-database RAM and the device pages in use, never the kernel image | Decided |
| OD31 | RAM size is assumed to be 128 MiB (`abi::layout::RAM_SIZE`; the boot test passes `-m 128M`) until the device manager parses the device tree (S6) | Decided (limitation) |
| OD32 | Scheduling classes are chosen at build time by a Cargo feature (`sched-round-robin`; default fixed-priority), which satisfies "selected at boot" until there is a command line to read; the class interface lives in `kcore::sched::SchedClass`, the core keeps all mechanism and validates the policy's choices | Decided |
| OD33 | Priorities 0 (idle), 1-31 (normal), 32-63 (real-time); a tick is 1 ms (SBI timer) and a time slice is 10 ticks; a thread preempted by a better one returns to the front of its level, one whose slice ends goes to the back | Decided |
| OD34 | The single block/wake path is `Sched::block_current`/`wake` with a one-shot wake token (a wake that races ahead of the block is not lost; wakes do not queue); sleeping is a timer plus that path; wait queues are built on it. S2 kernel threads have heap-allocated 32 KiB stacks with a canary checked at every switch; the kernel heap is interrupt-safe because the tick allocates | Decided |
| OD35 | External interrupts use AIA (APLIC + IMSIC), not the PLIC. Verified on QEMU 11.1.3 (local) and QEMU 10.2.1 with OpenSBI 1.8.1 (CI): the machine exposes `smaia`/`ssaia`, OpenSBI uses the IMSIC for IPIs, and the OS kernel passes its whole boot test with `aia=aplic-imsic` (`make os-boot-test-aia`, also in `make check` and CI). The driver-facing interrupt object stays independent of the controller. The home computer is unchanged (PLIC default) | Decided |
| OD36 | Syscall ABI: `ecall` from U-mode, number in `a7`, arguments `a0`-`a5`, status in `a0` (0 or a distinct negative code, no errno), results in `a1`+; handles are 32 bits (generation << 16 | slot + 1, 0 invalid); user pointers are validated and copied by the kernel, never read through `sstatus.SUM`; full table in `SYSCALLS.md` | Decided |
| OD37 | Kernel objects are reference counted (handles and in-flight message entries hold references). Reference counting cannot free cycles of channel endpoints queued in each other, so the kernel sweeps by reachability from all processes' handle tables when a process dies; the sweep is pure logic in `kcore::object` and tested | Decided |
| OD38 | S3 is built in slices: S3a pure handle table, object registry, channels and events with model tests; S3b user mode, syscall entry, ELF loader, `init` embedded in the kernel image, user-level test suite (user programs are Rust `no_std` on a small `rt` crate); S3c the remaining objects and process-death leak checks | Decided |
| OD39 | Object garbage is bounded by sweeping under pressure (`Registry::needs_sweep`, checked before object creation) as well as at process death; per-endpoint queue limits and a global object cap are stop-gaps until per-job quotas (Q9) exist, and the docs say so | Decided |
| OD40 | S3b user programs are Rust `no_std` crates (`rt` runtime, `init` the boot-time test suite) built for `riscv64imac-unknown-none-elf` (no FPU: threads save no FP state, INVARIANTS C7), linked at `0x1_0000`, embedded in the kernel image by `include_bytes!`; the kernel's ELF loader accepts only static ET_EXEC RISC-V ELF64, refuses W+X segments and executable stacks, and maps segments as lazily committed VMOs | Decided |
| OD41 | Traps from user mode switch stacks with the `sscratch` swap: `sscratch` is 0 while in the kernel and the thread's kernel stack top while in user mode; the trap frame lives on that kernel stack so a thread can be preempted or block in the middle of a syscall. Each process root page table copies the kernel's top-level (upper half) entries; teardown frees only the lower half | Decided |
| OD42 | User address layout: lower half from `0x1000`, stack top `0x20_0000_0000` with 64 lazily committed pages and a guard; user pointers are copied through page-table walks (never `sstatus.SUM`); a user fault the kernel cannot resolve ends the process | Decided |
| OD43 | Blocking is built on one kernel-wide wait queue: every syscall that can change a signal wakes all waiters, and each waiter re-checks its own condition with interrupts off before sleeping again (no lost wakeups, no per-object wait lists yet). Waits carry an absolute deadline in ticks. `channel_call` is write + a wait for a transaction-id match; the priority-donation hook is the call itself. Every process starts with one handle, a bootstrap channel (handle value 1) whose other end the creator keeps | Decided |
| OD44 | Every sweep of the object registry uses roots from all handle tables **including the kernel's own** (`UserState::roots`), computed in the same interrupts-off step as the sweep; creating an object can sweep, so it also wakes waiters | Decided |

---

## 18. Open questions (stop and ask the product owner)

- **Q1 (resolved, OD26): How is the OS track launched?** A separate kernel binary
  with its own `make` targets. A boot-menu entry on the existing kernel was
  rejected: it would blur the home computer's non-goals.
- **Q2 (resolved, OD27): Third-party runtime crates.** Small utility crates are
  allowed in the kernel and userland (the repo already uses
  `linked_list_allocator` and `libm`), each with its own decision-log entry. The
  kernel, network stack, filesystem and libc are written here. Test-time crates
  (loom, shuttle, proptest, Miri) are assumed fine.
- **Q3 (resolved, OD35): PLIC or AIA.** AIA (APLIC + IMSIC). Sstc timers: still open, optional; the SBI `TIME` call works and is what S2 uses.
- **Q4: Pager-backed VMOs:** when (S11 as listed, or earlier if `mmap` pain shows up).
- **Q5: `exec` model:** spawn-and-replace vs a kernel "reset process" operation.
- **Q6: Executable mappings** for the LOGO JIT: a VMO/VMAR permission and policy.
- **Q7: Real filesystem timing and format** (ext2-compatible first, own design later?).
- **Q8 (resolved, OD20): Confinement default.** The shell is confined by default;
  trusted programs listed in a config file get broader namespaces.
- **Q9: Quotas and resource accounting:** how far to take job limits.
- **Q10: Capability revocation:** accept proxy-based revocation, or design more.
- **Q11: Entropy and time:** virtio-rng and a wall clock source.
- **Q12: A name** for the OS.

---

## 19. Risks

- Nondeterministic SMP bugs consuming disproportionate time. Mitigations in
  sections 3 and 7.
- Shared blind spots between code and tests. Mitigation: oracles and an independent
  reviewer.
- Scope: this is a large project; the milestone ladder is built so that stopping at
  any rung leaves a finished artifact.
- Userspace drivers under DMA without an IOMMU do not contain malicious drivers
  (documented, accepted).
- Context drift across long builds. Mitigation: the docs in section 3.2, small
  modules and the decision log.
