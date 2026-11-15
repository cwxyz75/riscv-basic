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
| 1 | | | |
| 2 | | | |
| 3 | | | |
| 4 | | | |
| 5 | | | |
| 6 | | | |
| 7 | | | |
| 8 | | | |
| 9 | | | |
| 10 | | | |
| 11 | | | |
| 12 | | | |
