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

/// `None`: `n` is not a VMO call (the caller handles it).
#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
// INVARIANT: a is [u64; 6] and every `h(i)` is called with a constant below 6; offsets and lengths are bounded (len <= MAX_MSG_BYTES, commit pages <= MAX_COMMIT_PAGES) before any sum, and `checked_add` guards the VMO range
#[allow(clippy::large_stack_frames)] // KT13: audited, clippy's estimate over 1 KiB, left by T289 (see work/T289.md)
pub fn vmo_call<M: PhysMem>(n: usize, a: [u64; 6], c: &mut Ctx<'_, M>) -> Option<Reply> {
    if !matches!(
        n,
        nr::VMO_CREATE
            | nr::VMO_CREATE_PHYSICAL
            | nr::VMO_COMMIT
            | nr::VMO_GET_SIZE
            | nr::VMO_READ
            | nr::VMO_WRITE
            | nr::PAGER_CREATE_VMO
    ) {
        return None;
    }
    let Ctx {
        objs,
        handles,
        job,
        aspace,
        mm,
        vmos,
        jobs,
        windows,
        ..
    } = c;
    let job = *job;
    let windows: &[(u64, u64)] = windows;
    let h = |i: usize| Handle(a[i]);
    let r: Reply = (|| match n {
        nr::VMO_CREATE => {
            // (a0 is a size in bytes, rounded up to whole pages)
            if a[0] == 0 || a[0] > abi::user::TOP || a[2] & !abi::vmo_flags::RESIZABLE != 0 {
                return fail(Status::InvalidArgs);
            }
            let pages = a[0].div_ceil(4096);
            // a1: the job the memory is charged to (a handle with MANAGE), or 0 for the caller's own
            let job = if a[1] == 0 {
                job
            } else {
                ipc::job_of(objs, handles, h(1), Rights::MANAGE).map_err(|e| (e, None, None))?
            };
            vmos.reserve_one().map_err(|e| (e, None, None))?;
            let id = vmos.create_in(jobs, pages, job);
            if a[2] & abi::vmo_flags::RESIZABLE != 0 {
                vmos.set_resizable(id);
            }
            match ipc::vmo_object_create_in(
                objs,
                handles,
                id,
                &mut ipc::Quota {
                    jobs,
                    job: Some(job),
                },
            ) {
                Ok(h) => Ok((Some(h.0), None)),
                Err(e) => {
                    let _ = vmos.release(mm, jobs, id);
                    fail(e)
                }
            }
        }
        nr::PAGER_CREATE_VMO => {
            // (pager, key, bytes): a VMO whose pages the pager supplies (MM9); the memory is charged to the caller's job
            let pager =
                ipc::pager_of(objs, handles, h(0), Rights::MANAGE).map_err(|e| (e, None, None))?;
            if a[2] == 0 || !a[2].is_multiple_of(4096) || a[2] > abi::user::TOP {
                return fail(Status::InvalidArgs);
            }
            vmos.reserve_one().map_err(|e| (e, None, None))?;
            let id = vmos.create_pager_in(jobs, a[2] / 4096, job, pager, a[1]);
            match ipc::vmo_object_create_with(
                objs,
                handles,
                id,
                Rights::PAGER_VMO,
                &mut ipc::Quota {
                    jobs,
                    job: Some(job),
                },
            ) {
                Ok(h) => Ok((Some(h.0), None)),
                Err(e) => {
                    let _ = vmos.release(mm, jobs, id);
                    fail(e)
                }
            }
        }
        nr::VMO_CREATE_PHYSICAL => {
            ipc::resource_of(objs, handles, h(0)).map_err(|e| (e, None, None))?;
            let (pa, bytes) = (a[1], a[2]);
            if !kcore::devwin::in_window(windows, pa, bytes) {
                return fail(Status::InvalidArgs);
            }
            vmos.reserve_one().map_err(|e| (e, None, None))?;
            let id = vmos.create_physical(jobs, pa >> 12, bytes >> 12);
            let rights = Rights(
                Rights::READ.0
                    | Rights::WRITE.0
                    | Rights::MAP.0
                    | Rights::DUPLICATE.0
                    | Rights::TRANSFER.0,
            );
            match ipc::vmo_object_create_with(
                objs,
                handles,
                id,
                rights,
                &mut ipc::Quota {
                    jobs,
                    job: Some(job),
                },
            ) {
                Ok(h) => Ok((Some(h.0), None)),
                Err(e) => {
                    let _ = vmos.release(mm, jobs, id);
                    fail(e)
                }
            }
        }
        nr::VMO_COMMIT => {
            // (offset, length) in bytes, page aligned: commit those pages now (slice V)
            let id =
                ipc::vmo_of(objs, handles, h(0), Rights::COMMIT).map_err(|e| (e, None, None))?;
            vmo_commit_resolved(mm, vmos, jobs, id, a[1], a[2])
        }
        nr::VMO_GET_SIZE => {
            let id = ipc::vmo_of(objs, handles, h(0), Rights::NONE).map_err(|e| (e, None, None))?;
            let pages = vmos.pages(id).ok_or((Status::BadHandle, None, None))?;
            Ok((Some(pages * 4096), None))
        }
        nr::VMO_READ | nr::VMO_WRITE => {
            let writing = n == nr::VMO_WRITE;
            let need = if writing { Rights::WRITE } else { Rights::READ };
            let id = ipc::vmo_of(objs, handles, h(0), need).map_err(|e| (e, None, None))?;
            vmo_rw_resolved(writing, aspace, mm, vmos, jobs, id, a)
        }
        _ => fail(Status::BadSyscall),
    })();
    Some(r)
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
#[allow(clippy::large_stack_frames)] // KT13: audited, clippy's estimate over 1 KiB, left by T289 (see work/T289.md)
pub fn vmo_rw_resolved<M: PhysMem>(
    writing: bool,
    aspace: &mut kcore::aspace::AddressSpace,
    mm: &mut kcore::Mm<M>,
    vmos: &mut kcore::aspace::Vmos,
    jobs: &mut kcore::job::Jobs,
    id: u64,
    a: [u64; 6],
) -> Reply {
    let r: Reply = (|| {
        if vmos.kind(id) == Some(kcore::aspace::VmoKind::Device) {
            // device memory is reached only by mapping it, never through the kernel
            return fail(Status::NotSupported);
        }
        let (offset, buf, len) = (a[1], a[2], a[3]);
        if len > MAX_MSG_BYTES as u64 {
            return fail(Status::TooBig);
        }
        let len = len as usize;
        let size = vmos.pages(id).ok_or((Status::BadHandle, None, None))? * 4096;
        if offset.checked_add(len as u64).is_none_or(|end| end > size) {
            return fail(Status::InvalidArgs);
        }
        if writing {
            let data =
                copy_from_user(aspace, mm, vmos, jobs, buf, len).map_err(|e| (e, None, None))?;
            vmos.write(mm, jobs, id, offset, &data)
                .map_err(|e| match e {
                    kcore::aspace::VmoError::NeedPager => (Status::ShouldWait, None, None),
                    _ => (Status::NoMemory, None, None),
                })?;
        } else {
            verify_writable(aspace, mm, vmos, jobs, buf, len).map_err(|e| (e, None, None))?;
            let mut data: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
            data.try_reserve_exact(len)
                .map_err(|_| (Status::NoMemory, None, None))?;
            data.resize(len, 0);
            vmos.read(mm, id, offset, &mut data).map_err(|e| match e {
                kcore::aspace::VmoError::NeedPager => (Status::ShouldWait, None, None),
                _ => (Status::InvalidArgs, None, None),
            })?;
            copy_to_user_verified(aspace, mm, buf, &data).map_err(|e| (e, None, None))?;
        }
        Ok((None, None))
    })();
    r
}
