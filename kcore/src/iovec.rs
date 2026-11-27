//! Vector routes (IOMMU7; IOMMU-SPEC 8.2 to 8.5, 6.2, 10.6; invariants IO9, IO10, IO11, D5): the kernel's half of a granted MSI vector,
//! under the object lock (`OBJ`) with the interrupt identities (`kcore::msiid`) and the interrupt objects (`kcore::ipc`).
//!
//! A granted vector is: a valid MRIF entry of its domain's MSI page table and an interrupt file of its own in kernel memory (the
//! store's half, `kcore::iodomain`, under `IOMMU`); an identity on a hart, named as the entry's notice target; and an interrupt object
//! of the kind vector. This module keeps the route (which identities the vector has, which one the entry names, which object it
//! signals) and runs the protocol of 8.5 on the file. Every step here is one critical section; the caller (`ksys::iodomain`, the
//! kernel's interrupt path, the host models) waits for the fences in between with no lock held.
//!
//! * **Grant**: `grant_begin` (a route and an identity on the online hart with the fewest routes: opened here if that is the calling
//!   hart, else asked of its hart), `grant_ready` (the identity is open), `grant_write` (the entry, under `IOMMU`: `IoDomains::
//!   vector_grant`), the fence, `fenced`, then `attach` (the object, and the file armed).
//! * **Notice** (`notice`, the interrupt path, no other lock): the object is fired at most once per pending bit of the file's identity 1.
//! * **Acknowledge** (`ack`): the file's stray bits taken and counted (8.4), then re-armed; a bit that came meanwhile fires again.
//! * **Move** (`move_here` or a hart's evacuation, then `retarget_begin`, the fence, `fenced`): the old identity stays routed to the
//!   vector (`Slot::Moved`) until the fence after the entry's rewrite, and only then is it retired (IO10).
//! * **Revoke** (`revoke_begin` at the object's last handle, the fence, `revoke_end`) and **destruction** (`domain_gone` after the
//!   destruction's fence): the identities end only after the fence, are retired by their harts, and the route's charge is given back
//!   when the last one is free (`retired`, `take_spent`).
//!
//! The kernel touches a vector's file only while its route is Live (granted, not revoked, its domain not destroyed), and only with
//! atomic operations: the store frees the file after the fence that follows the route's end.

use crate::iodomain::{DomainId, IoDomains, IoError, Ticket, VECTOR_DATA};
use crate::ipc::{KObj, Kobjs, VectorRef};
use crate::job::JobId;
use crate::msiid::{MsiIds, Place, Route, Slot};
use crate::object::ObjId;
use alloc::vec::Vec;
use iommucore::msi::Mrif;
use iommucore::{PhysMem64, Regs, TableFrames};

/// The harts' interrupt files, as the vector steps see them (the kernel's `aia`, or a host model).
pub trait Harts {
    /// The calling hart's interrupt file (`None`: the IMSIC does not list it).
    fn here(&self) -> Option<u16>;
    /// May routes be placed on file `f` (its hart started and online)?
    fn usable(&self, f: u16) -> bool;
    /// Is file `f`'s hart going offline, or offline (no route is moved onto it)?
    fn leaving(&self, f: u16) -> bool;
    /// Open identity `id` of the calling hart's file (a stale pending bit is cleared first).
    fn open_here(&mut self, id: u16);
    /// Ask the hart of file `f` to do its work in its file (open Opening identities, close and retire Retiring ones).
    fn ask(&mut self, f: u16);
    /// The physical address of file `f`'s page: a notice's target (8.2).
    fn file_page(&self, f: u16) -> u64;
}

/// The state of a route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RState {
    Free,
    /// Granted, the entry not yet fenced (no object yet).
    Granting,
    /// Granted and fenced: its object fires.
    Live,
    /// Its entry is invalid and the fence (the route's ticket) is outstanding.
    Revoking,
    /// Revoked or its domain destroyed, after the fence: its identities are retiring.
    Dead,
    /// Every identity is free again: the charge is to be given back (`take_spent`).
    Spent,
    /// A fence of the vector was abandoned (IO8): its identities are never retired, its charge stays.
    Leaked,
}

#[derive(Debug, Clone, Copy)]
struct VecRoute {
    gen: u32,
    state: RState,
    obj: Option<ObjId>,
    domain: u64,
    index: u16,
    file: u64,
    job: JobId,
    /// The identity the entry names once its fence is done.
    named: Option<Place>,
    /// The identity an entry write in flight names (a grant's or a move's).
    target: Option<Place>,
    /// A grant's or a move's entry write is in flight: one at a time.
    busy: bool,
    /// The first `object_wait` or `interrupt_ack` placed it (8.5).
    placed: bool,
}

const FREE: VecRoute = VecRoute {
    gen: 0,
    state: RState::Free,
    obj: None,
    domain: 0,
    index: 0,
    file: 0,
    job: JobId::ROOT,
    named: None,
    target: None,
    busy: false,
    placed: true,
};

/// A step that cannot go on: try again later (`Busy`: another move is in flight; `NotReady`: an identity is not open yet).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveError {
    /// The object is dead (revoked, or its domain destroyed).
    Dead,
    /// The calling hart has no free identity.
    NoMemory,
    /// Another move of the vector is in flight, or an identity it moves to is not open yet: wait and try again.
    Busy,
}

/// What a move step did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Moved {
    /// Nothing to do: the vector is where it was asked to be (or the calling hart is leaving).
    Stays,
    /// A new identity is open here: rewrite the entry (`retarget_begin`), fence, `fenced`.
    Retarget,
}

/// The vector routes (`Route::Vector(n)` is slot `n`), sized at boot.
pub struct Vectors {
    routes: Vec<VecRoute>,
    cursor: usize,
    /// Stray bits taken from files at acknowledgement (8.4; the store counts the ones it finds at revoke and release).
    strays: u64,
    /// Routes not free (granting, live, revoking, dead, spent or leaked).
    used: u64,
    leaked: u64,
    /// Live vectors not yet placed by a first wait or acknowledgement (the kernel looks at waits only while this is not zero).
    unplaced: u64,
    spent: u64,
    /// A fence completed naming an identity its route had already moved away from (a hart went offline during a grant or a move):
    /// the caller runs the settle of the moves (`take_resettle`).
    resettle: bool,
}

/// The file protocol of a notice (8.5) on an armed vector: whether the object fires. Atomic operations only, no lock.
pub fn file_notice<M: PhysMem64>(mem: &M, file: u64) -> bool {
    let Ok(f) = Mrif::new(file) else {
        return false;
    };
    let _ = f.disable(mem, VECTOR_DATA);
    if f.take_pending(mem, VECTOR_DATA).unwrap_or(false) {
        return true;
    }
    // another path took the bit: armed again, and a bit that came in between is taken now
    let _ = f.enable(mem, VECTOR_DATA);
    if f.take_pending(mem, VECTOR_DATA).unwrap_or(false) {
        let _ = f.disable(mem, VECTOR_DATA);
        return true;
    }
    false
}

/// Arm a vector's file (8.5, an acknowledgement or the grant's end): set the enable bit, then take the pending bit; if it was set,
/// clear the enable bit again: the object fires at once from software.
pub fn file_arm<M: PhysMem64>(mem: &M, file: u64) -> bool {
    let Ok(f) = Mrif::new(file) else {
        return false;
    };
    let _ = f.enable(mem, VECTOR_DATA);
    if f.take_pending(mem, VECTOR_DATA).unwrap_or(false) {
        let _ = f.disable(mem, VECTOR_DATA);
        return true;
    }
    false
}

/// The acknowledgement's half of D5 for a vector: `file_arm`. The negative-control builds break it (IOMMU9): `fault-vector-no-unmask`
/// leaves the identity disabled (nothing is delivered after the first), `fault-vector-no-repend` enables it but drops the message
/// that came while it was masked.
fn ack_arm<M: PhysMem64>(mem: &M, file: u64) -> bool {
    if cfg!(feature = "fault-vector-no-unmask") {
        return false;
    }
    if cfg!(feature = "fault-vector-no-repend") {
        if let Ok(f) = Mrif::new(file) {
            let _ = f.enable(mem, VECTOR_DATA);
        }
        return false;
    }
    file_arm(mem, file)
}

/// Take and count the stray bits of a file (8.4): pending bits of identities other than the granted one.
pub fn file_strays<M: PhysMem64>(mem: &M, file: u64) -> u64 {
    Mrif::new(file)
        .ok()
        .and_then(|f| f.stray(mem, VECTOR_DATA).ok())
        .map_or(0, |w| w.iter().map(|x| u64::from(x.count_ones())).sum())
}

fn fire(objs: &mut Kobjs, obj: ObjId) -> bool {
    match objs.get_mut(obj) {
        Some(KObj::Interrupt(i)) if i.vector.is_some() => {
            i.claims = i.claims.saturating_add(1);
            let first = !i.fired;
            i.fired = true;
            first
        }
        _ => false,
    }
}

impl Vectors {
    /// `n` routes (the kernel: twice the identities of every file, so that routes whose charge is not given back yet never starve a
    /// grant). `None` without memory.
    pub fn new(n: usize) -> Option<Vectors> {
        let mut routes = Vec::new();
        routes.try_reserve_exact(n).ok()?;
        routes.resize(n, FREE);
        Some(Vectors {
            routes,
            cursor: 0,
            strays: 0,
            used: 0,
            leaked: 0,
            unplaced: 0,
            spent: 0,
            resettle: false,
        })
    }

    fn at(&self, v: VectorRef) -> Option<&VecRoute> {
        self.routes
            .get(usize::try_from(v.n).ok()?)
            .filter(|r| r.gen == v.gen && r.state != RState::Free)
    }

    fn at_mut(&mut self, v: VectorRef) -> Option<&mut VecRoute> {
        self.routes
            .get_mut(usize::try_from(v.n).ok()?)
            .filter(|r| r.gen == v.gen && r.state != RState::Free)
    }

    fn by_n(&self, n: u32) -> Option<VectorRef> {
        let r = self.routes.get(usize::try_from(n).ok()?)?;
        (r.state != RState::Free).then_some(VectorRef { n, gen: r.gen })
    }

    /// The state of route `v` (`None`: free, or a stale generation).
    pub fn state(&self, v: VectorRef) -> Option<RState> {
        self.at(v).map(|r| r.state)
    }

    /// The domain and index of route `v`.
    pub fn vector_of(&self, v: VectorRef) -> Option<(DomainId, u16)> {
        self.at(v).map(|r| (DomainId::from_raw(r.domain), r.index))
    }

    /// The identity the entry of route `v` names (fenced), and the one the route is on now (Opening while a move is not yet open).
    pub fn named(&self, v: VectorRef) -> Option<Place> {
        self.at(v)?.named
    }

    /// The identity route `v` is on now, and whether it is open (false: a move or a grant whose hart has not opened it yet).
    pub fn current(&self, ids: &MsiIds, v: VectorRef) -> Option<(Place, bool)> {
        self.at(v)?;
        current(ids, v.n)
    }

    /// Routes in use, live vectors, leaked ones, stray bits counted at acknowledgement, vectors not yet placed, routes spent.
    pub fn counts(&self) -> VecCounts {
        let live = self
            .routes
            .iter()
            .filter(|r| {
                matches!(
                    r.state,
                    RState::Granting | RState::Live | RState::Revoking | RState::Dead
                )
            })
            .count() as u64;
        let dead = self
            .routes
            .iter()
            .filter(|r| r.state == RState::Dead)
            .count() as u64;
        VecCounts {
            used: self.used,
            live,
            dead,
            leaked: self.leaked,
            strays: self.strays,
            unplaced: self.unplaced,
            spent: self.spent,
        }
    }

    /// The vectors not yet placed by a first wait or acknowledgement (one load for the wait path).
    pub fn unplaced(&self) -> u64 {
        self.unplaced
    }

    // ---- grant ------------------------------------------------------------------------------------------------------------

    /// 8.3, the first step (under `OBJ`, the caller charged the job: `charge_vector`): a route for vector `index` of `domain`, and an
    /// identity for it on the usable hart with the fewest routes, opened at once if that is the calling hart, else asked of its hart.
    /// `NoMemory` (nothing changed): no free route, or no usable hart has a free identity.
    pub fn grant_begin(
        &mut self,
        ids: &mut MsiIds,
        h: &mut dyn Harts,
        domain: DomainId,
        index: u16,
        job: JobId,
    ) -> Result<VectorRef, IoError> {
        let len = self.routes.len();
        let n = (0..len)
            .filter_map(|k| self.cursor.wrapping_add(k).checked_rem(len))
            .find(|&k| self.routes.get(k).is_some_and(|r| r.state == RState::Free))
            .ok_or(IoError::NoMemory)?;
        let nn = u32::try_from(n).map_err(|_| IoError::NoMemory)?;
        let p = ids
            .alloc_any(Route::Vector(nn), |f| h.usable(f))
            .ok_or(IoError::NoMemory)?;
        if Some(p.hart) == h.here() {
            h.open_here(p.id);
            let _ = ids.opened(p);
        } else {
            h.ask(p.hart);
        }
        self.cursor = n.wrapping_add(1);
        let r = self.routes.get_mut(n).ok_or(IoError::NoMemory)?;
        *r = VecRoute {
            gen: r.gen,
            state: RState::Granting,
            domain: domain.raw(),
            index,
            job,
            busy: true,
            placed: true,
            ..FREE
        };
        self.used = self.used.saturating_add(1);
        Ok(VectorRef { n: nn, gen: r.gen })
    }

    /// 8.3: the hart a grant chose did not open its identity within the bound. The route takes an identity on the calling hart instead,
    /// opened at once; the one it leaves is retired by its hart when that hart answers. `NoMemory`: the calling hart has no free
    /// identity (the route is unchanged); `BadState`: the route is not granting.
    pub fn grant_here(
        &mut self,
        ids: &mut MsiIds,
        h: &mut dyn Harts,
        v: VectorRef,
    ) -> Result<(), IoError> {
        if self.at(v).map(|r| r.state) != Some(RState::Granting) {
            return Err(IoError::BadState);
        }
        let route = Route::Vector(v.n);
        let here = h.here().filter(|&f| h.usable(f)).ok_or(IoError::NoMemory)?;
        let id = ids.alloc(here, route).ok_or(IoError::NoMemory)?;
        let new = Place { hart: here, id };
        loop {
            let old = ids
                .places_of(route)
                .find(|&(p, s)| p != new && !matches!(s, Slot::Retiring { .. }))
                .map(|(p, _)| p);
            let Some(p) = old else { break };
            if ids.end(p).is_err() {
                break;
            }
            h.ask(p.hart);
        }
        h.open_here(id);
        let _ = ids.opened(new);
        Ok(())
    }

    /// Is the grant's identity open? `Ok(Some)`: open (and where); `Ok(None)`: not yet; `BadState`: the route ended (the domain was
    /// destroyed meanwhile).
    pub fn grant_ready(&self, ids: &MsiIds, v: VectorRef) -> Result<Option<Place>, IoError> {
        let r = self.at(v).ok_or(IoError::BadState)?;
        if r.state != RState::Granting {
            return Err(IoError::BadState);
        }
        Ok(current(ids, v.n).and_then(|(p, ready)| ready.then_some(p)))
    }

    /// 8.3, under `OBJ` then `IOMMU`: the entry, naming the grant's open identity (`IoDomains::vector_grant`). The fence's ticket.
    /// `NotDone`: the identity is not open yet (it moved: a hart went offline). The store's errors otherwise.
    #[allow(clippy::too_many_arguments)]
    pub fn grant_write<M: PhysMem64, R: Regs, F: TableFrames>(
        &mut self,
        ids: &MsiIds,
        h: &dyn Harts,
        store: &mut IoDomains,
        mem: &M,
        regs: &mut R,
        frames: &mut F,
        v: VectorRef,
    ) -> Result<Ticket, IoError> {
        let p = self.grant_ready(ids, v)?.ok_or(IoError::NotDone)?;
        let r = self.at(v).ok_or(IoError::BadState)?;
        let (d, index) = (DomainId::from_raw(r.domain), r.index);
        let (t, file) =
            store.vector_grant(mem, regs, frames, d, index, h.file_page(p.hart), p.id)?;
        if let Some(r) = self.at_mut(v) {
            r.file = file;
            r.target = Some(p);
        }
        Ok(t)
    }

    /// The fence of a grant's or a move's entry write has completed: the entry names the identity written (`Live` for a grant),
    /// and every identity the route moved away from is retired now (IO10). `Ok(false)`: the write did not take effect as a grant or a
    /// move (a revoke or the domain's destruction took the vector). `NotDone` before the fence.
    pub fn fenced(
        &mut self,
        ids: &mut MsiIds,
        h: &mut dyn Harts,
        store: &mut IoDomains,
        v: VectorRef,
    ) -> Result<bool, IoError> {
        let Some(r) = self.at(v) else {
            return Ok(false);
        };
        let (d, index) = (DomainId::from_raw(r.domain), r.index);
        let done = store.vector_fenced(d, index)?;
        let Some(r) = self.at_mut(v) else {
            return Ok(false);
        };
        r.busy = false;
        if !done || !matches!(r.state, RState::Granting | RState::Live) {
            return Ok(false);
        }
        r.named = r.target.take();
        r.state = RState::Live;
        let named = r.named;
        if current(ids, v.n).is_some_and(|(p, ready)| !ready || Some(p) != named) {
            self.resettle = true;
        }
        // (the identities the route moved away from: no entry names them any more, cached or not: retired by their harts now)
        loop {
            let next = ids
                .places_of(Route::Vector(v.n))
                .find(|&(p, s)| matches!(s, Slot::Moved { .. }) && Some(p) != named)
                .map(|(p, _)| p);
            let Some(p) = next else { break };
            let _ = ids.drained(p);
            h.ask(p.hart);
        }
        Ok(true)
    }

    /// A grant's or a move's fence will not complete (6.4): the vector is leaked (IO8): its identities are never retired (an entry
    /// that names one may stay cached), and its charge stays.
    pub fn abandon<M: PhysMem64>(&mut self, store: &mut IoDomains, mem: &M, v: VectorRef) {
        let Some(r) = self.at(v) else {
            return;
        };
        let (d, index) = (DomainId::from_raw(r.domain), r.index);
        if store.vector_abandon(d, index) {
            self.leak(mem, v);
        }
    }

    /// Leak route `v` (IO8). Its file is disabled first (its frame is kept, never freed: writing it is safe), so a device that goes
    /// on sending to an entry still cached sets a pending bit and sends no notice: a leaked vector cannot flood a hart (8.4).
    fn leak<M: PhysMem64>(&mut self, mem: &M, v: VectorRef) {
        let Some(r) = self.at_mut(v) else {
            return;
        };
        if matches!(r.state, RState::Leaked | RState::Spent | RState::Dead) {
            return;
        }
        if let Ok(f) = Mrif::new(r.file) {
            let _ = f.disable(mem, VECTOR_DATA);
        }
        r.state = RState::Leaked;
        r.busy = false;
        let unplaced = !r.placed;
        r.placed = true;
        self.leaked = self.leaked.saturating_add(1);
        if unplaced {
            self.unplaced = self.unplaced.saturating_sub(1);
        }
    }

    /// The grant failed before its entry was written (no object, an error to return): the route ends, its identity is retired, and
    /// its charge comes back through `take_spent` like any route's.
    pub fn grant_cancel(&mut self, ids: &mut MsiIds, h: &mut dyn Harts, v: VectorRef) {
        if self.at(v).is_some_and(|r| r.state == RState::Granting) {
            self.end(ids, h, v);
        }
    }

    /// 8.3, the end: the interrupt object `obj` (of the kind vector, naming `v`) is made; the file is armed, and a message that came
    /// before fires the object at once. False if the route is not live (the caller revokes it like any closed vector).
    pub fn attach<M: PhysMem64>(
        &mut self,
        objs: &mut Kobjs,
        mem: &M,
        v: VectorRef,
        obj: ObjId,
    ) -> bool {
        let Some(r) = self.at_mut(v).filter(|r| r.state == RState::Live) else {
            return false;
        };
        r.obj = Some(obj);
        r.placed = false;
        let file = r.file;
        self.unplaced = self.unplaced.saturating_add(1);
        if file_arm(mem, file) {
            fire(objs, obj);
        }
        true
    }

    // ---- delivery -----------------------------------------------------------------------------------------------------------

    /// A message on identity `p` (the interrupt path, under `OBJ`, no other lock): if it is a vector's (live, moved away from, or
    /// retiring after a move), the notice of 8.5 on the vector's file. Whether an object was signalled. `None`: not a vector's.
    pub fn notice<M: PhysMem64>(
        &mut self,
        objs: &mut Kobjs,
        ids: &MsiIds,
        mem: &M,
        p: Place,
    ) -> Option<bool> {
        let n = match ids.slot(p)? {
            Slot::Live {
                route: Route::Vector(n),
                ..
            }
            | Slot::Moved {
                route: Route::Vector(n),
                ..
            }
            | Slot::Retiring {
                route: Route::Vector(n),
                moved: true,
            } => n,
            _ => return None,
        };
        let Some(r) = self.by_n(n).and_then(|v| self.at(v)) else {
            return Some(false);
        };
        let (Some(obj), RState::Live) = (r.obj, r.state) else {
            return Some(false); // (granting: the arm takes the bit; revoking or dead: nobody's)
        };
        let file = r.file;
        let fired = matches!(objs.get(obj), Some(KObj::Interrupt(i)) if i.fired);
        if fired {
            return Some(false); // (a stale notice: the bit is the acknowledgement's to take)
        }
        Some(file_notice(mem, file) && fire(objs, obj))
    }

    /// `interrupt_ack` on vector object `obj` (route `v`), under `OBJ`: BAD_STATE for a dead vector or one that has not fired; else
    /// INTERRUPT is cleared, the file's stray bits are taken and counted (8.4), the file is armed again, and a message that came while
    /// it was masked fires it again at once. Returns the claims so far and whether this is the vector's first wait or acknowledgement
    /// (the caller then moves it to the calling hart: 8.5, placement).
    pub fn ack<M: PhysMem64>(
        &mut self,
        objs: &mut Kobjs,
        mem: &M,
        obj: ObjId,
        v: VectorRef,
    ) -> Result<(u64, bool), abi::status::Status> {
        use abi::status::Status;
        let r = self
            .at(v)
            .filter(|r| r.state == RState::Live && r.obj == Some(obj))
            .ok_or(Status::BadState)?;
        let file = r.file;
        let place = !r.placed;
        match objs.get_mut(obj) {
            Some(KObj::Interrupt(i)) if i.fired => i.fired = false,
            Some(KObj::Interrupt(_)) => return Err(Status::BadState),
            _ => return Err(Status::BadHandle),
        }
        self.strays = self.strays.saturating_add(file_strays(mem, file));
        if ack_arm(mem, file) {
            fire(objs, obj);
        }
        if place {
            self.placed(v);
        }
        let claims = match objs.get(obj) {
            Some(KObj::Interrupt(i)) => i.claims,
            _ => 0,
        };
        Ok((claims, place))
    }

    fn placed(&mut self, v: VectorRef) {
        if let Some(r) = self.at_mut(v).filter(|r| !r.placed) {
            r.placed = true;
            self.unplaced = self.unplaced.saturating_sub(1);
        }
    }

    /// The first `object_wait` on vector object `obj` (8.5, placement): `Some(route)` the first time (the caller moves it to the
    /// calling hart), `None` after, or for a wired source or a dead vector.
    pub fn first_wait(&mut self, objs: &Kobjs, obj: ObjId) -> Option<VectorRef> {
        let v = match objs.get(obj) {
            Some(KObj::Interrupt(i)) => i.vector?,
            _ => return None,
        };
        let r = self.at(v)?;
        if r.placed || r.state != RState::Live || r.obj != Some(obj) {
            return None;
        }
        self.placed(v);
        Some(v)
    }

    // ---- moves --------------------------------------------------------------------------------------------------------------

    /// A move to the calling hart (`interrupt_deliver_here`, a first wait or acknowledgement), the first step under `OBJ`: an identity
    /// here, opened at once (the old one is Moved: still routed to the vector until the entry's rewrite is fenced). Then
    /// `retarget_begin`, the fence and `fenced`, with the route busy meanwhile.
    pub fn move_here(
        &mut self,
        ids: &mut MsiIds,
        h: &mut dyn Harts,
        v: VectorRef,
    ) -> Result<Moved, MoveError> {
        let r = self.at(v).ok_or(MoveError::Dead)?;
        if r.state != RState::Live {
            return Err(MoveError::Dead);
        }
        if r.busy {
            return Err(MoveError::Busy);
        }
        let (cur, ready) = current(ids, v.n).ok_or(MoveError::Dead)?;
        if !ready {
            return Err(MoveError::Busy); // (a hart's evacuation is moving it: let that settle first)
        }
        let Some(here) = h.here() else {
            return Ok(Moved::Stays);
        };
        if cur.hart == here || h.leaving(here) {
            // (where it is asked to be; the entry may still name an identity it moved away from: the retarget follows that)
            return Ok(if r.named == Some(cur) {
                Moved::Stays
            } else {
                self.mark_busy(v);
                Moved::Retarget
            });
        }
        let id = ids.move_to(cur, here).ok_or(MoveError::NoMemory)?;
        let p = Place { hart: here, id };
        h.open_here(id);
        let _ = ids.opened(p);
        self.mark_busy(v);
        Ok(Moved::Retarget)
    }

    fn mark_busy(&mut self, v: VectorRef) {
        if let Some(r) = self.at_mut(v) {
            r.busy = true;
        }
    }

    /// A move whose identity a hart's evacuation chose (`IoDomains` is not involved yet): a live vector that is not busy and whose
    /// entry does not name its current identity, marked busy for the retarget that follows. One at a time.
    pub fn next_to_settle(&mut self, ids: &MsiIds) -> Option<VectorRef> {
        let n = (0..self.routes.len()).find(|&k| {
            let Some(r) = self.routes.get(k) else {
                return false;
            };
            if r.state != RState::Live || r.busy {
                return false;
            }
            let Ok(nn) = u32::try_from(k) else {
                return false;
            };
            current(ids, nn).is_some_and(|(p, ready)| !ready || Some(p) != r.named)
        })?;
        let v = self.by_n(u32::try_from(n).ok()?)?;
        self.mark_busy(v);
        Some(v)
    }

    /// A move, the second step, under `OBJ` then `IOMMU`: rewrite the entry's notice target to the route's open identity with one
    /// store (A16), and queue the invalidation and a fence. `Ok(None)`: nothing to rewrite (the entry already names it; the route is
    /// no longer busy). `NotDone`: the identity is not open yet (try again). `BadState`: the vector is being revoked or its domain
    /// destroyed (the route is no longer busy; the revoke or the destruction ends it).
    #[allow(clippy::too_many_arguments)]
    pub fn retarget_begin<M: PhysMem64, R: Regs>(
        &mut self,
        ids: &MsiIds,
        h: &dyn Harts,
        store: &mut IoDomains,
        mem: &M,
        regs: &mut R,
        v: VectorRef,
    ) -> Result<Option<Ticket>, IoError> {
        let r = *self.at(v).ok_or(IoError::BadState)?;
        if r.state != RState::Live {
            self.unbusy(v);
            return Err(IoError::BadState);
        }
        let (p, ready) = current(ids, v.n).ok_or(IoError::BadState)?;
        if !ready {
            return Err(IoError::NotDone);
        }
        if r.named == Some(p) {
            self.unbusy(v);
            return Ok(None);
        }
        let (d, index) = (DomainId::from_raw(r.domain), r.index);
        match store.vector_retarget(mem, regs, d, index, h.file_page(p.hart), p.id) {
            Ok(t) => {
                if let Some(r) = self.at_mut(v) {
                    r.target = Some(p);
                }
                Ok(Some(t))
            }
            Err(e) => {
                self.unbusy(v);
                Err(e)
            }
        }
    }

    /// Did a fence complete naming an identity its route had moved away from since the last look (the moves are to be settled)?
    pub fn take_resettle(&mut self) -> bool {
        core::mem::replace(&mut self.resettle, false)
    }

    /// Give up a move before its entry write (the caller could not wait for an identity to open).
    pub fn unbusy(&mut self, v: VectorRef) {
        if let Some(r) = self.at_mut(v) {
            r.busy = false;
        }
    }

    // ---- revoke and destruction -------------------------------------------------------------------------------------------------

    /// The last handle of route `v`'s vector object went (its cleanup, under `OBJ` then `IOMMU`; a route has one object, ever): 8.5 revoke, steps 1 to 3 (the enable bit
    /// cleared, the entry invalid, the invalidation and a fence queued: `IoDomains::vector_revoke`). The fence's ticket, for
    /// `revoke_end` (the kernel's drain waits for it with no lock held). `None`: nothing to wait for (a dead or leaked vector, or
    /// its domain's destruction has begun: that ends the route).
    pub fn revoke_begin<M: PhysMem64, R: Regs>(
        &mut self,
        store: &mut IoDomains,
        mem: &M,
        regs: &mut R,
        v: VectorRef,
    ) -> Option<Ticket> {
        self.revoke(store, mem, regs, v, true)
    }

    /// A live vector the grant could not give an object (the handle table was full): revoked as a closed one is.
    pub fn revoke_orphan<M: PhysMem64, R: Regs>(
        &mut self,
        store: &mut IoDomains,
        mem: &M,
        regs: &mut R,
        v: VectorRef,
    ) -> Option<Ticket> {
        self.revoke(store, mem, regs, v, false)
    }

    fn revoke<M: PhysMem64, R: Regs>(
        &mut self,
        store: &mut IoDomains,
        mem: &M,
        regs: &mut R,
        v: VectorRef,
        closing: bool,
    ) -> Option<Ticket> {
        let r = self.at(v).filter(|r| r.obj.is_some() == closing)?;
        let (d, index, state) = (DomainId::from_raw(r.domain), r.index, r.state);
        self.placed(v);
        if let Some(r) = self.at_mut(v) {
            r.obj = None;
        }
        if !matches!(state, RState::Live | RState::Granting) {
            return None;
        }
        let t = store.vector_revoke(mem, regs, d, index)?;
        if let Some(r) = self.at_mut(v) {
            r.state = RState::Revoking;
        }
        Some(t)
    }

    /// A revoke whose fence nobody has completed yet (the kernel's drain completes them: `revoke_end`).
    /// The search starts at route `from` (a drain that cannot complete one yet goes on to the next).
    pub fn next_revoking(&self, from: u32) -> Option<VectorRef> {
        let from = usize::try_from(from).ok()?;
        self.routes
            .iter()
            .enumerate()
            .skip(from)
            .find_map(|(k, r)| {
                (r.state == RState::Revoking).then_some(VectorRef {
                    n: u32::try_from(k).ok()?,
                    gen: r.gen,
                })
            })
    }

    /// 8.5 revoke, the end, once the fence has completed (under `OBJ` then `IOMMU`): the file freed by the store (its stray bits
    /// counted), then the route's identities end and are retired by their harts (IO10: after the fence). `NotDone` before the fence.
    #[allow(clippy::too_many_arguments)]
    pub fn revoke_end<M: PhysMem64, F: TableFrames>(
        &mut self,
        ids: &mut MsiIds,
        h: &mut dyn Harts,
        store: &mut IoDomains,
        mem: &M,
        frames: &mut F,
        v: VectorRef,
    ) -> Result<(), IoError> {
        let Some(r) = self.at(v).filter(|r| r.state == RState::Revoking) else {
            return Ok(());
        };
        let (d, index) = (DomainId::from_raw(r.domain), r.index);
        store.vector_revoked(mem, frames, d, index)?;
        self.end(ids, h, v);
        Ok(())
    }

    /// A revoke's fence will not complete: the vector is leaked (IO8).
    pub fn revoke_abandon<M: PhysMem64>(&mut self, store: &mut IoDomains, mem: &M, v: VectorRef) {
        if self.at(v).is_some_and(|r| r.state == RState::Revoking) {
            self.abandon(store, mem, v);
        }
    }

    /// End route `v`: every identity it has (Opening, Live, Moved) is Retiring, and its harts are asked to retire them. Dead.
    fn end(&mut self, ids: &mut MsiIds, h: &mut dyn Harts, v: VectorRef) {
        let route = Route::Vector(v.n);
        loop {
            let next = ids
                .places_of(route)
                .find(|(_, s)| !matches!(s, Slot::Retiring { .. }))
                .map(|(p, _)| p);
            let Some(p) = next else { break };
            if ids.end(p).is_err() {
                break; // (not reached: every state but Free and Retiring ends)
            }
        }
        let mut asked: u64 = 0;
        for (p, _) in ids.places_of(route) {
            let bit = 1u64.checked_shl(u32::from(p.hart)).unwrap_or(0);
            if asked & bit == 0 || bit == 0 {
                h.ask(p.hart);
                asked |= bit;
            }
        }
        if let Some(r) = self.at_mut(v) {
            r.state = RState::Dead;
            r.busy = false;
            r.named = None;
            r.target = None;
        }
        self.placed(v);
        self.check_spent(ids, v);
    }

    fn check_spent(&mut self, ids: &MsiIds, v: VectorRef) {
        if self.at(v).is_some_and(|r| r.state == RState::Dead)
            && ids.places_of(Route::Vector(v.n)).next().is_none()
        {
            if let Some(r) = self.at_mut(v) {
                r.state = RState::Spent;
            }
            self.spent = self.spent.saturating_add(1);
        }
    }

    /// An identity of route `Route::Vector(n)` was retired by its hart (the kernel's interrupt path, after `MsiIds::retired`): a dead
    /// route whose last identity this was is spent. True if it became spent (its charge is due: `take_spent`).
    pub fn retired(&mut self, ids: &MsiIds, n: u32) -> bool {
        let Some(v) = self.by_n(n) else {
            return false;
        };
        let before = self.spent;
        self.check_spent(ids, v);
        self.spent > before
    }

    /// A spent route: freed now (its generation moves on, so its object is dead for good); its job is returned, for the caller to
    /// give back the vector's charge (`credit_vector`, `JOBS` under `OBJ`).
    pub fn take_spent(&mut self) -> Option<JobId> {
        if self.spent == 0 {
            return None;
        }
        let r = self.routes.iter_mut().find(|r| r.state == RState::Spent)?;
        let job = r.job;
        *r = VecRoute {
            gen: r.gen.wrapping_add(1),
            ..FREE
        };
        self.spent = self.spent.saturating_sub(1);
        self.used = self.used.saturating_sub(1);
        Some(job)
    }

    /// Domain `d`'s destruction fence has completed (6.3, 10.6 step 4; before the store releases its files): every route of the
    /// domain ends and its identities are retired. Linear in the routes.
    pub fn domain_gone(&mut self, ids: &mut MsiIds, h: &mut dyn Harts, d: DomainId) {
        for k in 0..self.routes.len() {
            let Some(r) = self.routes.get(k) else {
                continue;
            };
            if r.domain != d.raw()
                || !matches!(r.state, RState::Granting | RState::Live | RState::Revoking)
            {
                continue;
            }
            let Ok(n) = u32::try_from(k) else { continue };
            let v = VectorRef { n, gen: r.gen };
            self.end(ids, h, v);
        }
    }

    /// Domain `d`'s destruction fence will not complete (6.4): every route of the domain that has not ended is leaked (IO8).
    pub fn domain_leaked<M: PhysMem64>(&mut self, mem: &M, d: DomainId) {
        for k in 0..self.routes.len() {
            let Some(r) = self.routes.get(k) else {
                continue;
            };
            if r.domain != d.raw()
                || !matches!(r.state, RState::Granting | RState::Live | RState::Revoking)
            {
                continue;
            }
            let Ok(n) = u32::try_from(k) else { continue };
            let v = VectorRef { n, gen: r.gen };
            self.leak(mem, v);
        }
    }
}

/// Counters of the vector routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VecCounts {
    pub used: u64,
    /// Granting, live, revoking or dead (identities not yet all free): the leak oracle wants 0 at the end.
    pub live: u64,
    /// Ended, with identities still retiring.
    pub dead: u64,
    pub leaked: u64,
    pub strays: u64,
    pub unplaced: u64,
    pub spent: u64,
}

/// The identity route `n` is on now: an Opening one (a move or a grant whose hart has not opened it yet; `false`), else its Live one
/// that is not moving (`true`).
fn current(ids: &MsiIds, n: u32) -> Option<(Place, bool)> {
    let mut live = None;
    for (p, s) in ids.places_of(Route::Vector(n)) {
        match s {
            Slot::Opening { .. } => return Some((p, false)),
            Slot::Live { .. } => live = Some(p),
            _ => {}
        }
    }
    live.map(|p| (p, true))
}
