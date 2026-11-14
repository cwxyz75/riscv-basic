//! The service directory's pure logic (slice SD, SD-SPEC.md): names, the listeners behind them, stale
//! entries, and **views**. A view is a capability-style attenuation: the set of names a connection to the
//! directory may see. Hidden names are indistinguishable from names that do not exist, a view can only be
//! narrowed, and handing a program a view is the grant.
//!
//! Generic over the listener `L` (the runtime stores a channel handle; the model tests store a number).

use crate::Refuse;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

/// The longest service name, and the most names a directory keeps.
pub const MAX_NAME: usize = 64;
pub const MAX_NAMES: usize = 256;

/// Is `name` something that can be published? Non-empty, short, printable ASCII without `/` or space
/// (names are path components under `/svc`, matched exactly).
pub fn valid_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name.iter().all(|&b| (0x21..0x7f).contains(&b) && b != b'/')
}

/// Why a lookup did not produce a listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup {
    /// No such name (or not in the view: the same answer).
    NotFound,
    /// The name is published but its service is gone (its listener closed); try again later, the service
    /// may be restarting. The directory queues nothing.
    Unavailable,
}

/// The names a connection may use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    /// `None`: every name (the directory's own full connection).
    names: Option<BTreeSet<Vec<u8>>>,
}

impl View {
    pub fn all() -> View {
        View { names: None }
    }

    pub fn allows(&self, name: &[u8]) -> bool {
        self.names.as_ref().is_none_or(|n| n.contains(name))
    }

    /// A narrower view with exactly `names`; every one must already be allowed (`Invalid` otherwise: a view
    /// cannot grant what its parent could not use) and valid.
    pub fn restrict(&self, names: &[&[u8]]) -> Result<View, Refuse> {
        let mut set = BTreeSet::new();
        for &n in names {
            if !valid_name(n) || !self.allows(n) {
                return Err(Refuse::Invalid);
            }
            set.insert(n.to_vec());
        }
        Ok(View { names: Some(set) })
    }

    /// Is this the unrestricted view?
    pub fn is_all(&self) -> bool {
        self.names.is_none()
    }
}

struct Entry<L> {
    listener: L,
    stale: bool,
}

/// The published names.
pub struct Directory<L> {
    entries: BTreeMap<Vec<u8>, Entry<L>>,
}

impl<L> Default for Directory<L> {
    fn default() -> Self {
        Self::new()
    }
}

impl<L> Directory<L> {
    pub fn new() -> Directory<L> {
        Directory {
            entries: BTreeMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Publish `name`. A name whose service is alive is `Invalid` to take (no hijacking; unpublish first);
    /// a *stale* name is replaced, and the old listener returned for the runtime to close. `Quota` past
    /// `MAX_NAMES`.
    pub fn publish(&mut self, name: &[u8], listener: L) -> Result<Option<L>, Refuse> {
        if !valid_name(name) {
            return Err(Refuse::Invalid);
        }
        match self.entries.get_mut(name) {
            Some(e) if !e.stale => Err(Refuse::Invalid),
            Some(e) => {
                let old = core::mem::replace(&mut e.listener, listener);
                e.stale = false;
                Ok(Some(old))
            }
            None => {
                if self.entries.len() >= MAX_NAMES {
                    return Err(Refuse::Quota);
                }
                self.entries.insert(
                    name.to_vec(),
                    Entry {
                        listener,
                        stale: false,
                    },
                );
                Ok(None)
            }
        }
    }

    /// Remove a name, returning its listener to close.
    pub fn unpublish(&mut self, name: &[u8]) -> Option<L> {
        self.entries.remove(name).map(|e| e.listener)
    }

    /// The service behind `name` is gone: later lookups say `Unavailable` until it is republished.
    pub fn mark_stale(&mut self, name: &[u8]) {
        if let Some(e) = self.entries.get_mut(name) {
            e.stale = true;
        }
    }

    /// The listener to forward a connection to, for a connection with `view`.
    pub fn lookup(&self, view: &View, name: &[u8]) -> Result<&L, Lookup> {
        if !view.allows(name) {
            return Err(Lookup::NotFound);
        }
        match self.entries.get(name) {
            None => Err(Lookup::NotFound),
            Some(e) if e.stale => Err(Lookup::Unavailable),
            Some(e) => Ok(&e.listener),
        }
    }

    /// The names a connection with `view` can see, in order (stale ones included: they exist).
    pub fn list(&self, view: &View) -> Vec<Vec<u8>> {
        self.entries
            .keys()
            .filter(|n| view.allows(n))
            .cloned()
            .collect()
    }

    /// Every listener (to watch them for closing): `(name, listener)`.
    pub fn listeners(&self) -> impl Iterator<Item = (&[u8], &L)> {
        self.entries
            .iter()
            .map(|(n, e)| (n.as_slice(), &e.listener))
    }
}
