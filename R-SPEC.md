# R: the service runtime (specification draft, v0)

Status: **draft v0, written unattended on 2026-11-14 for discussion; nothing here is built or decided.** Decisions
OD77 (a shared service runtime, accounted connections) fixes the direction; this file proposes the shape and
lists what needs the owner. Basis: design review findings D7, D8, D11, D12 (`reviews/2026-11-14-design-review.md`)
and a reading of the five server loops (`memfsd`, `consoled`, `launcher`, `devmgr`, `blkd`).

## 1. What the five loops have in common (read from the code)

* One **port**; each served channel is bound with `READABLE | PEER_CLOSED` and a key (`memfsd::Server::bind`).
* A **connection table** keyed by that key: the channel handle, per-connection state (`memfsd::Conn`: the node
  and rights it serves), and a cap on the number of connections (`MAX_CONNECTIONS` 200).
* A **held reply** when the client's queue is full (`memfsd::Pending`, `PENDING_TIMEOUT`, `PENDING_CAP`): no further
  requests are read from that connection until it drains, and the channel's WRITABLE signal cannot be trusted
  for this (T28), so a timer polls instead.
* **Protocol violations** end the connection (or, for `launcher`, the whole service: T30).
* A refusal to serve a handle that is not a channel end we can read and write, or both ends of one channel
  (`memfsd::add_conn` probes with a zero-sized read).
* A `rt::startup` read of the roles, then `port_wait` with a deadline, and exit when the last client is gone.

They differ in: backpressure (poll, timed wait, drop-oldest in `consoled`, exit in others), the violation policy,
and what a "connection" is (a node in `memfsd`, a tty session in `consoled`, the one client in `launcher`).

## 2. Proposal

`rt::service` (a module of `rt`, `no_std` + `alloc`, `forbid(unsafe_code)` apart from the syscalls it wraps):

```text
trait Service {
    type Conn;                                  // per-connection state
    fn open(&mut self, ctx: &mut Ctx, first: &Handles) -> Result<Self::Conn, Refuse>;
    fn request(&mut self, ctx: &mut Ctx, conn: &mut Self::Conn, req: Request<'_>) -> Action;
    fn closed(&mut self, ctx: &mut Ctx, conn: Self::Conn);
}
enum Action { Reply(Reply), Hold(Reply), Close, Violation }   // one backpressure and violation policy
struct Server<S: Service> { port, table, accounts, limits, ... }
impl Server { fn run(&mut self, deadline policy) -> ExitReason }
```

* **One backpressure policy** (settles T28): a reply the peer's queue cannot take is *held*; the connection is not
  read again until it drains (polled on a timer, not by WRITABLE); held bytes are counted against the account
  (section 3); a hold older than a timeout ends the connection. `consoled`'s drop-oldest output stays a service-
  specific choice (it is a stream, not a request/reply).
* **One violation policy**: a malformed frame, a wrong handle count or an unexpected op closes that connection
  and counts against its account; it never ends the service (retires T30's launcher-exits-on-violation).
* **Request/reply is real**: the typed client side (`rt::service::Client`) does `channel_call` with a transaction
  id, so the call path the kernel can optimise (OD73's hand-off) has callers.
* **Held-handle and endpoint hygiene** (`add_conn`'s probe, both-ends-of-one-channel) lives in the library once.
* **Supervision is not in this slice**: restart strategies come with the registry (slice SD).

## 3. Accounted connections (OD77, finding D11)

Servers cannot tell clients apart (INVARIANTS H5), so every server limit is a pool one client can drain. The
mechanism: a **root connection** is minted for a program by whoever starts it (the shell or launcher) with an
`Account` request (`account = open_account(parent_conn, limits)`); every connection opened or cloned from a
connection inherits its **account**; limits (connection count, held bytes, requests in flight) apply per account,
with a smaller total pool shared by all accounts. No identities exist: an account is only "the set of
connections descended from this root".

Proposed wire change (file protocol, S4-SPEC section 7): a new op `Account(limits)` on a directory connection
(`op 8`) answered with a fresh root connection to the same node; `Open`/`Clone` from an accounted connection are
charged to its account. Rights are unchanged. A server that does not support accounts answers `NOT_SUPPORTED`.

## 4. Open questions for the owner

1. **Account limits and defaults**: what are the per-account connection and held-byte limits, and who sets them
   (the launcher per program, a fixed default, the parent's job limits)?
2. **Do accounts nest** (an account's holder minting sub-accounts for its own children), as jobs do?
3. **Library or macro**: a `Service` trait with callbacks (above), or a plain event loop the service drives itself
   (closer to today's code, less to learn, less enforced)? I propose the trait.
4. **Multiple threads per service**: one loop per port, or a thread pool sharing a port? That needs the wake-one rule
   (W-SPEC PW, not yet built) to avoid duplicate wakes; until then one thread per port.
5. **Order of porting**: `memfsd` first (the most complex, the proof), then `consoled`, `launcher`, `devmgr`, `blkd`.

## 5. Acceptance (to write before code)

`memfsd` passes all its existing tests unchanged on the library; a client that opens connections without end
exhausts only its own account (a new QEMU test with two clients); a slow client stalls only itself and is cut off
after the hold timeout, everywhere the same; the same violation behaviour for every ported server (table-driven
test); leak checks exact; a request/reply round-trip benchmark on `channel_call` (the W step-0 ping-pong's
cousin); host tests of the library's pure parts (held-reply queue, account arithmetic, the connection table).

## 6. Steps

0. This spec, once the questions are answered. 1. The library's pure core (connection table, accounts, hold queue)
with host tests. 2. The runtime around it (port loop, held replies, the typed client). 3. `memfsd` on the library;
accounts in the file protocol. 4. The other servers. 5. Documents; T28 and T30 resolved or narrowed.
