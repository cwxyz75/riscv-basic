# NET-DESIGN: an architecture proposal for the network stack (S8 and S9)

Status: **draft v1, 2026-11-14, a proposal for discussion; nothing here is decided or built.** The concrete contract for S8, with
every message, constant and acceptance check, is `NET-SPEC.md`; this file is the architecture and its reasons. v1 folds in the scheduler,
threads and service-directory work that landed after v0 (`SCHED-SMP-SPEC.md`, slices R, SD, DM, T) and three owner
settlements: doorbells are events, the threading model is left alone for now, and priorities are allowed for but not set
(section 15 lists exactly what changed). It is design only (no code
was written for it). It turns OD74's direction into something concrete enough to write an S8 spec from, and it
records what a survey of existing stacks suggests we keep and avoid. When the owner accepts parts of it they become
ODs (section 13) and `NET-SPEC.md` (the S8 slice spec, with its acceptance checks, written before any code).

Sources of constraint: `OS-ARCHITECTURE.md` section 13 and OD18, OD59, OD64-OD66, OD73-OD97; `R-SPEC.md`; `W-SPEC.md`;
`SD-SPEC.md`; `SCHED-SMP-SPEC.md`; `S6-SPEC.md`; `SYSCALLS.md`; `reviews/2026-11-14-design-review.md` (D4, D8, D10, D11, D12).

## 1. What the platform gives us, and what it does not

This is the box the design has to fit in. Each line is read from the specs, not assumed.

**IPC and memory primitives**

* **Channel**: messages up to 64 KiB and 64 handles; 128 queued messages and a byte cap per endpoint; `channel_call` with
  transaction ids; peer-closed is a signal. Copies twice (into the kernel queue, out of it).
* **Socket**: a byte stream, 64 KiB inbound buffer per end, copies through the kernel. **No call reads a socket's fill
  level, and user signal bits exist only on events** (D4). Both ends are charged to the job that created the pair.
* **Event**: 8 user signal bits, works across processes, can be bound to a port. This is our only cross-process *doorbell*
  (futexes are process-private, OD68). A shared-memory ring therefore needs an event or a channel to wake the other side.
* **VMO**: shared memory. Slice V is built: **the creator pays up front** and a stranger's mapping needs a committed range,
  so a recipient cannot fault on someone else's quota (OD81). A `COMMIT` right can be narrowed away.
* **DMA VMOs** (OD64): physically contiguous, at most 64 pages each, **4 MiB for the whole system**, committed at creation;
  the `DMA` right gates `vmo_phys_addr`, so a duplicate without it can map and touch the memory but cannot learn physical
  addresses. A driver gets only an MMIO slot, a DMA VMO and an interrupt (OD65); the supervisor keeps the handles across
  driver lifetimes and resets the device (T42).
* **Port**: after slice W, registrations woken by thread id, level-triggered by default, one-shot bindings with
  `port_rearm`, per-job binding quota. **One thread per port** until the wake-one baton exists (R-SPEC question 4, T60; it is task 21 of `SCHED-SMP-SPEC.md`,
  after S8).
* **Interrupts** (OD75): AIA, delivered to the hart that asked; plain `virt` has none and drivers poll.

**Service structure (slices R, SD, DM, before S8)**

* One service loop in `rt` (`Service` trait: `open`, `request`, `closed`; `Reply`, `Hold`, `Close`, `Violation`), one
  backpressure rule (hold the reply, poll on a timer, cut off after a timeout), one violation rule (close the
  connection, never the service).
* **Accounted connections** (OD77): a root connection per program, descendants inherit its account; limits per account.
* A **registry** names services (`/svc/net`); a program sees only the names in its view; a dead service leaves a stale
  entry and clients see `UNAVAILABLE` and retry (OD78). Restart strategies are *not* in R; they come with SD.
* **Where those slices stand now:** the service runtime exists (`rt::service`, with `other_event` for a non-channel wake
  source; **not done: the typed request/reply client**, which S8 needs) and its defaults are about 16 connections per account and a
  pool of 200; the registry service is built (`Publish`, `View`, `UNAVAILABLE`, the `PUBLISH` right) but **step 4, servers
  publishing themselves and clients reconnecting, is deliberately deferred until S8 needs it**, so S8 is the first real client
  of the directory; the device manager serves several clients and several devices (a net device kind is still to come).
* **Threads and scheduling** (`SCHED-SMP-SPEC.md`): `thread_spawn` with a kernel-provisioned stack is built (default 128 threads
  per launched program; thousands fit); each thread costs about 25 to 30 KiB, **charged to the job that creates it**; the
  scheduler has a real-time and a normal band run from shared queues under the big kernel lock. **Not before S8, and
  therefore not assumed:** per-hart run queues and the scheduler out of the big lock (tasks 11, 12, planned with the network stack
  as their load), wake-one on ports (21), donation through `channel_call` (22), direct hand-off (24), timer slack and one-shot
  timers (13), real-time budget enforcement (25), `thread_interrupt` (26) and `rt::looper` (27).
* **Time**: `clock_get` and deadlines are 1 ms ticks today; the move to nanoseconds is one flag-day change in task 6 of
  `SCHED-SMP-SPEC.md` (check its status when the S8 spec is written).
* **Priorities**: threads have a band and a priority bounded by their job (`max_prio`); the planned defaults are 20 for launched
  programs and an RT ceiling of 47 with a 20 ms in 100 ms budget for drivers started by `devmgr`. **There is no real-time
  budget enforcement and no launcher or `devmgr` policy yet**, so nothing about real-time behaviour can be tested today.
* **Jobs** charge objects and queued IPC bytes to the creator (OD59). A server's own heap is the server's problem, which
  is why OD74 makes the client create what holds its data.

**Things that do not exist and that this design must not assume**

* A scatter-gather or zero-copy path between a client and a server other than a shared VMO the client chose to share.
* A way for a server to learn that a peer has read from a socket (no fill level; only `WRITABLE`).
* `WRITABLE` precise for a given size (T28).
* Threads cheap to *run* in numbers (creation is cheap now, but wake-one, per-hart queues and hand-off come after S8), and
  threads that are charged to a *client*: a thread made inside `netd` is charged to `netd`'s job.
* Identities. Everything is a handle and, with accounts, a *set of connections*.
* A way for the shell to grant "the network" to a program (D10, slice SD).

## 2. Survey and what it teaches

Four parallel web surveys, 2026-11-14. **Confidence matters here:** only some pages were actually fetched (the Erlang `inet`
docs, the virtio 1.3 spec, the QEMU net page, the LionsOS paper, smoltcp's docs, parts of Fuchsia's FIDL). Everything marked
*(memory)* is background knowledge that was not checked, and a few fetches were rate-limited. Nothing below is load-bearing
without being re-verified when `NET-SPEC.md` is written (section 12 lists what to check).

### 2.1 Library stacks: smoltcp, lwIP, picoTCP, Zephyr

* **smoltcp** is the closest relative: no threads, no heap needed, **the caller owns the clock** (an `Instant` goes into every
  call; the stack arms no timers and only reports `poll_at`/`poll_delay`), sockets own caller-supplied ring buffers, and a
  packet is a single borrowed `&[u8]` for the length of one call (no chains). The driver seam is the `Device` trait with
  single-use **RxToken/TxToken** consumed through closures; `transmit()` returning `None` is the TX backpressure signal;
  devices compose as wrappers (fault injector, pcap writer, tracer, fuzz injector). Polling is split into bounded steps so a
  server loop cannot be starved by a receive flood. Known weaknesses: panics on a full fixed-size socket set, and TCP lacks
  SACK and timestamps.
* **lwIP** has two modes: a callback API with the host calling timers (`NO_SYS`), or one `tcpip_thread` owning everything with
  other threads messaging it. Its `pbuf` chains buy header prepending without copying at the price of complexity; static
  pools sized at compile time make exhaustion a tuning problem. Lesson: **thread-affinity rules on the raw API are a bug
  farm**, and a blocking call on the core thread stalls the whole stack.
* **picoTCP** (partly verified): a global tick and its own timer table; RX can be zero-copy with a notify when the stack is
  done with the buffer (the right ownership shape for returning ring slots); **TX always copies** because the frame is freed
  right after `send`.
* **Zephyr**: driver allocates from separate RX/TX pools and hands packets over by FIFO; exhaustion drops. A clear ownership
  rule with fixed pools.

### 2.2 Capability and microkernel systems

* **seL4 sDDF / LionsOS** (paper fetched): NIC driver, TX and RX *virtualisers*, optional copier; the **driver never maps
  payload memory**. Between components: four **single-producer/single-consumer power-of-two queues** (TX available/free, RX
  available/free) carrying *buffer offsets into a shared region, never data*; a producer signals **only on the
  empty-to-non-empty edge** (a consumer can ask to be told on non-full); overload means **drop at the NIC**; "all
  policy-independent state lives in shared memory" so a component restarts with a glitch; the protocols were model-checked in
  SPIN. Memory is static, so there is no dynamic accounting to learn from. The whole driver, mux and copier are a
  few hundred lines each. Clients link their own lwIP: the *stack is in the client*, outside the trusted base.
* **Genode**: driver, then `nic_router`, clients use a `Nic` session (raw frames); the IP stack is a library in the client
  behind a VFS socket plugin. **The client donates RAM and capabilities with the session request, so the client pays**, and
  the packet stream is a submit queue and an ack queue over a client-donated buffer, signalling on transitions. Crash: init
  restarts components, sessions break, clients reconnect.
* **Fuchsia netstack3** (partly fetched). **Correction to a premise of OD74**: the Fuchsia netstack *creates* the data
  `zx::socket` (the client calls `StreamSocket(...)`, then `Describe()`, and `Accept()` returns a new socket channel);
  **we found no Fuchsia path where the client supplies the socket**. What Fuchsia does show: a channel for control and a
  separate kernel byte pipe for stream data, nonblocking everywhere with the C library doing the blocking, the direct API
  discouraged and BSD semantics supplied by a shim, datagrams as messages, and a known pain (the pipe's buffer and the TCP
  buffer double-count, which muddles `SO_SNDBUF`). It also pays for the buffers itself. **So OD74's client-pays variant is
  our own invention, justified by OD59, not borrowed**, and it is the least-proven part of the design (section 7.4).
* **Redox** (`smolnetd` over a `network:` scheme) and **MINIX 3** (`inet` plus driver, restarted by the reincarnation server):
  one global stack with no per-client accounting; MINIX shows a **network driver restart is nearly invisible** because TCP
  and IP already tolerate loss, while the stack restarting loses its connections. Redox's single `network:` scheme made
  several interfaces awkward (a warning against a global singleton). Managarm and gVisor were noted *(memory)*; gVisor's netstack
  is an in-process library with a link-endpoint seam, which is smoltcp's `Device` again.

### 2.3 virtio-net and QEMU facts (spec fetched; flagged items need checking)

* With `VERSION_1` the header is always **12 bytes** (`flags, gso_type, hdr_len, gso_size, csum_start, csum_offset,
  num_buffers`), with or without `MRG_RXBUF`. A modern-only driver can hard-code it. `blkd` already requires modern
  virtio-mmio (version 2) and the `-global virtio-mmio.force-legacy=false` flag, so the transport is settled.
* Minimum features: `VERSION_1`, and `MAC`. **Do not negotiate** CSUM, GUEST_CSUM, any TSO/UFO/GSO, `CTRL_VQ`, `MQ`, `RSS`.
  Without CSUM the guest sends complete checksums with `flags = 0`; without GUEST_CSUM it receives complete or verified
  ones. The stack computes and verifies every checksum itself (which also makes the checksum code a fully testable pure
  function). Skip `MRG_RXBUF`: one chain is one frame; post receive buffers of at least 1526 bytes (use 2048).
* Queue 0 is receive, queue 1 is transmit; there is no control queue unless negotiated. A tx buffer must not be reused until it
  appears in the used ring; keep receive stocked or the device drops. Interrupt suppression is advisory, so a polling or
  batched driver sets "no interrupt" while draining, then clears it and **re-checks the ring to avoid a lost wakeup**.
* QEMU user networking (page fetched): guest 10.0.2.15, gateway and DHCP 10.0.2.2, DNS 10.0.2.3; pinging the gateway works,
  inbound ping does not (already in section 13 of the architecture); `hostfwd=udp::HOSTPORT-:GUESTPORT`. *(memory, verify)*:
  netmask /24, MTU 1500, slirp answers ARP for .2 and .3, `-object filter-dump,id=f0,netdev=n0,file=x.pcap` for capture,
  and IP fragments are not delivered reliably (so the design neither sends nor reassembles fragments in S8).

### 2.4 OTP and testing practice

* **Erlang's flow control** is the most transferable idea: `{active,false}` (owner pulls), `{active,once}` (one message then
  passive), `{active,N}` (a credit count, with a notice at zero), against the dangerous default `{active,true}` where a fast
  peer fills the owner's mailbox without bound. Send side: high and low watermarks and a busy state that suspends senders.
  **A socket belongs to its controlling process and closes when that process dies.**
* **Supervision.** Erlang's TCP state is not in a restartable process, but ours is; so a stack crash loses every connection and
  that has to be the stated contract, with an RST for segments that arrive for connections the new stack does not know.
  What may sit outside the crashing process: configuration, generation counters, a small ownership registry. Do not persist
  or replicate TCP state. A **restart-intensity limit** matters more for a stack than for most servers (a crash caused by
  traffic recurs when the traffic does), and Ranch's rule that connections are **temporary and never restarted** is the
  right default. The OTP agent recommended `rest_for_one` (driver, then core); section 9 explains why we deviate.
* **Testing** that suits a sans-IO, single-threaded core: scripted packet tests (packetdrill-style), a **simulated link with
  loss, reordering, duplication and delay driven by one seed**, deterministic simulation testing in the FoundationDB and
  TigerBeetle style, stateful property tests against a byte-stream model, parser fuzzing (a panic is a crash and a restart),
  pcap replay as a cheap regression corpus, differential testing used selectively.

### 2.5 What this adds up to

1. **Pure, clock-injected, I/O-free core** (smoltcp, and everything about testability).
2. **Offset rings, signalled only on edges, with a driver that never maps payload** (sDDF).
3. **Drop at the edge, count the drops, bound every queue** (sDDF, Zephyr, Erlang's watermarks).
4. **The client donates the memory** (Genode) is the model for OD74; Fuchsia is the model for *control channel plus data pipe*.
5. **A stack crash is a defined event; a driver crash is nearly invisible** (MINIX, Erlang).
6. **Per-connection process isolation is the OTP idea we do not copy** (section 8): our isolation is accounts, not processes.

## 3. Principles for this design

* **P1. Sans-IO.** The stack's logic is a pure `no_std`, `forbid(unsafe_code)` state machine: bytes in, bytes out, time as an
  argument. Hardware and kernel calls live in thin adapters. This is what makes the three-tier test plan possible.
* **P2. The client pays.** Memory that holds a client's data is created by the client's job. The server's own use is bounded
  per account and counted against it (OD59, OD77).
* **P3. Every queue is bounded and its full behaviour is written down in advance.** Receive: drop newest and count. Transmit:
  fail the enqueue. Datagram to a slow client: drop and count. Reply to a slow client: hold, then cut off (R's rule).
* **P4. Authority is a handle, and there are three of them**: the *socket service* (open endpoints), the *config capability*
  (change addresses, routes, read tables) and the *driver rings*. No ambient raw-packet or low-port power.
* **P5. One thread, one port per process.** Concurrency is state machines, not threads (section 8).
* **P6. Crashes are designed events** with a stated contract for what clients, the driver and the supervisor see.
* **P7. Diagnosable headlessly** (project rule): counters, a flight recorder and a structured `##` dump on crash.

## 4. The architecture

```text
                     ┌──────────────────────────── supervisor: netsup ────────────────────────────┐
                     │ holds: DMA pools, MMIO slot, ring VMOs, event pair, config blob, flight rec │
                     └──────┬──────────────────────┬──────────────────────────────┬───────────────┘
                            │ starts, restarts     │                              │
                     ┌──────▼──────┐  rings (VMO)  ┌▼──────────────┐  /svc/net    ┌▼─────────┐
   QEMU virtio-net ◄─┤   netdrv    ├══════════════►│     netd      ├◄════════════►│ clients  │ ping, udp echo,
                     │ virtio only │  + 2 events   │ stack server  │ endpoint     │ libc     │ later: libc sockets
                     └─────────────┘               │ (pure core)   │ channels     └──────────┘
                                                   └──▲──────────┬─┘
                                       /svc/net-config│          │ /svc/net (registered by netd once attached)
                                                   ┌──┴───┐
                                                   │dhcpc │  a client holding the socket service and the config cap
                                                   └──────┘
```

Roles:

* **`netsup`** (supervisor): obtains the virtio-net slot, the DMA VMOs and the interrupt from `devmgr` (slice DM adds the
  net device kind), creates the ring VMOs and the doorbell events, starts `netdrv`, `netd` and `dhcpc`, restarts them
  independently, and holds everything that must outlive a child (OD65's supervisor role, T42's reset rule).
* **`netdrv`**: the only code that touches the device. Speaks virtio-mmio v2, stocks the receive queue, moves frames between
  the virtqueues and the rings, applies the drop policy, reports link state. A few hundred lines; it does not parse
  packets.
* **`netd`**: the stack server: the pure core plus adapters (ring `Device`, service loop, timers). Registers `/svc/net`
  and `/svc/net-config` with the registry once attached.
* **`dhcpc`**: the DHCP client as an ordinary supervised program (section 6.3).
* **Clients** (`ping`, a UDP echo program for S8; the libc sockets shim for S9).

### 4.1 Why two processes and not one

Rejected: **one process that is both driver and stack** (smaller, no ring protocol). Under OD14 DMA is *trusted-driver*:
a process holding a DMA-capable device can make it write anywhere in physical memory, because there is no IOMMU. A single
process would put the parser of every hostile packet on the wire in the same address space as that power. Splitting them
means the code with kernel-equivalent authority reads no packets and the code that reads packets holds no device. The
price (a ring protocol and two doorbells) is the same one every surveyed microkernel system pays. The seam is a `Device`
trait, so a one-process build for a first ping is possible as a stepping stone, but the **target and the protocol are the
split** (section 12, Q1).

Rejected: **the stack as a library in each client** (sDDF, Genode, gVisor). It moves TCP state and buffers to the client's
budget, which is attractive here, but it needs a shared-address-resolution and port-allocation authority anyway, makes
`hostfwd`-style inbound delivery and a shared ARP/route table harder, and every C and Lua program would carry a stack.
Recorded as a possible later *mode* once there is a server to compare it against.

Rejected: **per-protocol servers** (ARP, IP, UDP, TCP as separate processes): the cost of crossing a boundary on every packet
buys fault isolation inside one failure domain that restarts together anyway.

## 5. The driver/stack data path

### 5.1 Memory

(`NET-SPEC.md` 5.1 and 4.3 fix the sizes, and one detail differs from the sentence below: only the root resource can create DMA VMOs, so
`devmgr` creates the three DMA VMOs and they are charged to its job, a recorded shortcut; `netsup` creates the ring VMO, the config blob and
the flight recorder.)

Two DMA pools (RX and TX), each one DMA VMO of up to 64 pages, which gives **128 slots of 2 KiB** (a 1514-byte frame plus the
12-byte virtio header fit with room). A third DMA VMO holds the virtqueues themselves (descriptors, avail and used rings: the
device reads them). One ordinary shared VMO holds the **index rings and a control header**. The system DMA budget is 4 MiB; this
uses about 0.6 MiB. All of it is created and committed by `netsup` (creator pays, slice V), so neither child can fault on
quota (OD81).

Handle rights narrow what each side can do:

| Region | `netdrv` | `netd` |
|---|---|---|
| RX pool | DMA right (knows physical addresses); does not map it | maps **read-only**; no DMA right, so cannot learn physical addresses |
| TX pool | DMA right; does not map it | maps read-write; no DMA right |
| virtqueues | maps read-write (owner) | none |
| index rings and header | maps read-write | maps read-write |

The driver never maps payload (sDDF), so a bug in it cannot corrupt or read a frame, and the stack cannot name physical memory.

### 5.2 Rings and ownership

Four single-producer, single-consumer rings of **slot numbers and lengths** (never pointers; power-of-two sizes), with
indices that only grow:

* **`rx_used`** (driver to stack): "slot s holds a frame of length n".
* **`rx_free`** (stack to driver): "I am done with slot s; post it to the device again".
* **`tx_ready`** (stack to driver): "slot s holds a frame of length n; send it".
* **`tx_done`** (driver to stack): "the device has finished with slot s".

A slot has exactly one owner at a time (device, driver queue, stack, or free list), which is a state the model tests can check
after every operation. The stack parses receive frames in place, **copying header fields out before acting on them** (no
double fetch), and builds transmit frames directly in a TX slot. The stack validates every slot number and length it
reads from a ring (a hostile or buggy driver gets the violation rule: the supervisor restarts it), and the driver does
the same in the other direction.

### 5.3 Doorbells

Two **events**, one per direction (decided by the owner, 2026-11-14: events, not channels; events are our only cross-process
wake object that carries no queued bytes, and futexes are process-private; `SCHED-SMP-SPEC.md` 5.7 now says the same). Following
sDDF, a producer signals **only when the ring went from empty to non-empty and the consumer has set its `waiting` flag**;
the consumer sets the flag, re-checks the ring, then sleeps (the classic way to avoid a lost wakeup, and the same trick the
virtio spec recommends for interrupt suppression). The `netd` loop binds the event to its port (one-shot binding and
`port_rearm`, from slice W, suit this). The driver's own wake source is the device interrupt (AIA, OD75) or, on a machine
with no controller, a poll on the timer tick.

### 5.4 Full queues, drops and counters

* No RX slots free: the device runs out of receive buffers and drops; `netdrv` counts "device would have dropped" where it can
  tell. `rx_used` full: drop newest, count. Both are visible in a **stats page** in the control header.
* `netd` out of TX slots: `Device::transmit()` returns "none", the core leaves the packet where it was (ARP-pending queue,
  an endpoint's send queue) or drops it, per the rule of the layer that holds it.
* **Nothing in the driver ever blocks on a slow stack.**

### 5.5 Link state and re-attachment

The control header carries `link_up` and a **driver generation number**. A small channel between `netd` and `netdrv`
carries `Attach{generation}`, `Detach` and `Link{up}`; its peer-closed signal is how `netd` learns the driver died.
On a new generation `netd` forgets every outstanding slot it believed was in flight, re-initialises its view of the rings and
treats the event like a cable pull: the core is told the link went down and up (ARP entries age out; TCP will retransmit).
Frames in flight when the driver died are lost, and that is fine.

## 6. The stack core

### 6.1 Crates (all `no_std`, `#![forbid(unsafe_code)]`, host-tested)

* **`netwire`**: views and builders over byte slices for Ethernet, ARP, IPv4, ICMPv4, UDP, DHCP and the Internet checksum.
  No allocation, no panics on any input (fuzzed), IPv6 left room for but not built.
* **`netcore`**: the interface (address, mask, gateway, MAC, MTU), the ARP cache with a bounded pending queue, IPv4 receive
  and transmit, ICMP echo (answer, and the client side), the UDP socket table, timers, per-account counters, and the
  `Device` seam. **No fragmentation, either way, in S8** (receive fragments are dropped and counted; a send that does not
  fit the MTU is TOO_BIG). TCP arrives in S9 as `nettcp` in the same core.
* **`dhcp`**: the DHCP client as a pure state machine (section 6.3).
* **`proto::net`**: the native wire protocol (section 7), next to `proto::blk` and `proto::file`.
* **`netsim`** (host only): the simulated link, clock and fault injection used by tests (section 11).
* Adapters in `init/src/bin`: `netd`, `netdrv`, `netsup`, `dhcpc`; the virtio queue logic that `blkd` and `netdrv` would
  both need is a candidate for one small shared pure crate (decide when writing the spec).

### 6.2 The seam and the loop

The core exposes, in smoltcp's spirit (an idea, not its code):

```text
core.ingress(frame: &[u8], now)               // one received frame, parsed in place
core.egress(dev: &mut impl Device, now) -> n  // build and queue outgoing frames, bounded
core.next_deadline() -> Option<Instant>      // when it next wants to run
trait Device { fn rx(&mut self) -> Option<RxFrame>; fn tx_slot(&mut self) -> Option<TxFrame>; }  // none = full
```

`Instant` is an opaque monotonic nanosecond count: the core never assumes a resolution, so the ABI's move from 1 ms ticks to
nanoseconds (task 6) is a change in the adapter alone, which converts today's ticks; the only timer is `next_deadline`, which
becomes the deadline of `port_wait`. The adapter is built on `rt::service`'s `Core`, with the ring doorbell arriving as its
`other_event` (the way `consoled` bridges its streams), and every call a client library makes carries a deadline (the
message-loop rule of `SCHED-SMP-SPEC.md` 5.8). The `netd` loop is bounded in every phase: drain at most *N* receive frames, serve at most *K* client
messages, run the core, flush at most *M* transmit frames, then wait on the doorbell, the service port and the deadline. A
receive flood therefore cannot starve client requests and the reverse.

### 6.3 DHCP

Recommended as its own program, **`dhcpc`**: it parses hostile input from the network (so crash isolation is worth having),
it needs two privileges an ordinary client must not have (sending from 0.0.0.0, broadcasting) which are rights on its
connection, and its output is exactly what the *config capability* exists for (set address, mask, gateway, DNS) which `ip`-like
tools need anyway. `netsup` stores the last lease in a config blob so a `netd` restart re-applies it immediately and `dhcpc`
renews instead of starting over. The state machine is a pure crate so it is tested against recorded exchanges and against
QEMU's own DHCP server (a real oracle).

## 7. The client-facing protocol (OD74 made concrete)

### 7.1 Shape

`/svc/net` is a service connection (an accounted connection, OD77, found through the registry, OD78). On it, requests are
`channel_call` request/reply with transaction ids. **Every endpoint is a channel pair created by the client**; the client keeps
one end and sends the other in an `Open` request. Whoever creates a conversation pays for both buffers (OD59), so the
queued bytes sit in the client's job, and the stack holds at most a bounded, accounted amount per endpoint.

```text
Service connection (call/reply):   Info | Open{kind, local addr/port, flags} + <endpoint channel end> | Close
Endpoint channel, client to stack: Send{cookie, dst addr, dst port, payload} | SetOption | Shutdown
Endpoint channel, stack to client: Recv{src addr, src port, payload} | Notice{state, error, dropped count}
```

S8 endpoint kinds: **Udp** and **IcmpEcho**. IcmpEcho is a ping socket in the Linux sense: the stack assigns the identifier
and delivers only matching replies, so there is no raw-packet authority and no identity needed (P4). S9 adds **TcpStream**
and **TcpListener**.

### 7.2 Datagrams and slow clients

* **A datagram to a client is one `channel_write` on its endpoint channel**; if the client's queue is full the write would
  block, and for UDP that means **drop, and count it** (reported in the next `Notice`). The client's own bounded, client-charged
  kernel queue is the mailbox bound, which is the property Erlang's `{active,true}` lacks, with no credit protocol needed.
  (T28's imprecise WRITABLE does not matter: we never wait on it.)
* **Sends are fire-and-forget** with a client cookie; the stack reports failures (no route, ARP failed, too big) as
  `Notice` messages naming the cookie. Replying to every send would double the traffic for the common case.
* **MTU 1500** gives a maximum UDP payload of 1472, which settles D4's worry that 64 KiB messages are tight.
* Ports are allocated by the stack for ephemeral binds; binding a specific low port or broadcasting are **rights on the
  connection**, granted by whoever builds the program's view (SD), not identities.

### 7.3 What survives of the OTP model here

The Erlang controlling-process rule falls out of capabilities: **an endpoint lives exactly as long as the client's channel
end**; closing it (or dying) makes the stack see peer-closed and free the endpoint. That is the OTP link and monitor with no
extra mechanism, and the kernel already does the cleanup when a process dies (OS-ARCHITECTURE 5.5).

### 7.4 The stream data path (S9; flagged now because it constrains S8's protocol)

OD74: the client creates the data socket and passes it in `Connect` or `Accept`, so the kernel's 64 KiB buffers are the
client's. Three honest problems, to be settled in the S9 spec, none blocking S8:

1. **No precedent** (Fuchsia, Genode, Redox all make the server pay). It is justified by OD59 alone; it needs a prototype.
2. **The stack cannot see how much the client has read** (no fill level). Either the kernel gains a small call (`socket_info`
   with free space, a tiny, safe addition decided at S9) or the stack holds at most one window per connection itself, counted
   against the account. The second needs nothing new and costs a bounded 16 KiB or so per connection.
3. **A retransmit buffer is unavoidable**: bytes already read out of the socket but not yet acknowledged must be kept by the
   stack. That memory is the stack's, bounded by the send window and by the account's connection limit. The design must state
   it so that nobody believes the client pays for everything.

### 7.5 Copies (S8 accepts them; the measurements decide S9)

Receive: device, then the RX slot (the only copy the device does), parsed in place, then one copy into a channel message
and one out of the kernel queue. Transmit mirrors it. OD74 and section 13 of the architecture already say to start with
copying and move to shared rings when throughput matters; this design leaves a place for them (an endpoint kind whose data
path is a client-created ring VMO, the Genode shape) without needing them for S8 or the first S9.

### 7.6 Endpoints in the service runtime's accounts

A channel per endpoint means every open socket is a connection in the server's table, so **slice R's defaults (about 16 connections
per account, 200 in the pool) would cap a program at about 16 sockets and the whole machine at 200**. The net service therefore
sets its own per-account and pool limits for endpoints, and the S8 spec must choose them (starting points: a few dozen per account,
a few hundred in all, each endpoint also charged its stack-side state). The accounts themselves are R's (OD77, OD84): a client's
root connection comes from whoever launches it, and every endpoint it opens is charged to that account. `rt` gains a small
client library for the service (typed requests with a mandatory deadline, reconnect on `UNAVAILABLE`), which is the typed
request/reply client slice R has not yet built.

## 8. Threads, and how far OTP goes

The question was whether an OTP-like model helps given our threading limits. The answer splits in two.

**What we take from OTP**

* **A supervision tree with restart intensity and explicit state ownership** (section 9).
* **Ownership by link:** socket lifetime tied to the owner's handle (7.3).
* **Receive pacing by the consumer**, not by the producer: the bounded client queue (7.2); credit-style `{active,N}` is kept as
  a possible later option if some endpoint kind needs batching.
* **A message-loop shape** for every process, which slice R's service loop provides.
* **Connections are temporary**: never restarted by the supervisor; the client reconnects.

**What we do not take (for now): a process or thread per connection.** Owner decision, 2026-11-14: leave the threading model
alone; T67 in `TIDY-UP.md` records that we want to reconsider it. Reasons as they stand after the scheduler work:

* Thread *creation* is now cheap and thousands fit, but each thread costs about 25 to 30 KiB of memory **charged to the job that
  makes it**, which for a thread inside `netd` is `netd`'s job, not the client's. A thread per connection would put the memory
  for every connection on the server's account, which is the opposite of principle P2. Only a thread made in a *client's* job
  (a client-side library) would keep that rule.
* Running many threads cheaply needs wake-one on ports (task 21), per-hart run queues and the scheduler out of the big kernel
  lock (tasks 11 and 12) and direct hand-off (task 24), all planned *after* S8, the first two with this stack as their load.
* TCP connections share tables and timers, so a per-connection actor needs the shared structure anyway.

What OTP's per-connection process buys is **fault isolation**, and we get it by a different route: each connection is a state
machine whose faults are *program errors we fuzz and model-test away*, the account bounds what any one client can consume, and
a crash is a restart of a supervised process, not a hang. The core's interface (events in, actions out) means a per-connection or
per-account actor could later wrap a core instance without changing its tests; the question to ask at S9, once tasks 11, 12 and 24
exist, is whether front-end loopers (`rt::looper`) in client-charged threads would pay for themselves.

**SMP:** `netdrv` and `netd` are separate processes, so they run on different harts without any new locking in the kernel.
Inside `netd` there is one thread; if one thread ever limits throughput, the measured answer comes after tasks 11, 12 and 24, which
use this stack as their load (section 11).

### 8.1 Priorities

Owner decision, 2026-11-14: **the design allows the driver to run in the real-time band, but S8 does not set it.** There is no
real-time budget enforcement before task 25 and no launcher or `devmgr` policy, so a real-time driver could not be tested
and a runaway one would not be bounded. So:

* `netdrv`, `netd` and `dhcpc` start at the default priority of any launched program. Their priority comes from the job policy at
  start, never from a constant in the code, so making `netdrv` real-time later is a policy change (the planned ceiling for a
  `devmgr` driver is 47 with a 20 ms in 100 ms budget), not a design change.
* **Nothing depends on priority for correctness.** Every queue is bounded and every full queue drops or fails by a stated rule
  (P3), so a slow or starved process degrades throughput, never safety.
* When real-time exists, the questions are: does a real-time `netdrv` plus donation through `channel_call` (task 22) to `netd`
  keep receive latency flat under a normal-band flood, and does budget enforcement (task 25) bound a looping driver. Tests with
  negative controls come with those tasks, not before.

## 9. Supervision and failure contracts

```text
netsup  (permanent; restart intensity: 3 restarts in 30 s, then the network is marked down and the registry entry goes stale)
 ├─ netdrv   permanent   one_for_one; restart resets the device (status = 0) and bumps the generation
 ├─ netd     permanent   one_for_one; restart re-applies the stored config, then re-attaches to the rings
 └─ dhcpc    permanent   one_for_one; restart resumes from the stored lease
 (clients: temporary, not ours; a client that dies loses only its own endpoints)
```

**Where we deviate from the agent's `rest_for_one` advice.** OTP's textbook reason to restart the core after the driver is that
the core's link state is stale. Our core handles link state explicitly (5.5): a driver restart is a link flap, so connections
survive it, which is exactly what MINIX gets from TCP, and killing every connection for a driver bug would be the worse
outcome. The other direction needs a re-attach (the new `netd` takes over the rings), not a driver restart. So the strategy is
`one_for_one` plus a **re-attach protocol**, which is also what makes it testable: the harness kills each child at random
points and checks the contracts below. (`rest_for_one` stays the fallback if re-attachment proves fragile.)

| Event | Clients see | Driver | Stack | Recovery |
|---|---|---|---|---|
| `netdrv` dies | nothing at first; packets lost | restarted, device reset, generation + 1 | link down then up; ARP re-learned; TCP retransmits | automatic |
| `netd` dies | every endpoint channel peer-closed; `UNAVAILABLE` from the registry until it returns | keeps running, rings unattached (drops, counted) | new instance re-applies stored config, re-registers `/svc/net`; **all connections are lost**, unknown segments get an RST | clients reconnect (`rt` retry helper) |
| `dhcpc` dies | nothing | none | keeps its address until the lease ends | restart renews |
| poison packet | as the row for `netd` dying, repeatedly | none | restart-intensity limit trips and the network goes down, rather than looping | supervisor stops and reports |
| ring violation | as above | killed and restarted if it wrote bad indices; the stack if it did | | |

**State kept outside the process that would lose it**, all small: the interface configuration and last DHCP lease (in the
supervisor's config blob), the driver generation counter, the TX/RX pools and ring VMOs themselves (supervisor-held, T42's
rule), a boot-time secret and clock base so S9's initial sequence numbers do not repeat after a restart, and a **flight
recorder** VMO (the last few dozen frame headers and events) that `netsup` prints as structured `##` lines when a child dies
(P7). TCP state is deliberately not persisted or replicated.

**Dependency:** slice SD does not currently include a supervisor library (R explicitly leaves restart strategies to the
registry work). `netsup` therefore needs one of (a) the supervisor library to land first, (b) a hand-rolled supervisor with
the restart-intensity logic as a pure, host-tested module and a `TIDY-UP.md` entry to port it. Recommended: (b) written so it
*becomes* that library (section 12, Q3).

## 10. Time

The core's time is an opaque monotonic nanosecond `Instant` (6.2); the kernel's clock is 1 ms ticks until the task-6 flag day,
and the adapter converts. ARP timeouts (about 1 s, 3 tries), ARP entry ageing, the ICMP echo timeout, DHCP retransmission and
renewal and (S9) the TCP timers all come out of one `next_deadline`. The poll period on a machine with no interrupt controller
is the timer tick. Per-thread timer slack and one-shot timers (task 13, after S8) will let nearby ARP, DHCP and TCP timers
coalesce into fewer wakes; nothing here depends on them. The core never reads a clock.

## 11. Testing (acceptance is written before code, per the project rule)

Placed by `TESTING.md` (logic on the host; syscall and wiring behaviour in the QEMU suite; only what the machine shows in QEMU).

**Tier 0, host, every edit**
* `netwire`: round trips (`parse(build(x)) == x`), checksum properties against an independent reference, **no panic on any
  input** under proptest and a fuzz target, golden byte vectors taken from real captures.
* `netcore`: scripted packet tests (inject exact frames, assert exact responses, with the clock under test control), ARP
  edge cases (pending queue overflow, gratuitous ARP, aging), ICMP echo, UDP bind/unbind, bounded queues.
* **Independent oracle:** `smoltcp` as a host-side test peer (already allowed by OD18 and section 13): our core and smoltcp
  exchange ARP, ping and UDP across `netsim`, and run the DHCP client against recorded transcripts from QEMU's own server.
* **Ring model test:** a model of the four rings and slot ownership (every slot has one owner after every operation), under
  loom/shuttle for the doorbell protocol (including the lost-wakeup case), mutation-checked.
* `netsim`: deterministic simulation (one seed drives the clock, loss, reordering, duplication, delay and child kills); this is
  the S9 lossy-link suite's engine, built now.

**Tier 1, QEMU smoke**
* The S8 acceptance as one boot: the guest ping to the gateway succeeds (an `IcmpEcho` client), a UDP echo server answers a
  host datagram through `hostfwd` (a host script, an independent client), the pcap from `filter-dump` is read back by a host
  tool that checks every checksum independently, and the structured output carries the drop counters.

**Tier 2, soak**
* **A network benchmark workload**, in the `##BENCH` format and `bench/baseline.tsv` (`make os-bench`, `bench_compare.py`): a
  UDP echo flood (packets per second and the drop counters), ping round-trip latency, and the big-lock profile and wake counters
  (`debug_stat`) during both. Tasks 11 and 12 of `SCHED-SMP-SPEC.md` are planned to use the network stack as their load and
  benchmark, so S8 must leave them one.
* Child-kill churn (kill `netdrv`/`netd`/`dhcpc` at random points under traffic; check the contracts of section 9 and a leak
  check of slots, handles and frames), the driver-and-stack race tests at `-smp 2` and `-smp 4`, negative controls (a `netd`
  with checksum verification removed, a driver that violates ring rules), and a flooding client (the account bounds it).

**Independent review:** one fresh-context review pass of the S8 spec before code (the ring protocol and the supervision
contracts are the parts most worth attacking), and of the finished slice against `INVARIANTS.md`, per the OS-track rule.

## 12. Open questions, each with a recommendation

1. **Two processes from the start, or one for a first ping?** Recommend the split from the start (4.1): the protocol is the
   hard-to-change part and the security argument is real. Fallback if schedule matters: a one-process `netd` built behind the same
   `Device` trait, replaced within S8.
2. **DHCP as a separate program?** Recommend yes (6.3). Fallback: host the same pure crate inside `netd` first.
3. **Supervisor: wait for a library or hand-roll `netsup`?** Recommend hand-rolling it with a pure, tested restart-intensity module
   and a TIDY-UP entry, designed to be extracted. The alternative delays S8 behind SD for little gain.
4. **Is `socket_info` (free space of a peer's socket) acceptable for S9?** Decide in the S9 spec; S8 does not need it.
5. **Fragmentation:** none in S8 (dropped and counted on receive, TOO_BIG on send). Recommend keeping it that way through S9
   unless a real client needs it.
6. **Who may bind low ports, broadcast, or send from 0.0.0.0?** Recommend rights on the service connection, set by the
   view-building code (SD), not an identity. The shell's powerbox syntax for granting them is D10/Q9 and stays open.
7. **DMA budget:** 4 MiB system-wide, 64 pages per object. S8 needs about 0.6 MiB. S9 throughput work may want a bigger one.
8. **Ping sockets rather than raw ICMP?** Recommend yes (7.1).
9. **IPv6:** out of scope; the crate layout leaves room.
10. **Account limits for endpoints** (7.6): per-account and pool values for the net service, chosen in the S8 spec.
11. **Settled by the owner, 2026-11-14:** doorbells are events (5.3); the threading model is left alone and T67 stands (section
    8); priorities are allowed for but not set (8.1).
12. **Verify before writing the spec** (all marked *memory* above): the QEMU `filter-dump` syntax and the exact flags for
    modern virtio-mmio on the net device; slirp's MACs, lease time and fragmentation behaviour; whether `virtio-net-device`
    needs `-global virtio-mmio.force-legacy=false` as `blkd` does; Fuchsia's current data-socket API; the sDDF queue
    details beyond the paper.

## 13. Dependencies, order and proposed records

Needs before S8 starts, and where each stands (re-read from the roadmap):

* **W** (targeted wake, so a doorbell wakes one waiter by id): built. Wake-one on ports (task 21) is *not* needed.
* **V** (creator-pays commit, the read-only mapping of a foreign VMO): built.
* **R** (service loop and accounts): built, with `memfsd` and four other servers on it. **Needed and not done: the typed
  request/reply client**; the endpoint limits of 7.6 are a net-service setting.
* **SD** (registry and views): the registry service is built; **step 4 (publishing, the shell's `/svc` view, reconnect on
  `UNAVAILABLE`) is deliberately left until the first new service needs it, so it becomes part of S8**, and S8's use shapes the
  reconnect policy.
* **DM**: several devices and several clients are served; the net device kind is S8's.
* **I** (AIA): built. **T** (kernel memory and threads): built to the extent S8 needs.
* **Scheduler tasks 1 to 10** of `SCHED-SMP-SPEC.md` (measurement, the two-band scheduler, `thread_spawn`, nanosecond time,
  the block/wake protocol): planned before S8 so that the service runtime and the network servers are written once against the
  final thread API. **Tasks 11 and 12** follow S8's first milestone, using this stack as the load.
* T28/T30/T60/T61 should be re-read when S8's spec is written; none blocks.

Proposed decision-log entries once the owner accepts the corresponding sections (the log's last entry is OD97, the service
directory, so numbers continue from OD98):

* **OD98** the process architecture: supervisor, driver, stack server, DHCP client, the config capability (sections 4, 6.3).
* **OD99** the driver/stack data path: offset rings, events as doorbells, signal on edge, the rights table (section 5).
* **OD100** the native network protocol, the datagram drop rule and the endpoint accounting (section 7), including that S9's
  stream data path is a prototype, not a given.
* **OD101** the failure contracts and the one_for_one plus re-attach strategy (section 9).
* A **`TIDY-UP.md`** entry for the hand-rolled supervisor, and one for "client-creates-the-socket is unproven". (T67, one
  thread per connection, already exists.)

Suggested slice shape for S8 (to be turned into `NET-SPEC.md`): step 0 the spec and acceptance tests; 1 `netwire` and `netsim`;
2 `netcore` (ARP, IPv4, ICMP, UDP) against smoltcp; 3 the rings and `netdrv`; 4 `netd`, `netsup`, the endpoint protocol, the typed client and SD step 4 (publishing `/svc/net`);
5 `dhcpc`; 6 the network benchmark workload, documents, tick the boxes only after the QEMU run at `-smp 2` and `-smp 4`.

## 14. Risks and what would change this design

* **The client-creates-the-socket idea fails its prototype** (7.4). Then streams fall back to Fuchsia's shape (the stack creates
  the pipe) with the stack's job charged per account, which changes S9 but not S8.
* **A single `netd` thread is too slow.** Measure under tasks 11, 12 and 24 (which use this stack as their load); the core's
  interface allows wrapping instances in front-end loopers later (T67).
* **Priority behaviour cannot be tested before real-time scheduling is enforced** (8.1). Mitigation: nothing depends on it, the
  policy is configuration, and the tests are written with the tasks that make it testable.
* **The nanosecond flag day lands under us.** Mitigation: the core takes an opaque `Instant` (6.2), so only the adapter changes.
* **Re-attachment proves fragile.** Fall back to `rest_for_one` (everything restarts together) and accept lost connections on a
  driver crash.
* **The ring protocol costs more than it saves at S8's traffic.** Mitigation: the `Device` seam allows the one-process build.
* **Memory-sourced survey claims are wrong** (section 12, Q10): the cost of checking them is small and it is the first task
  of the spec.

## 15. Revision notes (v0 to v1, 2026-11-14)

Why: the roadmap changed after v0 (the scheduler, SMP and threads work in `SCHED-SMP-SPEC.md`, and slices R, SD, DM and T moving).
What changed, and what did not:

* **Changed:** doorbells are events, settled by the owner (5.3; `SCHED-SMP-SPEC.md` 5.7 corrected to match). Time is an opaque
  nanosecond `Instant` (6.2, 10). Endpoint accounting against slice R's connection limits (7.6, a gap v0 missed). Threads:
  the reasoning is restated (8) with the client-pays problem made explicit; the decision is unchanged and T67 stands. Priorities:
  allowed for, not set (8.1). The network benchmark workload for tasks 11 and 12 (11). SD step 4 and the typed client become part of
  S8 (13). The proposed ODs are renumbered to OD98 to OD101 because OD97 is the service directory.
* **Unchanged:** the driver and stack split, the rings, the sans-IO core, the supervision contracts, the datagram drop rule,
  the open question about client-created sockets, and the testing plan.
