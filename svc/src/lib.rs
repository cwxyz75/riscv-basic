//! The pure core of the service runtime (slice R, R-SPEC.md): **accounts** (who may use how much of a
//! server), the **connection table** and **held replies** (the one backpressure policy). `no_std` +
//! `alloc`, `forbid(unsafe_code)`, no kernel access: the runtime in `rt` wraps it around a port and the
//! syscalls, and everything here is tested on the host.
//!
//! Servers cannot tell clients apart (INVARIANTS H5), so every server limit used to be a pool one client
//! could drain. An **account** is only "the set of connections descended from one root connection": it has
//! no identity, just limits (connections, held bytes, requests in flight). Accounts nest like jobs do: a
//! child's limits can only be tighter than its parent's, and everything the child uses is also charged to
//! the parent, so the root account is the server's total pool. A charge is all or nothing along the chain.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod accounts;
pub mod conns;
pub mod directory;

pub use accounts::{AccountId, Accounts, Limits, DEFAULT_LIMITS, ROOT};
pub use conns::{ConnTable, Held};
pub use directory::{Directory, Lookup, View};

/// Why the runtime refused something. Never a client's fault to retry blindly: it is a policy answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refuse {
    /// The account (or an ancestor) is at one of its limits.
    Quota,
    /// No such account (it was closed, or never existed).
    NoSuchAccount,
    /// No such connection.
    NoSuchConn,
    /// The request itself is malformed for the operation (for example a child limit above its parent's).
    Invalid,
}
