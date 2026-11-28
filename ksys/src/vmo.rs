//! The VMO calls (TS3b): create, physical creation, size, commit, read and write. (The two-argument `vmo_create_dma` and `vmo_phys_addr`
//! are retired, IOMMU-SPEC 4.8: their numbers are BAD_SYSCALL, and `vmo_create_dma_in` is in `iodomain`'s calls.)

use crate::{fail, Ctx, Reply};
use abi::limits::MAX_MSG_BYTES;
use abi::rights::Rights;
use abi::status::Status;
use abi::syscall as nr;
use abi::Handle;
use kcore::ipc;
use kcore::pagetable::PhysMem;
use kcore::usercopy::{copy_from_user, copy_to_user_verified, verify_writable};

/// `None`: `n` is not a VMO call (the caller handles it). Each call is a frame of its own (KT13): this one only routes.
pub fn vmo_call<M: PhysMem>(n: usize, a: [u64; 6], c: &mut Ctx<'_, M>) -> Option<Reply> {
    let a = &a;
    let r = match n {
        nr::VMO_CREATE => vmo_create(a, c),
        nr::PAGER_CREATE_VMO => pager_create_vmo(a, c),
        nr::VMO_CREATE_PHYSICAL => vmo_create_physical(a, c),
        nr::VMO_COMMIT => vmo_commit(a, c),
        nr::VMO_GET_SIZE => vmo_get_size(a, c),
        nr::VMO_READ | nr::VMO_WRITE => vmo_rw(n, a, c),
        _ => return None,
    };
    Some(r)
}

/// The object of a new VMO `id` and the caller's handle to it, with `rights`, charged to the caller's job; without them the VMO goes again.
#[inline(never)]
fn vmo_publish<M: PhysMem>(c: &mut Ctx<'_, M>, id: u64, rights: Rights) -> Reply {
    let mut quota = ipc::Quota {
        jobs: c.jobs,
        job: Some(c.job),
    };
    match ipc::vmo_object_create_with(c.objs, c.handles, id, rights, &mut quota) {
        Ok(h) => Ok((Some(h.0), None)),
        Err(e) => {
            let _ = c.vmos.release(c.mm, c.jobs, id);
            fail(e)
        }
    }
}

/// `vmo_create(bytes, job, flags)`: a0 is a size in bytes, rounded up to whole pages; a1 the job the memory is charged to (a handle with
/// MANAGE), or 0 for the caller's own.
#[inline(never)]
fn vmo_create<M: PhysMem>(a: &[u64; 6], c: &mut Ctx<'_, M>) -> Reply {
    if a[0] == 0 || a[0] > abi::user::TOP || a[2] & !abi::vmo_flags::RESIZABLE != 0 {
        return fail(Status::InvalidArgs);
    }
    let pages = a[0].div_ceil(4096);
    let job = if a[1] == 0 {
        c.job
    } else {
        match ipc::job_of(c.objs, c.handles, Handle(a[1]), Rights::MANAGE) {
            Ok(j) => j,
            Err(e) => return fail(e),
        }
    };
    if let Err(e) = c.vmos.reserve_one() {
        return fail(e);
    }
    let id = c.vmos.create_in(c.jobs, pages, job);
    if a[2] & abi::vmo_flags::RESIZABLE != 0 {
        c.vmos.set_resizable(id);
    }
    // (the handle's object is charged to the job the memory is charged to)
    let mut quota = ipc::Quota {
        jobs: c.jobs,
        job: Some(job),
    };
    match ipc::vmo_object_create_in(c.objs, c.handles, id, &mut quota) {
        Ok(h) => Ok((Some(h.0), None)),
        Err(e) => {
            let _ = c.vmos.release(c.mm, c.jobs, id);
            fail(e)
        }
    }
}

/// `pager_create_vmo(pager, key, bytes)`: a VMO whose pages the pager supplies (MM9); the memory is charged to the caller's job.
#[inline(never)]
fn pager_create_vmo<M: PhysMem>(a: &[u64; 6], c: &mut Ctx<'_, M>) -> Reply {
    let pager = match ipc::pager_of(c.objs, c.handles, Handle(a[0]), Rights::MANAGE) {
        Ok(p) => p,
        Err(e) => return fail(e),
    };
    if a[2] == 0 || !a[2].is_multiple_of(4096) || a[2] > abi::user::TOP {
        return fail(Status::InvalidArgs);
    }
    if let Err(e) = c.vmos.reserve_one() {
        return fail(e);
    }
    let id = c
        .vmos
        .create_pager_in(c.jobs, a[2] / 4096, c.job, pager, a[1]);
    vmo_publish(c, id, Rights::PAGER_VMO)
}

/// `vmo_create_physical(resource, pa, bytes)`: a VMO over device memory inside one of the device windows (PL5).
#[inline(never)]
fn vmo_create_physical<M: PhysMem>(a: &[u64; 6], c: &mut Ctx<'_, M>) -> Reply {
    if let Err(e) = ipc::resource_of(c.objs, c.handles, Handle(a[0])) {
        return fail(e);
    }
    let (pa, bytes) = (a[1], a[2]);
    if !kcore::devwin::in_window(c.windows, pa, bytes) {
        return fail(Status::InvalidArgs);
    }
    if let Err(e) = c.vmos.reserve_one() {
        return fail(e);
    }
    let id = c.vmos.create_physical(c.jobs, pa >> 12, bytes >> 12);
    let rights = Rights(
        Rights::READ.0 | Rights::WRITE.0 | Rights::MAP.0 | Rights::DUPLICATE.0 | Rights::TRANSFER.0,
    );
    vmo_publish(c, id, rights)
}

/// `vmo_commit(vmo, offset, length)`: (offset, length) in bytes, page aligned: commit those pages now (slice V).
#[inline(never)]
fn vmo_commit<M: PhysMem>(a: &[u64; 6], c: &mut Ctx<'_, M>) -> Reply {
    let id = match ipc::vmo_of(c.objs, c.handles, Handle(a[0]), Rights::COMMIT) {
        Ok(id) => id,
        Err(e) => return fail(e),
    };
    vmo_commit_resolved(c.mm, c.vmos, c.jobs, id, a[1], a[2])
}

#[allow(clippy::arithmetic_side_effects)] // INVARIANT: a VMO's size in bytes is below the user limit
#[inline(never)]
fn vmo_get_size<M: PhysMem>(a: &[u64; 6], c: &mut Ctx<'_, M>) -> Reply {
    let id = match ipc::vmo_of(c.objs, c.handles, Handle(a[0]), Rights::NONE) {
        Ok(id) => id,
        Err(e) => return fail(e),
    };
    match c.vmos.pages(id) {
        Some(pages) => Ok((Some(pages * 4096), None)),
        None => fail(Status::BadHandle),
    }
}

#[inline(never)]
fn vmo_rw<M: PhysMem>(n: usize, a: &[u64; 6], c: &mut Ctx<'_, M>) -> Reply {
    let writing = n == nr::VMO_WRITE;
    let need = if writing { Rights::WRITE } else { Rights::READ };
    let id = match ipc::vmo_of(c.objs, c.handles, Handle(a[0]), need) {
        Ok(id) => id,
        Err(e) => return fail(e),
    };
    vmo_rw_resolved(writing, c.aspace, c.mm, c.vmos, c.jobs, id, *a)
}

/// `vmo_commit` once the handle is resolved (`id`): the part that touches the VMO store, the job ledger and the frames, so a caller may run it with the memory
/// locks only (L20, T215: the pages are zeroed outside the object lock).
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: the length is bounded (commit pages <= MAX_COMMIT_PAGES) before any sum
pub fn vmo_commit_resolved<M: PhysMem>(
    mm: &mut kcore::Mm<M>,
    vmos: &mut kcore::aspace::Vmos,
    jobs: &mut kcore::job::Jobs,
    id: u64,
    offset: u64,
    len: u64,
) -> Reply {
    if !offset.is_multiple_of(4096) || !len.is_multiple_of(4096) {
        return fail(Status::InvalidArgs);
    }
    // (bounded: the call zeroes every frame it takes)
    if len / 4096 > abi::limits::MAX_COMMIT_PAGES {
        return fail(Status::TooBig);
    }
    match vmos.commit(mm, jobs, id, offset / 4096, len / 4096) {
        Ok(()) => Ok((None, None)),
        Err(kcore::aspace::VmoError::Gone) => fail(Status::BadHandle),
        Err(kcore::aspace::VmoError::NoMemory) => fail(Status::NoMemory),
        // (MM9) a page of a pager's VMO that is not there: the kernel asks the pager and waits, then the call says "try again"
        Err(kcore::aspace::VmoError::NeedPager) => fail(Status::ShouldWait),
        Err(_) => fail(Status::InvalidArgs),
    }
}

/// `vmo_read` and `vmo_write` once the handle is resolved (`id`, with READ or WRITE checked): the copy between the caller's memory (its address space, the
/// caller holds its lock) and the VMO, with the memory locks only (L20, T215: no object lock for the copy).
#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
// INVARIANT: a is [u64; 6] and every index is a constant below 6; len <= MAX_MSG_BYTES before any sum, and `checked_add` guards the VMO range
pub fn vmo_rw_resolved<M: PhysMem>(
    writing: bool,
    aspace: &mut kcore::aspace::AddressSpace,
    mm: &mut kcore::Mm<M>,
    vmos: &mut kcore::aspace::Vmos,
    jobs: &mut kcore::job::Jobs,
    id: u64,
    a: [u64; 6],
) -> Reply {
    match vmo_rw_in(writing, (aspace, mm, vmos, jobs), id, &a) {
        Ok(()) => Ok((None, None)),
        Err(e) => fail(e),
    }
}

/// `vmo_rw_resolved`'s checks, then the copy (a frame of its own: KT13).
#[allow(clippy::arithmetic_side_effects)]
// INVARIANT: len <= MAX_MSG_BYTES before any sum, and `checked_add` guards the VMO range
#[inline(never)]
fn vmo_rw_in<M: PhysMem>(
    writing: bool,
    (aspace, mm, vmos, jobs): (
        &mut kcore::aspace::AddressSpace,
        &mut kcore::Mm<M>,
        &mut kcore::aspace::Vmos,
        &mut kcore::job::Jobs,
    ),
    id: u64,
    a: &[u64; 6],
) -> Result<(), Status> {
    if vmos.kind(id) == Some(kcore::aspace::VmoKind::Device) {
        // device memory is reached only by mapping it, never through the kernel
        return Err(Status::NotSupported);
    }
    let (offset, buf, len) = (a[1], a[2], a[3]);
    if len > MAX_MSG_BYTES as u64 {
        return Err(Status::TooBig);
    }
    let len = len as usize;
    let size = vmos.pages(id).ok_or(Status::BadHandle)? * 4096;
    if offset.checked_add(len as u64).is_none_or(|end| end > size) {
        return Err(Status::InvalidArgs);
    }
    if writing {
        let data = copy_from_user(aspace, mm, vmos, jobs, buf, len)?;
        vmos.write(mm, jobs, id, offset, &data)
            .map_err(|e| match e {
                kcore::aspace::VmoError::NeedPager => Status::ShouldWait,
                _ => Status::NoMemory,
            })
    } else {
        vmo_read_out((aspace, mm, vmos, jobs), id, (offset, buf, len))
    }
}

/// `vmo_read`'s copy: the destination checked, the bytes read into a buffer (allocated first), then copied out.
#[inline(never)]
fn vmo_read_out<M: PhysMem>(
    (aspace, mm, vmos, jobs): (
        &mut kcore::aspace::AddressSpace,
        &mut kcore::Mm<M>,
        &mut kcore::aspace::Vmos,
        &mut kcore::job::Jobs,
    ),
    id: u64,
    (offset, buf, len): (u64, u64, usize),
) -> Result<(), Status> {
    verify_writable(aspace, mm, vmos, jobs, buf, len)?;
    let mut data: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    data.try_reserve_exact(len).map_err(|_| Status::NoMemory)?;
    data.resize(len, 0);
    vmos.read(mm, id, offset, &mut data).map_err(|e| match e {
        kcore::aspace::VmoError::NeedPager => Status::ShouldWait,
        _ => Status::InvalidArgs,
    })?;
    copy_to_user_verified(aspace, mm, buf, &data)
}
