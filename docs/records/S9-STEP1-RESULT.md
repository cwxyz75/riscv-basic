# S9 step 1: the data-path prototype, result (2026-11-15)

Gate for `S9-SPEC.md` section 4 (option 1: the client creates the socket pair, the stack keeps S and R). Eight suite tests
(`init/src/stream_proto.rs`, run by `make os-boot-test`; the child is `crash.rs` behaviour 70, a "socket hand" that obeys
one-letter commands). They were run at `-smp 1` and `-smp 4` (the gate runs one boot at four harts); the numbers below are QEMU TCG on a laptop, so they say
how the pieces compare, not how fast a machine is. No kernel change was needed or made.

**Result: section 4's recommendation stands. Nothing in P1 to P8 argues for option 3, `socket_info`, a settable buffer size or a ring.** A Fable review (`reviews/2026-11-15-s9-step1.md`) found the first draft of this result stronger than its evidence in several places; this version corrects them, and the tests were tightened (P1 exact, P5 with bytes queued both ways, P7 asserts, P4 with a deadline, P8 added).

| Check | What was shown |
|---|---|
| **P1** `stream_buffers_are_charged_to_the_client` | With 64 KiB queued each way in a pair, **the job that created the pair is charged exactly 131,072 bytes and the other job 0**, in both variants (client creates; server creates), and the charge stays with the creator after the end changed hands. After the receiving process exits the bytes queued toward it are freed. OD74 holds as written. |
| **P2** `a_client_at_quota_stalls_only_itself` | A client whose job has 40 KiB of IPC left: the server's write takes the quota and then fails with **NO_MEMORY while the end is still WRITABLE** (so a level-triggered wait would spin: the one-shot-plus-timer rule is needed, as 4.4 says). With the 20 ms doubling to 500 ms rule: **8 attempts in 2 s**. A second client's pair fills to its full 64 KiB unaffected; when the first client reads, the server's next write succeeds. |
| **P3** `one_socket_two_one_shot_bindings` | The kernel accepts **two one-shot bindings on one socket end in one port** (READABLE and WRITABLE, different keys). Each fires once, they re-arm independently, nothing fires while disarmed. The fallback was not needed. |
| **P4** `stream_pipe_numbers` (no threshold) | Per `socket_read`/`socket_write` call about **65 µs** (1-byte calls: 4,096 in about 265 ms). **Caveat: the socket hand writes and drains in 1 KiB pieces, so the larger rows measure the hand's call rate as much as the pair** (about 66 ms of the 83 ms for 1 MiB at 16 KiB chunks is the hand's 1,024 calls), and "never a wake per byte" is true by construction. What the rows do show: the cost of one call, about 5 MB/s at 512-byte chunks, 12 MB/s upper bound for 16 KiB chunks, and that a reader waiting for a writer that writes in pieces wakes at most once per piece (16 to 21 wakes per MiB at `-smp 4`, one per 64 KiB at `-smp 1`). A real measurement needs a hand that writes whole buffers (step 4, with the real server). |
| **P5** `a_dead_client_frees_everything` | Killing the client with 64 KiB queued toward the server and 32 KiB toward itself: the server's end sees PEER_CLOSED; the 64 KiB is still delivered, then end-of-stream; the client's job ledgers (IPC bytes and objects) return to 0 once the server also lets go of its end and the process handle is closed. (The client is killed while idle, not inside a call; a kill inside `socket_write` is covered by the kernel's own socket tests, not here.) |
| **P6** `a_bad_stream_handle_is_refused` | The checks of 8.3, written as a function and tried against the real kernel: a channel (wrong type), a socket without READ, without WRITE, without WAIT, the same socket twice, and the peer of a socket already held are each refused with the right reason, and a fresh good socket is admitted after all of them. The function moves into `netd` in step 4. |
| **P7** `stream_budgets_with_the_launcher_defaults` | A job with the launcher's defaults (256 objects, 192 KiB IPC) holds **127 empty socket pairs** before the object quota stops it, and **3 pairs with 64 KiB queued in one direction** before the IPC quota does. A stream is more than that: **4 objects** (channel pair and socket pair), and a pair can hold **128 KiB** (P1: 64 KiB each way), so the worst case is **about 63 streams and one fully stalled stream** under the launcher defaults, about 127 streams and 8 fully stalled ones under the proposed 1 MiB / 512 objects, and **16 fully stalled streams exhaust the 2 MiB system budget** (computed, not exhausted: the suite's servers share it). |
| **P8** `bystander_cost_of_sleeping_bindings` | A thread sleeping in `port_wait` on 64, 256 and 768 one-shot bindings while two other threads ping-pong 120 round trips: **24, 24, 25 and 30 ticks at `-smp 1`** (0, 64, 256, 768 bindings), 29, 30, 30, 35 at `-smp 4`; the sleeper caused no (at most one) spurious wakes. T60's cost is real but small at these sizes: about 25 % on that benchmark at 768 bindings. Task 21 does not need to move ahead of step 4 for 8 to 16 live streams; at the section 12 limits (256 connections, up to 3 bindings each) it is worth re-measuring in step 8. |

What this changes in the proposal:

* **6.4 (the client's quotas):** the launcher default of 192 KiB IPC is one fully stalled stream (three one-direction-full
  pairs) and about 63 streams by object count, as the spec feared; the proposed 1 MiB IPC and 512 objects for a program with a
  network view give about 127 streams and 8 fully stalled ones. The 2 MiB system budget is reachable by 16 fully stalled
  streams, so T79 (the budget assumes a small machine) matters as soon as S9 runs; this is open point 3 and the owner's decision.
* **A finding the review made while checking P2 (not tested):** a channel message is charged to the job that created the
  receiving *endpoint*, all or nothing (`kcore/src/ipc.rs`). A client at its IPC quota therefore cannot receive `PeerFin`,
  `Reset` or `Accepted` on its own endpoint channel, so rule 8.5 (end-of-file is clean only after `PeerFin`) needs a rule for
  that case before step 4: the stack must hold the notice and retry like it does for data, and the client library must treat
  an endpoint channel that stays silent while the socket is at end-of-stream as a reset after a bound.
* **P6 does not cover** a client that keeps a duplicate of the end it passed (it can read what the stack should read, or
  `socket_shutdown` it); 8.3 has no rule for that and none is needed for safety (the client only hurts itself), but the stack must
  tolerate BAD_STATE / PEER_CLOSED from its own end at any time.
* **7 and 4.4:** one socket end may carry two one-shot bindings, so the per-stream binding count in 7.2 can stay at three or
  fewer.
* **Not measured:** the server-creates variant's throughput (only its accounting, P1); wake counts with a real second
  thread in `netd` (this needs step 4); the effect of tasks 11 and 12. The `##PROTO` lines are printed for people and read by
  no harness; the `##BENCH stream_pipe_*` benchmarks of 14.3 come with step 8.
