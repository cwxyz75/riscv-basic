//! The VMO calls (TS3b): create, physical and DMA creation, size, commit, read and write, and the DMA address.

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
#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: a is [u64; 6] and every `h(i)` is called with a constant below 6; offsets and lengths are bounded (len <= MAX_MSG_BYTES, commit pages <= MAX_COMMIT_PAGES) before any sum, and `checked_add` guards the VMO range
pub fn vmo_call<M: PhysMem>(n: usize, a: [u64; 6], c: &mut Ctx<'_, M>) -> Option<Reply> {
    if !matches!(
        n,
        nr::VMO_CREATE
            | nr::VMO_CREATE_PHYSICAL
            | nr::VMO_CREATE_DMA
            | nr::VMO_PHYS_ADDR
            | nr::VMO_COMMIT
            | nr::VMO_GET_SIZE
            | nr::VMO_READ
            | nr::VMO_WRITE
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
        windows,
        ..
    } = c;
    let job = *job;
    let windows: &[(u64, u64)] = windows;
    let h = |i: usize| Handle(a[i]);
    let r: Reply = (|| match n {
        nr::VMO_CREATE => {
            if a[0] == 0 || a[0] > abi::limits::MAX_VMO_PAGES {
                return fail(Status::InvalidArgs);
            }
            // a1: the job the memory is charged to (a handle with MANAGE), or 0 for the caller's own
            let job = if a[1] == 0 {
                job
            } else {
                ipc::job_of(objs, handles, h(1), Rights::MANAGE).map_err(|e| (e, None, None))?
            };
            vmos.reserve_one().map_err(|e| (e, None, None))?;
            let id = vmos.create_in(a[0], job);
            match ipc::vmo_object_create_in(
                objs,
                handles,
                id,
                &mut ipc::Quota {
                    jobs: &mut vmos.jobs,
                    job: Some(job),
                },
            ) {
                Ok(h) => Ok((Some(h.0), None)),
                Err(e) => {
                    let _ = vmos.release(mm, id);
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
            let id = vmos.create_physical(pa >> 12, bytes >> 12);
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
                    jobs: &mut vmos.jobs,
                    job: Some(job),
                },
            ) {
                Ok(h) => Ok((Some(h.0), None)),
                Err(e) => {
                    let _ = vmos.release(mm, id);
                    fail(e)
                }
            }
        }
        nr::VMO_CREATE_DMA => {
            ipc::resource_of(objs, handles, h(0)).map_err(|e| (e, None, None))?;
            vmos.reserve_one().map_err(|e| (e, None, None))?;
            let id = match vmos.create_dma(mm, a[1], job) {
                Ok(id) => id,
                Err(kcore::aspace::VmoError::OutOfRange) => return fail(Status::InvalidArgs),
                Err(_) => return fail(Status::NoMemory),
            };
            // the creator's handle carries DMA; duplicates may drop it
            let rights = Rights(
                Rights::READ.0
                    | Rights::WRITE.0
                    | Rights::MAP.0
                    | Rights::DUPLICATE.0
                    | Rights::TRANSFER.0
                    | Rights::DMA.0,
            );
            match ipc::vmo_object_create_with(
                objs,
                handles,
                id,
                rights,
                &mut ipc::Quota {
                    jobs: &mut vmos.jobs,
                    job: Some(job),
                },
            ) {
                Ok(h) => Ok((Some(h.0), None)),
                Err(e) => {
                    let _ = vmos.release(mm, id);
                    fail(e)
                }
            }
        }
        nr::VMO_PHYS_ADDR => {
            let id = ipc::vmo_of(objs, handles, h(0), Rights::DMA).map_err(|e| (e, None, None))?;
            match vmos.phys_addr(id) {
                Some(pa) => Ok((Some(pa), None)),
                None => fail(Status::WrongType),
            }
        }
        nr::VMO_COMMIT => {
            // (offset, length) in bytes, page aligned: commit those pages now (slice V)
            let id =
                ipc::vmo_of(objs, handles, h(0), Rights::COMMIT).map_err(|e| (e, None, None))?;
            let (offset, len) = (a[1], a[2]);
            if !offset.is_multiple_of(4096) || !len.is_multiple_of(4096) {
                return fail(Status::InvalidArgs);
            }
            // (bounded: the call zeroes every frame it takes with the big lock held)
            if len / 4096 > abi::limits::MAX_COMMIT_PAGES {
                return fail(Status::TooBig);
            }
            match vmos.commit(mm, id, offset / 4096, len / 4096) {
                Ok(()) => Ok((None, None)),
                Err(kcore::aspace::VmoError::Gone) => fail(Status::BadHandle),
                Err(kcore::aspace::VmoError::NoMemory) => fail(Status::NoMemory),
                Err(_) => fail(Status::InvalidArgs),
            }
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
                    copy_from_user(aspace, mm, vmos, buf, len).map_err(|e| (e, None, None))?;
                vmos.write(mm, id, offset, &data)
                    .map_err(|_| (Status::NoMemory, None, None))?;
            } else {
                verify_writable(aspace, mm, vmos, buf, len).map_err(|e| (e, None, None))?;
                let mut data: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
                data.try_reserve_exact(len)
                    .map_err(|_| (Status::NoMemory, None, None))?;
                data.resize(len, 0);
                vmos.read(mm, id, offset, &mut data)
                    .map_err(|_| (Status::InvalidArgs, None, None))?;
                copy_to_user_verified(aspace, mm, buf, &data).map_err(|e| (e, None, None))?;
            }
            Ok((None, None))
        }
        _ => fail(Status::BadSyscall),
    })();
    Some(r)
}
