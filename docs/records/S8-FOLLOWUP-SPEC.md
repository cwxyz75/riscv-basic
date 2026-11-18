# S8 follow-ups: making S8 ready for S9 (slice brief)

Status: brief for one slice, written 2026-11-15. S8 steps 1 to 5 are merged and verified (see `ROADMAP.md`, S8 progress); this
slice does the cheap changes that `S9-SPEC.md` section 18 says S9 needs from S8, **before** S9 code starts, because they are
awkward to retrofit under tests that already exist. It starts no TCP. Tests come first, as always.

Read first: `S9-SPEC.md` section 18 (the list, 12 items) and the sections it points at; `NET-SPEC.md` sections 3.1, 3.2, 4.2, 6.2
to 6.6, 7.1, 7.4, 8.1, 9, 11 (N6) and 12 (H6, H9); `TIDY-UP.md` T84, T86, T87; the S8 crates (`netwire`, `netcore`, `netsim`,
`netring`, `proto`, `rt::net`, `netd`, `netsup`).

## 1. Goal

Walk the 12 items of `S9-SPEC.md` section 18 against what S8 **actually built** (T87 lists where the code already differs from
the spec) and for each one decide, in writing:

- **A. Spec wording**: change `NET-SPEC.md` only (items 3, 5, 11 are mostly this).
- **B. Cheap code change in S8**, with a test written first (items 1, 2, 4, 8, 9, 10 are candidates).
- **C. Already true or no longer applicable**, with the evidence (a line reference or a test name).
- **D. Deferred**, with a reason and a `TIDY-UP.md` row.

The deliverable is the item-by-item table (one row per item: A/B/C/D, what changed, which test) at the end of this file, filled
in, in the PR description too.

## 2. Candidate changes (verify each against the code first)

| # | Section 18 item | Likely class | What done looks like |
|---|---|---|---|
| 1 | Reserve `Open` kinds 3 and 4, statuses 10 to 16, endpoint ops 16 to 19, notice codes 16 to 21; a request may carry more than one handle | B | `proto::net` constants reserved and documented; decoders reject reserved values with the right status (host test per value); the typed client and `netd`'s decoder accept a request with 2 handles (a test with a two-handle message that the S8 stack answers `BadRequest`/the documented status, not a decode error or a handle leak) |
| 2 | The account record carries `stream_bytes` beside the connection count | B | the field exists, defaults to 512 KiB (`S9-SPEC.md` 12), is unused by S8 and shown in `GetStats`/account tests; H9 updated; existing accounting tests unchanged |
| 3 | Ephemeral ports: S8 may keep the rotating start but must not promise it; UDP and TCP port spaces are separate | A | `NET-SPEC.md` 6.6 reworded; no code change unless S8 promised a sequence in a test (then loosen that test) |
| 4 | Protocol 6 dispatch; a received destination-unreachable is passed to the transport it quotes | B (small) | `netcore` has a dispatch point for protocol 6 that in S8 counts `rx_unknown_protocol` (as now); an ICMP error quoting a UDP datagram is delivered to the UDP layer's hook (a host test), still only counted in S8 |
| 5 | N6 wording: an endpoint exists exactly as long as its channel, for the client's control of it | A | `NET-SPEC.md` 11 and `INVARIANTS.md` (if N6 is mirrored there) reworded |
| 6 | 8.1: two more bounded loop phases; constants N, K, M joined by P | A | `NET-SPEC.md` 8.1 notes the extension; no code |
| 7 | 4.2 and 9: `netsup` holds the secret VMO and passes it read-only to `netd`; flight recorder reserves event kinds for TCP | A, maybe B | spec wording; if the flight recorder has an event-kind enum (T87 says it was reduced to one `##NETFR` line) just reserve the numbering in the doc |
| 8 | 6.2: IP identification counter and DF bit unstated; S9 assumes DF clear; a loopback path | A, B | state the identification counter and DF bit as built; add the in-guest loopback path in `netcore` (host-tested with `netsim`) only if it is small, else D |
| 9 | `netsim` needs corruption, a rate limit with a tail-drop queue, a hook to replace one endpoint (the restart) | B | each fault added with a test of its own (a model that checks the fault does what it says), H6 extended; this is the most valuable item here |
| 10 | Harness line gains `hostfwd=tcp`; the capture checker's `make docs-check` line gains the TCP capture | B (part) | add the `hostfwd=tcp` entry to the harness netdev line used by the net boot tests so it is ready; the checker line is S9 step 0's job, leave it |
| 11 | 3.2: slirp is itself a TCP endpoint; the host's `connect` succeeds before the guest answers via `hostfwd` | A | two table rows in `NET-SPEC.md` 3.2, marked "from the S9 probe, 2026-11-14" until S9 step 0 re-verifies them |
| 12 | The TIDY-UP "client creates the stream socket is unproven" entry | A | confirm whether that row was written (NET-SPEC step 6); if so point it at `S9-SPEC.md` section 4 and step 1; if not write it |

## 3. Out of scope

- Any TCP code, any `proto::net` behaviour beyond reserving values and rejecting them.
- The S9 probe scripts and capture (see `S9-STEP0-SPEC.md`).
- T86 (smoltcp interop) and T87 (S8 deviations): these are the owner's decisions, not this slice's. Touch them only where a
  section 18 item overlaps, and say so in the table.
- The roadmap ticks: S8's boxes are already ticked; do not tick anything new unless a verified test backs it.

## 4. Process and acceptance

1. Write the table (section 5 below) first, from reading the code, before changing anything.
2. For each B item, write the failing test, then the change. Host tests where possible (`make os-test`), the suite or QEMU only
   if the change touches booted code.
3. Gate: `make check` green. If a B change touches booted code (`netd`, `netsup`, `netdrv`, the harness line), also run the
   net boot test at `-smp 2` and `-smp 4` and say which was run; icount runs only one hart (T21).
4. No kernel changes. If one seems needed, stop and ask.
5. Record any shortcut in `TIDY-UP.md` in the same PR.
6. One PR (several commits). Ordinary review; ask for Fable only if a change reaches the ring, accounting or authority code.

## 5. Result table (to be filled in by the slice)

| # | Class (A/B/C/D) | What changed or the evidence | Test |
|---|---|---|---|
| 1 | B | `proto::net::reserved` (kinds 3-4, statuses 10-16, endpoint ops 16-19, notices 16-21) and `classify_open_kind`; `netd` answers a reserved kind `NotSupported` and any other unknown kind `BadRequest`; `rt::net::Net::call_with` sends any number of handles; the stack closes every handle it is sent | `proto/tests/wire.rs` `reserved_values::*` (5 host tests); QEMU suite `net_clients_are_confined` (reserved kind, unknown kind, two handles, no handle: each status, each handle seen closed, account untouched) |
| 2 | B | `svc::Limits.stream_bytes` with `charge_stream`/`release_stream`/`stream_usage`, parent cap, close refused while charged; `limits::STREAM_BYTES_PER_ACCOUNT` 512 KiB, `POOL_STREAM_BYTES` 4 MiB; netd's accounts carry them; S8 charges none | `proto/tests/net_accounts.rs` `stream_bytes_are_charged_and_refunded_exactly_down_the_chain`; existing accounting tests unchanged |
| 3 | A | `NET-SPEC.md` 6.6: the sequence is not a promise; UDP and TCP port spaces are separate. No S8 test pins a sequence. | none needed |
| 4 | B | `netcore`: `PROTO_TCP` dispatch (`rx_tcp`, counts `rx_unknown_protocol`); a received destination-unreachable is checked (ours: IPv4, whole header, our source address, room for ports) and handed to the UDP, TCP or other hook; S8 hooks count (`icmp_error_udp/tcp/other_protocol/malformed`, appended so counter ids are stable) | `netcore/tests/scripted.rs`: `a_tcp_segment_reaches_the_tcp_dispatch_which_in_s8_counts_it`, `an_icmp_error_goes_to_the_transport_it_quotes`, `an_icmp_error_quoting_something_we_did_not_send_is_not_passed_on` (two mutations checked) |
| 5 | A | `NET-SPEC.md` 11 and `INVARIANTS.md` NET6 reworded (client's control; TCP orphans drain, charged) | none (wording) |
| 6 | A | `NET-SPEC.md` 8.1 notes the two extra phases and the constant P | none |
| 7 | A | `NET-SPEC.md` 4.2: `netsup` also holds the secret VMO, passes it read-only to `netd` only; flight-recorder kinds 32-63 reserved for TCP (S8 has no kind enum: one `##NETFR` line) | none |
| 8 | A + D | A: `NET-SPEC.md` 6.2 states what is built: the identification is a counter from 1, **DF is set on everything sent** (this contradicts S9-SPEC's "DF clear": flagged there for S9 step 2). D: no loopback path (not small, and nothing uses it yet): T89 | none; T89 |
| 9 | B | `netsim`: `corrupt_pct` (one bit), `rate` + `queue_limit` per direction with tail drop, `Link::replace_endpoint`; counters `corrupted`, `tail_dropped`, `orphaned`; H6 text extended | three `netsim` tests, each mutation-checked (queue bound, replace, bit flip) |
| 10 | B (part) | harness netdev line gains `hostfwd=tcp:127.0.0.1:TCPHOSTPORT-:7778` (a free port, `OS_NET_TCP_HOSTPORT` overrides); the capture checker's docs-check line is S9 step 0's | the net boot tests at `-smp 2` and `-smp 4` (273 tests, capture check clean) |
| 11 | A | two rows added to `NET-SPEC.md` 3.2, marked "from the S9 probe, 2026-11-14", pending S9 step 0 | none |
| 12 | A | the row was **not** written: T90 now states it and points at `S9-SPEC.md` section 4 and step 1 | none |
