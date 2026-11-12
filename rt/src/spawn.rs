//! Building and starting a process from an ELF image in userland (S4-SPEC sections 5, 6, 10):
//! the kernel never parses an executable for this. It is the core of the launcher service and
//! of `init`, which has to start the first services without one.

extern crate alloc;

use crate::sys;
use abi::rights::Rights;
use abi::user::STACK_TOP;
use alloc::vec::Vec;
use kcore::elf;

/// Largest image, segments and total mapped memory a spawn accepts (S4-SPEC section 6).
pub const MAX_IMAGE: usize = 4 * 1024 * 1024;
pub const MAX_SEGMENTS: usize = 16;
pub const MAX_MAPPED: u64 = 16 * 1024 * 1024;
/// The stack every spawned process gets: 256 KiB below `STACK_TOP`, committed lazily.
pub const STACK_PAGES: u64 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnError {
    /// Not a loadable static ELF (or over the limits).
    BadElf,
    TooBig,
    /// A syscall failed (the kernel status).
    Sys(i32),
}

const PF_R: u64 = 1;
const PF_W: u64 = 2;
const PF_X: u64 = 4;

fn sys_err(status: i32) -> SpawnError {
    SpawnError::Sys(status)
}

/// The rights a launcher gives its caller on a started process: everything but `MANAGE`
/// (the caller cannot map into the child or add threads to it).
pub fn caller_rights() -> u32 {
    Rights::WAIT.0 | Rights::INSPECT.0 | Rights::KILL.0 | Rights::DUPLICATE.0 | Rights::TRANSFER.0
}

/// Create a process from `image`, send `startup` with `handles` (they move into the child's
/// bootstrap message), and start it. Returns the process handle narrowed to
/// [`caller_rights`]. **The handles are consumed in every case**: they move into the child's
/// message, or (on failure before that) are closed here. On failure everything created here is
/// released.
pub fn spawn(image: &[u8], startup: &[u8], handles: &[u32]) -> Result<u32, SpawnError> {
    spawn_in(image, startup, handles, 0)
}

/// Like [`spawn`], with the process and all the memory built for it charged to `job` (a job
/// handle with MANAGE; 0 means the caller's own job). The new process lives in `job`, so its
/// limits are the program's limits.
pub fn spawn_in(
    image: &[u8],
    startup: &[u8],
    handles: &[u32],
    job: u32,
) -> Result<u32, SpawnError> {
    let early = |handles: &[u32], e: SpawnError| {
        for &h in handles {
            sys::handle_close(h);
        }
        Err(e)
    };
    if image.len() > MAX_IMAGE {
        return early(handles, SpawnError::TooBig);
    }
    let Ok(parsed) = elf::parse(image) else {
        return early(handles, SpawnError::BadElf);
    };
    if parsed.segments.len() > MAX_SEGMENTS {
        return early(handles, SpawnError::BadElf);
    }
    let mut total = STACK_PAGES * 4096;
    for seg in &parsed.segments {
        let (first, end) = seg.pages();
        total += (end - first) * 4096;
    }
    if total > MAX_MAPPED {
        return early(handles, SpawnError::TooBig);
    }
    let p = sys::process_create_in(job);
    if p.status != 0 {
        return early(handles, sys_err(p.status));
    }
    let proc = p.a1 as u32;
    let mut moved = false;
    let result = build_and_start(proc, &parsed, image, startup, handles, job, &mut moved);
    match result {
        Ok(()) => {
            // Narrow the handle we hand back: replace, then the narrowed handle is the only one.
            let r = sys::handle_replace(proc, caller_rights());
            if r.status != 0 {
                // The child is already running: end it rather than leave it with no owner.
                sys::process_kill(proc, -1);
                sys::handle_close(proc);
                return Err(sys_err(r.status));
            }
            Ok(r.a1 as u32)
        }
        Err(e) => {
            // Closing the last handle to a process that never ran frees it with everything
            // mapped into it.
            sys::handle_close(proc);
            if !moved {
                for &h in handles {
                    sys::handle_close(h);
                }
            }
            Err(e)
        }
    }
}

fn build_and_start(
    proc: u32,
    parsed: &elf::Image,
    image: &[u8],
    startup: &[u8],
    handles: &[u32],
    job: u32,
    moved: &mut bool,
) -> Result<(), SpawnError> {
    for seg in &parsed.segments {
        let (first, end) = seg.pages();
        let pages = end - first;
        let v = sys::vmo_create_in(pages, job);
        if v.status != 0 {
            return Err(sys_err(v.status));
        }
        let vmo = v.a1 as u32;
        let lead = seg.vaddr % 4096;
        let Some(bytes) = image.get(seg.offset as usize..(seg.offset + seg.filesz) as usize) else {
            sys::handle_close(vmo);
            return Err(SpawnError::BadElf);
        };
        let mut done = 0usize;
        while done < bytes.len() {
            let n = (bytes.len() - done).min(32 * 1024);
            let w = sys::vmo_write(vmo, lead + done as u64, bytes[done..].as_ptr(), n);
            if w.status != 0 {
                sys::handle_close(vmo);
                return Err(sys_err(w.status));
            }
            done += n;
        }
        let mut flags = PF_R;
        if seg.perms.write {
            flags |= PF_W;
        }
        if seg.perms.exec {
            flags |= PF_X;
        }
        let m = sys::process_map(proc, vmo, first * 4096, 0, pages * 4096, flags);
        sys::handle_close(vmo); // the mapping keeps the memory alive
        if m.status != 0 {
            return Err(sys_err(m.status));
        }
    }
    let s = sys::vmo_create_in(STACK_PAGES, job);
    if s.status != 0 {
        return Err(sys_err(s.status));
    }
    let stack = s.a1 as u32;
    let m = sys::process_map(
        proc,
        stack,
        STACK_TOP - STACK_PAGES * 4096,
        0,
        STACK_PAGES * 4096,
        PF_R | PF_W,
    );
    sys::handle_close(stack);
    if m.status != 0 {
        return Err(sys_err(m.status));
    }
    let c = sys::channel_create();
    if c.status != 0 {
        return Err(sys_err(c.status));
    }
    let (mine, theirs) = (c.a1 as u32, c.a2 as u32);
    // The startup message goes in before the child runs; then our end is closed (the message
    // stays readable, and the child cannot park budget on a channel we still hold).
    let w = sys::channel_write(
        mine,
        startup.as_ptr(),
        startup.len(),
        handles.as_ptr(),
        handles.len(),
    );
    sys::handle_close(mine);
    *moved = w.status == 0;
    if w.status != 0 {
        sys::handle_close(theirs);
        return Err(sys_err(w.status));
    }
    let started = sys::process_start(proc, parsed.entry, STACK_TOP, theirs, 0);
    if started.status != 0 {
        sys::handle_close(theirs);
        return Err(sys_err(started.status));
    }
    Ok(())
}

/// Read `len` bytes at `offset` from a VMO into memory (for an image). `None` if the extent is
/// outside the VMO or too large.
pub fn read_image(vmo: u32, offset: u64, len: u64) -> Option<Vec<u8>> {
    if len == 0 || len > MAX_IMAGE as u64 {
        return None;
    }
    let size = sys::vmo_get_size(vmo);
    if size.status != 0 || offset.checked_add(len)? > size.a1 as u64 {
        return None;
    }
    let mut img = Vec::new();
    img.try_reserve_exact(len as usize).ok()?;
    img.resize(len as usize, 0);
    let mut at = 0usize;
    while at < img.len() {
        let n = (img.len() - at).min(32 * 1024);
        let r = sys::vmo_read(vmo, offset + at as u64, img[at..].as_mut_ptr(), n);
        if r.status != 0 {
            return None;
        }
        at += n;
    }
    Some(img)
}
