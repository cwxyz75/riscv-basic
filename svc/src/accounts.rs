//! Accounts: limits and usage, nested (R-SPEC.md section 3, decided 2026-11-14).

use crate::Refuse;
use alloc::vec::Vec;

/// What an account may use. Each field is a ceiling; `u32::MAX` is "no limit of its own" (an ancestor
/// still applies).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Open connections.
    pub conns: u32,
    /// Bytes of replies held for slow readers.
    pub held_bytes: u32,
    /// Requests being served at once.
    pub inflight: u32,
}

/// The default for a program that does not ask for something else (R-SPEC.md section 4, question 1): 16
/// connections, 64 KiB held, 16 requests in flight. The launcher can override it per program; the parent's
/// limits are the outer cap.
pub const DEFAULT_LIMITS: Limits = Limits {
    conns: 16,
    held_bytes: 64 * 1024,
    inflight: 16,
};

/// The most accounts a server keeps at once (accounts cost memory too).
pub const MAX_ACCOUNTS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AccountId(pub u32);

/// The root account: the server's whole pool. Connections made before any account exists belong to it.
pub const ROOT: AccountId = AccountId(0);

#[derive(Debug)]
struct Acct {
    parent: Option<AccountId>,
    limits: Limits,
    conns: u32,
    held: u32,
    inflight: u32,
    /// Protocol violations by connections of this account (a statistic, for the server's policy).
    violations: u32,
    /// Live child accounts (an account with children, or connections, cannot be closed).
    children: u32,
    alive: bool,
}

/// The accounts of one server.
#[derive(Debug)]
pub struct Accounts {
    v: Vec<Acct>,
    free: Vec<u32>,
}

impl Accounts {
    /// A server whose total pool is `pool` (the root account's limits).
    pub fn new(pool: Limits) -> Accounts {
        Accounts {
            v: alloc::vec![Acct {
                parent: None,
                limits: pool,
                conns: 0,
                held: 0,
                inflight: 0,
                violations: 0,
                children: 0,
                alive: true,
            }],
            free: Vec::new(),
        }
    }

    fn rec(&self, id: AccountId) -> Result<&Acct, Refuse> {
        self.v
            .get(id.0 as usize)
            .filter(|a| a.alive)
            .ok_or(Refuse::NoSuchAccount)
    }

    /// Accounts alive, the root included.
    pub fn count(&self) -> usize {
        self.v.iter().filter(|a| a.alive).count()
    }

    /// A child account of `parent` with `limits`. Its limits may not exceed the parent's (a child can only
    /// be tighter: `Invalid` otherwise); the number of accounts is bounded (`Quota`).
    pub fn open(&mut self, parent: AccountId, limits: Limits) -> Result<AccountId, Refuse> {
        let p = self.rec(parent)?;
        if limits.conns > p.limits.conns
            || limits.held_bytes > p.limits.held_bytes
            || limits.inflight > p.limits.inflight
        {
            return Err(Refuse::Invalid);
        }
        if self.count() >= MAX_ACCOUNTS {
            return Err(Refuse::Quota);
        }
        let acct = Acct {
            parent: Some(parent),
            limits,
            conns: 0,
            held: 0,
            inflight: 0,
            violations: 0,
            children: 0,
            alive: true,
        };
        let id = match self.free.pop() {
            Some(i) => {
                self.v[i as usize] = acct;
                i
            }
            None => {
                self.v.try_reserve(1).map_err(|_| Refuse::Quota)?;
                self.v.push(acct);
                self.v.len() as u32 - 1
            }
        };
        self.v[parent.0 as usize].children += 1;
        Ok(AccountId(id))
    }

    /// Close an account with no connections and no children.
    pub fn close(&mut self, id: AccountId) -> Result<(), Refuse> {
        let a = self.rec(id)?;
        if id == ROOT || a.conns != 0 || a.children != 0 || a.held != 0 || a.inflight != 0 {
            return Err(Refuse::Invalid);
        }
        let parent = a.parent.expect("only the root has no parent");
        self.v[id.0 as usize].alive = false;
        self.v[parent.0 as usize].children -= 1;
        self.free.push(id.0);
        Ok(())
    }

    /// The account's limits.
    pub fn limits(&self, id: AccountId) -> Result<Limits, Refuse> {
        Ok(self.rec(id)?.limits)
    }

    /// Current usage `(conns, held bytes, in flight)` of the account, descendants included.
    pub fn usage(&self, id: AccountId) -> Result<(u32, u32, u32), Refuse> {
        let a = self.rec(id)?;
        Ok((a.conns, a.held, a.inflight))
    }

    /// Violations counted against the account itself.
    pub fn violations(&self, id: AccountId) -> Result<u32, Refuse> {
        Ok(self.rec(id)?.violations)
    }

    /// Count a protocol violation against the account.
    pub fn note_violation(&mut self, id: AccountId) {
        if let Some(a) = self.v.get_mut(id.0 as usize).filter(|a| a.alive) {
            a.violations = a.violations.saturating_add(1);
        }
    }

    /// Charge `n` of one resource to the account and every ancestor; all or nothing.
    fn charge(
        &mut self,
        id: AccountId,
        n: u32,
        pick: fn(&mut Acct) -> (&mut u32, u32),
    ) -> Result<(), Refuse> {
        self.rec(id)?;
        let mut cur = Some(id);
        while let Some(c) = cur {
            let a = &mut self.v[c.0 as usize];
            let parent = a.parent;
            let (used, limit) = pick(a);
            if used.checked_add(n).is_none_or(|u| u > limit) {
                return Err(Refuse::Quota);
            }
            cur = parent;
        }
        let mut cur = Some(id);
        while let Some(c) = cur {
            let a = &mut self.v[c.0 as usize];
            let parent = a.parent;
            *pick(a).0 += n;
            cur = parent;
        }
        Ok(())
    }

    fn credit(&mut self, id: AccountId, n: u32, pick: fn(&mut Acct) -> (&mut u32, u32)) {
        let mut cur = Some(id);
        while let Some(c) = cur {
            let Some(a) = self.v.get_mut(c.0 as usize) else {
                return;
            };
            let parent = a.parent;
            let used = pick(a).0;
            *used = used.saturating_sub(n);
            cur = parent;
        }
    }

    pub fn charge_conn(&mut self, id: AccountId) -> Result<(), Refuse> {
        self.charge(id, 1, |a| (&mut a.conns, a.limits.conns))
    }
    pub fn release_conn(&mut self, id: AccountId) {
        self.credit(id, 1, |a| (&mut a.conns, a.limits.conns));
    }
    pub fn charge_held(&mut self, id: AccountId, bytes: u32) -> Result<(), Refuse> {
        self.charge(id, bytes, |a| (&mut a.held, a.limits.held_bytes))
    }
    pub fn release_held(&mut self, id: AccountId, bytes: u32) {
        self.credit(id, bytes, |a| (&mut a.held, a.limits.held_bytes));
    }
    pub fn charge_inflight(&mut self, id: AccountId) -> Result<(), Refuse> {
        self.charge(id, 1, |a| (&mut a.inflight, a.limits.inflight))
    }
    pub fn release_inflight(&mut self, id: AccountId) {
        self.credit(id, 1, |a| (&mut a.inflight, a.limits.inflight));
    }
}
