# NET-SPEC: S8, the network stack's lower layers (specification, draft v0)

Status: **draft v0, 2026-11-14; steps 1 to 5 built on 2026-11-15 (see ROADMAP S8 and TIDY-UP T86, T87 for what differs).** This is the written acceptance contract for S8 (CLAUDE.md: spec,
then acceptance checks, then code, then an independent review). It makes `NET-DESIGN.md` (the architecture and its reasons)
concrete: every message, constant, state machine and check an implementer needs. Where the two disagree, `NET-DESIGN.md`
is the intent and this file is the contract; fix the other one in the same PR. Decisions made here are logged as OD98 and
later in `OS-ARCHITECTURE.md` section 17 once the owner accepts them (section 13 lists them). Open points are in section 15.

Reading order: section 1 (what S8 is), 2 (steps), 3 (the environment, with facts checked), 4 to 9 (the parts), 10 (limits), 11
(invariants), 12 (acceptance: the part that matters most), 13 to 15 (records, risks, open points).

## 1. Goal and non-goals

**Goal.** The guest has an Ethernet interface and a small IPv4 stack that other programs reach through handles:

1. a user-mode virtio-net driver moves frames between the device and the stack through shared rings;
2. a stack server answers ARP and ICMP echo, sends and receives UDP datagrams for client programs, and sends ICMP echo
   requests for a `ping` client;
3. a DHCP client configures the interface from QEMU's built-in server;
4. the three processes are supervised, and a crash of any one is survived with a stated contract.

**Acceptance of S8 (the roadmap's, made precise):** the guest pings its gateway (10.0.2.2) and receives a reply; a UDP
echo program in the guest answers datagrams sent from the host through `hostfwd`; the packet capture of the run passes an
independent checksum check; and killing each of the three processes under traffic leaves a working network within a
bounded time. The full list is section 12.

**Non-goals (S8).** No TCP, no sockets-in-libc (S9). No IP fragmentation or reassembly, no IPv6, no multicast or IGMP, no
VLAN, no raw sockets, no forwarding or NAT, no more than one interface, no DNS resolver (S9), no `SO_*` options, no
checksum or segmentation offload, no multi-queue, no hot-plug, no virtio-pci. No kernel change and no new syscall (section 3.4
says why that holds; step 3 verifies it). No real-time priorities (NET-DESIGN 8.1). No per-connection threads (T67).

## 2. Steps

Each step is one PR (or a few), green on `make smoke`, bootable. The independent review is a Fable review for steps 3 and 4 (the
ring protocol and the new authority arrangement) and the default reviewer otherwise (OS-track rule, and the owner's standing
preference for kernel and security slices).

| Step | Contents | Needs | Review |
|---|---|---|---|
| 0 | This spec; the harness additions that need no guest code (**built in this PR**: the slirp probe, the capture checker with its self-test, the first real capture; still to do: the QEMU flags in the boot harness and the host UDP client for Q4); the acceptance tests written and failing | nothing | owner |
| 1 | `netwire` (parsers, builders, checksum) and `netsim` (clock, link, fault injection), host-only, with goldens from a real capture | 0 | default |
| 2 | `netcore` (interface, ARP, IPv4, ICMP echo, UDP, timers, counters, the `Device` seam) tested against smoltcp as a peer; the pure `supervise` (restart-intensity) module | 1 | default |
| 3 | The ring layout and the ring model (`netring`), `netdrv`, the `devmgr` net claim, `netsup`'s device half (pools, rings, events, attach/re-attach); a loopback test of the whole data path with no stack | 2, DM | **Fable** |
| 4 | `netd` on `rt::service`, `proto::net`, the typed request/reply client and `rt::net`, SD step 4 (publishing `/svc/net*`, reconnect on `UNAVAILABLE`), the `ping` and `udpecho` programs; the ping and UDP-echo acceptance runs | 2, 3, R, SD | **Fable** |
| 5 | `dhcp` (pure) and `dhcpc`; the config blob; static configuration through the config capability | 4 | default |
| 6 | The network benchmark workload; flight recorder and the structured dumps; the restart-churn and flood soak tests; documents (SYSCALLS unchanged, INVARIANTS N1 to N10, TIDY-UP, ROADMAP ticks, OD98 to OD101) | 5 | default |

Sizes are not estimated here: the project's own estimates have been poor (the roadmap says so), and step 0 will measure the
first numbers.

## 3. The environment

### 3.1 Test machine and flags

The S8 boot tests add one device to the existing QEMU command line (`scripts/os_boot_test.py`, `Makefile`):

```text
-netdev user,id=n0,net=10.0.2.0/24,host=10.0.2.2,dhcpstart=10.0.2.15,hostfwd=udp:127.0.0.1:HOSTPORT-:7777
-device virtio-net-device,netdev=n0,mac=52:54:00:12:34:56
-object filter-dump,id=f0,netdev=n0,file=OUT.pcap
```

plus the existing `-global virtio-mmio.force-legacy=false` (the transport is modern virtio-mmio, version 2, exactly as `blkd`).
The network, host and DHCP start addresses and the MAC are **set explicitly** so no test depends on a default that was only
remembered. `HOSTPORT` is chosen by the harness (a free port, not a constant).

### 3.2 Facts checked on QEMU 11.1.3 (macOS, 2026-11-14) and facts still to check

Checked by starting QEMU with these options:
* the options above are accepted with `-machine virt` together with `-global virtio-mmio.force-legacy=false`;
* `-object filter-dump,id=f0,netdev=n0,file=X` creates `X` with a classic pcap header (magic `d4 c3 b2 a1`, link type 1,
  Ethernet);
* `virtio-net-device` offers `csum`, `guest_csum`, the GSO/TSO family, `ctrl_vq` and the hash options by default (the driver simply
  does not negotiate them: section 5.2);
* the device tree of the `virt` machine lists all eight virtio-mmio slots (`0x10001000` to `0x10008000`) whether or not a device is
  attached, so which slot the net device occupies is **not** assumed: `devmgr` finds it by device id, as it already does for disks.

**Checked against slirp itself** with `scripts/net_probe_slirp.py` (step 0, built in this PR): QEMU runs with no guest, slirp and a socket netdev
share a hub, and the script is a fake guest that sends real frames and records what slirp answers. Its capture is committed as
`net/tests/data/slirp-probe.pcap` (28 frames), the first real golden data for `netwire`. Results (QEMU 11.1.3, the flags of 3.1):

| Question | Answer observed |
|---|---|
| ARP for 10.0.2.2, 10.0.2.3 | answered, MACs **52:55:0a:00:02:02** and **52:55:0a:00:02:03**; 10.0.2.4 and an unused address get **no** answer |
| DHCP | OFFER and ACK both sent to **ff:ff:ff:ff:ff:ff / 255.255.255.255** (the client set the broadcast flag); `yiaddr` 10.0.2.15; options present are exactly **53, 54 (server 10.0.2.2), 1 (255.255.255.0), 3 (router 10.0.2.2), 6 (DNS 10.0.2.3, one address), 51 (lease 86400 s)**; **no options 58 or 59**, so the client must compute T1 and T2 |
| ICMP echo to 10.0.2.2 and to 10.0.2.3 | both answered, **TTL 255**, identifier and data preserved |
| UDP to 10.0.2.2 port 9 (closed) | **no answer**: slirp sends no ICMP unreachable, so nothing may depend on one |
| host datagram to `HOSTPORT`, 1, 100 and 1472 bytes | arrives as UDP from **10.0.2.2** to 10.0.2.15 port 7777 (source port is the host socket's, constant per host socket); sizes 1, 100, 1472 give frames of **60 (padded), 142 and 1514 bytes**, no fragmentation; a reply addressed back through slirp reaches the host intact |
| host datagram of 1473 and 3000 bytes | **slirp fragments them** (a first fragment of 1514 bytes with MF set, then a 60-byte fragment, etc.); slirp also accepts guest frames above 1514 bytes (it echoed 1515 and 3042), so the stack must itself never exceed 1514 |
| slirp ARPing for the guest | **never observed**: slirp learned the guest's MAC from the guest's own frames; the stack must still answer ARP requests (6.4), and it always speaks first (ARP or DHCP) before anything is forwarded to it |
| MTU | frames up to 1514 pass; 1500 IP MTU |

Still **not** checked (step 0 or later): which virtio-mmio slot the net device takes when disks are attached too (found by device id regardless);
behaviour of a real guest driver (nothing here exercises virtio); slirp's lease renewal behaviour (a renewing REQUEST in state `Renewing`); the
`tcpdump` and capture facts below.

**`tcpdump` as an oracle, and why there is a second one.** `tcpdump -nn -vv -r` reports a wrong IPv4 header checksum (`bad cksum`), but it only verifies a UDP
checksum for protocols it dissects (it printed `[udp sum ok]` for DHCP and nothing for an arbitrary port), and prints no ICMP verdict. So the capture checks use
`scripts/net_pcap_check.py` (standard library only: IPv4 header, ICMP and UDP checksums, using the IP length so Ethernet padding is not summed), with
`tcpdump -vv` as a second opinion. The checker has a `--self-test` that corrupts one byte of each kind in a copy of the capture and requires all three
corruptions to be reported, so it is known to be able to fail; the committed capture passes with 22 IPv4 frames, 4 ICMP and 13 UDP checked.

### 3.3 Existing pieces this relies on

| Piece | State | Use |
|---|---|---|
| Modern virtio-mmio and DMA/MMIO VMOs, `blkd`'s register code | built (S6) | the driver's transport |
| `devmgr` table-driven, several devices and clients (`proto::devmgr` ops 1 and 2) | built (DM) | extended with a net claim (section 4.3) |
| Interrupt objects on AIA delivered to the requesting hart, or polling | built (I) | driver wake source |
| `rt::service` (`Core`, `Service`, `other_event`), accounts (`svc`), `memfsd` accounts | built (R) | `netd`'s loop and endpoint accounting |
| Registry service, `PUBLISH` right, `Publish`/`View`, `UNAVAILABLE`, arbitrary namespace prefixes | built (SD steps 1 to 3) | naming; **step 4 is built here** |
| Events (8 user bits, `event_signal`), ports with one-shot bindings and `port_rearm` | built (W) | doorbells |
| `vmo_commit` and the foreign-mapping rule | built (V) | pools are committed at creation, so mapping them in `netd` is allowed |
| `thread_spawn`, job-derived thread limits | built (task 9) | unused by S8 (one thread per process), noted for T67 |
| `##BENCH`, `make os-bench`, `bench/baseline.tsv` | built (task 1) | the network workload (step 6) |
| Typed request/reply client in `rt` | **not built** | built in step 4 |
| The nanosecond clock ABI | **not built** (task 6 flag day; confirm status) | the core uses an opaque `Instant` so it does not matter |

### 3.4 No kernel change

S8 needs no new syscall, right or object. Everything it needs exists: DMA VMOs whose duplicates can lack the `DMA` and `WRITE`
rights (so `netd` can map the receive pool read-only and cannot learn physical addresses), committed VMOs (so mapping them in
another job is allowed under OD81), events with `SIGNAL` and `WAIT` rights, ports, interrupts. **If step 3 finds this false it
stops and the owner decides** (a kernel change would make step 3 a kernel slice with its own review).

## 4. Processes, authority and naming

### 4.1 The processes

| Process | Program | Holds | Does not hold |
|---|---|---|---|
| `netsup` | `init/src/bin/netsup.rs` | everything long-lived (section 4.2), the registry `PUBLISH`-capable connection, process handles for its children | a way to name the network as a client |
| `netdrv` | `netdrv.rs` | MMIO slot (RW), interrupt (or none), the three DMA VMOs **with `DMA` right** (not mapped, except the queue page), the ring VMO, the doorbell events, the driver control channel | any packet content (never maps a pool), any registry or service handle |
| `netd` | `netd.rs` | the RX pool (read-only map, no `DMA`), the TX pool (RW map, no `DMA`), the ring VMO, the doorbell events, the driver control channel, `PUBLISH` for the three names, the config blob (RW, its half) | the device, physical addresses, the interrupt |
| `dhcpc` | `dhcpc.rs` | a `/svc/net-priv` and a `/svc/net-config` connection, the config blob (RW, its half) | everything else |
| clients | `ping`, `udpecho`, `netinfo`, later libc | a `/svc/net` connection (an accounted one), optionally others | |

### 4.2 What `netsup` holds across child lifetimes (T42's rule)

The MMIO slot and interrupt (from `devmgr`), the three DMA VMOs, the ring VMO, the two doorbell events, the config blob VMO, the
flight-recorder VMO, the stats page (inside the ring VMO's header). It resets the device (virtio status 0) before any new driver
starts, exactly as OD65 describes for `blkd`.

### 4.3 The `devmgr` net claim

`proto::devmgr` gains **`OP_CLAIM_NET = 3`** (request layout as `OpenBlock`: `txid u32 | op u16 | index u16`; `index` counts virtio
net devices in ascending slot address). The reply is `txid u32 | status i32 | flags u32 | pool sizes` with handles in this
order: MMIO VMO (RW), RX DMA VMO, TX DMA VMO, queue DMA VMO (each with the `DMA` right), and, if `flags` bit 0 is set, the
interrupt. Statuses add `NoDevice` (no net device) and reuse `Busy` (already claimed: released when the claimant's channel
closes, after which `devmgr` resets the device before it can be claimed again). The DMA VMOs are created by `devmgr` (only the root
resource can), so **they are charged to `devmgr`'s job** (a shortcut recorded in TIDY-UP: the claimant's job should pay); the
three together are 130 pages of the 1024-page system DMA budget.

### 4.4 Names and classes of authority

`netd` publishes three names in the registry (SD, OD78). **Authority is which name a program's view contains**, not an identity and
not a new right:

| Name | Class | May | May not |
|---|---|---|---|
| `/svc/net` | normal | `Info`; open UDP endpoints on local ports 1024 to 65535; send from the interface address; send unicast | bind below 1024; send broadcast; send with source 0.0.0.0; configure |
| `/svc/net-priv` | privileged | everything normal may, plus bind any port, send limited broadcast, send from 0.0.0.0 (while unconfigured) | configure |
| `/svc/net-config` | configure | `Info`; set and clear the address, mask, gateway and DNS; read the ARP table and the counters | open endpoints |

The shell's view-building code decides who gets what; until the shell can grant (D10, SD step 4's shell half) the test programs are
launched with explicit views.

## 5. The driver and the data path

### 5.1 Memory

Created by `devmgr` (DMA) and `netsup` (rings):

| Region | Size | Created as | Contents |
|---|---|---|---|
| RX pool | 64 pages | DMA VMO | 128 slots of 2048 bytes |
| TX pool | 64 pages | DMA VMO | 128 slots of 2048 bytes |
| Queue memory | 2 pages | DMA VMO | the receive virtqueue in page 0, the transmit one in page 1 (descriptor table, available and used rings, 128 entries each) |
| Ring VMO | 2 pages | ordinary VMO, committed by `netsup` | the control header (page 0) and the four rings (page 1) |
| Config blob | 1 page | ordinary VMO | section 8.4 |
| Flight recorder | 1 page | ordinary VMO | section 9.4 |

A pool slot is 2048 bytes: **bytes 0 to 11 are the virtio-net header, bytes 12 onwards are the frame** (up to 1514 bytes, so
the largest frame ends at byte 1526 and the slot has 522 spare bytes). A frame's `len` in the rings is the frame length, without
the header.

### 5.2 virtio-net, as the driver implements it

* Transport: virtio-mmio version 2 (modern). The driver reads magic `0x74726976`, version 2, device id 1.
* Features: **only `VIRTIO_F_VERSION_1` (bit 32) and `VIRTIO_NET_F_MAC` (bit 5)** are written to the driver features register;
  none of CSUM, GUEST_CSUM, any GSO/TSO/UFO/USO, MRG_RXBUF, CTRL_VQ, MQ, RSS, HASH_REPORT, STATUS, MTU, EVENT_IDX or
  INDIRECT. If the device does not offer `VERSION_1` or `MAC` the driver exits with a code the supervisor records (and does not
  restart into a loop: section 9.2). It sets `FEATURES_OK` and re-reads the status as the spec requires.
* The header is 12 bytes. On transmit the driver writes it all zero (flags 0, gso_type 0, `num_buffers` 0). On receive it
  requires `num_buffers == 1` and `gso_type == 0` and `flags` without `NEEDS_CSUM`; a frame that violates this is dropped and
  counted (`rx_bad_header`).
* Queues: queue 0 receive, queue 1 transmit, each 128 entries, one descriptor per buffer (header and frame are contiguous in the
  slot, so one descriptor of up to 1526 bytes for receive posts `2048`). `avail.flags` is set to `NO_INTERRUPT` while the driver is
  draining and cleared before it sleeps, followed by a re-check of the used ring (the lost-wakeup rule).
* Link is assumed up after `DRIVER_OK` (STATUS is not negotiated); `link_up` in the control header says so.
* The MAC is read from device configuration space (offset 0, six bytes) and published in the control header.
* **Wake source:** an interrupt object if `interrupt_create` succeeds (the AIA default machine, delivered to the hart that asked),
  otherwise a poll on the timer tick (plain `virt`, no controller). Both are tested (section 12). In both cases the driver also
  re-polls its rings and the device at least every 10 ms (the safety poll, 5.5).

### 5.3 The control header and the four rings (`netring`, a pure crate)

All integers little-endian; every shared index is a `u32` accessed with `Acquire`/`Release` atomics (RISC-V `fence`s) and only
**grows** (it is reduced modulo the capacity when used).

Control header (page 0 of the ring VMO):

| Offset | Size | Field | Written by |
|---|---|---|---|
| 0 | 4 | `magic` `NETR` (0x5254454e) | `netsup`, once |
| 4 | 4 | `version` = 1 | `netsup` |
| 8 | 8 | `generation` | `netdrv` (at each attach) |
| 16 | 4 | `state` (0 none, 1 driver ready, 2 stack attached) | `netdrv` / `netd` (per the table in 5.6) |
| 20 | 4 | `link_up` (0/1) | `netdrv` |
| 24 | 4 | `slots` = 128 | `netsup` |
| 28 | 4 | `slot_size` = 2048 | `netsup` |
| 32 | 4 | `hdr_len` = 12 | `netsup` |
| 36 | 4 | `frame_max` = 1514 | `netsup` |
| 40 | 6 | `mac` | `netdrv` |
| 48 | 4 | `net_waiting` (the stack is about to sleep) | `netd` |
| 52 | 4 | `drv_waiting` (the driver is about to sleep) | `netdrv` |
| 64 to 255 | | the stats counters (64-bit each): `rx_frames`, `rx_dropped_ring_full`, `rx_bad_header`, `rx_bad_len`, `tx_frames`, `tx_dropped_no_slot`, `tx_dropped_device`, `doorbells_sent`, `doorbells_suppressed`, `ring_violations` | `netdrv` and `netd` (each its own) |
| 256 to 1023 | | indices: for each ring a producer and a consumer `u32`, each on its own 64-byte line | |

Rings (page 1), each **capacity 128 entries** (a power of two equal to the slot count) of 8 bytes: `slot u32 | len u16 | flags u16
(0)`:

| Ring | Producer | Consumer | Meaning |
|---|---|---|---|
| `rx_used` | driver | stack | slot `s` holds a received frame of `len` bytes |
| `rx_free` | stack | driver | the stack is finished with receive slot `s`; post it to the device again |
| `tx_ready` | stack | driver | slot `s` holds a frame of `len` bytes; send it |
| `tx_done` | driver | stack | the device has finished with transmit slot `s` |

Because there are only 128 slots and a slot is in exactly one place at any time, **no ring can ever hold more than 128 entries**;
a producer that finds its ring full has found a protocol violation (section 5.6), not a normal condition.

### 5.4 Slot ownership (the thing the model test checks)

Each of the 256 slots (128 RX, 128 TX) is always in exactly one of these states:

* RX: `DeviceHas` (posted to the device) → `DriverHas` (device used it, driver is publishing it) → `InRxUsed` → `StackHas` →
  `InRxFree` → `DriverHas` → `DeviceHas`.
* TX: `StackFree` (in the stack's own free list) → `StackHas` (being filled) → `InTxReady` → `DriverHas` → `DeviceHas` →
  `DriverHas` (used) → `InTxDone` → `StackFree`.

The stack keeps its own transmit free list (a stack of slot numbers, initially all 128). The driver needs no free list for
receive: it re-posts a slot as soon as it reads it from `rx_free`.

Every slot number and length read from a ring is **validated before use**: `slot < 128`; `len <= 1514`, and for `rx_used` also
`len >= 14`; a slot the reader's state table says is not in the claimed state is a violation. A violation is counted
(`ring_violations`), the entry is ignored, and the reader sets `state` back to 0 and exits with a recorded code (the supervisor
restarts it: 9.1).

### 5.5 Doorbells and the safety poll

Two **events** created by `netsup`: `drv_to_net` and `net_to_drv`. Bits: bit 0 "a ring I consume became non-empty" (for the stack:
`rx_used` or `tx_done`; for the driver: `rx_free` or `tx_ready`), bit 1 "state/generation/link changed". Each side holds `WAIT` on
the event it consumes and `SIGNAL` on the other (and `SIGNAL` on its own to clear bits, which is unavoidable and harmless: see
the poll below).

Protocol, per consumer (this is the lost-wakeup-free handshake of NET-DESIGN 5.3, stated precisely):
1. drain the ring(s) until empty;
2. set my `*_waiting` flag to 1 (release);
3. **re-check** the ring(s); if non-empty, clear the flag and go to 1;
4. sleep on the port (bound to my event, one-shot, re-armed by `port_rearm`) with the deadline of 5.6;
5. on waking, clear the flag, clear the event bits I will service, go to 1.

Producer: after publishing entries (release), if the consumer's flag reads 1 **and** the ring was empty before this publish,
signal bit 0; otherwise count `doorbells_suppressed`.

**The safety poll:** doorbells are hints, never state. Both loops re-poll their rings at least every 10 ms regardless of events, so a
lost or never-sent doorbell (a bug, or a peer clearing bits it should not) costs at most 10 ms of latency and is visible as a
counter, never a hang. A test (12, Q19) removes the doorbell and requires progress.

### 5.6 Attach, re-attach and the two small state machines

Control channel between `netd` and `netdrv` (created by `netsup`; messages `txid u32 | op u16 | pad u16 | body`):

| Op | From | Body | Meaning |
|---|---|---|---|
| 1 `READY` | driver | `generation u64, mac[6], link u8, pad` | the rings and pools are initialised and valid for this generation |
| 2 `LINK` | driver | `link u8` | link changed (always 1 in S8: no STATUS) |
| 3 `ATTACH` | stack | `generation u64` | the stack has reset its view and is taking this generation |
| 4 `DETACH` | stack | none | the stack is going away cleanly |

**Driver:** `Starting` (map, reset device with status 0, set up queues) → writes all ring indices to 0, `generation += 1`, stocks
the receive queue, `state = 1`, sends `READY` → `Ready` (waiting for `ATTACH{generation}`) → on `ATTACH`, `state = 2`, serves rings
→ `Attached`. On `DETACH` or the control channel's peer-closed: `state = 1`, **discard** everything in the four rings (zero the
indices, reclaim all slots to the driver), keep the device running with the receive queue stocked and frames dropped (counted),
send nothing, wait for a new channel from `netsup` and a new `ATTACH`. The driver's own crash is the supervisor's business.

**Stack:** `Detached` (rings are undefined; nothing is read) → on `READY{generation}` with `generation` greater than any it has
seen: forget every outstanding slot, rebuild the TX free list (all 128), treat it as a link flap (the core gets `link_down` then
`link_up`), send `ATTACH{generation}` → `Attached`. A `READY` with an old or equal generation, or while `Attached`, is a
violation. On the control channel's peer-closed it goes `Detached`, tells the core `link_down`, and waits for a new control
channel from `netsup`.

In `Detached`/`Starting` neither side touches the other's indices; the rings are valid only between `READY` and the next
`DETACH`/peer-closed, which is what makes a concurrent reset safe.

## 6. The stack core (`netwire`, `netcore`)

Both `no_std`, `#![forbid(unsafe_code)]`, no panics on any input (no `unwrap`, `expect`, indexing or arithmetic that can overflow on
packet-derived values; the fuzz and property tests below enforce it).

### 6.1 `netwire`

Zero-allocation views over `&[u8]` and builders into `&mut [u8]`, returning `Result<_, WireError>`:

* Ethernet II (14 bytes; ethertypes 0x0800 and 0x0806; frames shorter than 14 or with an unknown type are dropped and counted);
* ARP (Ethernet/IPv4 only: hardware type 1, protocol 0x0800, lengths 6 and 4, operations 1 and 2);
* IPv4 (version 4; IHL 5 to 15, options skipped and not interpreted; total length at most the frame and at least IHL×4; header
  checksum verified on receive and computed on transmit; fragment offset or MF set → `Fragment` error);
* ICMPv4 (echo request 8 and reply 0 with checksum; destination unreachable type 3 codes 3 and 4 for transmit);
* UDP (length 8 to the IP payload; a zero checksum on receive is accepted, a non-zero one is verified over the pseudo-header;
  transmit always computes it and never sends zero);
* DHCP (BOOTP header and the options the client uses, sections 8.2; every option length is bounds-checked; options beyond the
  buffer end the parse);
* the Internet checksum (RFC 1071), with incremental update not needed.

### 6.2 `netcore`: the interface and its tables

State: interface (address, mask, gateway, DNS, MAC, MTU 1500, link), the ARP cache, the UDP endpoint table, the ICMP echo
endpoint table, counters, timers. Everything is bounded (section 10).

API (a sketch; the types are the implementer's):

```text
core.ingress(frame: &[u8], now: Instant) -> Vec<Event>       // events for the adapter: Deliver{endpoint, ...}, Notice{...}
core.send_udp(ep, dst, port, payload, now) -> Result<(), SendError>
core.send_echo(ep, dst, seq, payload, now) -> Result<(), SendError>
core.egress(dev: &mut impl Device, now: Instant) -> usize    // at most the caller's M frames
core.next_deadline() -> Option<Instant>
core.configure(cfg) / core.clear_config() / core.link(up: bool)
trait Device { fn rx(&mut self) -> Option<RxFrame>; fn tx_slot(&mut self) -> Option<TxFrame>; }   // none = full
```

`Instant` is an opaque monotonic nanosecond count (NET-DESIGN 6.2). The core reads no clock, allocates only the fixed tables
and the bounded queues of section 10, and never blocks.

### 6.3 Receive rules

For each frame, in this order, first failure drops it and increments exactly one counter:
1. link down or shorter than 14 bytes (`rx_runt`); destination MAC neither ours, nor broadcast (`rx_not_for_us`);
2. ethertype ARP → section 6.4; IPv4 → continue; other → `rx_unknown_ethertype`;
3. IPv4 header invalid (`rx_ip_malformed`), checksum bad (`rx_ip_checksum`), fragment (`rx_fragment`), TTL 0 (`rx_ttl`),
   destination not our address, not `255.255.255.255` and not the subnet broadcast (`rx_ip_not_for_us`); if we have no address
   only DHCP traffic (UDP to port 68 with a broadcast destination) is accepted;
4. protocol ICMP → 6.5; UDP → 6.6; other → `rx_unknown_protocol`.

The receive path **never allocates in proportion to anything the packet claims**.

### 6.4 ARP

* Cache of at most 64 entries, each `(ip, mac, expires)`; an entry lives 60 s after the last confirmation; when full, the entry
  closest to expiry is evicted.
* We answer a request for our address and learn from every request or reply addressed to us (the sender's mapping), only for
  addresses on our subnet and never for broadcast or multicast senders.
* To send to an address with no entry: queue the packet (at most 4 per entry and 16 overall, oldest dropped, counted), send an ARP request
  (broadcast), retry at 1 s and 2 s, and after the third unanswered request drop the queue and report `NoRoute` through a `Notice` for
  UDP sends (cookie-tagged) and silently for echo.
* A gratuitous ARP (request for our own address, sender and target equal) is sent when an address is configured, once.

### 6.5 ICMP

* Echo request to us: reply with the same identifier, sequence and data, TTL 64; at most **100 echo replies per second** in all (token
  bucket; excess dropped and counted `icmp_echo_rate_limited`) so we are not a reflector.
* Echo reply: delivered to the `IcmpEcho` endpoint whose identifier matches; no match is dropped (`icmp_unmatched`).
* Destination unreachable received: counted only (S8 has no connected UDP).
* We send **destination unreachable (port unreachable)** for a UDP datagram to a port with no endpoint, at most 10 per second in all,
  never for broadcast destinations.

### 6.6 UDP and endpoints

* An endpoint is `(local port, optional local address)`; the first to bind a port gets it; a second bind to a bound port is
  `AddrInUse`. Ephemeral ports are allocated from 49152 to 65535, scanning from a rotating start so a freed port is not reused at once.
* A datagram for a bound port becomes a `Deliver` event; payload is at most 1472 bytes (MTU 1500 less 28).
* A send of more than 1472 bytes is `TooBig`. A send with no address configured (source needed) is `NotConfigured` unless the
  endpoint is privileged and the destination is the limited broadcast.
* Route: destination on the subnet → ARP for it; else → ARP for the gateway; no gateway and off-subnet → `NoRoute`.
* Broadcast: limited broadcast or the subnet broadcast address goes to `ff:ff:ff:ff:ff:ff` and requires privilege.

### 6.7 Counters

All the counters named above, 64-bit, readable through `/svc/net-config` (`GetStats`) and printed in the structured output at exit
and on a child's death.

## 7. The client-facing protocol (`proto::net`)

All integers little-endian. A **service connection** (from the registry) carries call/reply messages with transaction ids; an
**endpoint channel** carries the datagram messages. Every decoder is total (no panic on any input) and has goldens.

### 7.1 Service connection (request/reply)

Request: `txid u32 | op u16 | flags u16 | body`. Reply: `txid u32 | status i32 | body`.

| Op | Name | Classes | Request body | Reply body |
|---|---|---|---|---|
| 1 | `Info` | all | none | `addr u32, mask u32, gateway u32, dns u32, mac[6], mtu u16, link u8, configured u8, pad u16` (26 bytes) |
| 2 | `Open` | normal, priv | `kind u16 (1 Udp, 2 IcmpEcho), local_port u16 (0 ephemeral), local_addr u32 (0 any), oflags u32`, plus **one handle: the client's end of a new endpoint channel** | `port u16, pad u16, addr u32` (the binding made) |
| 16 | `SetConfig` | config | `addr u32, mask u32, gateway u32, dns u32` | none |
| 17 | `ClearConfig` | config | none | none |
| 18 | `GetArp` | config | none | up to 64 entries of `ip u32, mac[6], pad u16, age_ms u32` |
| 19 | `GetStats` | config | none | the counter block of 6.7 (a count then `(name_id u16, value u64)` pairs) |

`oflags` (Open): bit 0 `BROADCAST` (privileged only), bit 1 `SRC_ANY` (privileged only: source 0.0.0.0 while unconfigured). An op
or flag the connection's class does not allow is `Refused`.

Status (`i32`): 0 `Ok`, 1 `BadRequest`, 2 `Refused`, 3 `AddrInUse`, 4 `TooBig`, 5 `NoRoute`, 6 `Quota`, 7 `NotConfigured`,
8 `NoBuffers`, 9 `NotSupported`. (`UNAVAILABLE` is the registry's, `proto::file` -109, not ours.)

### 7.2 Endpoint channel

The client creates the channel pair, keeps one end and sends the other in `Open`; the stack serves the other end as a connection of
the same account as the service connection (so the account's connection limit counts endpoints: 7.4). The endpoint lives exactly as long as the
channel: closing it or dying frees the binding; the stack closing its end tells the client the endpoint is gone.

Client to stack:

| Op | Kind | Body |
|---|---|---|
| 1 `SendUdp` | Udp | `cookie u32, dst_addr u32, dst_port u16, pad u16, payload` (payload at most 1472) |
| 2 `SendEcho` | IcmpEcho | `cookie u32, dst_addr u32, seq u16, pad u16, payload` (at most 1472 less 8) |

Stack to client:

| Op | Kind | Body |
|---|---|---|
| 0x81 `Recv` | Udp | `dropped u32, src_addr u32, src_port u16, pad u16, payload` |
| 0x82 `EchoReply` | IcmpEcho | `dropped u32, src_addr u32, seq u16, ttl u8, pad u8, payload` |
| 0x83 `Notice` | both | `code u16, pad u16, cookie u32, detail u32` |

`dropped` is the number of datagrams discarded for this endpoint since the previous delivered one (sums, saturating), so a flood is
visible without extra messages. `Notice` codes: 1 `SendFailed` (detail = status `NoRoute`/`TooBig`/`NotConfigured`), 2 `LinkDown`
(detail 0), 3 `LinkUp`.

The stack sets the ICMP identifier of an `IcmpEcho` endpoint (unique among live endpoints; never reused while one is live) and
delivers only replies carrying it. Sends are **fire and forget**: success is silent, failure is a `Notice` naming the cookie.

### 7.3 Backpressure and the drop rule

* **Toward a slow client:** each delivered datagram is one `channel_write` on the endpoint channel; if the client's queue is full
  the write returns `SHOULD_WAIT` and the datagram is **dropped and counted** (`endpoint_dropped`, and the next delivered
  message carries it). The stack never waits for a client. The client-charged kernel queue is the bound (OD59).
* **Toward the stack:** a client that sends faster than the stack drains is held by the kernel's channel limits (`SHOULD_WAIT` to the
  client); the stack reads at most K = 16 endpoint messages per loop iteration (section 9.3).
* **Service connection replies** use slice R's hold-and-poll policy unchanged.
* **Violations** (unknown op, wrong size, an oversize payload, a message of the wrong kind) close that endpoint or connection and
  count against its account, per R; they never end the service.

### 7.4 Accounts and limits

Connections, including endpoint channels, are charged to the account of the service connection that opened them (R, OD77, OD84).
Slice R's file-protocol defaults (about 16 connections per account, 200 in the pool) are **not** used: `netd` sets **32 connections per
account** (one is the service connection) and a pool of **256**, and a stack-side per-endpoint state (ARP queue entries are shared and bounded
globally; an endpoint's own state is its port binding and a small record) is charged as one connection each. These constants are in
one place (`proto::net::limits`) and overridable by the launcher per program.

### 7.5 The client library (`rt::net`)

Built on the typed request/reply client that slice R has not yet built (built here, in `rt`). Every call carries a **mandatory
deadline** (no forever form; the default helper uses 5 s). It reconnects on `UNAVAILABLE` with a bounded retry (the policy SD step 4
shapes: first retry after 100 ms, doubling to 1 s, giving up at 10 s). API sketch: `Net::connect(name)`, `net.info()`,
`net.open_udp(port) -> Udp`, `udp.send_to(addr, port, payload, deadline)`, `udp.recv(deadline) -> Datagram` (with `dropped`),
`net.open_echo() -> Echo`, `echo.ping(addr, seq, payload, deadline) -> Reply`.

### 7.6 Programs delivered by S8

`ping [-c N] ADDR` (an `IcmpEcho` client; prints one structured line per reply and a summary), `udpecho PORT` (binds PORT and echoes every
datagram to its sender), `netinfo` (calls `Info`), `dhcpc` (section 8). They go in the boot image (`bin/`); the launcher job for each
has the default 128 threads (it uses one).

## 8. `netd`, DHCP and the configuration

### 8.1 `netd`'s loop (NET-DESIGN 6.2, made concrete)

One thread, one port, built on `rt::service::Core`. Bound on the port: the three listener channels (names of 4.4), each service
connection and endpoint channel (R's keys), the `drv_to_net` event and the driver control channel (`other_event`, keys with the top bit
set). Each iteration, in order, with the limits N = 32, K = 16, M = 32:

1. read at most N receive frames from `rx_used`, hand each to `core.ingress`, return each slot through `rx_free` as soon as the core
   has finished with it (the core copies nothing it needs beyond the frame's lifetime);
2. apply the `Deliver`/`Notice` events: one `channel_write` each (7.3);
3. serve at most K client messages (service connections and endpoints), in arrival order;
4. `core.egress` for at most M frames into free TX slots, publish them through `tx_ready`, reclaim `tx_done`;
5. doorbells and the waiting handshake (5.5);
6. wait on the port with the deadline `min(core.next_deadline(), now + 10 ms)`.

### 8.2 DHCP (`dhcp`, pure; `dhcpc`, the program)

The state machine is the RFC 2131 client: `Init` → `Selecting` (DISCOVER broadcast, wait for OFFER) → `Requesting` (REQUEST, wait for ACK)
→ `Bound` → `Renewing` (at T1 = lease/2, unicast REQUEST) → `Rebinding` (at T2 = 7/8 lease, broadcast) → `Init` on expiry or NAK.
The client sets the BOOTP **broadcast flag** (it cannot receive a unicast reply before it has an address; slirp then answers to 255.255.255.255, as observed), and
because slirp sends no options 58 or 59 it **computes T1 and T2** (lease/2 and 7/8 of the lease) when they are absent. Retransmission: 4 s, 8 s, 16 s, then 32 s, with the jitter supplied by the caller from a seeded source (so tests are deterministic),
never more than 4 packets in any 10 s while unconfigured. It uses options 53 (message type), 54 (server id), 51 (lease), 1 (mask), 3
(router), 6 (DNS, first only), 58 and 59 (T1, T2, when present), 50 (requested address), 61 (client id) and ignores every other option. Parsing never
trusts a length. It does **not** probe the offered address with ARP (non-goal) and it does not implement INFORM, release
or multiple interfaces.

`dhcpc` holds a `/svc/net-priv` connection (an `Open` with `BROADCAST` and `SRC_ANY` on port 68, kind Udp) and a `/svc/net-config`
connection (`SetConfig` on `Ack`, `ClearConfig` on loss). It reads and writes its half of the config blob (8.4) so a restart resumes
(renews) instead of starting over.

### 8.3 Static configuration

`SetConfig` through the config connection works with no DHCP (the harness and tests use it: `net_static_config`).

### 8.4 The config blob (held by `netsup`)

One page, two halves, each written by one process with a sequence counter (odd while writing, even when stable; the reader retries
until it reads the same even value twice):

* **Interface half** (written by `netd`): `seq u32, valid u32, addr, mask, gateway, dns` and the time it was applied.
* **Lease half** (written by `dhcpc`): `seq u32, valid u32, server_id u32, offered addr, lease_s u32, t1_s u32, t2_s u32, acquired_at (ns, kernel
  monotonic, so it stays valid across restarts within one boot)`.

On start `netd` applies a valid interface half at once (so a restart does not wait for DHCP); `dhcpc` resumes in `Bound`/`Renewing`
from a valid lease half. A reader that finds nonsense (checksum field wrong) treats it as invalid.

## 9. Supervision

### 9.1 The tree and the contract

```text
netsup   permanent; restart intensity 3 restarts in 30 s per child and 6 in 60 s overall; then the network is marked down
 ├─ netdrv   permanent, one_for_one: reset device, generation + 1, re-attach
 ├─ netd     permanent, one_for_one: re-apply config blob, re-register names, attach
 └─ dhcpc    permanent, one_for_one: resume from the lease half
```

The contract table is NET-DESIGN section 9; this section adds the exact mechanics. "Marked down" means: `netsup` stops restarting, closes the
`netd` listeners it holds so the registry entries go stale (`UNAVAILABLE`), prints `##NETSUP down`, and stays alive (so the leaks it
holds are accounted and a human or test can query it). A child that exits with a code in the "no restart" set (a driver that finds the
device unusable, 5.2) counts as down immediately.

### 9.2 `supervise` (pure)

A small pure module (`no_std`, host-tested): `Supervisor::on_exit(child, now) -> Action` where `Action` is `Restart{delay}`, `Down`.
It holds the restart history per child and overall, applies the intensity rule, and uses a restart **delay** of 0 for the first restart, then
250 ms, then 1 s, so a crash loop does not spin. It is designed to become the shared supervisor library (TIDY-UP entry).

### 9.3 The `netsup` control channel

Given only to whoever launches the stack (the boot suite, the shell's admin). Messages `txid u32 | op u16 | pad u16 | body`:
`Status` (op 1: per child: running, restarts, last exit code, generation) and `Restart{child}` (op 2: kills the named child, which then goes
through the normal restart path; used by the churn tests and by an administrator). No other authority is exposed.

### 9.4 The flight recorder and the structured output

One page: 64 event records of 64 bytes (`time ns u64, kind u16, child u16, a u64, b u64, c u64` and padding) and the last 16 received frame
headers (`len u16`, the first 62 bytes). `netd` and `netdrv` append to it as they run (a plain store, no lock: each writes its own
region); `netsup` prints it as structured lines when a child dies or the network goes down:

```text
##NETSUP child=netd exit=-2001 restarts=2 gen=3
##NETFR t=123456 kind=rx_frame len=60 head=ffffffffffff525400123456...
##NETSUP down reason=intensity
```

All of this is `##` text through the existing serial path (OD52), so the headless harness reads it.

## 10. Limits (one place: `proto::net::limits` and `netring::consts`)

| Quantity | Value | Why |
|---|---|---|
| Slots per pool, slot size | 128, 2048 B | 1526 B needed; power of two; 256 KiB per pool, within the 64-page DMA object cap |
| Ring capacity | 128 | equals the slot count: a ring cannot legitimately overflow |
| Frame maximum | 1514 B | Ethernet without VLAN |
| UDP payload maximum | 1472 B | MTU 1500, no fragmentation |
| ARP cache, pending packets | 64 entries; 4 per entry, 16 total | bounded memory under an ARP storm |
| Endpoints | 32 connections per account, 256 in the pool | 7.4 |
| Echo replies sent, port unreachables sent | 100 per second, 10 per second | not a reflector |
| Loop bounds | N 32 rx, K 16 client messages, M 32 tx | no starvation either way |
| Safety poll | 10 ms | 5.5 |
| Restart intensity | 3 in 30 s per child, 6 in 60 s overall; delays 0, 250 ms, 1 s | 9.2 |
| DMA used | 130 pages of 1024 | 4.3 |
| Flight recorder | 64 events, 16 frame heads | 9.4 |

## 11. Invariants (each gets a check; INVARIANTS.md gains N1 to N10 in step 6)

* **N1** Every pool slot is in exactly one ownership state at all times (5.4); no slot is in two rings or lost. *Check:* the ring model,
  after every operation, and the QEMU leak check (all 256 slots accounted at the end of every run).
* **N2** The stack never learns a physical address and never maps a pool writable that it should not. *Check:* `netd`'s handles lack `DMA`;
  the RX pool mapping is read-only (a store to it faults: a QEMU test).
* **N3** The driver never maps a packet pool. *Check:* its handle table and address space contain no pool mapping (a test inspects `process_info`/region count).
* **N4** No value read from a shared ring, the control header or a packet is trusted without validation (5.4). *Check:* the violation tests (12, Q16, Q17).
* **N5** Every queue and table is bounded, and its full behaviour is the stated drop or error (section 10). *Check:* flood and storm tests.
* **N6** An endpoint exists exactly as long as its channel; closing or dying frees the port, the identifier and the account charge. *Check:* leak and
  port-reuse tests.
* **N7** Everything an endpoint holds is charged to its account; one account cannot exhaust another's (7.4). *Check:* Q6.
* **N8** `netd` and `netdrv` never block on a client or on each other (only on their own port with a deadline). *Check:* the stalled-client flood test, and the safety-poll test.
* **N9** No allocation is proportional to a packet-claimed value (6.3). *Check:* the proptest and fuzz suites with a counting allocator.
* **N10** No panic on any input from the wire, a ring or a client. *Check:* proptest and fuzz suites for every parser and `proto::net`.

## 12. Acceptance (runnable checks, written before the code)

Where each lives follows `TESTING.md` rule 5: pure logic on the host, behaviour in the QEMU suite, only what the machine shows in a
harness script. Tier 0 = host, tier 1 = the smoke boot, tier 2 = soak. **Every check is written in step 0 and fails until the step that makes it
pass.** "Mutation check" means: introduce the stated bug by hand once, confirm the named test fails, remove it.

### 12.1 Host (tier 0)

**`netwire`** (step 1)
* **H1 goldens from a real capture.** A pcap captured from QEMU (`filter-dump`) of an ARP exchange, a ping, DHCP discover/offer/request/ack
  and UDP in both directions is committed as `net/tests/data/*.pcap`; every frame parses to the expected fields and **rebuilds to the same bytes**.
  The same pcap is checked by `scripts/net_pcap_check.py` (the IPv4, ICMP and UDP checksums; its `--self-test` proves it can fail) and by host
  `tcpdump -nn -vv -r` as a second opinion (any `bad cksum` fails; see 3.2 for what tcpdump can and cannot verify). The first real capture,
  `net/tests/data/slirp-probe.pcap`, exists already and must parse and rebuild byte for byte, including the 60-byte padded frames (the payload comes
  from the IP length, never the frame length).
* **H2 properties.** `parse(build(x)) == x` for every message; the checksum equals a naive reference (a bytewise one's-complement sum written
  separately in the test) and the RFC 1071 example; flipping any single bit of a built frame makes the checksum check fail or the parse reject.
* **H3 totality.** Random bytes, random truncations and random mutations of valid frames (at least 200,000 cases in the test, a `cargo-fuzz` target per
  parser kept in the repo for long runs) never panic, never index out of bounds, and never allocate more than a stated constant (a counting
  allocator).

**`netcore`** (step 2)
* **H4 scripted packet tests** (inject exact frames with a controlled clock, assert exact responses): ARP request for us answered; ARP reply learned and
  used; the pending queue and its overflow and expiry; cache full eviction; three unanswered ARP requests report `NoRoute`; echo request answered
  with identical data; the echo rate limit; UDP delivered to the bound endpoint; UDP to a closed port gets port unreachable (rate-limited, never
  for broadcast); bad IP checksum, a fragment, TTL 0, wrong destination and a runt are each dropped with exactly their counter; the broadcast and
  subnet-broadcast rules; gateway versus on-link routing; no address configured; a 1473-byte send is `TooBig`; a frame padded to 60 bytes whose IP length says 29 delivers a 1-byte payload (not the padding); the first fragment and the next fragment of a 3000-byte datagram (taken from the capture) are each dropped and counted `rx_fragment`; link down drops everything.
  *Mutation checks:* drop the checksum verification; accept fragments; reuse an echo identifier; never evict.
* **H5 interop with smoltcp as an independent peer** (a test-only crate; OD18 allows smoltcp as a test peer, OD27 test-time crates): over `netsim`'s
  link our core and smoltcp exchange ARP, ICMP echo (each pings the other) and UDP in both directions, including 1472-byte datagrams and
  an address on each side of the gateway. Any disagreement is a failing test whose frames are printed as hex.
* **H6 deterministic simulation.** For seeds 1 to 500 (in tier 0; 20,000 in tier 2): two cores and a link with seeded loss, duplication, reordering
  and delay, a mix of sends and receives, endpoints opened and closed; properties: every delivered datagram equals one that was sent, none is
  delivered to the wrong endpoint, ARP always converges when frames get through, and no table or queue ever exceeds its bound. A failing seed
  prints and reproduces exactly.

**`dhcp`, `supervise`, `proto::net`, `netring`** (steps 2, 3, 4, 5)
* **H7 DHCP:** the captured exchange replayed (the OFFER and ACK from QEMU's server) drives the client to `Bound` with the expected address, mask, router, DNS
  and lease; a property test over arbitrary event and timer sequences: never panics, never sends more than 4 packets in 10 s unconfigured, timers
  never go backwards, NAK and expiry return to `Init`; hostile options (a length running past the end, duplicate options, option 255 early) are
  ignored without a panic. *Mutation check:* trust an option length.
* **H8 `supervise`:** against a reference that recomputes the intensity from the full history: the per-child and overall limits, the delay sequence,
  `Down` stays down. *Mutation check:* off-by-one in the window.
* **H9 `proto::net`:** goldens for every message; every decoder total; an account model for 7.4 against an independent recomputation (32 per account,
  256 in the pool, charges and refunds exact). *Mutation check:* skip the refund on close.
* **H10 ring model:** `netring`'s model of the four rings and the 256 slots against a reference that tracks each slot's state: random operations from both sides,
  after each one N1 holds; a malformed entry (slot out of range, length too big, a slot in the wrong state) is rejected and counted. Under loom and shuttle: the waiting-flag
  handshake of 5.5 with a producer and a consumer never loses a wakeup. *Negative control:* setting the waiting flag *after* the re-check
  must fail the model. *Mutation checks:* skip the release on publish; trust a length.

### 12.2 QEMU, in the suite (tier 1, one boot at four harts) unless noted

Each test starts the stack through `netsup` under the suite's launcher, with explicit views, and ends with the exact leak checks (frames,
objects, job pages, handles, all 256 pool slots) the suite already applies at its end.

* **Q1 `net_driver_attaches`:** `devmgr` claims the net device; `netdrv` reads the MAC and it equals `52:54:00:12:34:56` (set explicitly); the control header shows
  `state = 2`, `slots = 128`, `generation = 1`; `netd`'s RX mapping is read-only.
* **Q2 `net_static_config_and_ping_gateway`:** configure 10.0.2.15/24 gateway 10.0.2.2 through `/svc/net-config`; `ping` the gateway: at least 4 of 5 echo
  replies within 1 s each, correct identifier, sequence and data.
* **Q3 `net_dhcp_configures_the_interface`:** `dhcpc` gets an address from QEMU's server and `Info` reports it (10.0.2.15, mask 255.255.255.0, gateway
  10.0.2.2, DNS 10.0.2.3, lease 86400 s, the values observed in 3.2, with T1 and T2 computed because slirp sends neither); then Q2's ping succeeds. Also: a `ping` of
  10.0.2.3 (slirp answers) succeeds, and a `ping` of an unanswered address (10.0.2.99) gets no reply, with exactly three ARP requests sent (6.4) and the `NoRoute` outcome. (This is the S8 "ping the gateway" acceptance, over the real flow.)
* **Q4 `net_udp_echo_through_hostfwd`** (harness script, independent client): `udpecho 7777` runs in the guest; the **host script** sends datagrams of
  1, 100, 512 and 1472 bytes (random content) to `127.0.0.1:HOSTPORT` and requires each echoed back byte for byte from the same address and port; sends 200 in
  a row and requires all 200. It then sends 1473 and 3000 bytes: slirp fragments them (3.2), the stack drops the fragments, nothing is echoed, the guest
  stays responsive and `rx_fragment` counts them (no reassembly in S8 is a stated limit, not a bug).
* **Q5 `net_capture_is_clean`** (harness script): the pcap written by `filter-dump` during Q3 and Q4 is read by `scripts/net_pcap_check.py` (and `tcpdump -nn -vv -r` as a second opinion): no checksum is wrong, the guest's ARP
  request, DHCP DISCOVER and REQUEST, the echo requests and the UDP replies are all present with the expected counts; the guest never sent a frame
  longer than 1514 bytes or an IP fragment.
* **Q6 `net_account_confines_a_client`:** a client opens endpoints until refused: exactly 31 succeed (the 32nd connection is the service connection) and the
  next is `Quota`; a second client's endpoints are unaffected; closing releases exactly.
* **Q7 `net_authority_by_name`:** on `/svc/net` binding port 80, sending broadcast, `SRC_ANY` and `SetConfig` are each `Refused`; on `/svc/net-priv` the first three succeed and
  `SetConfig` is refused; on `/svc/net-config` `Open` is refused; a program whose view lacks a name cannot open it.
* **Q8 `net_flood_to_a_stalled_endpoint`:** the host script floods a guest endpoint whose owner does not read (thousands of datagrams); the guest still answers
  `ping` meanwhile; `netd` never exceeds its bounds (counters), the endpoint's `dropped` field is non-zero once the client drains, and nothing leaks.
* **Q9 `net_arp_and_echo_storms_are_bounded`** (host-side injector through a second QEMU netdev in tier 2): thousands of ARP requests for other
  addresses and echo requests per second: the cache stays at its bound, echo replies stay within 100 per second, and the stack stays responsive.
* **Q10 `net_endpoint_closes_with_its_channel`:** closing the client's endpoint end frees the port (immediately rebindable), the ICMP identifier and the
  account charge; a client that dies without closing does the same.
* **Q11 `net_driver_restart`:** with a ping loop running, `Restart{netdrv}`: pings resume within 3 s (link flap), `generation` increased by exactly one, an open
  UDP endpoint still works afterwards, no pool slot is lost (N1), the netsup output shows exactly one `##NETSUP child=netdrv` line.
* **Q12 `net_stack_restart`:** with an open endpoint and a ping loop, `Restart{netd}`: the client's endpoint channel is peer-closed, the registry answers `UNAVAILABLE` and
  then succeeds (the client library's reconnect), the interface address is back **without a new DHCP DISCOVER** (the pcap has none after the first), the old
  endpoint's port is free, and pings resume.
* **Q13 `net_dhcp_restart`:** `Restart{dhcpc}`: the address is kept, the new `dhcpc` resumes from the lease half (the pcap shows no new DISCOVER), and a renewal at a shortened test lease succeeds.
* **Q14 `net_supervisor_gives_up`:** a `netd` built with the negative-control feature `fault-crash-on-magic` (panics on a UDP payload starting with a fixed marker) is
  fed the marker four times in 30 s by the host script: `netsup` prints `##NETSUP down reason=intensity` and the flight recorder lines, the registry reports
  `UNAVAILABLE` for good, and nothing keeps restarting (a bounded count of `##NETSUP child=` lines).
* **Q15 `net_driver_violation_is_survivable`:** a `netdrv` built with `fault-bad-slot` (publishes an out-of-range slot) makes `netd` count `ring_violations`, reject the
  entry and ask for a driver restart; a `netd` built with `fault-bad-slot-tx` makes the driver do the same in the other direction; neither takes the machine
  down and the supervisor's output names the reason.
* **Q16 `net_pools_cannot_be_abused`:** from `netd`'s side a store to the RX pool faults; `vmo_phys_addr` on `netd`'s pool handle is refused; the driver's address space has
  no pool mapping.
* **Q17 `net_lost_doorbell_does_not_hang`:** a `netd` built with `fault-no-doorbell` never signals: traffic still flows within the 10 ms safety poll (a latency bound, not a hang),
  and `doorbells_suppressed`/the poll counters show why.
* **Q18 `net_modes`:** Q2, Q3, Q4 and Q11 also run on the plain `virt` machine (no interrupt controller: the driver polls) in tier 2, and at `-smp 2` and `-smp 4` in tier 2 (smoke runs
  four harts only; icount mode runs one hart, TIDY-UP T21, and is not used to claim SMP results).
* **Q19 `net_leak_and_churn`** (tier 2): 200 rounds of start the stack, open and close endpoints, kill each child at a random point under traffic, shut down; the end-of-run leak
  checks stay exact (frames, objects, pages, handles, slots) and the DMA budget returns to its start.

### 12.3 Benchmarks (tier 2, `make os-bench`; step 6)

`##BENCH net_udp_echo_pps` (packets per second with one echo client against the host script at maximum rate, with the drop counters), `##BENCH net_ping_rtt` (median and
99th percentile of 1000 pings to the gateway), `##BENCH net_bkl_profile` (big-lock acquisitions, waits and the wake counters during both). The first numbers become `bench/baseline.tsv`
rows; they are the load tasks 11 and 12 of `SCHED-SMP-SPEC.md` are planned to use.

### 12.4 Negative controls and independent oracles, summarised

* Negative-control builds: `fault-crash-on-magic` (netd), `fault-bad-slot` (netdrv), `fault-bad-slot-tx` (netd), `fault-no-doorbell` (netd), `fault-skip-rx-checksum` (netcore, host only: H4's
  bad-checksum frame must then be accepted, proving the test can fail).
* Oracles the writing context did not produce: smoltcp (H5), `tcpdump`'s checksum verification and real QEMU captures (H1, Q5), QEMU's own DHCP server (H7, Q3) and slirp's ARP and ICMP replies
  (Q2), the host's UDP stack as the echo client (Q4).

## 13. Records this slice proposes (numbers continue from OD97)

* **OD98** the process architecture and authority: `netsup`, `netdrv`, `netd`, `dhcpc`; classes of authority by registry name; the `devmgr` net claim (sections 4, 8).
* **OD99** the driver/stack data path: pools, the four rings, ownership, events as doorbells, the safety poll, attach/re-attach (section 5).
* **OD100** the native network protocol, the datagram drop rule and endpoint accounting (section 7).
* **OD101** the failure contracts, the restart policy and the flight recorder (section 9).
* **TIDY-UP** entries to write in step 6: DMA charged to `devmgr`'s job (4.3); the hand-rolled supervisor to extract into a library (9.2); "client creates the stream socket is unproven" (S9);
  no fragmentation; no ARP probe before using a DHCP address; the 10 ms safety poll and any measured doorbell cost.
* Documents updated when built: INVARIANTS (N1 to N10), ROADMAP (S8 boxes, only when verified in QEMU at `-smp 2` and `-smp 4`, saying plainly which ran), `NET-DESIGN.md` (status and any deviation),
  `TESTING.md` (the new tests' tiers and budgets), `SD-SPEC.md`/ROADMAP (step 4 done).

## 14. Risks

* **A kernel assumption in 3.4 is false** (a pool cannot be mapped read-only in another job, or a narrowed DMA duplicate misbehaves). Step 3 would stop and ask.
* **The re-attach protocol is subtle** (5.6). Mitigations: two tiny state machines, the generation number, Q11 and Q12, and the fallback of restarting both together (NET-DESIGN 14).
* **The typed request/reply client and SD step 4 are not built**, so step 4 is larger than a typical one. Mitigation: they land first in that step, each with its own tests, and may be split into their own PR.
* **Facts in 3.2 not yet checked** could change a constant (the lease option set, slirp's ARP behaviour). Step 0 checks them first; the cost is small.
* **Timing on the shared big lock:** latency and throughput numbers will be poor before tasks 11 and 12; the benchmarks record that rather than hide it, and no S8 check asserts a latency bound
  tighter than the 10 ms safety poll.
* **No fragmentation** may bite a real DNS reply or a large UDP application in S9; the stack counts every fragment it drops so the need would be visible.

## 15. Open points (answered by the owner on 2026-11-14; kept for the record)

1. **Names:** `netsup`, `netdrv`, `netd`, `dhcpc`, `ping`, `udpecho`, `netinfo` and the registry names `/svc/net`, `/svc/net-priv`, `/svc/net-config`: fine, or others?
2. **Limits:** 32 connections per account and 256 in the pool (7.4), the 100 and 10 per second reply limits (6.5), 128 slots (5.1): accept as starting values, to be measured?
3. **DMA charged to `devmgr`** (4.3): accept as a recorded shortcut, or should a claim take the claimant's job (needs a small `devmgr` and possibly kernel change)?
4. **Both QEMU variants:** smoke runs only the default AIA machine at four harts and the rest is soak (12.2 Q18), matching `TESTING.md`; is that enough to tick the roadmap box, or do you want one plain-`virt`
   run in smoke as well?
5. **Resolved (2026-11-14):** step 0's slirp checks were wanted and are done (3.2); what remains unchecked is listed there. Owner answers recorded: 1 names yes, 2 limits yes, 3 DMA charged to `devmgr` accepted as a shortcut (TIDY-UP T84), 4 one smoke boot yes.
