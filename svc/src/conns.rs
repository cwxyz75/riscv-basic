//! The connection table and held replies: one backpressure policy for every service (R-SPEC.md section 2).
//!
//! A reply the client's queue cannot take is **held**: the connection is not read again until the hold
//! drains (the runtime polls on a timer, not by WRITABLE, T28); the held bytes are charged to the
//! connection's account; and a hold older than the timeout ends the connection.

use crate::{AccountId, Accounts, Refuse};
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

/// A reply waiting for room in the client's queue.
#[derive(Debug)]
pub struct Held {
    pub bytes: Vec<u8>,
    /// A handle travelling with the reply (the runtime closes it if the reply is dropped).
    pub handle: Option<u64>,
    /// When it was first held (the runtime's clock), for the timeout.
    pub since: u64,
}

struct Conn<C> {
    state: C,
    account: AccountId,
    held: Option<Held>,
}

/// The connections of one server, keyed by the number the port binding reports.
pub struct ConnTable<C> {
    map: BTreeMap<u64, Conn<C>>,
    next: u64,
}

impl<C> Default for ConnTable<C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C> ConnTable<C> {
    pub fn new() -> ConnTable<C> {
        ConnTable {
            map: BTreeMap::new(),
            next: 1,
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Add a connection owned by `account` (charged one connection, along the chain). The key is never
    /// reused while the server runs, so a stale port event cannot name a newer connection.
    pub fn insert(
        &mut self,
        accounts: &mut Accounts,
        account: AccountId,
        state: C,
    ) -> Result<u64, Refuse> {
        accounts.charge_conn(account)?;
        let key = self.next;
        self.next += 1;
        self.map.insert(
            key,
            Conn {
                state,
                account,
                held: None,
            },
        );
        Ok(key)
    }

    /// Remove a connection, releasing its connection charge and anything it was holding.
    pub fn remove(&mut self, accounts: &mut Accounts, key: u64) -> Option<C> {
        let c = self.map.remove(&key)?;
        if let Some(h) = &c.held {
            accounts.release_held(c.account, h.bytes.len() as u32);
        }
        accounts.release_conn(c.account);
        Some(c.state)
    }

    pub fn get(&self, key: u64) -> Option<&C> {
        self.map.get(&key).map(|c| &c.state)
    }

    pub fn get_mut(&mut self, key: u64) -> Option<&mut C> {
        self.map.get_mut(&key).map(|c| &mut c.state)
    }

    pub fn account(&self, key: u64) -> Option<AccountId> {
        self.map.get(&key).map(|c| c.account)
    }

    /// Is a reply being held for this connection (so it must not be read again)?
    pub fn is_held(&self, key: u64) -> bool {
        self.map.get(&key).is_some_and(|c| c.held.is_some())
    }

    /// Hold `bytes` for the connection, charged to its account. `Quota` if the account cannot take them
    /// (the runtime then closes the connection: a client that will not read and cannot be held for);
    /// `Invalid` if something is already held (a connection is not read while held, so this is a bug).
    pub fn hold(
        &mut self,
        accounts: &mut Accounts,
        key: u64,
        bytes: Vec<u8>,
        handle: Option<u64>,
        now: u64,
    ) -> Result<(), Refuse> {
        let c = self.map.get_mut(&key).ok_or(Refuse::NoSuchConn)?;
        if c.held.is_some() {
            return Err(Refuse::Invalid);
        }
        let n = u32::try_from(bytes.len()).map_err(|_| Refuse::Quota)?;
        accounts.charge_held(c.account, n)?;
        c.held = Some(Held {
            bytes,
            handle,
            since: now,
        });
        Ok(())
    }

    /// Take the held reply back (to retry sending it), releasing its charge. The runtime holds it again,
    /// with the original `since`, if the queue is still full (`rehold`).
    pub fn take_held(&mut self, accounts: &mut Accounts, key: u64) -> Option<Held> {
        let c = self.map.get_mut(&key)?;
        let h = c.held.take()?;
        accounts.release_held(c.account, h.bytes.len() as u32);
        Some(h)
    }

    /// Put a reply back that `take_held` returned and could not be sent, keeping its first `since`.
    pub fn rehold(&mut self, accounts: &mut Accounts, key: u64, held: Held) -> Result<(), Refuse> {
        let c = self.map.get_mut(&key).ok_or(Refuse::NoSuchConn)?;
        let n = held.bytes.len() as u32;
        accounts.charge_held(c.account, n)?;
        c.held = Some(held);
        Ok(())
    }

    /// Keys with a held reply, oldest first (the runtime polls these on its timer).
    pub fn held_keys(&self) -> Vec<u64> {
        let mut v: Vec<(u64, u64)> = self
            .map
            .iter()
            .filter_map(|(&k, c)| c.held.as_ref().map(|h| (h.since, k)))
            .collect();
        v.sort_unstable();
        v.into_iter().map(|(_, k)| k).collect()
    }

    /// Keys whose hold is older than `timeout` at `now`: these connections end.
    pub fn expired(&self, now: u64, timeout: u64) -> Vec<u64> {
        self.map
            .iter()
            .filter_map(|(&k, c)| {
                c.held
                    .as_ref()
                    .filter(|h| now.saturating_sub(h.since) >= timeout)
                    .map(|_| k)
            })
            .collect()
    }

    /// All keys.
    pub fn keys(&self) -> Vec<u64> {
        self.map.keys().copied().collect()
    }
}
