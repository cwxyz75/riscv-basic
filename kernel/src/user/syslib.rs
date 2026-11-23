//! The system library in the kernel (AB7, KERNEL-V1-SPEC 3.1): `libsys.so`, built by `build.rs` and embedded, is made into two shared VMOs
//! once (its text, and its data page, which the kernel fills), and mapped into every process at a random place when the process is started.
//! The first thread gets the base in `a2`; the process records the base and the text range.

use super::*;
use libsys::page::{self, Page};

static IMAGE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/libsys.so"));

/// One loadable segment of the library, as a VMO the kernel keeps.
struct Seg {
    vmo: u64,
    /// The first page, relative to the library's base.
    first: u64,
    pages: u64,
    perms: Perms,
}

pub(super) struct Lib {
    segs: Vec<Seg>,
    /// The span of address space one mapping needs.
    span: u64,
    text_end: u64,
}

// (guarded by the big kernel lock, like the user state)
static mut LIB: Option<Lib> = None;

fn slot() -> &'static mut Option<Lib> {
    debug_assert!(
        crate::bkl::held(),
        "the library state needs the big kernel lock"
    );
    // SAFETY: the big kernel lock is held (debug-checked); every caller drops the reference before a context switch.
    unsafe { &mut *addr_of_mut!(LIB) }
}

/// What the kernel puts in the data page.
fn page_contents() -> Page {
    let (mult, shift, base) = crate::timer::clock_params();
    Page {
        abi_version: page::ABI,
        time_mult: mult,
        time_shift: shift,
        page_size: 4096,
        // RVA23 requires both extensions; the block sizes of the machines this runs on (QEMU `virt`) are 64 bytes
        cbom_block: 64,
        cboz_block: 64,
        harts_online: crate::smp::online() as u64,
        flags: 0,
        time_base: base,
    }
}

/// Make the library's VMOs, once. Panics at boot-time on a library the loader cannot read: `build.rs` checked it.
#[allow(
    clippy::expect_used,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::large_stack_frames
)]
// INVARIANT: the object was built and checked by `build.rs` (a failure here means the kernel image is broken); `parse` checked every segment's offsets and sizes against the file and its
// addresses against the user limit; the frame is the data page's 72-byte record and the loop's temporaries, once at the first process start
pub(super) fn ensure(k: &mut kmem::Kmem) {
    if slot().is_some() {
        return;
    }
    let parsed = elf::parse(IMAGE).expect("libsys.so is a valid ELF object");
    let mut segs = Vec::new();
    let mut text_end = 0;
    for seg in &parsed.segments {
        let (first, end) = seg.pages();
        let id = k.vmos.create(k.jobs, end - first);
        if seg.perms.exec {
            let bytes = &IMAGE[seg.offset as usize..(seg.offset + seg.filesz) as usize];
            k.vmos
                .write(k.mm, k.jobs, id, seg.vaddr % 4096, bytes)
                .expect("memory for the library's text");
            text_end = seg.vaddr + seg.filesz;
        } else {
            // the data page: written now, so that its frame exists before any process maps it
            k.vmos
                .write(k.mm, k.jobs, id, 0, &page_contents().to_bytes())
                .expect("memory for the library's data page");
        }
        segs.push(Seg {
            vmo: id,
            first,
            pages: end - first,
            perms: seg.perms,
        });
    }
    *slot() = Some(Lib {
        segs,
        span: parsed.span,
        text_end,
    });
}

/// Map the library into `aspace` at a random place: the text read-only and executable, the data page read-only. `Err(NoMemory)` when no gap is left or
/// the address space has too many regions (nothing is left mapped then).
#[allow(clippy::arithmetic_side_effects)]
// INVARIANT: `base` was placed with room for `lib.span`, which covers every segment (`parse` computed it from them), so the sums stay inside the arena and the user half
pub(super) fn map_into(
    mm: &mut kcore::Mm<kmem::DirectMap>,
    vmos: &mut kcore::aspace::Vmos,
    jobs: &mut kcore::job::Jobs,
    aspace: &mut AddressSpace,
) -> Result<(ksys::LibRange, u64), Status> {
    let lib = slot().as_ref().ok_or(Status::BadState)?;
    let base = ksys::place_anywhere(
        &mut |n| crate::entropy::placement_below(n),
        aspace,
        lib.span,
    )
    .ok_or(Status::NoMemory)?;
    for (i, s) in lib.segs.iter().enumerate() {
        if aspace
            .map_region(vmos, base + s.first * 4096, s.pages, s.vmo, 0, s.perms)
            .is_err()
        {
            // undo what is mapped: the regions start at known places
            for done in lib.segs.iter().take(i) {
                let _ = aspace.unmap_region(mm, vmos, jobs, base + done.first * 4096);
            }
            return Err(Status::NoMemory);
        }
    }
    Ok((
        ksys::LibRange {
            base,
            text_start: base,
            text_end: base + lib.text_end,
        },
        lib.span,
    ))
}

/// The mapper `process_start` calls (`ksys::StartMap`).
pub(super) struct KernelLib<'a> {
    pub mm: &'a mut kcore::Mm<kmem::DirectMap>,
    pub vmos: &'a mut kcore::aspace::Vmos,
}

impl ksys::StartMap for KernelLib<'_> {
    fn map(
        &mut self,
        aspace: &mut AddressSpace,
        jobs: &mut kcore::job::Jobs,
    ) -> Result<(ksys::LibRange, u64), Status> {
        map_into(self.mm, self.vmos, jobs, aspace)
    }
    fn unmap(
        &mut self,
        aspace: &mut AddressSpace,
        jobs: &mut kcore::job::Jobs,
        range: ksys::LibRange,
        span: u64,
    ) {
        let _ = aspace.unmap_range(self.mm, self.vmos, jobs, range.base, span);
    }
}
