//! Copying between the kernel and user memory (SYSCALLS.md, "Copying from and to user
//! memory"; OD42). Every user pointer is *validated and walked*, never dereferenced:
//! the range must lie in the user half, each page must be mapped (a lazily backed page is
//! committed through the fault path first), user-accessible and permit the access; the
//! bytes then move through the direct map. Anything else is `INVALID_ARGS`, and a call
//! that fails changes no user memory.

use crate::aspace::{Access, AddressSpace, Vmos};
use crate::frame::PAGE_SIZE;
use crate::pagetable::PhysMem;
use crate::Mm;
use abi::status::Status;
use abi::user::{BASE, TOP};
use alloc::vec::Vec;

/// The physical address backing `va` for the given access, committing the page if it is
/// lazily backed and not yet mapped.
fn resolve<M: PhysMem>(
    aspace: &mut AddressSpace,
    mm: &mut Mm<M>,
    vmos: &mut Vmos,
    va: u64,
    access: Access,
) -> Result<u64, Status> {
    let page = va & !(PAGE_SIZE - 1);
    let mut t = aspace.table().translate(mm, page);
    if t.is_none() {
        aspace
            .handle_fault(mm, vmos, page, access)
            .map_err(|_| Status::InvalidArgs)?;
        t = aspace.table().translate(mm, page);
    }
    let t = t.ok_or(Status::InvalidArgs)?;
    let allowed = t.perms.user
        && match access {
            Access::Read => t.perms.read,
            Access::Write => t.perms.write,
            Access::Exec => t.perms.exec,
        };
    // device memory (an MMIO mapping) is reachable by user code only, never through the
    // kernel's direct map
    if !allowed || !mm.db.is_managed(t.pfn) {
        return Err(Status::InvalidArgs);
    }
    Ok((t.pfn << 12) | (va & (PAGE_SIZE - 1)))
}

fn check_range(va: u64, len: usize) -> Result<(), Status> {
    let end = va.checked_add(len as u64).ok_or(Status::InvalidArgs)?;
    if va < BASE || end > TOP {
        return Err(Status::InvalidArgs);
    }
    Ok(())
}

/// Read the 32-bit word at user address `va` with **one aligned 64-bit load** (so a store by another
/// hart's user thread is seen whole, never half: a bytewise copy could tear). `va` must be 4-aligned
/// and readable (`INVALID_ARGS`); a lazily backed page is committed first.
pub fn read_user_u32<M: PhysMem>(
    aspace: &mut AddressSpace,
    mm: &mut Mm<M>,
    vmos: &mut Vmos,
    va: u64,
) -> Result<u32, Status> {
    if va & 3 != 0 {
        return Err(Status::InvalidArgs);
    }
    check_range(va, 4)?;
    let pa = resolve(aspace, mm, vmos, va, Access::Read)?;
    let word = mm.mem.read_u64(pa & !7);
    Ok((word >> ((pa & 4) * 8)) as u32)
}

/// Read `len` bytes at user address `va`. A zero length succeeds without looking at `va`.
pub fn copy_from_user<M: PhysMem>(
    aspace: &mut AddressSpace,
    mm: &mut Mm<M>,
    vmos: &mut Vmos,
    va: u64,
    len: usize,
) -> Result<Vec<u8>, Status> {
    if len == 0 {
        return Ok(Vec::new());
    }
    check_range(va, len)?;
    let mut out: Vec<u8> = Vec::new();
    out.try_reserve_exact(len).map_err(|_| Status::NoMemory)?;
    out.resize(len, 0);
    let mut done = 0;
    while done < len {
        let at = va + done as u64;
        let n = ((PAGE_SIZE - (at & (PAGE_SIZE - 1))) as usize).min(len - done);
        let pa = resolve(aspace, mm, vmos, at, Access::Read)?;
        mm.mem.read_bytes(pa, &mut out[done..done + n]);
        done += n;
    }
    Ok(out)
}

/// Write `data` at user address `va`. Every page is checked before the first byte is
/// written, so a bad range leaves user memory untouched.
pub fn copy_to_user<M: PhysMem>(
    aspace: &mut AddressSpace,
    mm: &mut Mm<M>,
    vmos: &mut Vmos,
    va: u64,
    data: &[u8],
) -> Result<(), Status> {
    if data.is_empty() {
        return Ok(());
    }
    check_range(va, data.len())?;
    let mut pages: Vec<(u64, usize)> = Vec::new();
    pages
        .try_reserve(data.len() / PAGE_SIZE as usize + 2)
        .map_err(|_| Status::NoMemory)?;
    let mut done = 0;
    while done < data.len() {
        let at = va + done as u64;
        let n = ((PAGE_SIZE - (at & (PAGE_SIZE - 1))) as usize).min(data.len() - done);
        pages.push((resolve(aspace, mm, vmos, at, Access::Write)?, n));
        done += n;
    }
    let mut at = 0;
    for (pa, n) in pages {
        mm.mem.write_bytes(pa, &data[at..at + n]);
        at += n;
    }
    Ok(())
}

/// Check that `len` bytes at `va` could be written (every page mapped or committable, user
/// accessible and writable) without writing anything. A zero length succeeds without
/// looking at `va`. Used before a call that consumes something (a channel read) so a bad
/// destination cannot lose the data.
pub fn verify_writable<M: PhysMem>(
    aspace: &mut AddressSpace,
    mm: &mut Mm<M>,
    vmos: &mut Vmos,
    va: u64,
    len: usize,
) -> Result<(), Status> {
    if len == 0 {
        return Ok(());
    }
    check_range(va, len)?;
    let mut done = 0;
    while done < len {
        let at = va + done as u64;
        resolve(aspace, mm, vmos, at, Access::Write)?;
        done += ((PAGE_SIZE - (at & (PAGE_SIZE - 1))) as usize).min(len - done);
    }
    Ok(())
}

/// Write `data` at `va` after `verify_writable` has accepted the same range. It allocates nothing
/// and commits nothing: every page is already mapped, so it cannot fail for lack of memory, and
/// the data a call has already taken (a channel message) is never lost to a late failure. (It
/// still reports an error, rather than ignoring it, for the case that a page is no longer there;
/// with one hart that cannot happen, and with several it is the case the lock must exclude.)
pub fn copy_to_user_verified<M: PhysMem>(
    aspace: &mut AddressSpace,
    mm: &mut Mm<M>,
    va: u64,
    data: &[u8],
) -> Result<(), Status> {
    if data.is_empty() {
        return Ok(());
    }
    check_range(va, data.len())?;
    let mut done = 0;
    while done < data.len() {
        let at = va + done as u64;
        let n = ((PAGE_SIZE - (at & (PAGE_SIZE - 1))) as usize).min(data.len() - done);
        let t = aspace
            .table()
            .translate(mm, at & !(PAGE_SIZE - 1))
            .ok_or(Status::InvalidArgs)?;
        if !(t.perms.user && t.perms.write) || !mm.db.is_managed(t.pfn) {
            return Err(Status::InvalidArgs);
        }
        mm.mem.write_bytes(
            (t.pfn << 12) | (at & (PAGE_SIZE - 1)),
            &data[done..done + n],
        );
        done += n;
    }
    Ok(())
}
