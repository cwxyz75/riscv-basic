# S9-SPEC: TCP, sockets in the C library, HTTP and DNS (specification, draft v0)

Status: **draft v0, 2026-11-14; a proposal for the owner; nothing here is built and nothing here is decided.** This is the written
acceptance contract for S9 (CLAUDE.md: spec, then acceptance checks, then code, then an independent review). It continues
`NET-DESIGN.md` (the architecture and its reasons) and `NET-SPEC.md` (the S8 contract, whose format this file follows) and it
assumes S8 is built as `NET-SPEC.md` says. Where this file and `NET-DESIGN.md` disagree, `NET-DESIGN.md` is the intent and this
file is the contract; fix the other one in the same PR. Everything marked **PROPOSAL** is a choice the owner has to accept before
code is written; accepted ones are logged as OD102 and later in `OS-ARCHITECTURE.md` section 17 (section 15 lists them). Section 19
separates what was checked while writing this from what is stated from memory.

Reading order: 1 (what S9 is), 2 (steps), 3 (the environment, with the facts checked against slirp), **4 (the stream data path: the
decision that most needs the owner)**, 5 (TCP), 6 (memory accounting), 7 (how it fits S8's architecture), 8 (the protocol), 9 (the C
library), 10 and 11 (HTTP and DNS), 12 (limits), 13 (invariants), **14 (acceptance)**, 15 to 19 (records, risks, open points, what S8
must change, what is verified).

## 1. Goal and non-goals

**Goal.** Programs in the guest hold reliable byte streams to the outside world and the C library speaks BSD sockets:

1. the stack server gains TCP: a pure state machine (`nettcp`) with retransmission, flow control and congestion control,
   driven through the same sans-IO core, the same loop and the same accounts as S8;
2. a stream is a native endpoint (a control channel) plus a kernel socket for the bytes, **created by the client** (OD74), so
   the client's job pays for the buffers the kernel holds;
3. the C library gets `socket`, `connect`, `bind`, `listen`, `accept`, `send`/`recv`, `poll`/`select`, `getaddrinfo` and the
   rest of section 9 as a shim over the native protocol, with a stated list of what is refused;
4. an HTTP server (`httpd`) serves files from the memory filesystem, a fetch tool (`fetch`) downloads a URL and prints its
   SHA-256, and a stub DNS resolver turns names into addresses;
5. a stack restart loses every connection, by design, and both sides find out promptly (a reset, never a silent hang and
   never a false end-of-file).

**Acceptance of S9 (the roadmap's, made precise):** `fetch` downloads files of several sizes from a host
`python3 -m http.server` and the SHA-256 it prints equals the one the host computed (Q21); host `curl` downloads files from
the in-guest `httpd`, served out of the memory filesystem, through `hostfwd`, and the hashes match (Q22); the lossy-link suite
passes on the host for every seed (H16, H17) and once through a real lossy relay in QEMU (Q30); the TCP state machine passes
its packet scripts (H13); the capture of the QEMU runs passes an independent TCP checksum and flow check (Q23). The full
list is section 14.

**Non-goals (S9).** No window scaling, no selective acknowledgements, no TCP timestamps or PAWS, no ECN, no TCP Fast Open, no
keep-alive, no urgent data, no path-MTU discovery, no SYN cookies, no MD5 or AO signatures, no congestion control beyond
Reno with NewReno recovery (section 5.3 says why for each). No IP fragmentation or reassembly (5.12). No IPv6, no raw
sockets, no `AF_UNIX`. No TLS, so no HTTPS. No HTTP keep-alive, chunked encoding, ranges, compression or redirects. No DNS
over TCP, no EDNS, no AAAA, no DNSSEC, no shared resolver cache. No per-connection threads (T67; owner decision). **No
kernel change and no new syscall** in the recommended design (4.6 says what would need one, and that it would be its own
slice with its own review). No real-time priorities.

## 2. Steps

Each step is one PR (or a few), green on `make smoke`, bootable. Kernel- or security-sensitive steps get a Fable review in
their own small PR (the owner's standing preference); the others get the default reviewer. Step 1 is a **gate**: its
measurements decide whether section 4's recommendation stands, and steps 4 onwards do not start until the owner has seen
them.

| Step | Contents | Needs | Review |
|---|---|---|---|
| 0 | This spec accepted; the slirp TCP and DNS probe and its capture committed (`scripts/net_probe_slirp.py --tcp`, `net/tests/data/slirp-tcp-probe.pcap`); `net_pcap_check.py` extended for TCP with its self-test (14.5); the packet-script runner and the first scripts, written and failing; harness flags (TCP `hostfwd`, the host HTTP server, the fake DNS server, the lossy relay) | S8 | owner |
| 1 | **The data-path prototype** (4.7): a throwaway test server that takes a client-created socket, in the suite, with the measurements P1 to P8; a short written result; the owner confirms or changes section 4 | S8 step 4 | **Fable** (it tests kernel accounting from user mode) |
| 2 | `netwire` TCP (segment view, builder, options) and `nettcp` part 1: the state machine, the reassembly buffer, the send buffer, timers, RTO; host-only: H11 to H14, H18 | 0 | default |
| 3 | `nettcp` part 2: congestion control, fast retransmit, NewReno, persist, Nagle, delayed ACK; `netsim` gains the bottleneck queue and corruption; the smoltcp peer; the lossy-link suite and the deterministic simulation: H15 to H17 | 2 | default |
| 4 | Streams in `netd`: `proto::net` kinds 3 and 4, the client-created socket path, pre-posted accepts, accounting and orphans (section 6), loopback, the boot secret and ISNs, resets after a restart; `rt::net` streams; the in-guest echo test and the restart tests (Q20, Q24 to Q27, Q31) | 1, 3 | **Fable** (new authority and accounting in the stack; the restart contract) |
| 5 | The boot secret's entropy source (5.10, open point 4): a one-shot virtio-rng read, or the recorded stand-in | 4 | **Fable** if a driver is added; default for the stand-in |
| 6 | The C library: the socket descriptor kind, the calls of section 9, `poll`/`select` over a port, the errno table; C test programs (Q28) | 4 | default |
| 7 | `dns` (pure) and the resolver in `rt::net` and `getaddrinfo`; `sha256`; `fetch`; `http1` (pure) and `httpd`; the two roadmap acceptance runs (Q21, Q22, Q29, Q34) | 4, 6 | default |
| 8 | The QEMU lossy relay (Q30), the benchmarks (14.3), churn and soak tests (Q32, Q33), documents (SYSCALLS unchanged, INVARIANTS N11 to N24, TIDY-UP, ROADMAP ticks only after the QEMU runs at `-smp 2` and `-smp 4`, OD102 onwards) | 7 | default |
| 9 (optional) | An unmodified third-party C HTTP server ported onto the C library as an oracle the writing context did not produce (open point 9) | 6 | default |

Sizes are not estimated (NET-SPEC section 2 says why). Order against the scheduler work (`ROADMAP.md`, the gate of 2026-11-14):
tasks 1 to 10 land before S8 and tasks 11 and 12 after S8's first milestone, so **S9 may assume tasks 1 to 12 and nothing
later**. In particular it does not assume timer slack or one-shot timers (task 13), the wake-one baton and the exact ready
set on ports (task 21, T60), donation through `channel_call` (22), direct hand-off (24), `thread_interrupt` (26) or
`rt::looper` (27). Section 7.6 says what each would improve.

## 3. The environment

### 3.1 Test machine and flags

S8's flags (NET-SPEC 3.1) with a TCP forward added, and for the fetch tests a host HTTP server the harness starts:

```text
-netdev user,id=n0,net=10.0.2.0/24,host=10.0.2.2,dhcpstart=10.0.2.15,hostfwd=udp:127.0.0.1:UPORT-:7777,hostfwd=tcp:127.0.0.1:TPORT-:8080
-device virtio-net-device,netdev=n0,mac=52:54:00:12:34:56
-object filter-dump,id=f0,netdev=n0,file=OUT.pcap
host:  python3 -m http.server HPORT --bind 127.0.0.1 --directory DIR      (the guest reaches it as 10.0.2.2:HPORT)
```

For the lossy tests in QEMU (tier 2) the guest's netdev is a socket that connects to a relay the harness runs, and slirp
lives in a second QEMU that has no machine (the topology `scripts/net_probe_slirp.py` already uses):

```text
guest QEMU:   -netdev socket,id=n0,connect=127.0.0.1:RELAY -device virtio-net-device,netdev=n0,mac=52:54:00:12:34:56
slirp QEMU:   -machine none -netdev user,id=u0,...,hostfwd=tcp:... -netdev hubport,id=h0,hubid=0,netdev=u0
              -netdev socket,id=s0,listen=127.0.0.1:SLIRP -netdev hubport,id=h1,hubid=0,netdev=s0
relay:        scripts/net_lossy_relay.py --seed N --loss P --dup P --reorder P --delay MS   (frames are 4-byte length-prefixed)
```

### 3.2 Facts checked against slirp on QEMU 11.1.3 (macOS, 2026-11-14)

Checked with a scripted fake guest in the style of `scripts/net_probe_slirp.py` (no guest code: slirp and a socket netdev on a
hub, the script sends real frames). The script and its capture (334 TCP frames, every TCP checksum verified by the script) were
kept outside the repository because this PR is documents only; **step 0 commits them** and re-runs them, and until then the
table is the record. The most important line is the first: **in user-mode networking the guest's TCP peer is slirp's own TCP,
not the host kernel's.** Every option decision in 5.3 follows from what that peer does.

| Question | Answer observed |
|---|---|
| Guest SYN to 10.0.2.2:HPORT carrying MSS 1460, window scale 7, SACK-permitted and timestamps | SYN-ACK from 10.0.2.2 carries **only MSS 1460**; window 65535; TTL 64; DF clear. **slirp negotiates no window scaling, no SACK and no timestamps**, so none of them can be exercised in QEMU |
| Bulk, host to guest, 200,000 bytes, guest ACKs each segment | 140 segments, **1440 bytes each** (not 1460), largest frame 1494 bytes, never an IP fragment; the hash of what the fake guest received matched; then FIN |
| Guest announces MSS 536 and window 4096, never ACKs data | slirp **ignores the small MSS** (it sent 1440-byte segments) but respects the window (2,880 bytes then stops); it retransmits the first segment after **1.5 s, then 3 s, then 6 s** |
| How slirp ACKs guest data | **every segment at once** (40 segments, 40 ACKs, the first 0.1 ms after the data); no delayed ACK |
| Three segments after a hole | **three duplicate ACKs** (no SACK blocks); filling the hole advances the ACK over all four |
| Host program does not read | slirp accepted about 685 KB, then advertised **window 0**; a 1-byte probe is answered with window 0 and the ACK unchanged; when the program reads, slirp sends a window update **unprompted** |
| Guest SYN to a closed host port | **RST+ACK at once** (`seq 0, ack = our seq + 1`), so "connection refused" is testable |
| A stray ACK with data for no connection | **RST** with `seq` = our ACK number, the RFC 9293 form |
| Host connects to the forwarded port | the guest sees a SYN **from 10.0.2.2**, an ephemeral source port, MSS 1460 only, window 65535, to its own MAC |
| The guest does not answer that SYN | slirp retries after **6 s** (one retry seen in 8 s): a dropped inbound SYN is expensive |
| The guest answers that SYN with RST | the host's `connect` has **already succeeded** (slirp accepts first), then the host reads end-of-file. `curl` reports exit 52, "Empty reply from server", not "connection refused" |
| The guest resets an established forwarded connection | the host reads **end-of-file, not an error**: through slirp the host cannot tell a guest RST from a FIN |
| slirp sending to the guest before any ACK | one segment in one run and two in another, then it waits: its initial window is small, so a delayed ACK on the first segment would cost its whole delay (5.6) |
| DNS: UDP query to 10.0.2.3:53 | answered from 10.0.2.3 port 53: `example.com` A in 12 ms (61 bytes, two answers), AAAA (85 bytes), a nonexistent name NXDOMAIN with one authority record (112 bytes); none fragmented. `localhost` is NXDOMAIN: **no name resolves without the host having a working resolver**, so hermetic DNS tests need their own server (11.4) |
| TCP SYN to 10.0.2.3:53 | answered with SYN-ACK: DNS over TCP is reachable (not used, 11.3) |
| `-netdev socket,connect=` with `virtio-net-device` on `virt` | accepted, QEMU connects to the relay: the lossy-relay topology of 3.1 is possible |
| `virtio-rng-device` | listed by this QEMU (`-device help`): an entropy device exists if the owner wants one (5.10) |

Host tools, checked: `python3 -m http.server` (Python 3.8.10) answers `HTTP/1.0 200 OK` with `Content-Length` and closes, for an
HTTP/1.0 and an HTTP/1.1 request alike, and `404` with a `Content-Length`; `curl` 8.7.1 sends
`GET /a.txt HTTP/1.1`, `Host`, `User-Agent`, `Accept: */*` and accepts an HTTP/1.0 reply with a `Content-Length`. `smoltcp`'s
current release is 0.14.0 (`cargo search`).

Still **not** checked: anything through a real guest driver; slirp's behaviour when the guest's window is zero for a long time
(its persist timing); slirp's handling of a simultaneous open; how the relay topology behaves under load; the Linux CI host
(the table is from macOS).

### 3.3 Facts checked in this repository

| Fact | Where | Consequence |
|---|---|---|
| A kernel socket end has a 64 KiB inbound buffer; both ends are charged to the job that created the pair; a write takes only what fits | `SYSCALLS.md` "Sockets", "Jobs and limits"; `kcore/src/ipc.rs` `socket_write_in` | the client-created socket puts both buffers on the client (OD74 works as stated) |
| `socket_write` returns **SHOULD_WAIT** when the peer's buffer is full, but **NO_MEMORY** when the paying job's IPC quota or the system budget is exhausted | `kcore/src/ipc.rs` lines 775 to 790 | the stack must treat NO_MEMORY on a client's socket as "wait", not as an error (4.4) |
| **WRITABLE on a socket means "the peer's buffer is not full"**; it says nothing about quota | `kcore/src/ipc.rs` line 663 | a level-triggered wait on WRITABLE would spin when the client's quota is the limit: one-shot bindings and a timer (4.4) |
| No call returns a socket's fill level (`socket_peek` is internal to `socket_read`) | `kcore/src/ipc.rs`, `SYSCALLS.md` | D4 stands; section 4 works without one |
| `MAX_IPC_BYTES` is **2 MiB for the whole system**; a launched program's job gets **192 KiB** of IPC bytes and **256 objects** | `abi/src/lib.rs`, `init/src/bin/launcher.rs` | one stalled TCP connection can hold 128 KiB; the defaults are too small for a network program (6.4, open point 3) |
| `object_info` returns only koid and peer koid | `SYSCALLS.md`, design review D17 | the stack validates a passed socket by probing it (8.3) |
| A sleeping port waiter is re-evaluated on every syscall, in proportion to its bindings, until task 21 | TIDY-UP T60 | bindings per connection are a system-wide cost (7.2, risk 3) |
| The C library has a 64-entry descriptor table with two kinds (stream, file), no `poll`, no `O_NONBLOCK`, about 25 errno values | `libc/src/fd.rs`, `libc/include/errno.h` | section 9 adds all of it |
| No hash function, no entropy source and no random numbers exist in the tree; Q11 (entropy) is open; D15 raised it | `OS-ARCHITECTURE.md` section 18, design review D15 | `sha256` and `siphash` are written here; the boot secret needs a decision (5.10) |
| `clock_get` and deadlines are documented as 1 ms ticks; the nanosecond ABI is task 6 | `SYSCALLS.md` | the core takes an opaque `Instant`; only the adapter cares (7.6) |

### 3.4 What S9 relies on from S8, and what it needs changed

Everything in NET-SPEC sections 4 to 9: the processes, the rings, `netcore`, `proto::net`, `rt::net` and the typed client,
accounts, the three registry names, `netsup` and the config blob, `netsim`. Section 18 lists the places where S8's spec should
leave room so S9 does not have to reopen it.

## 4. The stream data path (PROPOSAL; the decision that most needs the owner)

### 4.1 The question

OD74 says: for a byte stream the client creates the kernel socket and passes one end in with `Connect` or `Accept`, so the
client's job pays for the buffers. NET-DESIGN 7.4 flags this as unproven and names three problems: (1) there is no precedent
(Fuchsia's netstack creates the pipe; Genode's client donates memory to a session, not a pipe); (2) the stack cannot read how
full the client's buffer is, so it cannot size its advertised window from it; (3) the stack needs a retransmit buffer of its
own whatever is done, so "the client pays for everything" is not true.

### 4.2 What a TCP connection has to hold, whoever pays

Four byte stores exist in any design. Naming them makes the options comparable:

| Store | Contents | Size | Can it live outside the stack? |
|---|---|---|---|
| **A** application to stack | bytes the application wrote that the stack has not taken | up to 64 KiB (kernel socket) | yes: it is the stack end's inbound buffer of the kernel socket |
| **S** send buffer | bytes taken from A, sent or not, **not yet acknowledged** | the send window | only in a shared ring, where "taken" can mean "still in the ring" |
| **R** receive buffer | segments received out of order, plus in-order bytes the client's buffer could not take | the advertised window | only in a shared ring |
| **B** stack to application | in-order bytes the application has not read | up to 64 KiB (kernel socket) | yes: the client end's inbound buffer |

Problem 3 is store S. Problem 2 is about R and B. The observation that settles problem 2: **R has to exist anyway**, because a
receiver that discards out-of-order segments turns every loss into a retransmission of everything after it (slirp does not
SACK, and three duplicate ACKs are all we get, 3.2). Once R exists, in-order bytes that B cannot take simply stay in R, and
the advertised window is "free space in R". B then acts as extra buffering in front of the application that the window does
not count. That is the double buffering Fuchsia has and D4 predicted; its cost is bounded (R's size) and it needs nothing new.

### 4.3 The options

| | Option | Who pays for A and B | Who pays for S and R | Kernel change | Copies between the packet slot and the application's buffer (receive, send) | Main cost |
|---|---|---|---|---|---|---|
| **1** | **Client-created kernel socket, the stack keeps S and R** (OD74 as written, problem 2 solved by 4.2) | the client's job, enforced by the kernel | the stack, reserved against the client's account (section 6) | **none** | 2 (3 when staged in R), 3; a syscall per chunk on each side | per-connection reservation in `netd`; pre-posted accepts; the program's IPC quota must be raised |
| 1b | As 1, plus a `socket_info(handle) -> free bytes of the peer's buffer` syscall | as 1 | as 1; R could be smaller only if out-of-order segments were dropped | **yes** (a new call, its own slice and Fable review) | as 1 | a kernel slice that saves almost nothing, because R is still needed for reassembly |
| **2** | **Client-created shared ring**: a committed VMO with two byte rings and an event pair, mapped by `netd` (the Genode shape) | the client (the VMO) | **the client**: S is the part of the transmit ring not yet acknowledged, R is the receive ring written at offsets | none (slice V's commit rule already allows the mapping) | 2, 2 | a second data path; `netd` maps a client's memory per connection and must treat every index as hostile; the stream is no longer a plain socket handle; committed memory is paid up front even when idle |
| **3** | **The stack creates the socket** (Fuchsia) | the stack's job; counted per account by the stack's own bookkeeping | the stack | none | as 1 | `netd`'s job must be given IPC quota for every connection (128 KiB each against a 2 MiB system budget); the bound is the stack's arithmetic, not the kernel's; a remote peer fills buffers the stack pays for |

Other things looked at and set aside: the stack as a library in each client (rejected in NET-DESIGN 4.1); data as channel
messages (double copies, message framing on a stream, and the 128-message queue becomes the window); a kernel socket with a
settable buffer size (a kernel change that would let option 1 bound a connection below 128 KiB: recorded as the thing to ask
for if P7 shows the quota pressure is real).

### 4.4 Recommendation: option 1, with no kernel change (PROPOSAL)

**A stream is the endpoint channel (control) plus a kernel socket pair the client creates; the stack holds one end, keeps S and
R itself with sizes fixed when the connection is made, and charges them to the client's account.** Why:

* It is what OD74 decided, and the open problem in it dissolves: the window comes from R, which must exist regardless. No
  `socket_info`, no kernel slice.
* The kernel enforces the bound on A and B, per job, with no bookkeeping in `netd`: a client that does not read fills its own
  quota and nobody else's. Under option 3 a remote peer and a lazy client together spend the stack's quota.
* **A connected stream is an ordinary socket handle.** `read`, `write` and `poll` in the C library already work on one; it can
  be handed to a child as its standard input (an `inetd`-style server costs nothing); half-close is `socket_shutdown`. None
  of that is true of a ring.
* Options 1 and 3 differ only in who calls `socket_create` and in how accepts are posted. If the prototype fails, the
  fallback to option 3 changes the accept flow and the accounting, not `nettcp` or the loop.
* Option 2 is the right answer to "throughput matters" (NET-DESIGN 7.5 already leaves a place for it as another endpoint
  kind) and the wrong first step: it needs the hostile-index discipline of the driver rings applied to every client, and it
  cannot be compared with anything until option 1 exists and has numbers.

How the stack uses the socket (the rules that make option 1 safe):

* **Receive:** an in-order segment that arrives when R is empty is written straight from the receive slot into the client's
  socket (`socket_write`); what does not fit, and anything out of order, is copied into R. The window is the free space in R.
* **A short or refused write is not an error.** SHOULD_WAIT (the client's buffer is full) and NO_MEMORY (the client's job
  quota or the system budget is full; 3.3) both mean "keep it in R and try later". For SHOULD_WAIT the stack arms a
  **one-shot** binding for WRITABLE on its socket end; for NO_MEMORY, where WRITABLE stays set and would spin, it retries on a
  timer (20 ms, doubling to 500 ms). The window closes by itself as R fills; the peer's zero-window probes keep the connection
  alive. The stack never blocks on a client and never drops acknowledged data.
* **Send:** the stack reads from its socket end (`socket_read`) only as much as S has room for, and only then; A is the
  client's. A one-shot binding for READABLE is armed only while S has room.
* **What the stack pays** is S and R, a fixed reservation made when the connection is made and charged to the account
  (section 6). This is stated in every place that says "the client pays": the client pays for A and B, and its *account* is
  charged for S and R, which are the stack's memory.

### 4.5 What it costs

* Stack-side memory: `snd_buf + rcv_buf` per connection (default 16 KiB each), reserved whether used or not.
* One copy more than a ring on the send path, one more on the receive path only when bytes are staged in R, and a syscall per
  chunk on each side where a ring needs only a doorbell (measured in step 1 and in 14.3).
* Four kernel objects per connection on the client's job (a channel pair and a socket pair) and up to three port bindings in
  `netd`.
* Accepts must be posted ahead (8.4), so a listener's backlog is real memory accounting rather than a number, and the C
  library hides it.
* A program that uses the network needs more IPC quota than the launcher's 192 KiB default (6.4).
* An honest gap: the stack cannot bound what the kernel socket holds below 64 KiB per direction. A remote sender and a
  client that does not read can pin 64 KiB of the client's quota per connection (and of the 2 MiB system budget) until the
  client reads or closes.

### 4.6 What would change the recommendation

* P1 fails (the charge does not land on the client's job in every case): option 3, and TIDY-UP records OD74 as not met.
* P2 or P4 shows the wake pattern is pathological (a wake per byte, or a spin under quota pressure that the timer does not
  cure): ask for the precise wait (T28) or `socket_info`, as a kernel slice with its own Fable review, before step 4.
* P7 shows the 2 MiB system budget is reachable by a handful of connections in practice: ask for a settable socket buffer
  size or a larger budget tied to RAM (T79), again as a kernel slice.
* The benchmark of 14.3 shows the copies dominate once tasks 11 and 12 exist: build option 2 as a second endpoint kind and
  compare, which is T67's question again.

### 4.7 The prototype (step 1): what must be built and measured before TCP is wired in

A throwaway server on `rt::service` in the suite (`streamproto`), with no TCP in it: it takes a client-created socket in a
request and echoes, sinks or sources bytes. It is built in both variants (client creates, server creates) so the numbers
compare options 1 and 3 directly. Each line is a test with a pass condition, written before the code:

* **P1 `stream_buffers_are_charged_to_the_client`:** with 64 KiB queued in each direction, `job_info(client, 5)` rose by the
  queued bytes and `job_info(server, 5)` did not move; the same after the handle was transferred, and after the client's
  duplicate was closed. In the server-creates variant the opposite holds (the test can tell them apart).
* **P2 `a_client_at_quota_stalls_only_itself`:** a client whose job has 64 KiB of IPC quota left: the server's `socket_write`
  returns NO_MEMORY while WRITABLE is set; with the timer rule the server makes at most 60 attempts a second (a counter), a
  second client's stream is unaffected, and the first resumes when it reads.
* **P3 `one_socket_two_one_shot_bindings`:** the same socket end bound twice in one port, one-shot, with different keys and
  masks (READABLE and WRITABLE), re-armed independently. If the kernel refuses, the fallback (one binding, re-bound when the
  mask changes) is measured instead.
* **P4 wakes and throughput:** 1 MiB each way with a reader that takes 1 byte, 512 bytes and 16 KiB at a time: server wakes per
  megabyte, bytes per wake, megabytes per second, at one and four harts (`##BENCH stream_pipe_*`). No pass threshold: these
  are the numbers the owner decides on.
* **P5 `a_dead_client_frees_everything`:** kill the client mid-transfer: the server sees PEER_CLOSED, both jobs' ledgers
  return to their start, nothing leaks.
* **P6 `a_bad_stream_handle_is_refused`:** a channel instead of a socket, a socket without READ or WRITE, both ends of one
  pair, a socket already in use by another connection: each refused, none harms the server.
* **P7 the budgets:** how many streams a program with the launcher's defaults can hold (objects 256, IPC 192 KiB), and how many
  stalled streams it takes to exhaust the 2 MiB system budget; the result sets 6.4's numbers.
* **P8 the bystander cost:** the existing `channel_call_rt` benchmark on an unrelated pair of threads while the server sleeps
  on 64, 256 and 768 one-shot bindings (T60's cost, measured). If it is large, task 21 moves ahead of step 4 or the limits
  of section 12 shrink.

## 5. TCP (`netwire` additions and `nettcp`)

`nettcp` is `no_std`, `#![forbid(unsafe_code)]`, sans-IO, with no panic on any input and no allocation proportional to
anything a packet claims (N9, N10 extended). It is a module family inside the core, as NET-DESIGN 6.1 planned: `netcore`
dispatches protocol 6 to it and it emits segments through the same `egress`. References: RFC 9293 (TCP), RFC 1122 (host
requirements), RFC 5681 (congestion control), RFC 6298 (retransmission timer), RFC 6582 (NewReno), RFC 5961 (blind in-window
attacks), RFC 6528 (initial sequence numbers), RFC 6056 (port randomisation), RFC 6691 (MSS), RFC 7323 and RFC 2018 for what
is left out.

### 5.1 `netwire`: the segment

A view over `&[u8]` and a builder into `&mut [u8]`: ports, sequence and acknowledgement numbers, data offset (5 to 15), the
flags FIN, SYN, RST, PSH, ACK, URG (ECE and CWR are read and ignored), window, checksum (verified over the pseudo-header on
receive; a segment with a bad checksum is dropped and counted `tcp_checksum`), urgent pointer (ignored). Options: end (0), no-op
(1), MSS (2, length 4, **only on a SYN**); every other kind is skipped by its length; a length of 0 or 1, or one that runs past
the header, ends the option parse and the segment is still processed without the remaining options. Window scale, SACK-permitted,
SACK and timestamps are recognised by number only so the counters can say they were offered.

### 5.2 The connection record and the state machine

A connection is identified by (local address, local port, remote address, remote port). The states are RFC 9293's eleven:
CLOSED, LISTEN (a listener, a separate record), SYN-SENT, SYN-RECEIVED, ESTABLISHED, FIN-WAIT-1, FIN-WAIT-2, CLOSE-WAIT, CLOSING,
LAST-ACK, TIME-WAIT. Segment processing follows RFC 9293 section 3.10.7 in its order (sequence check, RST, SYN, ACK, data, FIN),
with these choices stated:

* **Simultaneous open is supported** (SYN-SENT that receives a SYN moves to SYN-RECEIVED); it is a few lines in a state machine
  and a classic script.
* **Sequence arithmetic** is modulo 2^32 through one comparison helper, tested at the wrap.
* **RST and SYN validation follow RFC 5961:** a RST is acted on only if its sequence number is exactly `RCV.NXT`; one elsewhere
  in the window gets a challenge ACK; one outside is dropped. A SYN on a synchronised connection gets a challenge ACK. An ACK
  for data never sent gets an ACK and is dropped; an ACK older than `SND.UNA - MAX.SND.WND` is dropped. Challenge ACKs are
  limited to 100 a second in all.
* **A segment for no connection** gets a reset in RFC 9293's two forms (if it has ACK: `seq = SEG.ACK`, RST; otherwise
  `seq = 0, ack = SEG.SEQ + SEG.LEN`, RST+ACK), never in answer to a RST, never to a broadcast or multicast address, at most
  100 a second in all (`tcp_rst_rate_limited` counts the rest). slirp does the same (3.2).
* **Overlapping data:** bytes already received are never replaced (first copy wins); a segment is trimmed to the window on both
  sides; data beyond the window is dropped and ACKed.
* **MSS:** we announce 1460 (MTU 1500 less 40, RFC 6691). We send at most `min(peer's MSS, 1460)`, and 536 if the peer sent
  none. We **accept** segments of any size up to the frame limit whatever we announced, because slirp ignores a small MSS (3.2).
* **The IP header:** DF clear, TTL 64, a per-destination identification counter. No path-MTU discovery: with DF clear a router
  may fragment and the far end reassembles; under slirp there is no router.

### 5.3 What is in, what is out, and why (PROPOSAL)

| Feature | S9 | Reason |
|---|---|---|
| Slow start, congestion avoidance (RFC 5681) | **in** | the roadmap asks for it; needed for the bottleneck tests |
| Fast retransmit and fast recovery, **NewReno** partial ACKs (RFC 6582) | **in** | without SACK, NewReno is what recovers several losses in one window without waiting for the timer each time; slirp sends the three duplicate ACKs it needs (3.2) |
| Retransmission timer, Karn's rule, exponential backoff (RFC 6298) | **in** | |
| Delayed ACK, with a quick-ACK rule (5.6) | **in** | the roadmap asks for it; the quick-ACK rule is there because slirp starts with one or two segments |
| Nagle's algorithm, `TCP_NODELAY` | **in** | small and expected by ported programs |
| Zero-window probes (persist timer), silly-window avoidance on both sides | **in** | required for correctness with a slow reader, which the design creates on purpose (4.4) |
| RFC 5961 checks, keyed ISNs (RFC 6528), keyed ephemeral ports (RFC 6056) | **in** | cheap, and a stack that restarts needs unpredictable, non-repeating ISNs (5.10) |
| **Window scaling** (RFC 7323) | **out** | R and S are at most 64 KiB, so no window above 65,535 is ever useful to us in either direction; slirp does not negotiate it, so the code could only be tested on the host. `OS-ARCHITECTURE.md` section 13 lists it under "the hard part": this is a deliberate deferral and an open point (17.2). Trigger: buffers above 64 KiB (the ring data path) or a network backend other than slirp |
| **SACK** (RFC 2018, RFC 6675) | **out** | slirp does not negotiate it; NewReno covers the lossy suite; SACK scoreboard logic is the largest single piece of a modern TCP. Trigger: measured multi-loss recovery that NewReno cannot meet |
| **Timestamps and PAWS** (RFC 7323) | **out** | slirp does not negotiate them; RTT comes from one timed segment per window with Karn's rule; sequence numbers cannot wrap within a connection at these speeds in less than hours. Trigger: window scaling (they belong together) |
| ECN, Fast Open, keep-alive, urgent data, MD5/AO, SYN cookies, path-MTU discovery, CUBIC or BBR | **out** | not needed by any acceptance check. Keep-alive's absence means a connection whose peer vanished silently stays until the application closes it or the stack restarts; `SO_KEEPALIVE` is refused, not ignored (9.4). Urgent: the URG flag and pointer are ignored and the byte stays in line (RFC 6093's advice) |

### 5.4 Sending

S is a ring of `snd_buf` bytes holding everything taken from the application and not yet acknowledged. A segment is built from
S at `egress` time directly in a transmit slot (no per-connection frames are held between calls, so a connection owns no slot).
The usable window is `min(cwnd, SND.WND) - (SND.NXT - SND.UNA)`. Nagle: a segment smaller than the MSS is sent only if nothing
is unacknowledged, or `TCP_NODELAY` is set, or it carries the FIN. Sender-side silly-window avoidance: send if a full segment
fits, or at least half the largest window the peer ever offered, or everything queued fits and Nagle allows. PSH is set on the
segment that empties S. One connection gets at most 8 segments per `egress` call (round robin over connections that can send),
so a bulk sender cannot take every transmit slot.

### 5.5 The retransmission timer (RFC 6298)

`SRTT`, `RTTVAR` and `RTO` in integer arithmetic exactly as the RFC states it (`alpha = 1/8`, `beta = 1/4`, `K = 4`, clock
granularity G = 1 ms): first sample `SRTT = R, RTTVAR = R/2`; `RTO = SRTT + max(G, 4 * RTTVAR)`. One segment is timed at a time
and a retransmitted segment is never timed (Karn). **Initial RTO 1 s; minimum 200 ms; maximum 60 s** (the RFC says the minimum
SHOULD be 1 s; 200 ms is today's common practice and round trips here are well under a millisecond: a recorded deviation). On
expiry: retransmit the earliest unacknowledged segment, double the RTO, restart the timer, and apply 5.7's loss rule. Give up
when the oldest unacknowledged byte has been outstanding for **100 s** (RFC 1122's R2): the connection is reset locally with
reason `TimedOut`. SYN: retransmitted after 1, 2, 4 and 8 s, failure at 15 s (RFC 1122 asks for three minutes: a recorded
deviation, a toy on a local link); if a SYN was retransmitted the initial RTO after the handshake is 3 s (RFC 6298 5.7).
SYN-ACK: retransmitted after 1, 2 and 4 s, then the half-open connection is dropped.

### 5.6 Receiving, acknowledging and the window

R is a ring of `rcv_buf` bytes (at most 65,535) addressed by sequence number from `RCV.NXT`, with a bounded list of at most 8
out-of-order ranges; a segment that would need a ninth range is dropped (counted `tcp_ooo_dropped`). The window advertised is
the free space in R measured from `RCV.NXT`; its right edge **never moves left**. Receiver silly-window avoidance: the
advertised window grows only in steps of at least `min(MSS, rcv_buf / 2)`.

ACK rules: an ACK is sent **at once** for an out-of-order segment, for a segment that fills a hole, for a FIN, for a segment
that fails the sequence check, and when a window update of at least two segments or half of R becomes possible; otherwise an ACK
is **delayed** until a second full-sized segment arrives or **40 ms** pass, whichever is first (RFC 5681 4.2; the limit is
500 ms). **Quick-ACK rule:** the first 16 data segments of a connection, and the first after the connection has been idle for
longer than its RTO, are acknowledged at once, because the peer's congestion window is then small and slirp starts with one or
two segments (3.2). The 40 ms and the 16 are starting values; the benchmark of 14.3 is how they get revised.

### 5.7 Congestion control (RFC 5681, RFC 6582)

Counted in bytes. **Initial window** per RFC 5681: 3 segments for an MSS of 1460 (4 below 1096, 2 above 2190); 1 segment if the
SYN or SYN-ACK was retransmitted. `ssthresh` starts unbounded. **Slow start** (`cwnd < ssthresh`): `cwnd += min(bytes newly
acknowledged, MSS)` per ACK. **Congestion avoidance:** `cwnd += MSS * MSS / cwnd` per ACK (at least 1). **Timeout:**
`ssthresh = max(FlightSize / 2, 2 * MSS)`, `cwnd = 1 MSS`. **Three duplicate ACKs** (same ACK number, no data, no window change,
something outstanding): retransmit the first unacknowledged segment, `ssthresh` as above, `cwnd = ssthresh + 3 * MSS`, record
`recover = SND.NXT`; each further duplicate adds one MSS; a **partial ACK** (below `recover`) retransmits the next hole, deflates
by the amount acknowledged and stays in recovery; a full ACK sets `cwnd = ssthresh` and leaves it. **After idle** longer than
the RTO, `cwnd = min(cwnd, initial window)`. The current `cwnd`, `ssthresh`, state and RTO are readable by tests (and by
`GetStats` per connection), which is what lets a packet script assert them exactly.

### 5.8 Closing

* **The application closes its write side** (`socket_shutdown`, or closing the stream): when S and A are empty the stack sends
  FIN (FIN-WAIT-1 or LAST-ACK). The FIN is retransmitted like data.
* **The peer's FIN:** when everything before it has been written into the client's socket the stack sends the notice `PeerFin`
  on the endpoint channel and **then** shuts down its socket end, so the client reads to end-of-file. Section 8.5 gives the
  rule that makes a reset distinguishable from this.
* **TIME-WAIT lasts 10 s** (2 x MSL with an MSL of 5 s; the RFC's MSL is 2 minutes: a recorded deviation). A TIME-WAIT record is
  small, holds no buffers and no client objects, and lives in a table of 64 whose oldest entry is evicted when full. A
  retransmitted FIN is re-ACKed and restarts the wait; a SYN with a higher sequence number than the old connection's last is
  accepted as a new connection (RFC 1122 4.2.2.13).
* **Close with unread data** (bytes in R, or received after the client's socket is gone): the connection is **reset**, not
  closed (RFC 1122 4.2.2.13), so the peer learns its data was not read.
* **Abort** (the `Abort` request, used for `SO_LINGER` with zero time): RST at once.
* **Orphans:** when the client's channel and socket close with data still in A or S (a program that writes and exits), the
  connection keeps draining with no owner, as Unix does. An orphan is bounded: at most 20 s, and section 6.3's limits; when
  either is exceeded it is reset.

### 5.9 Listening

A listener is `(local address or any, port)`. **The backlog is the set of accepts the client has posted** (8.4): each posted
accept carries the channel and socket for one future connection. A SYN for a listening port takes a free posted accept and
reserves the connection's buffers from the account (section 6); the connection goes to SYN-RECEIVED and, when the handshake
completes, to ESTABLISHED on that accept's channel and socket. If there is **no free posted accept, or the account cannot
reserve the buffers, the SYN is dropped silently and counted** (`tcp_listen_overflow`): no RST, so the peer retries. A
half-open connection whose SYN-ACK goes unanswered frees its accept after 7 s (5.5). There are no SYN cookies: a SYN flood can
occupy a listener's posted accepts, which denies that listener and costs nobody else anything (recorded in TIDY-UP). A dropped
SYN costs the peer 6 s under slirp (3.2), so the C library posts the whole `listen` backlog and re-posts on every `accept`
(9.2).

### 5.10 Initial sequence numbers, the boot secret and ports (PROPOSAL)

`ISN = M + F(local address, local port, remote address, remote port, secret)` (RFC 6528), where `M` is the kernel's monotonic
clock in 4-microsecond units and `F` is SipHash-2-4 truncated to 32 bits. The **secret** is 16 bytes in a one-page **secret
VMO** that `netsup` creates at boot and holds for the whole boot; `netd` gets a read-only handle and nobody else gets any.
Because `M` is the kernel's clock and the secret outlives `netd`, **a restarted stack continues the same ISN sequence**: for
any connection four-tuple the ISN only moves forward within a boot, which is the property NET-DESIGN section 9 asked the
supervisor to keep, with no TCP state persisted. Ephemeral ports (49152 to 65535) are chosen with RFC 6056's hash-based
algorithm keyed by the same secret, for TCP and (changing NET-SPEC 6.6's rotating start) for UDP, so a resolver's source port
is not predictable either.

Where the 16 bytes come from is Q11 (entropy), which is open, and design review D15 asked for it before S8. Two ways:

* **(a) Recommended:** QEMU gets `-device virtio-rng-device` and a one-shot program `rngseed`, started by `netsup` before
  `netd`, claims the device through `devmgr` (a new claim, like the net claim), reads 32 bytes, writes them into the secret
  VMO and exits. It is a second small virtio driver (about the size of the queue code `blkd` and `netdrv` already share) and
  proves D6's "several device kinds". It is a DMA-capable driver, so it is its own step with a Fable review (step 5).
* **(b) Stand-in:** `netsup` fills the secret from SipHash over the MAC, the clock at several points of boot and the DHCP
  exchange. That is guessable by anyone who can observe the boot, and it is recorded in TIDY-UP as a medium-severity debt.
  Under slirp nothing off-path can reach the guest, which is why the stand-in is tolerable for a first S9.

Tests always inject a fixed secret, so nothing in section 14 depends on which is chosen.

### 5.11 The stack restarts: connections die with `netd` (the contract)

TCP state is not persisted or replicated (NET-DESIGN section 9). When `netd` dies:

* every endpoint channel and every stack-held socket end closes; a client sees its control channel peer-closed **without**
  a `PeerFin` or `Reset` notice having arrived, which the client library reports as a reset (`ECONNRESET`), never as
  end-of-file (8.5);
* the new `netd` has no connections and no listeners; **any segment for a connection it does not know gets a reset** (5.2), so
  the remote end finds out on its next transmission (at the latest its next retransmission) instead of hanging;
* listeners are gone until their owners open them again; the client library does not re-open sockets by itself (a
  connection cannot be resumed and a listener's owner must decide); `httpd` shows the pattern (it re-opens with the registry
  retry of NET-SPEC 7.5);
* ISNs and ports do not repeat (5.10).

A **driver** restart is a link flap: connections survive it and TCP retransmits (NET-DESIGN section 9, Q26).

### 5.12 IP fragmentation: still none (PROPOSAL)

S8 drops fragments and counts them. **S9 keeps that.** Reasons, each checked in 3.2: TCP never needs it (both sides use an MSS
of at most 1460 and slirp's largest frame in a 200 KB transfer was 1494 bytes, never a fragment); DNS without EDNS is limited
to 512 bytes by the protocol and the observed replies were 61 to 112 bytes; the only fragments slirp produced were host UDP
datagrams above 1472 bytes sent to a forwarded port, which no S9 program needs. The stack never sends a fragment (a TCP segment
is cut to the MSS; a UDP send above 1472 bytes is still `TooBig`). `rx_fragment` keeps counting, so a real need would be
visible. Reassembly (a bounded buffer, a timer and an old attack surface) stays out until a client needs it.

### 5.13 Loopback (PROPOSAL)

A packet addressed to the interface's own address or to 127.0.0.1 is queued inside the core (at most 32 packets; a full queue
drops, TCP retransmits) and fed back to `ingress` on the next iteration without touching the device. It costs a few dozen lines
and buys a great deal for testing: a client and a server in the guest can exercise TCP, the C library and `httpd` with no host
script, in the smoke boot, at any hart count. Loopback packets never reach a ring (N23).

### 5.14 ICMP and TCP

A destination-unreachable for a connection in SYN-SENT is remembered as the connection's soft error and reported if the
connect then times out (`EHOSTUNREACH` instead of `ETIMEDOUT`); for an established connection it is counted only (RFC 5927's
advice: an ICMP error is not a reset). The quoted header must match a live connection and a sequence number in flight, or it is
ignored.

### 5.15 Counters

Per stack: segments in and out, `tcp_checksum`, `tcp_bad_header`, `tcp_no_connection`, `tcp_rst_sent`, `tcp_rst_rate_limited`,
`tcp_challenge_ack`, `tcp_listen_overflow`, `tcp_ooo_dropped`, `tcp_retransmit_timeout`, `tcp_fast_retransmit`,
`tcp_partial_ack`, `tcp_persist_probe`, `tcp_delayed_ack`, `tcp_reset_received`, `tcp_timed_out`, `tcp_orphans`,
`tcp_orphan_reset`, `tcp_options_seen` (window scale, SACK, timestamps offered by peers), `client_write_wait`,
`client_quota_wait`. All through `GetStats`, the structured output and the flight recorder's new event kinds.

## 6. Memory accounting (PROPOSAL)

### 6.1 What is charged to whom

| Memory | Held by | Charged to | Bound |
|---|---|---|---|
| A and B (the kernel socket's two buffers, up to 64 KiB each) | the kernel | **the client's job** (OD59; the client created the pair) | the client's job IPC quota |
| The endpoint channel's queues | the kernel | the client's job | the client's job IPC quota |
| S and R (`snd_buf + rcv_buf`) | `netd`'s heap | **the client's account** in `netd`, as a reservation | the account's `stream_bytes` limit; the pool |
| The connection record (about 300 bytes), timers | `netd`'s heap | the account, as one connection | the account's connection limit |
| A posted accept (its channel and socket are the client's) | `netd`: a small record | the account, as one connection; **no buffer reservation until a SYN takes it** | the listener's accept limit |
| TIME-WAIT records, the loopback queue, challenge-ACK and RST limiters | `netd` | nobody (fixed tables) | section 12 |
| Orphans (5.8) | `netd` | their account while it lives, then the orphan pool | 6.3 |

### 6.2 The reservation rule

A connection's `snd_buf` and `rcv_buf` are chosen when it is made (from the `Connect` or `Accept` request, default 16 KiB each,
4 KiB to 64 KiB, `rcv_buf` capped at 65,535) and **reserved in full against the account at that moment**: on `Connect`, or
when a SYN takes a posted accept. If the account's `stream_bytes` limit or the pool cannot cover it the answer is `Quota` (for
`Connect`) or a dropped SYN and an `AcceptStarved` notice to the listener (at most one a second). The reservation is released
exactly when the connection record is freed (after TIME-WAIT is entered, since a TIME-WAIT record has no buffers). The memory
itself may be allocated lazily; what matters is that **the sum of reservations never exceeds the pool, and the pool is what
`netd`'s own job limit is sized for** (N13). Nothing a remote peer sends can make `netd` use memory that was not reserved by a
local client first.

### 6.3 Orphans

An orphan keeps its reservation. While its account exists it stays charged there (a program that leaves many draining
connections behind uses its own budget). When the account is gone (the program ended and its root connection closed), the
orphan moves to the **orphan pool**: at most 32 connections and 512 KiB. If the pool is full the oldest orphan is reset. Every
orphan is reset 20 s after it lost its owner if it has not finished.

### 6.4 The client's own quotas

The launcher's defaults (3.3) do not fit a network program: one busy stream can hold 128 KiB of the job's 192 KiB of IPC bytes
and uses 4 of its 256 objects. **Proposal:** a program whose view includes a network name is launched with an IPC limit of
1 MiB and an object limit of 512 (launcher policy, in one place, overridable); P7's measurement confirms or changes the
numbers. The system-wide 2 MiB IPC budget is then reachable by two such programs with stalled streams; that is recorded (T79
already says the constants assume a small machine) and is open point 3.

## 7. How S9 fits the S8 architecture

### 7.1 The core and the seam

Unchanged in shape. The core gains (a sketch; the types are the implementer's):

```text
core.tcp_listen(account, addr, port) -> ListenerId            core.tcp_post_accept(listener, opts) -> ConnId
core.tcp_connect(account, local, remote, opts, now) -> ConnId core.tcp_close(conn) / tcp_shutdown_write(conn) / tcp_abort(conn)
core.tcp_tx_space(conn) -> usize                              core.tcp_tx_push(conn, bytes: &[u8]) -> usize
core.tcp_rx_peek(conn) -> (&[u8], &[u8])                      core.tcp_rx_consume(conn, n)
core.tcp_rx_direct(...)   // ingress hands an in-order payload to the adapter before copying it into R (4.4)
events: Connected{conn, local, remote} | PeerFin{conn} | Reset{conn, reason} | Closed{conn} | AcceptStarved{listener}
```

Bytes in, bytes out, time as an argument: the core makes no kernel call and knows nothing about sockets. That is what lets the
packet scripts, the lossy suite and the smoltcp peer drive exactly the code `netd` runs.

### 7.2 `netd`'s loop

Still one thread and one port (T67: **no per-connection threads**; NET-DESIGN section 8 and the owner's decision stand, and
nothing here needs more). NET-SPEC 8.1's iteration gains two phases and keeps every phase bounded:

1. read at most N = 32 receive frames, `core.ingress` each (in-order TCP payload is offered to the client's socket directly);
2. apply events: datagrams as before; for at most P = 32 connections with bytes in R (round robin), `socket_write` what the
   client's socket takes;
3. serve at most K = 16 client messages (service connections and endpoint channels);
4. for at most P = 32 connections whose S has room and whose socket is readable (round robin), `socket_read` at most
   `min(room, 16 KiB)` and `tcp_tx_push` it; a socket that reports end-of-stream becomes `tcp_shutdown_write`;
5. `core.egress` for at most M = 32 frames (at most 8 per connection, 5.4), publish, reclaim;
6. doorbells and the waiting handshake;
7. wait on the port until `min(core.next_deadline(), now + 10 ms)`.

Bindings per stream: the endpoint channel (READABLE and PEER_CLOSED, level-triggered, as every R connection), and on the socket
end two **one-shot** bindings, one for READABLE, PEER_CLOSED and WRITE_CLOSED (armed only while S has room) and one for
WRITABLE and PEER_CLOSED (armed only while R holds bytes the client's socket refused). One-shot matters twice: a level-triggered
binding would spin while S is full or the client is at quota, and T60 makes every binding a cost to the whole system (P8).
Doorbells remain events; a stream needs none of its own, because the kernel socket's signals are the wake.

### 7.3 Accounts and endpoint limits

Unchanged mechanism (NET-SPEC 7.4): every endpoint, stream, listener and posted accept is a connection of the account of the
service connection that opened it. S9 adds one limit to the account, `stream_bytes` (default 512 KiB), and one to the pool
(4 MiB); the connection limits stay 32 per account and 256 in the pool as starting values (16 default-sized streams fit an
account's byte limit; the pool's byte limit is reached by 128). The launcher can override both per program.

### 7.4 Names and classes of authority

The three names stay; no new name and no new right. What each class may do with streams:

| Name | May | May not |
|---|---|---|
| `/svc/net` | connect to any address and port; listen on ports 1024 to 65535; resolve names (DNS is ordinary UDP to the configured server) | listen below 1024; connect from a chosen local port below 1024 |
| `/svc/net-priv` | everything above, and listen on or bind any port | configure |
| `/svc/net-config` | read per-connection state through `GetStats` (addresses, state, counters; never data) | open endpoints |

`httpd` in the demo listens on 8080 with a normal view. There is still no ambient network: a program without a name in its
view cannot open a socket, and the C library's `socket()` then fails with `EACCES` at the first call that needs the stack.

### 7.5 Supervision

The tree, the intensity limits and the contracts of NET-SPEC section 9 are unchanged. `netsup` additionally creates and holds
the **secret VMO** (5.10) and, if open point 4 chooses it, starts `rngseed` once before the first `netd`. `netd` restarts do
not touch it. The flight recorder gains event kinds for connection open, close, reset (with reason) and retransmission
timeout, so a crash dump shows what TCP was doing.

### 7.6 Timers and time

The core's time stays the opaque nanosecond `Instant`; every TCP timer (retransmission, delayed ACK, persist, TIME-WAIT, the
SYN and orphan limits, the quota retry of 4.4) is an entry behind the one `next_deadline`, and the adapter reads the clock
once per iteration. A connection has at most one armed deadline (the earliest of its timers) in a structure ordered by time.
**Invariant: a timer never fires early** (N20); it may fire late by the loop's own latency, which is at most the 10 ms safety
poll, and every constant in 5.5 to 5.8 tolerates that. With 1 ms ticks (until the nanosecond ABI) round-trip samples are
quantised to 1 ms, which only means the RTO sits at its 200 ms floor.

What the later scheduler tasks change, none of them required: **task 13** (one-shot timers and timer slack: 50 microseconds by
default in the normal band, at most 1 ms) lets nearby TCP deadlines share a wake and is far below any TCP constant, so the
"never early, late by at most the slack plus the loop" statement holds unchanged; **task 21** removes T60's per-binding cost and
lifts the pressure on the binding count; **tasks 22 and 24** would cut the latency of the control calls; **task 26** would let a
blocking C-library call be interrupted (until then `SO_RCVTIMEO` and `poll` timeouts are the only way out of a wait, 9.3).

## 8. The protocol (`proto::net` additions)

All integers little-endian; every decoder total, with goldens (as NET-SPEC section 7).

### 8.1 Service connection

`Open` (op 2) gains kinds **3 `TcpStream`** and **4 `TcpListener`**. Request body as before (`kind, local_port, local_addr,
oflags`) with one handle, the client's end of a new endpoint channel. For a stream the binding is optional (port 0: chosen at
connect). `oflags` gains bit 2 `NODELAY`. New statuses: 10 `ConnRefused`, 11 `TimedOut`, 12 `ConnReset`, 13 `NotConnected`,
14 `AlreadyConnected`, 15 `HostUnreachable`, 16 `BadHandle` (a passed handle is not a usable socket, 8.3). `GetStats` gains a
per-connection block for the config class.

### 8.2 Stream endpoint channel

Client to stack:

| Op | Body | Handles |
|---|---|---|
| 16 `Connect` | `cookie u32, dst_addr u32, dst_port u16, pad u16, snd_buf u32, rcv_buf u32` (0: default) | **one: an end of a socket pair the client created** |
| 17 `Abort` | none | |
| 18 `SetOption` | `option u16, pad u16, value u32` (1 `NODELAY`) | |

Stack to client (all `Notice`-shaped, op 0x83 with these codes, `cookie` echoing the request where there is one):

| Code | Name | Detail |
|---|---|---|
| 16 | `Connected` | `local_addr, local_port, remote_addr, remote_port` |
| 17 | `ConnectFailed` | a status: `ConnRefused`, `TimedOut`, `HostUnreachable`, `NoRoute`, `Quota`, `NotConfigured`, `BadHandle` |
| 18 | `PeerFin` | the peer closed its side; the socket's end-of-file after this is clean |
| 19 | `Reset` | a reason: 1 the peer reset, 2 retransmission gave up, 3 aborted locally, 4 the link or configuration went away |

There is no data op and no shutdown op: **bytes travel only on the socket**, and half-close is the kernel's `socket_shutdown` on
the client's end, which the stack sees as end-of-stream after draining A and turns into a FIN. Closing the channel and the
socket is a normal close (5.8).

### 8.3 What the stack checks about a passed socket

Before using it: a zero-length `socket_read` and `socket_write` succeed (it is a socket, with READ and WRITE); it can be bound
(WAIT); its koid and peer koid (`object_info`) are not those of any socket the stack already holds, and are not each other's
(both ends of one pair). Failure is `BadHandle` and the handle is closed. Bytes already in the socket when it arrives are
simply the first bytes to send. The stack cannot check *who* created the pair, and does not need to: whoever did is paying, and
it is not the stack.

### 8.4 Listener endpoint channel and posted accepts

Client to stack: **`Accept`** (op 19): `cookie u32, snd_buf u32, rcv_buf u32` with **two handles: the client's end of a new
endpoint channel, and an end of a new socket pair**. A listener holds at most 16 posted accepts (more is `Quota`); each counts
as a connection of the account from the moment it is posted. Stack to client, on the listener's channel: `Accepted` (code 20:
`cookie, remote_addr, remote_port`) when a posted accept became an established connection, and `AcceptStarved` (code 21) when
SYNs are being dropped for want of accepts or budget. The new connection's own channel then carries `Connected` and behaves as
any stream. Because `Accepted` arrives on the one listener channel, a listener is a single thing to wait on.

### 8.5 End-of-file and reset: the rule

The data socket alone cannot tell a clean close from an abort (both end the stream). So: **end-of-file on the data socket is a
clean end only if `PeerFin` has arrived on the endpoint channel.** The stack always sends `PeerFin` before shutting down its
socket end, and `Reset` before closing it. A client library that reads end-of-stream therefore looks at the channel: `PeerFin`
means end-of-file; `Reset`, or the channel closed with neither, means `ECONNRESET`. This is what makes a stack crash, a peer's
RST and a retransmission timeout all visible as errors and never as a short but "successful" download (N18). It answers D4's
"RST versus clean EOF has no channel".

### 8.6 The client library (`rt::net`)

Adds `net.connect(addr, port, opts, deadline) -> Stream`, `net.listen(port, backlog) -> Listener`, `listener.accept(deadline) ->
Stream`, `stream.read/write` (thin wrappers over the socket that apply 8.5), `stream.shutdown_write()`, `stream.abort()`,
`net.resolve(name, deadline)` (section 11). Every call that talks to the stack carries a deadline (the message-loop rule);
`connect`'s default is 20 s, above the stack's own 15 s SYN limit.

## 9. Sockets in the C library (PROPOSAL)

BSD sockets are a shim over section 8, never the reverse (OD74, the roadmap's layered-API direction). The shim lives in
`libc/src/socket.rs` with its decisions (state per descriptor, the errno table) in a pure, host-tested module.

### 9.1 The descriptor

The descriptor table (64 entries today, **raised to 256**) gains a third kind beside the standard stream and the file: a
socket, in one of the states `New` (nothing opened yet: `socket()` talks to nobody), `Udp`, `Connecting`, `Stream`,
`Listener`, `Dead(errno)`. The process's `/svc/net` connection is opened lazily by the first call that needs it and shared by
all sockets; if the program's view has no network name that call fails with `EACCES`.

### 9.2 What maps to what

| C call | Native |
|---|---|
| `socket(AF_INET, SOCK_STREAM or SOCK_DGRAM, 0)`, with `SOCK_NONBLOCK`/`SOCK_CLOEXEC` accepted | a `New` descriptor; nothing sent |
| `bind` | remembered; for UDP an `Open{Udp}` now; for TCP used by the later `Open` |
| `connect` (TCP) | `Open{TcpStream}`, `socket_create`, `Connect` with one end; wait for `Connected` or `ConnectFailed` |
| `connect` (UDP) | records the default destination and filters received datagrams by it (in the library) |
| `listen(fd, backlog)` | `Open{TcpListener}`, then posts `min(backlog, 16)` accepts (at least 1), each a new channel and socket pair |
| `accept`, `accept4` | waits for `Accepted` on the listener's channel, returns that posted pair as a new descriptor, **posts a replacement** |
| `read`, `recv`, `write`, `send` on a stream | `socket_read`/`socket_write` on the data socket, with 8.5's rule at end-of-stream |
| `sendto`, `recvfrom`, `sendmsg`/`recvmsg` with one buffer and no ancillary data | `SendUdp` / `Recv` on the endpoint channel |
| `shutdown(SHUT_WR)` | `socket_shutdown`; `SHUT_RD` only marks the descriptor; `SHUT_RDWR` is both |
| `close` | closes the channel and the socket (a normal close, 5.8); `SO_LINGER {1, 0}` sends `Abort` first |
| `getsockname`, `getpeername` | from the `Open` reply and `Connected` |
| `setsockopt`/`getsockopt` | 9.4 |
| `poll`, `select` | 9.3 |
| `getaddrinfo`, `freeaddrinfo`, `gai_strerror`, `inet_pton`, `inet_ntop`, `htons` and friends | section 11 and plain code |
| `fcntl(F_GETFL/F_SETFL)` with `O_NONBLOCK` | a flag on the descriptor |

### 9.3 Blocking, nonblocking, `poll` and `select`

* The native objects are always nonblocking; **blocking is done in the library** (NET-DESIGN 2.2's lesson from Fuchsia) by waiting on
  the object's signals. A blocking `read` waits for READABLE, PEER_CLOSED or WRITE_CLOSED on the data socket; a blocking `write`
  for WRITABLE or PEER_CLOSED; a blocking `connect` or `accept` for READABLE on the control channel. The wait has the deadline
  of `SO_RCVTIMEO`/`SO_SNDTIMEO` if set and none otherwise. (A wait with no deadline is allowed here: it is a wait on the
  program's own objects, as a pipe read is, not a call to a server. Every *request* to the stack carries a deadline.)
* **Nonblocking:** `EAGAIN` where a blocking call would wait; a nonblocking `connect` returns `EINPROGRESS`, the descriptor
  becomes writable in `poll` when the outcome is known, and `getsockopt(SO_ERROR)` reports it.
* **`poll`** creates (once per thread, then reuses) a port, binds the objects behind each descriptor one-shot with the masks
  below, calls `port_wait` with the timeout, and unbinds. One thread per port, as the platform requires until task 21. A binding
  costs 32 bytes of the program's IPC quota while it exists. `select` is a wrapper over `poll` with `FD_SETSIZE` 256.

| Descriptor | `POLLIN` | `POLLOUT` | `POLLHUP` / `POLLERR` |
|---|---|---|---|
| stream | data socket READABLE, PEER_CLOSED or WRITE_CLOSED | data socket WRITABLE | control channel READABLE with `Reset`, or closed |
| connecting stream | never | control channel READABLE (the outcome arrived) | a failed connect sets `POLLERR` too |
| listener | listener channel READABLE (`Accepted` queued) | never | channel closed |
| UDP | endpoint channel READABLE | always | channel closed |
| file | always | always | never |
| standard stream | socket READABLE or closed | socket WRITABLE | |

* A blocked call cannot be interrupted until `thread_interrupt` exists (task 26); `EINTR` is defined and never returned.

### 9.4 What is refused, and how (never silently)

| Feature | Answer |
|---|---|
| `AF_INET6`, `AF_UNIX`, any other family | `EAFNOSUPPORT` |
| `SOCK_RAW`, `SOCK_SEQPACKET`, a non-zero protocol that does not match | `EPROTONOSUPPORT` |
| `SIGPIPE`, `SIGIO`, `SIGURG`, `O_ASYNC` | no asynchronous signals exist: a write to a closed stream is **always** `EPIPE` (as if `MSG_NOSIGNAL`); `MSG_NOSIGNAL` is accepted and means nothing |
| `MSG_OOB`, `SO_OOBINLINE` | `EOPNOTSUPP` (no urgent data, 5.3) |
| `MSG_PEEK` | `EOPNOTSUPP` (the kernel socket has no peek; recorded, since some programs want it) |
| `sendmsg`/`recvmsg` with ancillary data (`SCM_RIGHTS`) | `EOPNOTSUPP`: handles travel by the native protocol, not as descriptors |
| `SO_KEEPALIVE`, `SO_REUSEPORT`, `SO_BROADCAST` on a normal view, `IP_*` and `TCP_*` options other than `TCP_NODELAY` | `ENOPROTOOPT` (or `EACCES` for broadcast without the privileged name) |
| `SO_LINGER` other than off or `{1, 0}` | `EINVAL` |
| binding below 1024 or broadcasting without `/svc/net-priv` | `EACCES` |
| `ioctl` interface configuration, `getifaddrs` | `ENOTTY` / not provided: configuration is the config capability's |
| `epoll`, `kqueue`, `sendfile`, `socketpair`, `gethostbyname`, `/etc/hosts`, `/etc/resolv.conf`, `/etc/services` | not provided |
| sockets across `fork` | there is no `fork` until S11; the shim says so and S11 must revisit it |

**Accepted:** `SO_REUSEADDR` (success; the stack already allows a listener to bind while old connections are in TIME-WAIT),
`SO_RCVBUF`/`SO_SNDBUF` before `connect` or `listen` (they become `rcv_buf`/`snd_buf`, clamped to 4 to 64 KiB), `SO_RCVTIMEO`,
`SO_SNDTIMEO`, `SO_ERROR`, `SO_TYPE`, `TCP_NODELAY`, `MSG_DONTWAIT`, `MSG_WAITALL`.

### 9.5 The errno table

New values (Linux numbering, as the existing ones): `ENFILE` 23, `ENOTSOCK` 88, `EDESTADDRREQ` 89, `EMSGSIZE` 90,
`ENOPROTOOPT` 92, `EPROTONOSUPPORT` 93, `EOPNOTSUPP` 95, `EAFNOSUPPORT` 97, `EADDRINUSE` 98, `EADDRNOTAVAIL` 99, `ENETDOWN`
100, `ENETUNREACH` 101, `ECONNRESET` 104, `ENOBUFS` 105, `EISCONN` 106, `ENOTCONN` 107, `ECONNREFUSED` 111, `EHOSTUNREACH` 113,
`EALREADY` 114, `EINPROGRESS` 115, `EINTR` 4.

| Native outcome | errno |
|---|---|
| `Refused`; no network name in the view | `EACCES` |
| `AddrInUse` | `EADDRINUSE` |
| `TooBig` | `EMSGSIZE` |
| `NoRoute` / `HostUnreachable` | `ENETUNREACH` / `EHOSTUNREACH` |
| `NotConfigured`; the registry still `UNAVAILABLE` after the retries; a listener or UDP endpoint whose stack died | `ENETDOWN` |
| `ConnRefused` / `TimedOut` / `ConnReset`, and a stream whose stack died | `ECONNREFUSED` / `ETIMEDOUT` / `ECONNRESET` |
| `NotConnected` / `AlreadyConnected` | `ENOTCONN` / `EISCONN` |
| **`Quota`** (the account's connection or byte limit) | **`EMFILE`** ("this process has too many") |
| **`NoBuffers`** (the stack's pool) | **`ENOBUFS`** |
| kernel NO_MEMORY from `socket_create`, `channel_create` or a data write (the program's own job quota, or the kernel's memory: indistinguishable today) | `ENOBUFS` |
| kernel SHOULD_WAIT, or a receive timeout | `EAGAIN` |
| write to a stream the peer closed or that was shut down | `EPIPE` (`ECONNRESET` if a `Reset` notice arrived) |
| `NotSupported` | `EOPNOTSUPP` |
| `BadRequest`, `BadHandle` | `EINVAL` (a library bug; also logged) |

**On the QUOTA status** (design review D16 asked for NO_MEMORY to be split before the C library maps errno): the protocol
already separates the two cases that matter to a socket user (`Quota` and `NoBuffers`), so **S9 does not need the kernel
change**. Where it would still help is the third NO_MEMORY row: a program cannot tell "my job's quota" from "the kernel is out
of memory". That is a kernel ABI change with its own slice; it is open point 6, recommended but not required here.

## 10. HTTP: the server, the fetch tool and SHA-256

### 10.1 `httpd` (the demo server; PROPOSAL for its scope)

A native program (`init/src/bin/httpd.rs`) on `rt::net` and `rt::fs`, with its parser in a pure crate **`http1`** (`no_std`,
`forbid(unsafe_code)`, fuzzed), because it reads hostile input from anyone who can connect. `httpd [-p PORT] ROOT`; the demo
serves `/www` from the memory filesystem on port 8080 (a normal view suffices). One thread, one port: the listener and up to
8 connections are state machines (the native API makes that natural).

* **Requests:** `METHOD SP target SP HTTP/1.0 or HTTP/1.1 CRLF`, header lines, an empty line. HTTP/1.0 semantics for both
  versions: one request per connection, `Connection: close` always, the response says `HTTP/1.1`. `GET` and `HEAD` only.
  Headers are syntax-checked and otherwise ignored; a request that announces a body (`Content-Length` above 0 or
  `Transfer-Encoding`) is refused.
* **Targets:** origin form only (a leading `/`); the query is cut off; percent-escapes are decoded once (`%00` and an encoded
  `/` are refused); the path is split into segments and **any `..` segment is refused**; a directory serves its `index.html`
  if there is one; there are no listings.
* **Responses:** status line, `Content-Length`, `Content-Type` (a table of about a dozen extensions, otherwise
  `application/octet-stream`), `Connection: close`, `Server`; the file is streamed in 16 KiB pieces. After the body the server
  half-closes and reads until the peer closes or 1 s passes, so a client still sending does not get a reset.
* **Malformed or abusive requests:** each gets a status and a close, never a crash: 400 (bad syntax, bad escape, `..`, a
  body), 404, 405 with `Allow: GET, HEAD`, 408 (the head did not arrive within 5 s), 414 (target over 1024 bytes), 431 (head
  over 4096 bytes or more than 32 header lines), 500 (the file could not be read), 505 (another HTTP version). If a file
  cannot be finished after the head was sent the connection is **aborted** (a reset), so a truncated body is never mistaken
  for a complete one.
* **Limits:** 8 connections at once (the backlog is 8 posted accepts; more SYNs wait for the peer's retry), 5 s for a request
  head, 30 s for a whole exchange.
* When the stack restarts, `httpd` re-opens its listener (5.11).

### 10.2 `fetch`

`fetch [-o FILE] [--sha256] [-m SECONDS] [--dns ADDR:PORT] URL`, a native program. `http://host[:port]/path` only (`https` is
refused with a clear message). It resolves the host (section 11; a dotted quad is used as it is), connects, and sends
`GET path HTTP/1.0`, `Host`, `User-Agent`, `Accept: */*`. HTTP/1.0 on purpose: a 1.0 client can never be sent a chunked body.
It reads the status line and a head of at most 8 KiB, then the body: **if `Content-Length` is present exactly that many bytes
must arrive** (fewer is `truncated`), otherwise the body ends at a clean end-of-file (8.5; a reset is an error). Redirects are
reported, not followed. With `--sha256` it prints one structured line, `##FETCH status=200 bytes=N sha256=<64 hex digits>`, which
is what the harness compares with the digest the **host** computed. Exit status: 0 only for a complete 2xx body; distinct
non-zero codes for a resolve failure, a connect failure, a non-2xx status, a truncated or reset transfer, and the overall
timeout (30 s by default).

### 10.3 `sha256` and `siphash`

Two small pure crates written here (OD18's rule that the stack is ours is not at stake, but there is no reason to import
them): `sha256` (FIPS 180-4, streaming) for `fetch`, and SipHash-2-4 inside `netcore` for ISNs and ports. Both are tested
against published vectors and against the host's own implementation (H20, H23).

## 11. The DNS resolver (PROPOSAL)

### 11.1 A library, not a server

A stub resolver in the client: a pure crate **`dns`** (message builder, parser, and the query state machine, sans-IO) used by
`rt::net::resolve` and by `getaddrinfo`. It needs only a UDP endpoint on `/svc/net`, so it adds no process, no registry name
and no authority; a program that may use the network may resolve. The price is that there is no shared cache and each
program parses DNS replies itself (in its own address space, at its own risk). A `resolved` service is the alternative and is
open point 8.

### 11.2 What it does

* **Server:** the `dns` field of `Info` (10.0.2.3 from DHCP), port 53; a test hook replaces it (`fetch --dns`, the environment
  variable `RESOLVER=ADDR:PORT` for C programs).
* **Query:** one question, type A, class IN, recursion desired, no EDNS. Each lookup opens its own UDP endpoint (so it gets a
  fresh keyed-random source port, 5.10) and closes it afterwards.
* **Answer accepted only if** it comes from the server's address and port, the identifier matches, it is a response, and the
  question section repeats ours (name compared without regard to case). Anything else is ignored and the wait continues.
* **Records:** A, and CNAME followed inside the same answer (at most 8 links). Names are decompressed with a bound on pointer
  jumps and on total length (253), so a pointer loop cannot hang or grow memory. AAAA and everything else are skipped.
* **Retries:** the query is re-sent with a new identifier after 1 s and after a further 2 s; failure 4 s after the last send
  (7 s in all). SERVFAIL counts as no answer. NXDOMAIN and "no A record" fail at once with distinct errors.
* **Truncation (TC set):** if the truncated answer already holds an A record it is used; otherwise the lookup fails with
  `Truncated`. **No TCP fallback** in S9 (slirp would support it, 3.2): recorded in TIDY-UP.
* **Cache:** per process, 16 entries, positive answers only, for `min(TTL, 300 s)`.
* **Built in:** a dotted quad is parsed locally; `localhost` is 127.0.0.1.
* **`getaddrinfo`:** `AF_INET` and `AF_UNSPEC` (answers only IPv4), numeric service only, `AI_NUMERICHOST`, `AI_PASSIVE`;
  errors `EAI_NONAME`, `EAI_AGAIN` (timeout), `EAI_FAIL`, `EAI_FAMILY`, `EAI_SERVICE`, `EAI_MEMORY`.

### 11.3 What it does not do, stated

No unpredictable query identifiers in the strong sense: there is no entropy service for an ordinary program (Q11), so the
identifier comes from a keyed hash of a counter and the clock. The source port *is* keyed by the stack's secret. Under slirp
the "server" is the hypervisor, so spoofing is not the threat it is on a real network; recorded in TIDY-UP against Q11.

### 11.4 Testing it hermetically

No name resolves through 10.0.2.3 unless the host has a working resolver (3.2), so the gate cannot depend on it. The harness
runs a small scripted DNS server on the host (reachable as 10.0.2.2:PORT) and the guest is pointed at it with the test hook;
one tier-2 check uses the real 10.0.2.3 and reports SKIP, not failure, when the host is offline.

## 12. Limits (one place: `proto::net::limits` and `nettcp::consts`; all starting values)

| Quantity | Value | Why |
|---|---|---|
| MSS announced; default assumed for a silent peer | 1460; 536 | MTU 1500; RFC 9293 |
| `snd_buf`, `rcv_buf` | default 16 KiB each; 4 KiB to 64 KiB; `rcv_buf` at most 65,535 | no window scaling; NET-DESIGN 7.4's "16 KiB or so" |
| Out-of-order ranges per connection | 8 | bounded bookkeeping; more loss than that is a timeout case |
| Account: connections; `stream_bytes` | 32; 512 KiB | S8's value; 16 default streams |
| Pool: connections; `stream_bytes` | 256; 4 MiB | sizes `netd`'s job limit |
| Posted accepts per listener | 16 | the largest backlog |
| Orphans | 32 connections, 512 KiB, 20 s each | 6.3 |
| TIME-WAIT | 10 s; 64 records, oldest evicted | 5.8 |
| RTO | initial 1 s, minimum 200 ms, maximum 60 s, granularity 1 ms | 5.5 |
| Give-up times | data 100 s; SYN after retries at 1, 2, 4, 8 s (15 s); SYN-ACK after 1, 2, 4 s (7 s) | 5.5 |
| Delayed ACK; quick-ACK count | 40 ms; 16 segments | 5.6 |
| Persist probe | starts at the RTO, doubles to 60 s, 1 byte | RFC 9293 3.8.6.1 |
| Initial window | 3 segments at MSS 1460 (RFC 5681); 1 after a retransmitted SYN | 5.7 |
| Duplicate-ACK threshold | 3 | RFC 5681 |
| RSTs sent; challenge ACKs sent | 100 per second each, in all | not a reflector |
| Loop bounds | N 32 frames, P 32 connections each way, K 16 messages, M 32 frames, 8 segments per connection per pass, 16 KiB per `socket_read` | 7.2 |
| Quota-wait retry | 20 ms doubling to 500 ms | 4.4 |
| Loopback queue | 32 packets | 5.13 |
| Bindings in `netd` per stream | at most 3 (two of them one-shot) | 7.2, T60 |
| Program quotas with a network view | IPC 1 MiB, objects 512 | 6.4, to be set by P7 |
| C library | 256 descriptors; `FD_SETSIZE` 256 | 9.1 |
| `httpd` | 8 connections; head 4096 bytes, 32 header lines, target 1024 bytes; 5 s head, 30 s exchange; 16 KiB pieces | 10.1 |
| `fetch` | head 8 KiB; 30 s overall | 10.2 |
| DNS | 3 sends (at 0, 1 s, 3 s), failure at 7 s; 512-byte replies expected; 8 CNAME links; cache 16 entries, at most 300 s | 11.2 |

## 13. Invariants (each gets a check; INVARIANTS.md gains N11 to N24 in step 8)

* **N11 Stream integrity.** The bytes delivered to an application are, at every moment, a prefix of the bytes its peer wrote,
  in order, with nothing repeated or missing; after a clean close they are equal. *Check:* H16, H17, Q20 to Q22, Q30.
* **N12 No stall.** A connection with unsent or unacknowledged data, undelivered data or an unfinished close always has a
  timer armed, a packet in flight or a wait registered on its client's socket. *Check:* the quiescence check of H16 and H17
  (no deadline, an empty link and an unfinished transfer is a failure); `fault-no-retransmit`.
* **N13 Reservation.** A connection's S and R never exceed its reservation; the sum of reservations never exceeds the pool;
  no segment from the network makes `netd` use memory a local client did not reserve first. *Check:* H19 against an
  independent recomputation; H12 and H17 with a counting allocator; Q27.
* **N14 The client pays for the kernel's buffers.** A and B and the endpoint channel's queues are charged to the client's
  job and never to `netd`'s. *Check:* P1, Q27.
* **N15 The stack never waits for a client and never loses acknowledged data.** A full or over-quota client socket leaves the
  bytes in R and closes the window; it is never an error and never a drop. *Check:* H17's stalled-reader schedules; Q27; P2.
* **N16 The window is honest.** The advertised right edge never moves left; no byte beyond it is accepted; no byte is sent
  beyond the peer's. *Check:* H13, H16 (asserted on every segment), Q23's flow check.
* **N17 No state change from an unacceptable segment.** A segment that fails the sequence check, a RST that is not exactly at
  `RCV.NXT`, a SYN on a synchronised connection, or an ACK for unsent data changes nothing but counters and may cause one
  rate-limited ACK. *Check:* H13's reset and injection scripts; `fault-accept-rst-anywhere`.
* **N18 End-of-file means the peer closed.** An application sees a clean end-of-file only if the peer's FIN was received and
  every byte before it delivered; every other end (reset, timeout, abort, stack death) is an error. *Check:* H17 (the model
  forbids EOF otherwise), Q25, Q31; `fault-eof-on-reset`.
* **N19 Connections do not outlive the stack, and strangers get a reset.** After a `netd` restart no old connection exists; a
  segment for an unknown connection gets a correctly formed RST, within the rate limit, and a RST never gets one. *Check:*
  H13, H17's restart schedules, Q25.
* **N20 Timers.** No timer fires before its time; the RTO stays within its bounds and only grows between acknowledgements.
  *Check:* H14 against an independent reference; H13.
* **N21 Congestion bounds.** `cwnd` is at least one segment; bytes in flight never exceed `min(cwnd, peer window)` by more
  than the one segment a retransmission or a probe adds. *Check:* H16 on every send; H14.
* **N22 Bounded lifetime.** A closed connection's record is freed within TIME-WAIT; an orphan within 20 s; a half-open one
  within 7 s; tables and pools return exactly to their start. *Check:* H19, Q31, Q32's exact leak checks.
* **N23 Loopback stays inside.** A loopback packet never reaches a ring, and no frame from the device is accepted with a
  127.0.0.0/8 address. *Check:* H13, Q20 with the capture (it must contain none of the loopback traffic).
* **N24 Sequence numbers do not repeat or leak.** For one four-tuple the ISN only moves forward within a boot, across `netd`
  restarts too; the secret is readable only by `netsup` and `netd` and never appears in output or the flight recorder.
  *Check:* H20, Q25 (ISNs before and after the restart, from the capture), a handle-table check as S8's N3.

N4 to N10 of NET-SPEC (validation, bounds, endpoint lifetime, accounting, never blocking, no packet-proportional allocation, no
panic) apply to everything new here; N6's wording needs the orphan exception (section 18).

## 14. Acceptance (runnable checks, written before the code)

Placement follows `TESTING.md` rule 5. Tier 0 = host, tier 1 = the smoke boot, tier 2 = soak. **Every check is written in step
0 or at the start of its step and fails until the step that makes it pass.** Numbering continues NET-SPEC's (H1 to H10, Q1 to
Q19). "Mutation check" means: introduce the stated bug once by hand, see the named test fail, remove it.

### 14.1 Host (tier 0, with larger seed counts in tier 2)

**`netwire` and the parsers** (step 2, step 7)
* **H11 goldens from a real capture.** `net/tests/data/slirp-tcp-probe.pcap` (step 0; 334 TCP segments from slirp and the fake
  guest: handshakes with and without options, bulk data, retransmissions, RSTs of both forms, FINs, duplicate ACKs, a zero
  window): every segment parses to the expected fields and rebuilds to the same bytes. The same file passes
  `net_pcap_check.py` and `tcpdump -nn -vv -r` (which printed `(correct)` for all 334 TCP checksums when this was written).
* **H12 totality.** Random bytes, truncations and mutations of valid inputs (200,000 cases each in tier 0; a `cargo-fuzz`
  target each for long runs) for the TCP segment and option parser, the DNS message parser, the `http1` request parser, the
  `fetch` response-head parser and the new `proto::net` messages: no panic, no out-of-bounds index, no allocation above a
  stated constant (a counting allocator), no loop without a bound (a step counter). *Mutation checks:* trust an option
  length; follow a DNS pointer without the jump limit.

**`nettcp`** (steps 2 and 3)
* **H13 packet scripts** (14.6 defines the format and lists the scenarios). Each script injects exact segments at exact
  virtual times and requires exact segments back; an unexpected or missing segment fails with both shown. Scripts taken from
  the figures of RFC 9293 (the three-way handshake, simultaneous open, recovery from an old duplicate SYN, half-open
  discovery, the normal close, simultaneous close) carry the figure number and use its numbers, so the expected exchange was
  written by the RFC's authors, not by this project. *Mutation checks:* one per rule in 5.2 and 5.5 to 5.8 (accept a RST
  anywhere in the window; no delayed ACK timer; no backoff; time a retransmitted segment; shrink the window; ACK beyond a
  hole; skip the checksum).
* **H14 timer and window arithmetic against independent references.** The RTO estimator against a reference written
  separately in the test with floating point, over random sample sequences (equal within one granule), plus hand-worked
  examples from RFC 6298's rules; `cwnd` and `ssthresh` against a separate straightforward model of RFC 5681 and RFC 6582 over
  random ACK, duplicate and timeout sequences. *Mutation checks:* `beta` and `alpha` swapped; additive increase per ACK
  instead of per window.
* **H15 interop with smoltcp** (dev-dependency only, pinned to one exact version; OD18 allows it as a test peer): over
  `netsim`, our stack as client against smoltcp as server and the reverse: connect, transfer 1 byte, 1 MSS, 1 MSS + 1 and
  1 MiB each way, half-close each way, both close orders, a reset from each side, a refused connection; then the same under
  5% loss and reordering. Bytes equal, both ends reach closed, no stall. A disagreement prints the segment trace of both
  sides.
* **H16 the lossy-link suite** (the roadmap's "bytes in equals bytes out, no stalls"). Two instances of our stack across
  `netsim`. Link parameters, all from one seed: loss 0, 1, 5 and 20%; duplication 0 and 2%; reordering (per-frame extra delay
  up to 50 ms) 0, 5 and 20%; delay 0, 5 and 100 ms each way; corruption (a bit flip) 0 and 1%; and a **bottleneck** (a rate
  limit with a tail-drop queue of 8 frames) on and off. Workloads: one stream of 1 MiB each way; 8 streams at once; a
  request/response ping-pong of small writes; a reader that pauses. Properties, checked after **every** simulator step: N11,
  N12 (the quiescence check), N13, N16, N21; at the end both sides closed cleanly, every byte equal, every reservation
  released. Bounds in virtual time: a clean link moves 1 MiB in less than 2 s of virtual time at 5 ms delay; a 1% loss link
  in less than ten times the clean time; through the bottleneck, goodput is at least half the configured rate. 200 seeds per
  configuration in tier 0 (a budget of 15 s for the suite), 20,000 in tier 2. A failing seed prints itself and reproduces
  exactly. *Negative controls:* `fault-no-retransmit` must fail N12; `fault-no-fast-retransmit` must fail the 1% bound;
  `fault-skip-tcp-checksum` must fail N11 on a corrupting link.
* **H17 deterministic simulation with a byte-stream model.** One seed drives the link faults, the applications (write sizes,
  read sizes and pauses, half-close, close, abort, at random moments), listeners with random backlogs, account limits, the
  client's socket refusing bytes (the SHOULD_WAIT and NO_MEMORY cases of 4.4), and **stack restarts** (one side's core is
  replaced by a fresh one mid-run). The oracle is a reference model of a connection as two byte queues and a small state
  (open, half-closed each way, closed, reset): every event an application observes must be allowed by the model (data is the
  next bytes of the queue; end-of-file only after the peer's close and all its bytes; a reset only when the schedule aborted,
  restarted, partitioned beyond the give-up time, or closed with unread data). A stateful property test with shrinking covers
  the same space with short operation lists. 500 seeds in tier 0, 50,000 in tier 2.
* **H18 the reassembly and send buffers** against a naive reference (a byte map): random segments with overlaps, duplicates
  and wrap-around; first copy wins; never more than 8 ranges; never beyond the reservation. *Mutation check:* let a later
  copy overwrite.
* **H19 accounting.** The reservation, connection, posted-accept and orphan arithmetic of section 6 against an independent
  recomputation from the full operation history: limits exact, refunds exact, orphans move and expire as stated. *Mutation
  check:* skip the refund when a SYN-RECEIVED connection times out.
* **H20 ISNs and ports.** SipHash-2-4 against the reference implementation's published vectors; the ISN formula of 5.10
  (monotonic per four-tuple as the clock advances, different across secrets, continuing across a simulated restart with the
  same secret and clock); port selection never returns a port in use and covers the range. *Mutation check:* drop the clock
  term.

**The rest** (steps 4, 6, 7)
* **H21 `dns`:** the replies captured in step 0 (an A answer with two records, an AAAA answer, an NXDOMAIN with an authority
  record) parse to the expected values; the query we build is byte-equal to one built independently by the host script;
  the state machine under arbitrary event and timer sequences never sends more than three queries, ignores answers with the
  wrong identifier, source, port or question, follows CNAMEs to the limit, and ends in exactly one outcome. *Mutation check:*
  accept any identifier.
* **H22 `http1`:** the exact request `curl` sends (recorded in 3.2) parses; a table of malformed requests maps to the
  statuses of 10.1; path normalisation against a reference over random paths never yields a path outside the root.
  *Mutation check:* decode escapes after checking for `..`.
* **H23 `sha256`:** the FIPS 180-4 examples (`abc`, the empty string, the 448-bit message, one million `a`), and a golden file
  of 200 random inputs with digests computed by the host's `hashlib`; streaming in random piece sizes equals one-shot.
* **H24 the C library's pure part:** the errno table of 9.5 is total over every native status; the descriptor state machine
  under random call sequences never reaches an undefined state and refuses what 9.4 says with the errno it says.
* **H25 `proto::net`:** goldens and totality for the new messages; the handle rules of 8.3 and 8.4 (one handle for `Connect`,
  two for `Accept`, none elsewhere; anything else is a violation).
* **H26 the test tools test themselves:** the script runner fails a script whose expectation is wrong in each field (flags,
  sequence, ACK, window, option, time); `net_pcap_check.py --self-test` detects a corrupted TCP checksum (14.5); the lossy
  relay with loss 100% delivers nothing and with loss 0 is byte-transparent.
* **H27 Miri** (tier 2, `make os-miri`): `nettcp`, `dns`, `http1` and `sha256` under Miri with a reduced seed count.

### 14.2 QEMU (the suite and harness scripts; tier 1 at four harts unless marked)

Each test starts the stack through `netsup` with explicit views and ends with the suite's exact leak checks (frames, objects,
job pages, handles, pool slots) plus S9's: every account and pool at zero, no connection, orphan or TIME-WAIT record left
after the stated times.

* **Q20 `tcp_loopback_echo`** (no host script): an echo server and a client in the guest over 127.0.0.1 and over the
  interface's own address: 1 MiB each way, SHA-256 equal on both sides; 32 connections one after another; half-close each
  way; the capture contains none of it (N23).
* **Q21 `fetch_from_a_host_http_server`** (roadmap acceptance 1; harness): the harness writes files of 0, 1, 1459, 1460, 1461,
  65,536 and 1,048,576 random bytes (8 MiB in tier 2), computes their SHA-256 with `hashlib`, and starts
  `python3 -m http.server`. For each, `fetch --sha256 http://10.0.2.2:HPORT/name` prints `##FETCH` with the same digest and
  byte count and exits 0; a missing file gives `status=404` and a non-zero exit. The oracle is the host's server and the
  host's hash.
* **Q22 `curl_against_httpd`** (roadmap acceptance 2; harness): `httpd` serves `/www` from the memory filesystem: `index.html`
  from the boot image and a 1 MiB file the suite writes from a seeded generator the harness also has. Host `curl` through
  `hostfwd` gets each with status 200, the right `Content-Length` and the SHA-256 the harness computed itself; `curl -I` gets
  the head and no body; a missing path gets 404; 100 requests in a row and 8 in parallel all succeed. The oracle is `curl`
  and the host's hash.
* **Q23 `tcp_capture_is_clean`** (harness): the `filter-dump` capture of Q21 and Q22 passes `net_pcap_check.py --flows`
  (14.5): every TCP checksum right, the flow rules hold, **zero retransmissions by the guest** on this clean link, the
  guest's SYNs carry exactly the MSS option; `tcpdump -nn -vv -r` reports no incorrect checksum.
* **Q24 `tcp_connect_failures`:** a connect to a closed host port fails with `ConnRefused` within 1 s (slirp resets, 3.2); a
  connect to 10.0.2.99 fails with `HostUnreachable` after exactly three ARP requests; a connect with no address configured
  fails with `NotConfigured`; a connect to an address whose SYNs are dropped (the lossy relay with a filter, tier 2) fails
  with `TimedOut` at 15 s after exactly five SYNs at the stated times.
* **Q25 `tcp_stack_restart_resets_connections`** (the contract of 5.11; harness): a guest client holds an idle connection to a
  host server and another mid-download; `Restart{netd}`. The guest clients get `ECONNRESET` (**not** end-of-file, and the
  download is reported `truncated or reset`, exit non-zero); when the host server next sends, the capture shows a **RST from
  the guest** whose sequence number is that segment's ACK number, and the host connection ends within 3 s (through slirp the
  host sees end-of-file, 3.2, so the assertion on the reset is made from the capture). A new connection then works, and its
  ISN and the old one's differ as 5.10 says. `httpd` is reachable again within 5 s without being restarted.
* **Q26 `tcp_survives_a_driver_restart`:** `Restart{netdrv}` in the middle of a 1 MiB fetch and of a `curl` download: both
  complete with the right hash; the counters show retransmissions; `generation` rose by one.
* **Q27 `tcp_accounts_and_a_stalled_reader`:** (a) a client opens streams until refused: the limit is the account's
  (`stream_bytes` first with default buffers: 16), the refusal is `Quota`/`EMFILE`, a second client is unaffected, closing
  releases exactly; (b) a guest client that does not read while the host sends 4 MiB: `netd`'s memory stays within the
  reservation, the capture shows the guest advertising **window 0**, `job_info` shows the queued bytes on the **client's** job
  and none on `netd`'s (N14), `ping` and a second stream keep working throughout; when the client reads, the transfer
  completes with the right hash; (c) the same with the client's job IPC quota set low enough that NO_MEMORY is the limit:
  `client_quota_wait` counts, nothing spins (a bound on `netd`'s loop iterations), the transfer completes.
* **Q28 `libc_sockets`** (C programs in `libc/tests/c`, run in the guest against host Python peers and against the native
  programs over loopback): a blocking client (`getaddrinfo`, `connect`, `write`, `read` to end-of-file, `close`); a server
  (`bind`, `listen`, `accept` in a `poll` loop, served to the host through `hostfwd`); nonblocking `connect` with
  `EINPROGRESS`, `poll` and `SO_ERROR`, to an open and to a closed port; `poll` and `select` timeouts with nothing ready; a
  write after the peer closed returns `EPIPE` and the program is still alive; `ECONNRESET` after `Restart{netd}`;
  `SO_RCVTIMEO`; UDP `sendto`/`recvfrom`; every refusal in 9.4 returns its errno. The transcript is compared with the same
  programs' output built and run on the host against the same peers (the method the existing C tests use), for the lines
  that do not depend on the platform.
* **Q29 `dns_resolves`** (harness): against the harness's DNS server: an A record; a CNAME to an A; NXDOMAIN; a first query
  the server ignores (the retry at 1 s succeeds); an answer with the wrong identifier, then the right one; a truncated
  answer; no server at all (failure at 7 s after three queries, from the capture). Then `fetch http://name.test:HPORT/file`
  end to end. Tier 2 only: one lookup of a real name through 10.0.2.3, reported as SKIP when there is no answer.
* **Q30 `tcp_through_a_lossy_relay`** (tier 2; the topology of 3.1): seeds 1 to 20 with loss 2%, duplication 1%, reordering
  5%, delay 5 ms: Q21's 1 MiB fetch and Q22's 1 MiB `curl` complete with the right hash within 60 s each; the capture taken
  on the guest's side passes the flow check with retransmissions present; the counters show fast retransmits. *Negative
  control:* the `fault-no-retransmit` build fails by timeout.
* **Q31 `tcp_close_semantics`:** a program writes 100,000 bytes and exits at once: the host receives all of it and a clean end
  (the orphan drains); a program that is killed with unread data pending: the capture shows a RST and the orphan pool is
  empty afterwards; `shutdown(SHUT_WR)` then reading the reply works (half-close); after 20 s no orphan remains.
* **Q32 `tcp_churn`** (tier 2): 2,000 connections in sequence and 32 at a time for 60 s, against the host and over loopback,
  with clients killed at random and `netd` restarted three times: the end-of-run leak checks are exact and the TIME-WAIT
  table never exceeded its bound.
* **Q33 `tcp_modes`** (tier 2): Q20, Q21, Q22 and Q25 at `-smp 2` and at `-smp 4`, and on the plain `virt` machine (the driver
  polls). Smoke runs four harts only; icount runs one hart (T21) and is not used to claim SMP results. The ROADMAP boxes are
  ticked only after this run, saying which configurations ran.
* **Q34 `httpd_survives_hostile_clients`** (harness; the slow cases in tier 2): a Python client sends each malformed request
  of H22's table and gets its status; a head sent one byte a second gets 408 at 5 s; 9 connections at once (the ninth waits
  and is then served); a client that connects and sends nothing; a client that resets mid-response. `httpd` stays up and
  answers a normal request after each, and its memory does not grow.

### 14.3 Benchmarks (tier 2, `make os-bench`; `##BENCH <name> <ns> ns`, smaller is better)

`stream_pipe_1mib`, `stream_pipe_wakes` (step 1, both variants: the numbers behind section 4); `tcp_loopback_1mib` (in-guest
transfer through loopback: the stack and the IPC with no device); `tcp_fetch_1mib` (from the host server); `tcp_serve_1mib`
(`httpd` to host `curl`, timed in the guest from accept to close); `tcp_connect_close_rt` (connect and close to the host
server); `tcp_echo_rt` (one-byte request and reply on an established connection, median); `http_get_small_rt` (a 1 KiB file
over loopback); and `net_bkl_profile_tcp` (big-lock acquisitions and waits, and the wake counters, during the bulk transfers).
At one and four harts. The first numbers become `bench/baseline.tsv` rows. No check asserts an absolute speed (QEMU debug
costs move with the machine, T75); `bench_compare.py`'s 3x rule guards regressions. These are also the load the lock peels
after S9 (tasks 15 to 20) are judged with.

### 14.4 Negative controls and independent oracles, summarised

* **Negative-control builds:** `fault-no-retransmit`, `fault-no-fast-retransmit`, `fault-skip-tcp-checksum`,
  `fault-accept-rst-anywhere` (all `nettcp`, host; the first also in QEMU for Q30), `fault-eof-on-reset` (`rt::net`: Q25 must
  fail), `fault-server-creates-socket` (the prototype: P1 must fail), `fault-level-writable` (`netd`: Q27c must detect the
  spin). They run in tier 2; each mutation check is run once, by hand, when its test is written (TESTING.md rule 6).
* **Oracles the writing context did not produce:** slirp's TCP (a separate BSD-derived implementation) in every QEMU test;
  the host's `python3 -m http.server`, `curl`, `hashlib` and Python sockets (Q21, Q22, Q28); `tcpdump`'s TCP checksum
  verification and real captures (H11, Q23); smoltcp (H15); the exchanges in RFC 9293's figures (H13); the published SipHash
  and SHA-256 vectors (H20, H23); the same C programs on the host (Q28); optionally an unmodified C HTTP server (step 9).
* **What is not independent, said plainly:** most packet scripts, the byte-stream model and the harness's DNS server are
  written by the same context as the code. They are guarded by mutation checks and by H26, and backed by the oracles above;
  open point 10 offers a way to check the scripts against Linux.
* **What cannot be tested in QEMU at all:** interoperation with the host kernel's TCP (slirp terminates TCP), and any option
  slirp does not negotiate.

### 14.5 `scripts/net_pcap_check.py`, extended (step 0)

* **TCP checksum** for every unfragmented TCP segment (pseudo-header, the IP length, so padding is not summed), counted in the
  summary; `--self-test` gains a fourth corruption (one byte of a TCP segment) that must be reported; `make docs-check` runs
  the self-test on `slirp-tcp-probe.pcap` as well.
* **`--flows`** (the guest is the side with the MAC given by `--guest-mac`): per connection and direction: (F1) a guest SYN
  or SYN-ACK carries exactly one option, MSS 1460; (F2) no guest segment carries more than `min(1460, peer's MSS)` bytes; (F3)
  the guest never sends past the window the peer last advertised (a one-byte probe excepted); (F4) the guest's ACK numbers
  and its window's right edge never go backwards; (F5) every byte the guest sent is acknowledged, or the flow ends in a
  reset; (F6) retransmissions are counted per flow, with `--max-retransmits` and `--min-retransmits` to assert them; (F7) no
  guest frame above 1514 bytes and no guest fragment; (F8) a RST sent by the guest has the sequence number RFC 9293 requires
  for the segment it answers.
* It stays standard-library Python, independent of the stack's code.

### 14.6 The packet-script format (`net/tests/scripts/*.pkt`) and the scenarios

The syntax of a packet line is packetdrill's, on purpose, so a script can be read by anyone who knows that tool and a subset
can be converted to run against Linux (open point 10). The difference is the other half: packetdrill drives system calls;
these scripts drive the core's API on a virtual clock.

```text
# passive open, one request, delayed ACK                                   (comment)
config mss=1460 rcv_buf=16384 snd_buf=16384 isn=5000 secret=fixed
+0     listen 80 ; post_accept
+0     < S 0:0(0) win 65535 <mss 1460>            # '<' inject a segment from the peer; numbers are relative to each ISN
+0     > S. 0:0(0) ack 1 win 16384 <mss 1460>     # '>' the stack must emit exactly this, at exactly this time
+.010  < . 1:1(0) ack 1 win 65535
+0     expect event Connected
+0     < P. 1:101(100) ack 1 win 65535
+0     > . 1:1(0) ack 101 win 16284               # quick-ACK rule: first segments are acknowledged at once
+0     read 100                                   # the application takes 100 bytes and they equal the pattern
+0     write 3000
+0     > . 1:1461(1460) ack 101 win 16284         # the window's right edge has not moved by a full segment, so it is not re-advertised
+0     > . 1461:2921(1460) ack 101 win 16284      # initial window: 3 segments; the third is the 80-byte tail, held by Nagle
+.200  > . 1:1461(1460) ack 101 win 16284         # no ACK came: the handshake's 10 ms sample puts the RTO at its 200 ms floor
+0     expect cwnd 1 ; expect counter tcp_retransmit_timeout 1
```

Rules: a time is `+seconds` after the previous line (decimal), on the virtual clock, exact; flags are `S`, `F`, `R`,
`P` and `.` for ACK; `seq:end(len)`; options in `<>`; `win *` matches any window. Payload bytes are a fixed pattern of the
stream offset, so a retransmission is checked byte for byte. Verbs: `listen`, `post_accept`, `connect ADDR:PORT`, `write N`,
`read N`, `shutdown`, `close`, `abort`, `restart` (a fresh core, same secret), `expect event NAME`, `expect state NAME`,
`expect cwnd N` / `ssthresh N` (in segments or bytes), `expect rto T`, `expect counter NAME N`, `client_refuses N` (the
client's socket takes only N more bytes). **Anything the stack emits that the script did not expect is a failure**, as is
anything still expected at the end.

Scenarios (each a file; about sixty in all):

* **Opening:** active open; passive open; simultaneous open; SYN retransmissions at 1, 2, 4, 8 s and failure at 15 s; SYN to a
  port nobody listens on (RST+ACK form); SYN-ACK retransmission and the half-open timeout; an old duplicate SYN (RFC 9293
  figure 8); a bad ACK in SYN-SENT gets a RST; a RST in SYN-SENT is honoured only with the right ACK; no MSS option means 536;
  an MSS above ours is clamped; window-scale, SACK-permitted and timestamp options on a SYN are ignored and not echoed; a
  malformed option list; no posted accept means the SYN is dropped and counted; the account cannot reserve.
* **Data:** in-order delivery; the delayed ACK at 40 ms and at the second segment; the quick-ACK rule and its end; an
  out-of-order segment gets an immediate duplicate ACK; filling the hole gets an immediate cumulative ACK; a ninth range is
  dropped; overlap on the left, on the right and inside (first copy wins); data beyond the window is trimmed and ACKed; a
  duplicate below `RCV.NXT` is ACKed; Nagle holds a small segment and `NODELAY` does not; sender and receiver silly-window
  avoidance; the window closes as the client refuses bytes, the peer's probes are answered, and it re-opens with an update;
  our persist timer probing a zero window with backoff; a bad checksum is dropped silently.
* **Loss and congestion:** a timeout retransmits, doubles and collapses `cwnd`; Karn's rule; the RTO after given samples
  (numbers worked by hand in the script's comment); three duplicate ACKs retransmit at once with the stated `cwnd` and
  `ssthresh`; a partial ACK retransmits the next hole (NewReno); a full ACK leaves recovery; slow start's `cwnd` after each
  ACK; congestion avoidance's growth over a window; restart after idle; give-up at 100 s with `Reset{TimedOut}`.
* **Closing:** active close through TIME-WAIT; passive close; simultaneous close; half-close with data still flowing the
  other way; FIN retransmission; a retransmitted FIN in TIME-WAIT is re-ACKed; a new SYN in TIME-WAIT; close with unread data
  resets; abort resets; an orphan drains and is then freed; an orphan that cannot finish is reset at 20 s.
* **Resets and injection (RFC 5961):** a RST at exactly `RCV.NXT`; one inside the window (challenge ACK); one outside
  (ignored); a SYN on an established connection (challenge ACK); an ACK for data not sent; data outside the window; the
  challenge-ACK and RST rate limits; a segment for no connection, with and without ACK; a RST for no connection gets nothing;
  **after `restart`**, a data segment of the old connection gets the right RST and a new SYN on the same four-tuple gets a
  higher ISN (RFC 9293's half-open discovery figures).
* **Loopback and ICMP:** a loopback connection emits nothing to the device; a destination-unreachable for a SYN in flight
  changes the connect failure to `HostUnreachable`; one that matches nothing is ignored.

### 14.7 Tier placement and budgets (TESTING.md gets these rows in step 8)

| Check | Tier | Budget |
|---|---|---|
| H11 to H14, H18 to H26; H15 to H17 at reduced seeds | 0 | 30 s of host time in all for the new crates (`make os-test`) |
| H15 to H17 at full seeds; H27 (Miri); the `cargo-fuzz` targets for a fixed time | 2 | |
| Q20 to Q25, Q27, Q28, Q29, Q31 (their short forms) | 1 | **at most 20 s added to the smoke boot**; if the boot outgrows its budget the slowest move to tier 2 by TESTING.md's own rule |
| Q26, Q30, Q32, Q33, Q34's slow cases, the 8 MiB transfers, the real-DNS check, every negative control, the benchmarks | 2 | |

## 15. Records this slice proposes

Decision-log entries, once the owner accepts the sections (the log's last entry is OD97; `NET-SPEC.md` proposes OD98 to OD101,
so these continue from OD102; renumber if S8's are accepted differently):

* **OD102** the stream data path: a client-created kernel socket plus the endpoint channel; the stack keeps the send and
  receive buffers; no `socket_info`; the prototype gate (section 4). Refines OD74 and records that "the client pays" covers
  the kernel's buffers, not the stack's.
* **OD103** the TCP feature set and its constants: Reno with NewReno, RFC 5961, no window scaling, SACK or timestamps, the
  deviations from RFC 1122 and RFC 6298 (minimum RTO, SYN give-up, MSL), no fragmentation (section 5).
* **OD104** stream memory accounting: full reservation per connection against the account, posted accepts as the backlog,
  orphans and their pool, the client quota defaults for network programs (section 6, 8.4).
* **OD105** the restart contract for streams: connections die with `netd`; the secret VMO in `netsup`, keyed ISNs and ports;
  RST for unknown segments; end-of-file only after `PeerFin` (5.10, 5.11, 8.5).
* **OD106** the C library's sockets: the mapping, the refused list, blocking in the library, `poll` over a per-thread port,
  the errno table and the `Quota` mapping (section 9).
* **OD107** HTTP and DNS: `httpd` and `fetch` scope; the resolver as a client library; `sha256`, `siphash`, `http1` and `dns`
  written here; smoltcp pinned as a test-only peer; loopback (sections 10, 11, 5.13).
* **OD108** (only if open point 4 chooses it) the entropy source: `virtio-rng` read once by `rngseed` (5.10); it would also
  answer part of Q11.

TIDY-UP entries to write when the steps land (numbered then; the register's last entry is T84 and S8's step 6 adds its own):

| Proposed entry | Sev | What, and the proper fix |
|---|---|---|
| The stack cannot bound the kernel socket's 64 KiB per direction | M | a stalled reader pins its own quota and part of the 2 MiB system budget; fix: a settable socket buffer size or `socket_info` (a kernel slice), or the ring data path |
| No window scaling, SACK or timestamps | L | fine under slirp; needed for buffers above 64 KiB or a real network |
| No SYN cookies; a SYN flood can occupy a listener's posted accepts | M | SYN cookies, or a small stack-paid SYN queue |
| A dropped SYN when no accept is posted costs the peer a retry (6 s under slirp) | L | complete the handshake with a zero window and hold a few connections until an accept is posted |
| Minimum RTO 200 ms, SYN give-up 15 s, MSL 5 s deviate from the RFCs | L | make them per-interface settings |
| The boot secret's entropy (if the stand-in is chosen); DNS identifiers without an entropy service | M | Q11: an entropy service |
| No keep-alive: a silently dead peer holds a connection until the application closes | L | keep-alive as an option |
| No DNS over TCP; a truncated answer without an address fails | L | the fallback, now that TCP exists |
| No IP reassembly (carried over from S8) | L | only if a client needs it |
| `MSG_PEEK` refused | L | a library-side buffer, or a kernel peek |
| `poll` binds and unbinds on every call | L | keep bindings across calls; revisit with task 21 |
| Up to three bindings per stream in `netd` under T60's per-binding cost | M | task 21; until then the pool limits |
| The stream data path costs a syscall and a copy more than a ring | L | the ring endpoint kind of NET-DESIGN 7.5, when the benchmark says so |
| One `netd` thread (T67, existing) | L | reconsider with the S9 benchmark, as T67 says |

Documents updated when built: INVARIANTS (N11 to N24, N6 amended), ROADMAP (S9 boxes, only after Q33's runs, saying which
ran), `NET-DESIGN.md` (7.4 settled, status), `NET-SPEC.md` (section 18's items), `TESTING.md` (14.7's rows and the benchmark
table), `SYSCALLS.md` (unchanged unless 4.6 triggers a kernel slice), TIDY-UP (the table above; T67 re-read against the
benchmark).

## 16. Risks

1. **The data-path prototype finds a kernel behaviour that breaks option 1** (the charge lands on the wrong job in some
   hand-over case, two bindings on one socket are refused, or a quota-limited client makes the stack spin despite the
   timer). This is why step 1 exists and is a gate. Fallbacks: option 3 with no change to `nettcp`, or a small kernel slice
   (4.6). Until step 1 has run, **section 4 is reasoning from the code and the documents, not a result**.
2. **The system-wide limits are small for TCP.** 2 MiB of queued IPC for the whole system, 192 KiB per program and T60's
   per-binding cost on every syscall were sized before a network existed. A few stalled connections can reach the first; the
   second makes one busy stream nearly fill a program's quota. The spec proposes launcher defaults and bounded pools, but the
   real answer (T79's review of the limits, task 21 for the bindings) is outside S9, and if it does not happen S9 works only
   with small numbers of connections. P7 and P8 measure it.
3. **slirp is the only TCP peer in QEMU, and it is an unusual one**: no options beyond MSS, ACKs at once, ignores our MSS, turns
   resets into end-of-file for the host, retries a SYN only after 6 s. Behaviour tuned against it (the quick-ACK rule, the
   backlog rule) may be wrong for a real network, and interoperation with a modern kernel's TCP is **untested by
   construction**. Mitigations: smoltcp on the host, the RFC-figure scripts, the lossy relay; open point 10 (scripts against
   Linux) and a tap-based run are the real answers and are not in S9.
4. **Tests written by the code's author prove less than they seem to.** TCP is the part of the project where a consistent
   misreading of an RFC passes its own scripts. Mitigations in 14.4; the independent review of step 2 and 3 should be asked to
   attack exactly this (compare scripts with the RFC text, not with the code).
5. **Time and scheduling.** Before tasks 13, 21, 22 and 24 the loop's latency is up to 10 ms and every wake goes through the
   big lock's remains; throughput and latency numbers will be poor and noisy, and timing-sensitive tests can flake. No check
   asserts a time tighter than the safety poll allows, and the lossy-link properties are asserted on the virtual clock.
6. **The step count is large** (TCP, a data path, a C-library layer, three programs, a resolver). Each is behind a seam, and
   steps 2 and 3 are host-only, so the risk is schedule, not a big-bang change; step 9 and the entropy device can be dropped
   without touching the acceptance.
7. **Pre-posted accepts are unusual** and the C library hides them; a program that expects a deep backlog to absorb a burst
   will instead see the peer's SYN retry. Recorded; the zero-window hold is the improvement.
8. **The smoke budget.** S8 and S9 both add QEMU tests with host scripts; 14.7 caps S9's share and TESTING.md's rule moves
   the slowest to soak.

## 17. Open points for the owner (each with a recommendation)

1. **The stream data path** (section 4): accept option 1 (client-created kernel socket, the stack keeps the send and receive
   buffers, no kernel change) subject to the step 1 prototype? *Recommended: yes.* The alternatives are the ring (option 2,
   later) and the stack-created socket (option 3, the fallback).
2. **Window scaling, SACK and timestamps left out** (5.3), although `OS-ARCHITECTURE.md` section 13 names window scaling among
   the hard parts. *Recommended: leave all three out of S9, with the triggers recorded.* If the owner wants window scaling
   for its own sake it is a small host-tested addition to step 3, but nothing in QEMU could exercise it.
3. **Quotas for network programs and the system IPC budget** (6.4): IPC 1 MiB and 512 objects for a program with a network
   view; and should `MAX_IPC_BYTES` (2 MiB) be raised or tied to RAM before S9 (a kernel constant, T79)? *Recommended: the
   launcher defaults now; decide the budget after P7.*
4. **The boot secret's entropy** (5.10): a `virtio-rng` one-shot reader (a second small driver, a Fable review, part of Q11) or
   the recorded stand-in? *Recommended: the device.*
5. **Loopback** (5.13) in S9, for in-guest tests? *Recommended: yes.*
6. **A QUOTA status in the kernel** (9.5, D16): not needed by S9. *Recommended: do it as its own small kernel slice when
   convenient, not as part of S9.*
7. **`Quota` maps to `EMFILE` and `NoBuffers` to `ENOBUFS`** (9.5): accept, or `ENOBUFS` for both?
8. **The resolver as a library in each client** (11.1) rather than a `resolved` service with a shared cache? *Recommended: the
   library.*
9. **Step 9: an unmodified third-party C HTTP server as an oracle** for the C library (the architecture's "ported software as
   oracles" names a small HTTP server). It needs a candidate to be chosen and checked (license, what it needs from the C
   library) and third-party code in `third_party/` as Lua is. *Recommended: yes, after step 7, not on the acceptance path.*
10. **Checking the packet scripts against Linux** (14.6): because the packet syntax is packetdrill's, the scripts whose
    behaviour is universal could be converted and run with the real packetdrill on the Linux soak runner, which would make
    them an independent oracle. It needs root on the runner and tolerance for window values. *Recommended: decide after step
    2, when the scripts exist.*
11. **The deviations from the RFCs' timing** (minimum RTO 200 ms, SYN give-up 15 s, MSL 5 s, data give-up 100 s kept): accept
    as starting values?
12. **`httpd` and `fetch` as native Rust programs, with C exercised by test programs (Q28) and optionally step 9**, rather
    than writing the demo server in C? *Recommended: as specified, so the parser that faces the network is a fuzzed pure
    crate.*
13. **Names:** `nettcp`, `dns`, `http1`, `sha256`, `httpd`, `fetch`, `rngseed`, `streamproto`; kinds `TcpStream` and
    `TcpListener`.

## 18. What S9 needs changed in S8's spec (`NET-SPEC.md`); listed, not edited

1. **7.1:** reserve `Open` kinds 3 and 4, statuses 10 to 16, endpoint ops 16 to 19 and notice codes 16 to 21; say that a
   request may carry more than one handle (`Accept` carries two) so the typed client and the decoders allow it from the start.
2. **7.4 and H9:** the account record carries a byte reservation (`stream_bytes`) beside the connection count from the
   start, so S9 does not change the account model under tests that already exist.
3. **6.6:** ephemeral ports from a keyed hash once the secret exists (S8 can keep the rotating start, but should not promise
   it); say that the UDP and TCP port spaces are separate.
4. **6.3 rule 4 and 6.5:** protocol 6 is dispatched to TCP (in S8 it is `rx_unknown_protocol`); a received
   destination-unreachable is passed to the transport it quotes, not only counted.
5. **N6** ("an endpoint exists exactly as long as its channel"): true for the client's control of it; a TCP connection may
   drain as an orphan for a bounded time (5.8, 6.3).
6. **8.1:** the loop has two more bounded phases (7.2); the constants N, K, M are joined by P.
7. **4.2 and 9:** `netsup` also holds the secret VMO and passes it read-only to `netd` only; the flight recorder reserves
   event kinds for TCP.
8. **6.2:** the IP identification counter and the DF bit are unstated; S9 assumes DF clear. A loopback path is added.
9. **`netsim` (step 1, H6):** needs, besides loss, duplication, reordering and delay: corruption, a rate limit with a
   tail-drop queue, and a hook to replace one endpoint (the restart). Cheaper to build in S8 than to retrofit.
10. **3.1:** the harness's netdev line gains `hostfwd=tcp`; the capture checker's `make docs-check` line gains the TCP capture.
11. **3.2:** two facts worth adding there because S8's tests may meet them: slirp is itself a TCP endpoint, and through
    `hostfwd` the host's `connect` succeeds before the guest has answered.
12. **15 / T84 follow-ups:** none; but S8's "TIDY-UP entries to write in step 6" includes "client creates the stream socket is
    unproven", which section 4 here replaces with a prototype step.

## 19. What was verified while writing this, and what is from memory

**Verified by running something (2026-11-14, this machine):** every row of 3.2 (two probe scripts with a fake guest against
QEMU 11.1.3's slirp; the capture has 334 TCP segments, all with correct checksums according to both the script and
`tcpdump -vv`); the `python3 -m http.server` and `curl` behaviour quoted in 3.2; that QEMU accepts the relay topology's flags
and lists `virtio-rng-device`; smoltcp's current version number; the SHA-256 values of `abc` and the empty string (H23).
The probe scripts and the capture are **not in this commit** (documents only); they were in a scratch directory and step 0
must recreate and commit them. Nothing was run in a guest: no guest code for S8 exists yet.

**Verified by reading this repository:** every row of 3.3 (the kernel's socket write path and its two refusals, the WRITABLE
rule, the limits, the launcher's quotas, the C library's descriptor table and errno list, the absence of a hash or entropy
source), the roadmap's task order, the decision log's last number, TIDY-UP's last number.

**From memory, not checked against the text** (to be checked by whoever writes step 2, against the RFCs themselves): the
section numbers cited in RFC 9293, RFC 1122, RFC 5681 and RFC 6298 and the exact constants taken from them (the RTO formula
and its 1 s initial value, the initial-window table, the fast-recovery arithmetic, R2 of 100 s, three minutes for a SYN, an
MSL of two minutes, the 500 ms delayed-ACK limit); RFC 5961's rules; RFC 6528's formula; RFC 6056's algorithm numbering; RFC
6093's advice on urgent data; RFC 5927's on ICMP; packetdrill's syntax; that smoltcp's TCP is suitable as a peer for
everything in H15 (its option support was not checked and does not matter, since we negotiate none); that a small permissive
single-file C HTTP server exists that would port (step 9 names none for that reason); the SipHash reference vectors (not
reproduced here). None of these is load-bearing for the design choices; several are load-bearing for constants, which is why
H13 and H14 take their numbers from the RFC text and not from this file.

**Not verified at all:** that S8 will be built as `NET-SPEC.md` says (it is a draft the owner has not accepted); every
performance expectation; everything in section 4 beyond what the code reading supports, until step 1 runs.

