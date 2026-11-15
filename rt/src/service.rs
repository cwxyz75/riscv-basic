//! The service runtime (slice R, R-SPEC.md, OD84): what every server used to hand-roll. A **port**, a table
//! of **connections** (each a channel end bound to the port), **accounts** (who may use how much of the
//! server), the **one backpressure policy** (a reply the client has no room for is held, polled on a timer,
//! charged to the connection's account and ended after a timeout) and the **one violation policy** (a
//! malformed frame closes that connection and counts against its account; it never ends the service).
//!
//! The pure parts (accounts, the connection table, held replies) are the `svc` crate, tested on the host;
//! this module wraps them around the syscalls. A server owns a [`Core`] next to its own state and implements
//! [`Service`]; [`run`] is the loop.

use crate::abi::signals;
use crate::abi::status::Status;
use crate::sys;
extern crate alloc;

use alloc::collections::BTreeSet;
use alloc::vec::Vec;
use svc::{AccountId, Accounts, ConnTable, Limits};

/// How often (ticks) held replies are retried.
pub const HOLD_POLL_TICKS: u64 = 10;

/// What the loop calls on a server.
pub trait Service {
    /// Per-connection state (what the server needs to know about who it is talking to).
    type Conn;
    /// The runtime the server owns (the server keeps its `Core` next to its own state).
    fn core(&mut self) -> &mut Core<Self::Conn>;
    /// One request arrived on connection `key`, with the handles that came with it (the callee owns them:
    /// it must close or keep each). Replies go out through `core().send`.
    fn request(&mut self, key: u64, msg: &[u8], handles: &[u32]);
    /// A connection ended (its state is handed back so the server can release what it held).
    fn closed(&mut self, conn: Self::Conn);
    /// An event on something that is not a connection (a key the server bound itself on the port, for example
    /// a listener it watches). Keys the runtime hands out count up from 1; use keys with the top bit set.
    fn other_event(&mut self, _key: u64, _observed: u32) {}

    /// The latest time (`sys::clock_get` ticks) the loop may sleep to even if nothing arrives (a server with timers, like
    /// the network stack's ARP retries and its safety poll). `None`: sleep until an event.
    fn next_deadline(&mut self) -> Option<u64> {
        None
    }

    /// Called once per loop iteration, after the events of that wake-up were dispatched (and after a timeout): the place
    /// for work that is not driven by a connection.
    fn tick(&mut self) {}
}

struct Slot<C> {
    ch: u32,
    /// The kernel object id of `ch`, to spot a client that hands over both ends of one channel.
    koid: u64,
    state: C,
}

/// How a read from a connection ended.
pub enum Read {
    /// A message and the handles that came with it.
    Msg(Vec<u8>, Vec<u32>),
    /// Nothing to read now (or a reply is being held, so nothing is read).
    Nothing,
    /// The connection ended (the peer closed, or the message broke the protocol: it has been closed).
    Ended,
}

/// How a send ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Sent {
    Sent,
    /// The client had no room: the reply is held and the connection is not read again until it drains.
    Held,
    /// The connection was closed (a hopeless write, no memory, or the account could not hold the reply).
    Closed,
}

/// The runtime around one port.
pub struct Core<C> {
    port: u32,
    accounts: Accounts,
    conns: ConnTable<Slot<C>>,
    /// Kernel object ids of the channel ends we serve.
    own: BTreeSet<u64>,
    rx: Vec<u8>,
    max_handles: usize,
    hold_timeout: u64,
    /// Connections that ended since the server last looked (see `drain_closed`).
    closed: Vec<C>,
    /// Set by `exit_with`: the loop returns this once the current request is done.
    exit: Option<i32>,
}

impl<C> Core<C> {
    /// A runtime on `port`: `pool` is the server's total (the root account's limits), `max_msg` the largest
    /// request in bytes, `max_handles` the most handles one request may carry (a message with more is a
    /// protocol violation), `hold_timeout` how long (ticks) a reply may be held before its connection ends.
    pub fn new(
        port: u32,
        pool: Limits,
        max_msg: usize,
        max_handles: usize,
        hold_timeout: u64,
    ) -> Core<C> {
        Core {
            port,
            accounts: Accounts::new(pool),
            conns: ConnTable::new(),
            own: BTreeSet::new(),
            rx: alloc::vec![0u8; max_msg],
            max_handles,
            hold_timeout,
            closed: Vec::new(),
            exit: None,
        }
    }

    /// End the service after the current request (a server whose device stopped answering, say): `run`
    /// returns `code`.
    pub fn exit_with(&mut self, code: i32) {
        self.exit = Some(code);
    }

    pub fn accounts(&mut self) -> &mut Accounts {
        &mut self.accounts
    }

    /// Would a new connection be admitted to `account` now? (A server that must not create or change
    /// anything for an open that will be refused asks first.)
    pub fn would_admit(&self, account: AccountId) -> bool {
        self.accounts.would_admit_conn(account)
    }

    /// The port (a server may bind its own objects on it, with keys that have the top bit set).
    pub fn port(&self) -> u32 {
        self.port
    }

    pub fn has_conn(&self, key: u64) -> bool {
        self.conns.get(key).is_some()
    }

    pub fn is_empty(&self) -> bool {
        self.conns.is_empty()
    }

    pub fn len(&self) -> usize {
        self.conns.len()
    }

    pub fn conn(&self, key: u64) -> Option<&C> {
        self.conns.get(key).map(|s| &s.state)
    }

    pub fn conn_mut(&mut self, key: u64) -> Option<&mut C> {
        self.conns.get_mut(key).map(|s| &mut s.state)
    }

    /// The account a connection belongs to.
    pub fn account_of(&self, key: u64) -> Option<AccountId> {
        self.conns.account(key)
    }

    /// Connections that ended since the last call, to release what they held (`Service::closed`).
    pub fn drain_closed(&mut self) -> Vec<C> {
        core::mem::take(&mut self.closed)
    }

    /// Watch a connection for requests, or (`held`) stop watching it: a connection with a reply waiting is
    /// polled on a timer instead. Its WRITABLE signal cannot be trusted for this: a channel is "writable"
    /// while it has room for *some* message, not for a 32 KiB one, so a binding on it would be ready all
    /// the time and the loop would spin (T28).
    fn bind(&self, key: u64, ch: u32, held: bool) {
        sys::port_unbind(self.port, key);
        if !held {
            sys::port_bind(self.port, ch, signals::READABLE | signals::PEER_CLOSED, key);
        }
    }

    /// Start serving `ch` as a connection of `account`. `None` if it cannot be served (not a channel end we
    /// may read and write, both ends of one channel, the port cannot watch it, or the account is at its
    /// limit): nothing changes then, and the handle is still the caller's to close.
    pub fn add_conn(&mut self, ch: u32, account: AccountId, state: C) -> Option<u64> {
        // Only a channel end we may read and write can be served: probe with a zero-sized read (an event
        // or a VMO answers WRONG_TYPE; a channel says SHOULD_WAIT, PEER_CLOSED or BUFFER_TOO_SMALL and
        // loses nothing).
        let probe = sys::channel_read(ch, core::ptr::null_mut(), 0, core::ptr::null_mut(), 0);
        if matches!(
            probe.status(),
            Some(Status::WrongType) | Some(Status::AccessDenied) | Some(Status::BadHandle)
        ) {
            return None;
        }
        // Both ends of one channel must never be served at once: neither would ever see PEER_CLOSED, the
        // connections could not be reclaimed, and a reply sent on one end would be read back as a request
        // on the other (an endless exchange).
        let info = sys::object_info(ch);
        if info.status != 0 || (info.a2 != 0 && self.own.contains(&(info.a2 as u64))) {
            return None;
        }
        let koid = info.a1 as u64;
        let key = self
            .conns
            .insert(&mut self.accounts, account, Slot { ch, koid, state })
            .ok()?;
        if sys::port_bind(self.port, ch, signals::READABLE | signals::PEER_CLOSED, key).status != 0
        {
            // (the state is dropped with the slot; the caller never got a key)
            self.conns.remove(&mut self.accounts, key);
            return None;
        }
        self.own.insert(koid);
        Some(key)
    }

    /// End a connection: unwatch and close its channel, drop a held reply (and the handle it carried), and
    /// queue its state for `drain_closed`.
    pub fn close_conn(&mut self, key: u64) {
        if let Some(h) = self.conns.take_held(&mut self.accounts, key) {
            if let Some(handle) = h.handle {
                sys::handle_close(handle);
            }
        }
        let account = self.conns.account(key);
        if let Some(slot) = self.conns.remove(&mut self.accounts, key) {
            sys::port_unbind(self.port, key);
            sys::handle_close(slot.ch);
            self.own.remove(&slot.koid);
            self.closed.push(slot.state);
            if let Some(a) = account {
                self.reap_account(a);
            }
        }
    }

    /// Close an account that has no connections and no sub-accounts left, and its parents that become
    /// idle too (an account is only "the connections descended from one root"; with none left it is gone).
    fn reap_account(&mut self, mut a: AccountId) {
        while a != svc::ROOT {
            let parent = self.accounts.parent(a);
            if self.accounts.close(a).is_err() {
                break;
            }
            match parent {
                Some(p) => a = p,
                None => break,
            }
        }
    }

    /// A protocol violation: counted against the connection's account, the connection ends, the service
    /// does not.
    pub fn violation(&mut self, key: u64) {
        if let Some(a) = self.conns.account(key) {
            self.accounts.note_violation(a);
        }
        self.close_conn(key);
    }

    /// Send `bytes` (and `handle`, which the runtime now owns) on a connection. If the client has no room
    /// the reply is held and the connection is not read again until it drains; if the account cannot hold
    /// it, the connection ends.
    pub fn send(&mut self, key: u64, bytes: Vec<u8>, handle: Option<u32>) -> Sent {
        let close_handle = |h: Option<u32>| {
            if let Some(h) = h {
                sys::handle_close(h);
            }
        };
        let Some(slot) = self.conns.get(key) else {
            close_handle(handle);
            return Sent::Closed;
        };
        let ch = slot.ch;
        let hs = [handle.unwrap_or(0)];
        let n = usize::from(handle.is_some());
        let r = sys::channel_write(ch, bytes.as_ptr(), bytes.len(), hs.as_ptr(), n);
        match r.status() {
            Some(Status::Ok) => Sent::Sent,
            Some(Status::ShouldWait) | Some(Status::NoMemory) => {
                match self
                    .conns
                    .hold(&mut self.accounts, key, bytes, handle, sys::clock_get())
                {
                    Ok(()) => {
                        self.bind(key, ch, true);
                        Sent::Held
                    }
                    Err(_) => {
                        // too much is already waiting on clients that do not read: this one goes
                        close_handle(handle);
                        self.close_conn(key);
                        Sent::Closed
                    }
                }
            }
            _ => {
                // the client is gone or the write is hopeless
                close_handle(handle);
                self.close_conn(key);
                Sent::Closed
            }
        }
    }

    /// Send a datagram without holding it: if the client has no room (or no memory) the message is **dropped** and
    /// `Sent::Held` says so (nothing is kept). A server that must not wait for a slow client (the network stack's
    /// endpoints, NET-SPEC 7.3) uses this instead of `send`. A client that is gone ends the connection.
    pub fn send_datagram(&mut self, key: u64, bytes: &[u8]) -> Sent {
        let Some(slot) = self.conns.get(key) else {
            return Sent::Closed;
        };
        let r = sys::channel_write(slot.ch, bytes.as_ptr(), bytes.len(), core::ptr::null(), 0);
        match r.status() {
            Some(Status::Ok) => Sent::Sent,
            Some(Status::ShouldWait) | Some(Status::NoMemory) => Sent::Held,
            _ => {
                self.close_conn(key);
                Sent::Closed
            }
        }
    }

    /// Send one reply that carries several handles (a device claim). No hold: if the client has no room or is gone, the
    /// handles are closed and the connection ends (a claim reply is only ever the answer to a request just read).
    pub fn send_handles(&mut self, key: u64, bytes: Vec<u8>, handles: Vec<u32>) -> Sent {
        let Some(slot) = self.conns.get(key) else {
            for h in handles {
                sys::handle_close(h);
            }
            return Sent::Closed;
        };
        let r = sys::channel_write(
            slot.ch,
            bytes.as_ptr(),
            bytes.len(),
            handles.as_ptr(),
            handles.len(),
        );
        if r.status() == Some(Status::Ok) {
            return Sent::Sent;
        }
        for h in handles {
            sys::handle_close(h);
        }
        self.close_conn(key);
        Sent::Closed
    }

    fn retry_held(&mut self, key: u64) {
        let Some(slot) = self.conns.get(key) else {
            return;
        };
        let ch = slot.ch;
        let Some(h) = self.conns.take_held(&mut self.accounts, key) else {
            return;
        };
        let hs = [h.handle.unwrap_or(0)];
        let n = usize::from(h.handle.is_some());
        let r = sys::channel_write(ch, h.bytes.as_ptr(), h.bytes.len(), hs.as_ptr(), n);
        match r.status() {
            Some(Status::Ok) => self.bind(key, ch, false),
            Some(Status::ShouldWait) | Some(Status::NoMemory)
                if sys::clock_get().saturating_sub(h.since) < self.hold_timeout =>
            {
                // still full: keep it, with its first `since`
                if self.conns.rehold(&mut self.accounts, key, h).is_err() {
                    self.close_conn(key);
                }
            }
            _ => {
                // too late, or the client is gone
                if let Some(handle) = h.handle {
                    sys::handle_close(handle);
                }
                self.close_conn(key);
            }
        }
    }

    /// Retry every held reply (oldest first); the ones held too long end their connections.
    pub fn poll_held(&mut self) {
        for k in self.conns.held_keys() {
            self.retry_held(k);
        }
    }

    /// Is any reply being held (so the loop must wake on a timer)?
    pub fn any_held(&self) -> bool {
        !self.conns.held_keys().is_empty()
    }

    /// Read one message from a connection. A message that breaks the protocol ends the connection (a
    /// violation); a held connection is not read.
    pub fn read(&mut self, key: u64) -> Read {
        let Some(slot) = self.conns.get(key) else {
            return Read::Ended;
        };
        if self.conns.is_held(key) {
            return Read::Nothing;
        }
        let ch = slot.ch;
        let mut hs = [0u32; svc_max_handles()];
        let r = sys::channel_read(
            ch,
            self.rx.as_mut_ptr(),
            self.rx.len(),
            hs.as_mut_ptr(),
            self.max_handles.min(hs.len()),
        );
        match r.status() {
            Some(Status::Ok) => {}
            Some(Status::ShouldWait) => return Read::Nothing,
            // PEER_CLOSED ends the connection quietly; BUFFER_TOO_SMALL (a message with more handles than
            // any request may carry, or too many bytes) is a protocol violation, as is any other error
            Some(Status::PeerClosed) => {
                self.close_conn(key);
                return Read::Ended;
            }
            _ => {
                self.violation(key);
                return Read::Ended;
            }
        }
        let (nb, nh) = (r.a1, r.a2);
        let handles: Vec<u32> = hs[..nh].to_vec();
        let mut msg = Vec::new();
        if msg.try_reserve_exact(nb).is_err() {
            for &h in &handles {
                sys::handle_close(h);
            }
            self.close_conn(key);
            return Read::Ended;
        }
        msg.extend_from_slice(&self.rx[..nb]);
        Read::Msg(msg, handles)
    }
}

/// The most handles one request may carry through the runtime (a message channel's own maximum).
const fn svc_max_handles() -> usize {
    crate::abi::limits::MAX_MSG_HANDLES
}

/// A copy that reports running out of memory instead of aborting the server.
pub fn try_copy(src: &[u8]) -> Option<Vec<u8>> {
    let mut v = Vec::new();
    v.try_reserve_exact(src.len()).ok()?;
    v.extend_from_slice(src);
    Some(v)
}

/// Requests handled per wake-up of one connection (the port rotates, so none starves).
pub const BATCH: usize = 4;

/// The loop: wait on the port, read and dispatch requests, retry held replies, hand back ended connections.
/// Returns 0 when every connection is gone (the root one included), 8 if the port itself fails.
pub fn run<S: Service>(service: &mut S) -> i32 {
    loop {
        let held = if service.core().any_held() {
            sys::clock_get() + HOLD_POLL_TICKS
        } else {
            u64::MAX
        };
        let deadline = held.min(service.next_deadline().unwrap_or(u64::MAX));
        let port = service.core().port;
        let r = sys::port_wait(port, deadline);
        match r.status() {
            Some(Status::Ok) => {
                let (key, observed) = (r.a1 as u64, r.a2 as u32);
                if !service.core().has_conn(key) {
                    service.other_event(key, observed);
                } else if observed & signals::READABLE != 0 {
                    for _ in 0..BATCH {
                        match service.core().read(key) {
                            Read::Msg(msg, handles) => service.request(key, &msg, &handles),
                            Read::Nothing | Read::Ended => break,
                        }
                        if service.core().exit.is_some() {
                            break;
                        }
                    }
                } else if observed & signals::PEER_CLOSED != 0 {
                    service.core().close_conn(key);
                }
            }
            Some(Status::TimedOut) => {}
            _ => return 8,
        }
        service.tick();
        service.core().poll_held();
        for c in service.core().drain_closed() {
            service.closed(c);
        }
        if let Some(code) = service.core().exit {
            return code;
        }
        if service.core().is_empty() {
            return 0;
        }
    }
}
