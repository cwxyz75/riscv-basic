# S9 step 1: the data-path prototype, result (2026-11-15)

Gate for `S9-SPEC.md` section 4 (option 1: the client creates the socket pair, the stack keeps S and R). Seven suite tests
(`init/src/stream_proto.rs`, run by `make os-boot-test`; the child is `crash.rs` behaviour 70, a "socket hand" that obeys
one-letter commands). They were run at `-smp 1` and `-smp 4` (the gate runs one boot at four harts); the numbers below are QEMU TCG on a laptop, so they say
how the pieces compare, not how fast a machine is. No kernel change was needed or made.

**Result: section 4's recommendation stands. Nothing in P1 to P7 argues for option 3, `socket_info`, a settable buffer size or a ring.**

| Check | What was shown |
|---|---|
| **P1** `stream_buffers_are_charged_to_the_client` | With 64 KiB queued each way in a pair, **the job that created the pair is charged exactly 131,072 bytes and the other job 0**, in both variants (client creates; server creates), and the charge stays with the creator after the end changed hands. After the receiving process exits the bytes queued toward it are freed. OD74 holds as written. |
| **P2** `a_client_at_quota_stalls_only_itself` | A client whose job has 40 KiB of IPC left: the server's write takes the quota and then fails with **NO_MEMORY while the end is still WRITABLE** (so a level-triggered wait would spin: the one-shot-plus-timer rule is needed, as 4.4 says). With the 20 ms doubling to 500 ms rule: **8 attempts in 2 s**. A second client's pair fills to its full 64 KiB unaffected; when the first client reads, the server's next write succeeds. |
| **P3** `one_socket_two_one_shot_bindings` | The kernel accepts **two one-shot bindings on one socket end in one port** (READABLE and WRITABLE, different keys). Each fires once, they re-arm independently, nothing fires while disarmed. The fallback was not needed. |
| **P4** `stream_pipe_numbers` (no threshold) | Per `socket_read`/`socket_write` call about **65 µs** (1-byte calls: 4,096 in about 265 ms). 512-byte chunks: 256 KiB in about 53 ms (about 5 MB/s). 16 KiB chunks: **1 MiB in 83 ms read (about 12 MB/s) and 84 ms write**. The reader woke once per 64 KiB round when the writer was a whole write ahead (`-smp 1`, `-smp 2`), and 16 to 21 wakes per MiB when it raced a writer that wrote in 1 KiB pieces (`-smp 4`): at most one wake per piece the writer made, never per byte. Rates are bounded by the syscall cost, not by the pair. |
| **P5** `a_dead_client_frees_everything` | Killing the client mid-transfer: the server's end sees PEER_CLOSED; the 64 KiB the client had queued is still delivered, then end-of-stream; the client's job ledgers (IPC bytes and objects) return to 0. |
| **P6** `a_bad_stream_handle_is_refused` | The checks of 8.3, written as a function and tried against the real kernel: a channel (wrong type), a socket without READ, without WRITE, without WAIT, the same socket twice, and the peer of a socket already held are each refused with the right reason, and a fresh good socket is admitted after all of them. The function moves into `netd` in step 4. |
| **P7** `stream_budgets_with_the_launcher_defaults` | A job with the launcher's defaults (256 objects, 192 KiB IPC) holds **127 empty socket pairs** before the object quota stops it, and **3 streams with a full 64 KiB queued** before the IPC quota does. The system-wide budget (2 MiB) is 32 such streams; it was computed, not exhausted, because the suite's servers share it. |

What this changes in the proposal:

* **6.4 (the client's quotas):** the launcher default of 192 KiB IPC is three stalled streams, as the spec feared; the proposed
  1 MiB IPC and 512 objects for a program with a network view stand (16 stalled streams, about 250 streams). The system budget
  of 2 MiB is reachable by 32 stalled streams, so T79 (the budget assumes a small machine) matters as soon as S9 runs; this is
  open point 3 and it is the owner's decision.
* **7 and 4.4:** one socket end may carry two one-shot bindings, so the per-stream binding count in 7.2 can stay at three or
  fewer.
* **Not measured:** the server-creates variant's throughput (only its accounting, P1); wake counts with a real second
  thread in `netd` (this needs step 4); the effect of tasks 11 and 12.
