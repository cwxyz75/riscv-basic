# SD: the service directory and namespaces (specification, v1, 2026-11-14)

Status: specified for roadmap slice SD (OD78 direction, OD97 this document). Step 1, the pure directory logic
(`svc::directory`: names, listeners, stale entries, views), is built and model-tested; the registry service, the startup
message change and the migration of the servers are not. Basis: design review D10 (`reviews/2026-11-14-design-review.md`).
The owner's decisions (OD78): a **registry service** holds the directory, not `init`; a service publishes a name by
handing the registry a **listener channel**; a client opens `/svc/<name>` with the file protocol's open-with-endpoint,
which the registry forwards to the listener; a dead service's entry goes stale and the client sees `UNAVAILABLE` and
retries (no queueing in the registry); a program is handed a **view** with only the names it may use (that is the grant);
the startup message's closed `Role` enum gives way to generic named entries. **Deliberately undecided** (with Q10, later):
passing a service connection to another program, and revocation.

## 1. Why

Today every service is wired by hand: `init` builds the startup messages, the closed `Role` enum names each handle
(`ServiceFs`, `ServiceLaunch`, `ServiceBlk`...), and the shell hardcodes who gets what. A new service (the network stack
of S8) would need a new enum value and new plumbing in every program that uses it, and a service that restarts (T15)
cannot be re-found by its clients (T30). A directory makes finding a service a lookup, makes a restart visible
(UNAVAILABLE, then success), and gives the grant a shape: the view.

## 2. The model

* **The registry** is a service on the service runtime (`rt::service`, OD84). It serves the file protocol as a
  *directory* (`/svc`): `Readdir` lists the names in a connection's view, `Open(name)+endpoint` connects to a service.
  It owns the `svc::Directory`.
* **A name** is a path component under `/svc`: 1 to 64 printable ASCII bytes, no `/` and no space (`valid_name`). The
  directory holds at most 256.
* **Publishing.** A directory connection with the new right `PUBLISH` may send `op 16 Publish(name; listener endpoint)`:
  the service hands the registry the *registry's end* of a fresh channel it listens on. A name whose service is alive
  cannot be taken (no hijacking: unpublish first); a **stale** name is replaced. `op 17 Unpublish(name)` removes one.
  `PUBLISH` is a right like any other: it only shrinks, and whoever starts a service gives it a connection that has it
  (the shell or `init`), not the services' clients.
* **Connecting.** `Open(name, flags, rights)+endpoint` on a connection whose **view** allows `name`: the registry
  forwards the request, with the client's endpoint, to the listener as an `Open` message with an empty path; the service
  serves that endpoint on its own port as a new connection (speaking whatever protocol it speaks: blk, launch, file...).
  The registry never sees the conversation. If the forward fails because the listener is closed, the entry becomes
  **stale** and the client gets `UNAVAILABLE` (new protocol status `-109`); the registry queues nothing, the client
  retries. A name that was never published, or that the view does not allow, is `NOT_FOUND`: a hidden name is
  indistinguishable from a missing one (it does not even answer UNAVAILABLE).
* **Views** are the capability attenuation (`svc::View`): a directory connection has a set of names it may use, or all.
  `op 18 View(names...)` is a `Clone` whose new connection allows exactly the listed names, each of which must already be
  allowed (a view cannot grant what its parent could not use: `BAD_REQUEST`). Views only narrow. **Handing a program a
  view is the grant**: the shell gives each program the view for the services that program may use.
* **Budgets** (OD77, OD84): every connection to the registry, listeners included, is charged to its budget; the
  accounts mechanism of slice R bounds a hostile publisher (names, connections) and a hostile client (connections,
  requests in flight) to what the connection it was given allows.
* **Restart** is visible, not hidden: a restarting service republishes (the stale entry is replaced), and `rt::service`'s
  client side retries on UNAVAILABLE with a bounded back-off. Supervision itself (who restarts what) is a later slice.

## 3. The startup message

The closed `Role` enum (`proto::startup`) names what a kernel-recognised program needs (`Stdin`, `Stdout`, `Stderr`,
`BootImage`, `Serial`, `Tty`, `Job`, `Resource`, the `Blk*` driver authority...). Service connections move out of it:

* The `namespace` list (today only `"."` and `"/"`, at most 4) accepts **arbitrary path prefixes** (validated: absolute,
  components of printable ASCII, no `..`), each naming a directory connection. `/svc` is one of them; `/` and `.` keep
  their meaning.
* `ServiceFs`, `ServiceLaunch`, `ServiceConsole`, `ServiceBlk`, `ServiceDevmgr`, `ServiceInit` stay in the enum for one
  release (so `init` and the existing programs need not change in one step) and are then retired for **clients**; a
  service's *listening* end is still handed in by the process that starts it, as a role.
* A program with no `/svc` entry has no services. There is no ambient directory.

## 4. Invariants (each gets a check)

* **SD1** A view only narrows: for any views `v` and `w = v.restrict(..)`, `w`'s names are a subset of `v`'s
  (`random_walk_against_the_reference`).
* **SD2** A name outside a view is `NOT_FOUND` whatever its state (the same test, and
  `a_hidden_name_is_indistinguishable_from_a_missing_one`).
* **SD3** A live name cannot be taken; a stale one can; every replaced or removed listener is returned to be closed
  (no handle leaks: the QEMU leak checks).
* **SD4** The registry holds no reference to a client's endpoint after forwarding it (the client's and the service's
  connection survive the registry's death; the registry holds only listeners).
* **SD5** Every name and connection is charged to a budget; a hostile publisher or client cannot exceed its own.

## 5. Acceptance (written first)

Publish, open, unpublish; a client sees UNAVAILABLE while a service is down and succeeds after it republishes; a
program whose view lacks a name cannot open it and cannot tell it exists; arbitrary namespace prefixes in the startup
message; the registry tolerates a hostile publisher (many names, a hijack attempt, a listener that never reads) and a
hostile client (floods, malformed frames), with the budgets of OD84; leak checks exact; the existing programs unchanged
(every current test passes).

## 6. Steps

0. This document and OD97. 1. (built) The pure directory logic with model tests. 2. (built) The registry service `registryd` on
`rt::service`: the `PUBLISH` right (bit 16) and ops 16 to 18 in `proto::file` (ops 9 to 15 are the file operations), status `UNAVAILABLE`, a QEMU test with a toy
service. 3. Arbitrary namespace prefixes in the startup message (`proto`, `rt`, the launcher). 4. The shell grants a
`/svc` view; `memfsd`, `launcher`, `devmgr`, `blkd` publish themselves; clients (`rt::fs`, the shell) reconnect on
UNAVAILABLE. 5. Documents; T15 and T30 narrowed.

## 7. Risks and open points

* Forwarding an `Open` with an endpoint to a listener that is slow or full: the registry must not block on one service
  (a held-reply style bound per listener, with the stale mark on overflow). Settled in step 2 with a hostile-listener test.
* The registry is a single point of finding things (not of conversation): if it dies the connections already made live on.
  Its own restart is `init`'s job in this slice.
* Naming collisions between services started by different principals are the owner's policy to decide when there is more
  than one (who may publish which name); today only the shell and `init` hold `PUBLISH`.
