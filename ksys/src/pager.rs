//! Pagers (MM9, KERNEL-V1-SPEC 4.4): `pager_create`, `pager_supply`, `pager_fail`, and the steps of a thread's wait for a page. Pure: the kernel
//! blocks and wakes the threads; this decides what is asked of the pager, moves the frames, and says who to wake.

use crate::proc::Machine;
use crate::UserState;
use abi::rights::Rights;
use abi::status::Status;
use abi::Handle;
use alloc::vec::Vec;
use kcore::aspace::{PagerNeed, VmoError, Vmos};
use kcore::ipc::{self, Quota};
use kcore::job::Jobs;
use kcore::pager::Outcome;
use kcore::pagetable::PhysMem;
use kcore::sched::Tid;
use kcore::Mm;

fn map_vmo_error(e: VmoError) -> Status {
    match e {
        VmoError::Gone => Status::BadHandle,
        VmoError::NoMemory => Status::NoMemory,
        VmoError::NotSuppliable => Status::BadState,
        _ => Status::InvalidArgs,
    }
}

/// `pager_create() -> (pager, channel)`: a pager and the user end of the channel the kernel writes its requests on (it keeps the other end).
pub fn pager_create(us: &mut UserState, jobs: &mut Jobs, me: u64) -> Result<(u64, u64), Status> {
    let caller = us.procs.get(&me).ok_or(Status::BadState)?;
    let job = caller.job;
    if caller.handles.room() < 2 {
        return Err(Status::NoMemory);
    }
    // (room for both handles first, so that the inserts below cannot fail and strand an end)
    us.procs
        .get_mut(&me)
        .ok_or(Status::BadState)?
        .handles
        .reserve(2)
        .map_err(|_| Status::NoMemory)?;
    let id = us.next_pager;
    let UserState {
        objs,
        ktable,
        procs,
        pager_ends,
        pagers,
        ..
    } = us;
    let mut q = Quota {
        jobs,
        job: Some(job),
    };
    let (kernel_end, user_end) = ipc::channel_create_in(objs, ktable, &mut q)?;
    let entry = ktable
        .remove_for_transfer(objs, user_end)
        .map_err(|_| Status::BadState)?;
    let handles = &mut procs.get_mut(&me).ok_or(Status::BadState)?.handles;
    let ch = match handles.insert(objs, entry.obj, entry.rights) {
        Ok(h) => h,
        Err(e) => {
            let _ = ktable.close(objs, kernel_end);
            return Err(e);
        }
    };
    match ipc::pager_object_create_in(objs, handles, id, Rights::PAGER, &mut q) {
        Ok((ph, _)) => {
            us.next_pager = id.saturating_add(1);
            pagers.create(id);
            pager_ends.insert(id, kernel_end);
            Ok((ph.0, ch.0))
        }
        Err(e) => {
            let _ = handles.close(objs, ch);
            let _ = ktable.close(objs, kernel_end);
            Err(e)
        }
    }
}

/// What a supply or a failure needs of its arguments: the pager, the VMO (one of that pager's), and the page range.
fn range(
    us: &UserState,
    vmos: &Vmos,
    me: u64,
    a: &[u64; 6],
) -> Result<(u64, u64, u64, u64), Status> {
    let caller = us.procs.get(&me).ok_or(Status::BadState)?;
    let pager = ipc::pager_of(&us.objs, &caller.handles, Handle(a[0]), Rights::MANAGE)?;
    let vmo = ipc::vmo_of(&us.objs, &caller.handles, Handle(a[1]), Rights::NONE)?;
    let (offset, length) = (a[2], a[3]);
    if !offset.is_multiple_of(4096) || !length.is_multiple_of(4096) || length == 0 {
        return Err(Status::InvalidArgs);
    }
    // (a VMO of another pager is not this pager's to fill)
    match vmos.pager_of(vmo) {
        Some((p, _)) if p == pager => {}
        Some(_) => return Err(Status::AccessDenied),
        None => return Err(Status::InvalidArgs),
    }
    let (first, count) = (offset / 4096, length / 4096);
    let size = vmos.pages(vmo).ok_or(Status::BadHandle)?;
    if first.checked_add(count).is_none_or(|end| end > size) {
        return Err(Status::InvalidArgs);
    }
    Ok((pager, vmo, first, count))
}

/// `pager_supply(pager, vmo, offset, length, source, source_offset)`: move the pages of `source` (an anonymous VMO of the caller's, WRITE) to
/// the pager's VMO and wake the threads waiting for them. The frames are not copied; the charge follows them. Returns the threads to wake.
pub fn pager_supply<M: PhysMem>(
    us: &mut UserState,
    mm: &mut Mm<M>,
    vmos: &mut Vmos,
    jobs: &mut Jobs,
    machine: &mut impl Machine,
    me: u64,
    a: [u64; 6],
) -> Result<Vec<Tid>, Status> {
    let (pager, vmo, first, count) = range(us, vmos, me, &a)?;
    let caller = us.procs.get(&me).ok_or(Status::BadState)?;
    let src = ipc::vmo_of(&us.objs, &caller.handles, Handle(a[4]), Rights::WRITE)?;
    let soffset = a[5];
    if !soffset.is_multiple_of(4096) {
        return Err(Status::InvalidArgs);
    }
    let sfirst = soffset / 4096;
    if src == vmo {
        return Err(Status::InvalidArgs);
    }
    // V1: a process of another job than the source's holds its mapping on the promise that touching it never fails; taking pages from under
    // it would break that (as `vmo_decommit` refuses)
    let src_job = vmos.job_of(src).ok_or(Status::BadHandle)?;
    if us
        .procs
        .values()
        .any(|p| p.job != src_job && p.aspace.held_ref().maps_vmo_range(src, sfirst, count))
    {
        return Err(Status::BadState);
    }
    // (refused for what is wrong with the VMOs before any mapping is touched)
    vmos.supply_check(vmo, first, count, src, sfirst)
        .map_err(map_vmo_error)?;
    // the source's pages leave every address space that maps them, and every hart drops its translation, before the frames change owner
    let mut cleared = 0u64;
    for p in us.procs.values_mut() {
        cleared =
            cleared.saturating_add(p.aspace.held_mut().unmap_vmo_pages(mm, src, sfirst, count));
    }
    if cleared > 0 {
        machine.shootdown();
        machine.flush_tlb();
    }
    let (moved, err) = match vmos.supply(mm, jobs, vmo, first, count, src, sfirst) {
        Ok(n) => (n, None),
        Err((n, e)) => (n, Some(e)),
    };
    let woken = us.pagers.supply(pager, vmo, first, moved);
    match err {
        // (the pages that did move are supplied, and their waiters wake; the error path of a call has nowhere to return them, so the kernel
        // takes them from `pending_wake`)
        Some(e) => {
            us.pending_wake.extend(woken);
            Err(map_vmo_error(e))
        }
        None => Ok(woken),
    }
}

/// `pager_fail(pager, vmo, offset, length, status)`: the pager cannot supply these pages; the threads waiting for them fail with `status`
/// (a faulting thread takes a PAGER exception, a kernel copy returns it). Returns the threads to wake.
pub fn pager_fail(
    us: &mut UserState,
    vmos: &Vmos,
    me: u64,
    a: [u64; 6],
) -> Result<Vec<Tid>, Status> {
    let (pager, vmo, first, count) = range(us, vmos, me, &a)?;
    // (a failure the reader can act on: not OK, and not one of the codes that mean "try again" or "stopped by the caller")
    let status = i32::try_from(a[4] as i64)
        .ok()
        .and_then(Status::from_code)
        .filter(|s| !matches!(s, Status::Ok | Status::ShouldWait | Status::Interrupted))
        .ok_or(Status::InvalidArgs)?;
    Ok(us.pagers.fail(pager, vmo, first, count, status))
}

/// The last handle to a pager was closed: the waits for its pages fail, and the kernel's end of its channel goes.
pub fn pager_destroyed(us: &mut UserState, id: u64) -> Vec<Tid> {
    let woken = us.pagers.destroy(id);
    if let Some(h) = us.pager_ends.remove(&id) {
        let UserState { objs, ktable, .. } = us;
        let _ = ktable.close(objs, h);
    }
    woken
}

/// Write the requests of `pager` that have not been written yet (its channel was full), oldest first, until one does not fit. Returns the
/// threads to wake if the pager's channel turned out to have no reader any more (every wait for its pages fails).
pub fn pager_flush(us: &mut UserState, vmos: &Vmos, jobs: &mut Jobs, pager: u64) -> Vec<Tid> {
    let Some(&end) = us.pager_ends.get(&pager) else {
        return Vec::new();
    };
    loop {
        let Some(r) = us.pagers.unsent(pager) else {
            return Vec::new();
        };
        let UserState {
            objs,
            ktable,
            pagers,
            ..
        } = us;
        let Some((_, key)) = vmos.pager_of(r.vmo) else {
            pagers.sent(pager); // (its VMO is gone: nobody can need the page)
            continue;
        };
        if ipc::channel_peer_closed(objs, ktable, end) {
            return pagers.destroy(pager);
        }
        let mut msg = Vec::new();
        for w in [key, r.page.saturating_mul(4096), 4096, abi::pager::READ, 0] {
            msg.extend_from_slice(&w.to_le_bytes());
        }
        match ipc::channel_write_in(objs, ktable, end, &msg, &[], jobs) {
            Ok(()) => pagers.sent(pager),
            // (full, or the pager's job has no room for the message: the request stays pending and the waiter's wait goes on)
            Err(_) => return Vec::new(),
        }
    }
}

/// A thread is about to wait for `need`: register it and queue the request (once per page, however many wait). `Err` when the pager is gone
/// (the access fails). Returns the threads to wake (if writing the request found the pager's channel closed, that includes this one).
pub fn pager_wait_begin(
    us: &mut UserState,
    vmos: &Vmos,
    jobs: &mut Jobs,
    tid: Tid,
    need: PagerNeed,
) -> Result<Vec<Tid>, Status> {
    // (a page supplied since the call found it missing, or a VMO that is gone, is nothing to wait for)
    if !vmos.still_needed(need) {
        return Ok(Vec::new());
    }
    // a pager whose channel has no reader is dead, whether or not it has anything to write
    if let Some(&end) = us.pager_ends.get(&need.pager) {
        if ipc::channel_peer_closed(&us.objs, &us.ktable, end) {
            let woken = us.pagers.destroy(need.pager);
            us.pending_wake.extend(woken);
            return Err(Status::PeerClosed);
        }
    }
    us.pagers.wait(tid, need.pager, need.vmo, need.page)?;
    Ok(pager_flush(us, vmos, jobs, need.pager))
}

/// The request channel of a pager lost its reader (its user end's last handle went): the pager is dead and every wait for its pages fails.
/// The kernel calls this after objects were destroyed; an empty table costs one test.
pub fn reap_closed_channels(us: &mut UserState) -> Vec<Tid> {
    if us.pager_ends.is_empty() {
        return Vec::new();
    }
    let UserState {
        objs,
        ktable,
        pager_ends,
        pagers,
        ..
    } = us;
    let dead: Vec<u64> = pager_ends
        .iter()
        .filter(|(p, h)| pagers.exists(**p) && ipc::channel_peer_closed(objs, ktable, **h))
        .map(|(p, _)| *p)
        .collect();
    let mut woken = Vec::new();
    for p in dead {
        woken.extend(pagers.destroy(p));
    }
    woken
}

/// The thread's wait is over (or abandoned): forget it. `None`: it ended without a result (killed or interrupted).
pub fn pager_wait_end(us: &mut UserState, tid: Tid) -> Option<Outcome> {
    us.pagers.cancel(tid)
}

/// Is a wait's result in?
pub fn pager_wait_done(us: &UserState, tid: Tid) -> bool {
    us.pagers.result(tid).is_some()
}
