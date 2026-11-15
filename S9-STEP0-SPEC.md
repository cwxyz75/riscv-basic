# S9 step 0: the slirp TCP probe and its capture (slice brief)

Status: brief for one slice, written 2026-11-15. This is step 0 of `S9-SPEC.md` (section 2), restricted to the **empirical
groundwork**: scripts and a real capture, no Rust. It is independent of `S8-FOLLOWUP-SPEC.md` and can run in parallel with it
(it touches `scripts/`, `net/tests/data/`, the `Makefile` docs-check line and `S9-SPEC.md` section 3.2 only).

Background: while drafting `S9-SPEC.md` the writer ran four probe scripts (`tcp_probe.py`, `tcp_probe2.py`, `relay_check.py`,
`http_check.py`) against QEMU 11.1.3's slirp and captured `slirp-tcp-probe.pcap` (334 TCP segments, all checksums good by
the script and by `tcpdump -vv`). They were written to a scratch directory and **were not kept**. The spec's section 3.2
facts rest on them, so this slice recreates and commits them, and re-verifies every claim.

Read first: `S9-SPEC.md` sections 2, 3 (all of 3.2) and 14.5; `NET-SPEC.md` section 3.2 and `scripts/net_probe_slirp.py`,
`scripts/net_pcap_check.py` (the S8 probe and checker this extends); the `docs-check` target in the `Makefile`.

## 1. Deliverables

1. `scripts/net_probe_slirp.py` gains a `--tcp` mode (the spec names this flag; keep one script, not four), or, if that gets
   unwieldy, `scripts/net_probe_slirp_tcp.py` beside it; say which in the PR. The mode does what the spec's 3.2 table
   describes, in the same style as the S8 probe: QEMU `-machine none`, slirp and a `socket,listen` netdev on a hub, a Python
   fake guest speaking 4-byte length-framed Ethernet. It exercises: a guest-initiated TCP connection to a host-side server (the
   host `python3 -m http.server` or a plain `socketserver`), a host-initiated connection through `hostfwd=tcp`, SYN/SYN-ACK
   option negotiation (MSS only), data both ways, retransmission timing of an unanswered segment, FIN/RST behaviour, and a
   lossy-relay variant (the `relay_check` idea: a Python relay on the socket side that drops or delays chosen frames).
2. `net/tests/data/slirp-tcp-probe.pcap`: the committed capture, produced by the script, small (target under 200 KiB).
3. `scripts/net_pcap_check.py` extended with a **TCP checksum check** (pseudo-header, segment length from the IP length, not
   the frame length, as for UDP) and its `--self-test` extended to corrupt one byte of a TCP segment and require detection.
   Stdlib only; run with `python3 -I`.
4. `Makefile`: the `docs-check` line that checks the UDP capture also checks the TCP one with `--self-test`.
5. `S9-SPEC.md` section 3.2: each row confirmed, or corrected with the measured value; a one-line note of the QEMU version and
   date at the top of the table. Section 19 ("what was verified") updated to say the scripts and capture are now committed.

## 2. What to re-verify (every row of section 3.2)

Run each and record the measured value. Do not copy a number from the spec without running it. At minimum: the MSS slirp
announces and accepts (the spec says 1460, no window scaling, SACK or timestamps); the retransmission schedule slirp uses for
an unanswered segment (the spec says about 1.5, 3, 6 s); that through `hostfwd` the host's `connect` succeeds before the guest
has answered; slirp as a TCP endpoint itself (it terminates and re-originates the connection); how slirp reacts to a
guest RST, a FIN with data outstanding, and a zero window; whether IP fragments are dropped or reassembled for TCP-sized
datagrams; the largest segment slirp will deliver. If a row cannot be reproduced, say so and mark it "not reproduced" in the
spec rather than keeping the old value.

## 3. Out of scope

- Any Rust, the step 1 data-path prototype, packet-script runner (that is step 0's later half, with the harness flags; leave it
  for the S9 implementation PR), DNS and HTTP server flags for the QEMU boot tests.
- Changes to `NET-SPEC.md` (the S8 follow-up slice owns those). If you find a fact that belongs in `NET-SPEC.md` 3.2, put it
  in `S9-SPEC.md` and list it in the PR for the other slice.

## 4. Acceptance

- `make docs-check` passes and now covers the TCP capture; the self-test fails if the TCP check is disabled (show the
  mutation once).
- Running the script from a clean checkout on a machine with `qemu-system-riscv64` reproduces a capture that the checker
  accepts. Say plainly the QEMU version and platform it ran on.
- A short list in the PR of every 3.2 row that changed or could not be reproduced.
- No personal or machine-specific details in committed files (no home paths, usernames or addresses beyond loopback and
  the slirp 10.0.2.0/24 range).
- Ordinary review; `make check` green (docs and scripts only, so the kernel jobs will skip).
