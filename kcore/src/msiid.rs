//! Interrupt identities and their routes (IOMMU-SPEC 8.1, invariant IO12; IOMMU4).
//!
//! Every message a hart takes from its IMSIC interrupt file carries an **identity** the kernel allocated, and every allocated identity
//! has one **route**: the inter-hart interrupt, a source the kernel owns, a wired APLIC source behind an interrupt object, or an MSI
//! vector of a device domain. Identities are allocated **per hart** (each hart's file has its own identity space, `riscv,num-ids` of
//! them): identity 0 does not exist and identity 1 is the inter-hart interrupt on every hart, so 2 to `num_ids` are given out.
//!
//! The life of an identity:
//!
//! * `alloc` (or `alloc_any`, or the target half of `move_to`) makes it **Opening**: taken, not yet open in its hart's file. Only that
//!   hart opens it (its own CSR write); `opened` records that it did, and the route is usable from then on (**Live**).
//! * A route that moves (`move_to`) keeps its old identity **Live** until the new one is opened: both route to it meanwhile. `opened`
//!   then makes the old one **Retiring**.
//! * A route that ends (`end`) makes its identity **Retiring** at once.
//! * A Retiring identity is closed by its hart, which reads and clears its pending bit (delivering a moved route's event if it was
//!   set: the message was sent before the move) and then calls `retired`: only then is the identity **Free**. So an identity is never
//!   given to another route while a message for its old owner can still arrive (IO10's half that is about the hart, IO12).
//!
//! Allocation is next-fit, not lowest-fit: reuse is delayed and the whole identity space is exercised. Everything is sized at
//! construction (from the hart count and `num_ids`): nothing allocates afterwards, so the interrupt and teardown paths that use it
//! never allocate (T177). Pure: no hardware, no locks (the kernel keeps it under `OBJ`, IOMMU-SPEC 11.1).

use alloc::vec::Vec;

/// The inter-hart interrupt's identity on every hart.
pub const IPI: u16 = 1;
/// The most identities an IMSIC interrupt file has (the AIA specification: 63 to 2047).
pub const MAX_IDS: u32 = 2047;

/// Where a message goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Route {
    /// The inter-hart interrupt (identity 1 of every hart; never allocated, moved or ended).
    Ipi,
    /// A wired APLIC source the kernel owns (never one a process may take).
    Kernel(u32),
    /// A wired APLIC source behind an interrupt object.
    Wired(u32),
    /// An MSI vector (IOMMU-SPEC 8.3): the number is the kernel's name for the vector object.
    Vector(u32),
}

/// An identity on a hart: `hart` is the index of the hart's interrupt file (its place in the IMSIC's list), `id` the identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Place {
    pub hart: u16,
    pub id: u16,
}

/// The state of one identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    /// Not taken (identity 0 stays here and is never given).
    Free,
    /// Taken for `route`, not yet opened by its hart. `from`: the identity the route is moving away from, if it is a move.
    Opening { route: Route, from: Option<Place> },
    /// Open and routed. `to`: an Opening identity the route is moving to.
    Live { route: Route, to: Option<Place> },
    /// No longer routed: its hart must close it, read and clear its pending bit, and then report `retired`. `moved`: the route went on
    /// elsewhere (a pending event is still the route's); otherwise the route ended (a pending event belongs to nobody).
    Retiring { route: Route, moved: bool },
}

/// What `evacuate` did, one call per identity it changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Evacuated {
    /// An Opening identity on the departing hart was given up (it is Retiring now, `moved` false; its route stays where it was).
    Cancelled { route: Route, at: Place },
    /// A route was moved: `to` is Opening on another hart, `from` stays Live until `to` is opened.
    Moved {
        route: Route,
        from: Place,
        to: Place,
    },
}

/// What `opened` did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Opened {
    pub route: Route,
    /// The identity the route moved away from, now Retiring: its hart must retire it.
    pub retiring: Option<Place>,
}

/// The identities of every hart's interrupt file and their routes.
#[derive(Clone, Debug)]
pub struct MsiIds {
    harts: u16,
    num_ids: u16,
    /// `harts * (num_ids + 1)` slots, hart-major.
    slots: Vec<Slot>,
    /// Per hart: where the next-fit search starts.
    cursor: Vec<u16>,
    /// Per hart: Free identities (2 to `num_ids`).
    free: Vec<u16>,
    /// Per hart: Opening and Live identities other than the inter-hart one (what placement balances).
    routes: Vec<u16>,
}

impl MsiIds {
    /// The identities of `harts` interrupt files of `num_ids` identities each (1 to `num_ids`; 1 is the inter-hart interrupt).
    /// `None` for no harts, more harts than a hart index can name, or `num_ids` outside 2 to `MAX_IDS`.
    pub fn new(harts: usize, num_ids: u32) -> Option<MsiIds> {
        let h = u16::try_from(harts).ok().filter(|&h| h > 0)?;
        if !(2..=MAX_IDS).contains(&num_ids) {
            return None;
        }
        let n = u16::try_from(num_ids).ok()?;
        let per = usize::from(n).checked_add(1)?;
        let mut slots = alloc::vec![Slot::Free; per.checked_mul(harts)?];
        for hart in 0..harts {
            let i = hart.checked_mul(per)?.checked_add(usize::from(IPI))?;
            *slots.get_mut(i)? = Slot::Live {
                route: Route::Ipi,
                to: None,
            };
        }
        Some(MsiIds {
            harts: h,
            num_ids: n,
            slots,
            cursor: alloc::vec![2; harts],
            free: alloc::vec![n.checked_sub(1)?; harts],
            routes: alloc::vec![0; harts],
        })
    }

    /// How many harts.
    pub fn harts(&self) -> u16 {
        self.harts
    }

    /// The identities of each hart's file: 1 to this.
    pub fn num_ids(&self) -> u16 {
        self.num_ids
    }

    fn index(&self, p: Place) -> Option<usize> {
        if p.hart >= self.harts || p.id > self.num_ids {
            return None;
        }
        usize::from(p.hart)
            .checked_mul(usize::from(self.num_ids).checked_add(1)?)?
            .checked_add(usize::from(p.id))
    }

    /// The state of identity `p` (`None`: no such identity).
    pub fn slot(&self, p: Place) -> Option<Slot> {
        self.slots.get(self.index(p)?).copied()
    }

    fn set(&mut self, p: Place, s: Slot) {
        if let Some(slot) = self.index(p).and_then(|i| self.slots.get_mut(i)) {
            *slot = s;
        }
    }

    /// Free identities on `hart` (0 for no such hart).
    pub fn free_on(&self, hart: u16) -> u16 {
        self.free.get(usize::from(hart)).copied().unwrap_or(0)
    }

    /// Free identities on every hart together.
    pub fn free_total(&self) -> u64 {
        self.free.iter().map(|&f| u64::from(f)).sum()
    }

    /// Opening and Live routes on `hart` other than the inter-hart interrupt.
    pub fn routes_on(&self, hart: u16) -> u16 {
        self.routes.get(usize::from(hart)).copied().unwrap_or(0)
    }

    fn count(v: &mut [u16], hart: u16, up: bool) {
        if let Some(c) = v.get_mut(usize::from(hart)) {
            *c = if up {
                c.saturating_add(1)
            } else {
                c.saturating_sub(1)
            };
        }
    }

    /// Take a Free identity on `hart` for `route` (Opening, not a move). Next-fit from the hart's cursor. `None`: the hart has none
    /// free, there is no such hart, or `route` is the inter-hart interrupt (never allocated).
    pub fn alloc(&mut self, hart: u16, route: Route) -> Option<u16> {
        self.take(hart, route, None)
    }

    fn take(&mut self, hart: u16, route: Route, from: Option<Place>) -> Option<u16> {
        if route == Route::Ipi || self.free_on(hart) == 0 {
            return None;
        }
        // the identities 2 ..= num_ids, from the cursor round
        let span = self.num_ids.checked_sub(1)?;
        let start = *self.cursor.get(usize::from(hart))?;
        for k in 0..span {
            let id = 2u16.checked_add(start.checked_sub(2)?.checked_add(k)?.checked_rem(span)?)?;
            let p = Place { hart, id };
            if self.slot(p) == Some(Slot::Free) {
                self.set(p, Slot::Opening { route, from });
                Self::count(&mut self.free, hart, false);
                Self::count(&mut self.routes, hart, true);
                let next = if id >= self.num_ids {
                    2
                } else {
                    id.checked_add(1)?
                };
                *self.cursor.get_mut(usize::from(hart))? = next;
                return Some(id);
            }
        }
        None
    }

    /// The hart `alloc_any` would choose: among the harts `usable` accepts that have a Free identity, the one with the fewest routes
    /// (the lowest index on a tie).
    pub fn best_hart(&self, usable: impl Fn(u16) -> bool) -> Option<u16> {
        (0..self.harts)
            .filter(|&h| usable(h) && self.free_on(h) > 0)
            .min_by_key(|&h| (self.routes_on(h), h))
    }

    /// Take an identity for `route` on the best usable hart (the default placement, IOMMU-SPEC 8.5). `None`: no usable hart has one.
    pub fn alloc_any(&mut self, route: Route, usable: impl Fn(u16) -> bool) -> Option<Place> {
        let hart = self.best_hart(usable)?;
        let id = self.alloc(hart, route)?;
        Some(Place { hart, id })
    }

    /// Move the route of Live identity `from` to a new identity on `hart` (Opening there; `from` stays Live, and both route to it until
    /// the new one is `opened`). `None`, with nothing changed: `from` is not Live, is the inter-hart interrupt, is already moving, `hart`
    /// is its own hart, or `hart` has no Free identity.
    pub fn move_to(&mut self, from: Place, hart: u16) -> Option<u16> {
        let Some(Slot::Live { route, to: None }) = self.slot(from) else {
            return None;
        };
        if route == Route::Ipi || hart == from.hart {
            return None;
        }
        // (the negative control `fault-msiid-free-at-move`: the old identity is freed at the move instead of at its retirement, so a
        // message still on its way to it reaches nobody, or another route: `msiid_model` and `interrupt_moves_do_not_lose_or_leak` must see it)
        if cfg!(feature = "fault-msiid-free-at-move") {
            let id = self.take(hart, route, None)?;
            self.set(from, Slot::Free);
            Self::count(&mut self.free, from.hart, true);
            Self::count(&mut self.routes, from.hart, false);
            return Some(id);
        }
        let id = self.take(hart, route, Some(from))?;
        self.set(
            from,
            Slot::Live {
                route,
                to: Some(Place { hart, id }),
            },
        );
        Some(id)
    }

    /// Hart `p.hart` opened identity `p` in its file: it is Live. If the route was moving, the identity it left is Retiring now (its hart
    /// must retire it: `retiring`). `None`, with nothing changed: `p` was not Opening.
    pub fn opened(&mut self, p: Place) -> Option<Opened> {
        let Some(Slot::Opening { route, from }) = self.slot(p) else {
            return None;
        };
        self.set(p, Slot::Live { route, to: None });
        let retiring = from.filter(|&f| self.slot(f) == Some(Slot::Live { route, to: Some(p) }));
        if let Some(f) = retiring {
            self.set(f, Slot::Retiring { route, moved: true });
            Self::count(&mut self.routes, f.hart, false);
        }
        Some(Opened { route, retiring })
    }

    /// The route of identity `p` ends (an object destroyed, a source given up): `p` is Retiring. A move under way ends with it: the
    /// Opening identity it was moving to is Retiring too, and is returned (its hart must retire it as well). Ending an Opening identity
    /// cancels that move only: the identity it was leaving stays Live. `Err`, with nothing changed: `p` is Free, Retiring or the
    /// inter-hart interrupt.
    #[allow(clippy::result_unit_err)]
    pub fn end(&mut self, p: Place) -> Result<Option<Place>, ()> {
        match self.slot(p) {
            Some(Slot::Opening { route, from }) => {
                if let Some(f) = from {
                    if self.slot(f) == Some(Slot::Live { route, to: Some(p) }) {
                        self.set(f, Slot::Live { route, to: None });
                    }
                }
                self.set(
                    p,
                    Slot::Retiring {
                        route,
                        moved: false,
                    },
                );
                Self::count(&mut self.routes, p.hart, false);
                Ok(None)
            }
            Some(Slot::Live { route, to }) if route != Route::Ipi => {
                self.set(
                    p,
                    Slot::Retiring {
                        route,
                        moved: false,
                    },
                );
                Self::count(&mut self.routes, p.hart, false);
                let cancelled = to.filter(|&t| {
                    self.slot(t)
                        == Some(Slot::Opening {
                            route,
                            from: Some(p),
                        })
                });
                if let Some(t) = cancelled {
                    self.set(
                        t,
                        Slot::Retiring {
                            route,
                            moved: false,
                        },
                    );
                    Self::count(&mut self.routes, t.hart, false);
                }
                Ok(cancelled)
            }
            _ => Err(()),
        }
    }

    /// Hart `p.hart` closed Retiring identity `p` and cleared its pending bit: it is Free. Returns the route it had and whether that
    /// route had moved on. `None`, with nothing changed: `p` was not Retiring.
    pub fn retired(&mut self, p: Place) -> Option<(Route, bool)> {
        let Some(Slot::Retiring { route, moved }) = self.slot(p) else {
            return None;
        };
        self.set(p, Slot::Free);
        Self::count(&mut self.free, p.hart, true);
        Some((route, moved))
    }

    /// Hart `from` goes offline (IOMMU-SPEC 8.5): every route on it moves to the best hart `usable` accepts (never `from`), and every
    /// move into it is cancelled (a cancelled move whose route sits on a hart `usable` refuses is moved on as well). All or nothing:
    /// `Err` with nothing changed when the other harts have too few Free identities. Each change is reported to `changed`; the Opening
    /// identities it made must be opened by their harts, the Retiring ones retired by theirs.
    #[allow(clippy::result_unit_err)]
    pub fn evacuate(
        &mut self,
        from: u16,
        usable: impl Fn(u16) -> bool,
        mut changed: impl FnMut(Evacuated),
    ) -> Result<(), ()> {
        if from >= self.harts {
            return Err(());
        }
        let ok = |h: u16| h != from && usable(h);
        let mut need: u64 = 0;
        for id in 2..=self.num_ids {
            match self.slot(Place { hart: from, id }) {
                Some(Slot::Live { to: None, .. }) => need = need.saturating_add(1),
                Some(Slot::Opening { from: Some(f), .. }) if !ok(f.hart) => {
                    need = need.saturating_add(1)
                }
                _ => {}
            }
        }
        let supply: u64 = (0..self.harts)
            .filter(|&h| ok(h))
            .map(|h| u64::from(self.free_on(h)))
            .sum();
        if need > supply {
            return Err(());
        }
        for id in 2..=self.num_ids {
            let at = Place { hart: from, id };
            match self.slot(at) {
                Some(Slot::Opening {
                    route,
                    from: origin,
                }) => {
                    let _ = self.end(at);
                    changed(Evacuated::Cancelled { route, at });
                    if let Some(o) = origin.filter(|o| !ok(o.hart)) {
                        self.move_away(o, route, &ok, &mut changed)?;
                    }
                }
                Some(Slot::Live { route, to: None }) if route != Route::Ipi => {
                    self.move_away(at, route, &ok, &mut changed)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn move_away(
        &mut self,
        at: Place,
        route: Route,
        ok: &impl Fn(u16) -> bool,
        changed: &mut impl FnMut(Evacuated),
    ) -> Result<(), ()> {
        let hart = self.best_hart(ok).ok_or(())?;
        let id = self.move_to(at, hart).ok_or(())?;
        changed(Evacuated::Moved {
            route,
            from: at,
            to: Place { hart, id },
        });
        Ok(())
    }

    /// Every identity of `hart` (2 to `num_ids`) whose hart has work to do: Opening (open it) or Retiring (close and retire it).
    pub fn work_on(&self, hart: u16) -> impl Iterator<Item = (u16, Slot)> + '_ {
        (2..=self.num_ids).filter_map(move |id| match self.slot(Place { hart, id }) {
            Some(s @ (Slot::Opening { .. } | Slot::Retiring { .. })) => Some((id, s)),
            _ => None,
        })
    }
}

/// A set of identities of one hart (up to `MAX_IDS`): what one claim of the interrupt file took, before the routes are looked up under
/// the lock. Fixed size, no allocation (it lives on the trap path's stack).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IdSet([u64; 32]);

impl IdSet {
    pub const fn new() -> IdSet {
        IdSet([0; 32])
    }

    /// Add `id` (an identity above `MAX_IDS` is not one: ignored).
    pub fn insert(&mut self, id: u32) {
        if let Some(w) = self.0.get_mut((id / 64) as usize) {
            *w |= 1u64 << (id % 64);
        }
    }

    pub fn contains(&self, id: u32) -> bool {
        self.0
            .get((id / 64) as usize)
            .is_some_and(|w| w & (1u64 << (id % 64)) != 0)
    }

    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|&w| w == 0)
    }

    /// The identities, ascending.
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.0.iter().enumerate().flat_map(|(k, &w)| {
            (0..64u32).filter_map(move |b| {
                if w & (1u64 << b) != 0 {
                    u32::try_from(k).ok()?.checked_mul(64)?.checked_add(b)
                } else {
                    None
                }
            })
        })
    }
}
