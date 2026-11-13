//! User memory access (usercopy), process page tables that share the kernel half, and
//! VMO reads and writes.
mod common;
use abi::status::Status;
use abi::user::{BASE, STACK_PAGES, STACK_TOP, TOP};
use common::{FakeMem, Rng, SEEDS};
use kcore::aspace::{AddressSpace, VmoError, Vmos};
use kcore::frame::FrameDb;
use kcore::pagetable::{PageTable, Perms, PhysMem};
use kcore::usercopy::{copy_from_user, copy_to_user};
use kcore::Mm;

const KERNEL_VA: u64 = 0xffff_ffd0_0000_0000;
/// In the first and last kernel top-level slots (256 and 511), to show the whole upper half is shared.
const KERNEL_LOW_SLOT: u64 = 0xffff_ffc0_0000_1000;
const KERNEL_HIGH_SLOT: u64 = 0xffff_ffff_8020_0000;
const KERNEL_LAST_SLOT: u64 = 0xffff_ffff_c000_0000; // slot 511

struct World {
    mm: Mm<FakeMem>,
    vmos: Vmos,
    kernel: PageTable,
    proc: AddressSpace,
    baseline: usize,
}

/// A kernel table with one mapped kernel page, and a process table sharing its upper half.
fn world() -> World {
    let mut mm = Mm {
        db: FrameDb::new(3000, 400),
        mem: FakeMem::default(),
    };
    let baseline = mm.db.free_count();
    let mut kernel = PageTable::new(&mut mm).unwrap();
    kernel.map(&mut mm, KERNEL_VA, 3399, Perms::RW).unwrap(); // a kernel-only page
    kernel
        .map(&mut mm, KERNEL_LOW_SLOT, 3398, Perms::RW)
        .unwrap();
    kernel
        .map(&mut mm, KERNEL_HIGH_SLOT, 3397, Perms::RX)
        .unwrap();
    kernel
        .map(&mut mm, KERNEL_LAST_SLOT, 3396, Perms::R)
        .unwrap();
    let proc = AddressSpace::new(PageTable::new_with_kernel_half(&mut mm, &kernel).unwrap());
    World {
        mm,
        vmos: Vmos::new(),
        kernel,
        proc,
        baseline,
    }
}

fn region(w: &mut World, start: u64, pages: u64, perms: Perms) -> u64 {
    let id = w.vmos.create(pages);
    w.proc
        .map_region(&w.vmos, start, pages, id, 0, perms)
        .unwrap();
    id
}

impl World {
    fn read(&mut self, va: u64, len: usize) -> Result<Vec<u8>, Status> {
        copy_from_user(&mut self.proc, &mut self.mm, &mut self.vmos, va, len)
    }
    fn write(&mut self, va: u64, data: &[u8]) -> Result<(), Status> {
        copy_to_user(&mut self.proc, &mut self.mm, &mut self.vmos, va, data)
    }
    fn teardown(mut self) {
        let starts: Vec<u64> = vec![0x1_0000, 0x2_0000, 0x3_0000, STACK_TOP - STACK_PAGES * 4096];
        for s in starts {
            let _ = self.proc.unmap_region(&mut self.mm, s);
        }
        let ids: Vec<u64> = (1..=8).collect();
        for id in ids {
            let _ = self.vmos.destroy(&mut self.mm, id);
        }
        self.proc.destroy_user(&mut self.mm);
        self.kernel.destroy(&mut self.mm);
        assert_eq!(
            self.mm.db.free_count(),
            self.baseline,
            "everything came back"
        );
    }
}

#[test]
fn round_trip_across_pages_with_lazy_commit() {
    let mut w = world();
    region(&mut w, 0x1_0000, 4, Perms::RW.user());
    let data: Vec<u8> = (0..9000u32).map(|i| (i % 251) as u8).collect();
    assert_eq!(
        w.write(0x1_0000 + 100, &data),
        Ok(()),
        "spans three pages, none committed yet"
    );
    assert_eq!(w.read(0x1_0000 + 100, 9000).unwrap(), data);
    assert_eq!(
        w.read(0x1_0000, 100).unwrap(),
        vec![0; 100],
        "untouched bytes of a committed page are zero"
    );
    assert_eq!(
        w.read(0x1_0000 + 3 * 4096, 4096).unwrap(),
        vec![0; 4096],
        "an untouched page reads as zero"
    );
    w.teardown();
}

#[test]
fn every_bad_pointer_is_invalid_args_and_changes_nothing() {
    let mut w = world();
    region(&mut w, 0x1_0000, 2, Perms::RW.user());
    region(&mut w, 0x2_0000, 1, Perms::RX.user());
    region(&mut w, 0x3_0000, 1, Perms::R); // mapped but not for users
    fn is_bad<T: std::fmt::Debug>(r: Result<T, Status>) {
        assert_eq!(r.err(), Some(Status::InvalidArgs));
    }
    is_bad(w.read(0, 1));
    is_bad(w.read(BASE - 1, 1));
    is_bad(w.read(KERNEL_VA, 1));
    is_bad(w.write(KERNEL_VA, &[1]));
    is_bad(w.read(0xffff_ffff_ffff_ffff, 1));
    is_bad(w.read(TOP - 1, 2));
    is_bad(w.read(u64::MAX - 3, 8));
    is_bad(w.read(0x9_0000, 1));
    is_bad(w.read(0x1_0000 + 2 * 4096 - 1, 2));
    is_bad(w.write(0x2_0000, &[1]));
    assert_eq!(w.read(0x2_0000, 4), Ok(vec![0; 4]), "but it can be read");
    is_bad(w.read(0x3_0000, 1));
    // no partial write: the second page of the destination is refused, so the first byte is untouched
    w.write(0x1_0000 + 4096 + 4095, &[7]).unwrap();
    let before = w.read(0x1_0000 + 4090, 6).unwrap();
    is_bad(w.write(0x1_0000 + 2 * 4096 - 2, &[1, 2, 3, 4]));
    assert_eq!(
        w.read(0x1_0000 + 4090, 6).unwrap(),
        before,
        "a refused write changed nothing"
    );
    // zero length never looks at the pointer
    assert_eq!(w.read(0, 0), Ok(vec![]));
    assert_eq!(w.read(KERNEL_VA, 0), Ok(vec![]));
    assert_eq!(w.write(0xdead_beef, &[]), Ok(()));
    w.teardown();
}

#[test]
fn a_process_table_shares_the_kernel_half_and_frees_only_its_own() {
    let mut w = world();
    // the kernel mapping is visible through the process's own table (shared top-level entry)...
    let t = w.proc.table().translate(&w.mm, KERNEL_VA).unwrap();
    assert_eq!((t.pfn, t.perms.user), (3399, false));
    assert_eq!(
        w.proc
            .table()
            .translate(&w.mm, KERNEL_LOW_SLOT)
            .unwrap()
            .pfn,
        3398,
        "top-level slot 256"
    );
    assert_eq!(
        w.proc
            .table()
            .translate(&w.mm, KERNEL_HIGH_SLOT)
            .unwrap()
            .pfn,
        3397,
        "top-level slot 510"
    );
    assert_eq!(
        w.proc
            .table()
            .translate(&w.mm, KERNEL_LAST_SLOT)
            .unwrap()
            .pfn,
        3396,
        "top-level slot 511"
    );
    let l1 = PhysMem::read_u64(&w.mm.mem, (w.kernel.root() << 12) + 320 * 8) >> 10;
    region(&mut w, 0x1_0000, 1, Perms::RW.user());
    w.write(0x1_0000, &[1]).unwrap();
    // ...and tearing the process down leaves the kernel's tables alone
    w.proc.unmap_region(&mut w.mm, 0x1_0000).unwrap();
    w.vmos.destroy(&mut w.mm, 1).unwrap();
    w.proc.destroy_user(&mut w.mm);
    assert_eq!(
        w.kernel.translate(&w.mm, KERNEL_VA).unwrap().pfn,
        3399,
        "the kernel table is intact"
    );
    assert_eq!(
        w.mm.db.refs(l1),
        Some(1),
        "the shared kernel table frames were not freed by the process teardown"
    );
    w.kernel.destroy(&mut w.mm);
    assert_eq!(w.mm.db.free_count(), w.baseline);
}

#[test]
fn a_stack_region_commits_lazily_and_a_guard_below_it_is_refused() {
    let mut w = world();
    let bottom = STACK_TOP - STACK_PAGES * 4096;
    region(&mut w, bottom, STACK_PAGES, Perms::RW.user());
    assert_eq!(
        w.write(STACK_TOP - 8, &[1; 8]),
        Ok(()),
        "the top word of the stack"
    );
    assert_eq!(w.write(bottom, &[1]), Ok(()), "the lowest stack page");
    assert_eq!(
        w.read(bottom - 1, 1),
        Err(Status::InvalidArgs),
        "the guard page below is not mapped"
    );
    assert_eq!(
        w.read(STACK_TOP, 1),
        Err(Status::InvalidArgs),
        "nor is anything above the stack"
    );
    w.teardown();
}

#[test]
fn vmo_write_and_read_back_with_bounds() {
    let mut w = world();
    let id = w.vmos.create(3);
    let data: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
    w.vmos.write(&mut w.mm, id, 4000, &data).unwrap(); // crosses a page boundary
    let mut out = vec![0u8; 5000];
    w.vmos.read(&mut w.mm, id, 4000, &mut out).unwrap();
    assert_eq!(out, data);
    let mut head = [9u8; 16];
    w.vmos.read(&mut w.mm, id, 0, &mut head).unwrap();
    assert_eq!(head, [0; 16], "bytes before the written range are zero");
    assert_eq!(
        w.vmos.write(&mut w.mm, id, 3 * 4096 - 1, &[1, 2]),
        Err(VmoError::OutOfRange)
    );
    assert_eq!(
        w.vmos.write(&mut w.mm, id, u64::MAX, &[1]),
        Err(VmoError::OutOfRange),
        "offset overflow"
    );
    assert_eq!(w.vmos.write(&mut w.mm, 99, 0, &[1]), Err(VmoError::Gone));
    let mut empty = [0u8; 0];
    assert_eq!(
        w.vmos.read(&mut w.mm, id, 3 * 4096, &mut empty),
        Ok(()),
        "an empty read at the end is fine"
    );
    // reading an uncommitted page does not commit it
    let fresh = w.vmos.create(2);
    let free = w.mm.db.free_count();
    let mut page = vec![1u8; 4096];
    w.vmos.read(&mut w.mm, fresh, 0, &mut page).unwrap();
    assert_eq!(page, vec![0; 4096]);
    assert_eq!(
        w.mm.db.free_count(),
        free,
        "a read of uncommitted memory allocates nothing"
    );
    w.vmos.destroy(&mut w.mm, id).unwrap();
    w.vmos.destroy(&mut w.mm, fresh).unwrap();
    w.proc.destroy_user(&mut w.mm);
    w.kernel.destroy(&mut w.mm);
    assert_eq!(w.mm.db.free_count(), w.baseline);
}

#[test]
fn byte_access_through_phys_mem_matches_a_plain_array() {
    // the default `read_bytes`/`write_bytes` at every alignment, against a Vec<u8> model
    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed);
        let mut mem = FakeMem::default();
        let mut model = vec![0u8; 3 * 4096];
        for i in 0..3 * 512u64 {
            mem.write_u64(0x10_0000 + i * 8, 0);
        }
        for _ in 0..30 {
            let (off, len) = (
                rng.below(3 * 4096 - 64) as usize,
                1 + rng.below(60) as usize,
            );
            if rng.chance(60) {
                let data: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
                mem.write_bytes(0x10_0000 + off as u64, &data);
                model[off..off + len].copy_from_slice(&data);
            } else {
                let mut got = vec![0u8; len];
                mem.read_bytes(0x10_0000 + off as u64, &mut got);
                assert_eq!(got, model[off..off + len], "seed {seed}: bytes at {off}");
            }
        }
    }
}

#[test]
fn a_region_outside_the_user_half_is_still_refused_by_the_copy_checks() {
    // A (buggy or hostile) mapping with the user bit below page 1 or in the upper half
    // must not make kernel memory reachable by a user pointer.
    let mut w = world();
    region(&mut w, 0, 1, Perms::RW.user()); // page zero
    region(&mut w, 0xffff_ffe0_0000_0000, 1, Perms::RW.user()); // upper half, user bit set
    assert_eq!(
        w.read(0, 4).err(),
        Some(Status::InvalidArgs),
        "page zero, even with a region over it"
    );
    assert_eq!(
        w.read(0xffff_ffe0_0000_0000, 4).err(),
        Some(Status::InvalidArgs),
        "upper half, even with a user mapping"
    );
    assert_eq!(
        w.write(0xffff_ffe0_0000_0000, &[1]).err(),
        Some(Status::InvalidArgs)
    );
    let _ = w.proc.unmap_region(&mut w.mm, 0);
    let _ = w.proc.unmap_region(&mut w.mm, 0xffff_ffe0_0000_0000);
    let ids: Vec<u64> = vec![1, 2];
    for id in ids {
        let _ = w.vmos.destroy(&mut w.mm, id);
    }
    w.proc.destroy_user(&mut w.mm);
    w.kernel.destroy(&mut w.mm);
    assert_eq!(w.mm.db.free_count(), w.baseline);
}

#[test]
fn region_perms_at_finds_the_covering_region() {
    let mut w = world();
    region(&mut w, 0x1_0000, 2, Perms::RX.user());
    region(&mut w, 0x4_0000, 1, Perms::RW.user());
    assert_eq!(w.proc.region_perms_at(0x1_0000), Some(Perms::RX.user()));
    assert_eq!(
        w.proc.region_perms_at(0x1_1fff),
        Some(Perms::RX.user()),
        "the last byte of a region"
    );
    assert_eq!(w.proc.region_perms_at(0x1_2000), None, "one past the end");
    assert_eq!(w.proc.region_perms_at(0xffff), None, "just below");
    assert_eq!(w.proc.region_perms_at(0x4_0800), Some(Perms::RW.user()));
    assert_eq!(w.proc.region_perms_at(0), None);
    w.teardown();
}

#[test]
fn a_verified_copy_needs_no_allocation_and_refuses_pages_that_are_not_there() {
    use kcore::usercopy::{copy_to_user_verified, verify_writable};
    let mut w = world();
    region(&mut w, 0x1_0000, 4, Perms::RW.user());
    region(&mut w, 0x3_0000, 1, Perms::R.user());
    let data: Vec<u8> = (0..6000u32).map(|i| (i % 253) as u8).collect();
    // before verification nothing is committed, so the bare verified copy refuses
    assert_eq!(
        copy_to_user_verified(&mut w.proc, &mut w.mm, 0x1_0000 + 50, &data),
        Err(Status::InvalidArgs),
        "an uncommitted page is not there yet"
    );
    // verification commits and maps the range; then the copy takes no frames at all
    verify_writable(
        &mut w.proc,
        &mut w.mm,
        &mut w.vmos,
        0x1_0000 + 50,
        data.len(),
    )
    .unwrap();
    let free = w.mm.db.free_count();
    assert_eq!(
        copy_to_user_verified(&mut w.proc, &mut w.mm, 0x1_0000 + 50, &data),
        Ok(())
    );
    assert_eq!(w.mm.db.free_count(), free, "the copy allocated nothing");
    assert_eq!(w.read(0x1_0000 + 50, data.len()).unwrap(), data);
    // a read-only mapping is refused even when it is mapped
    let _ = w.read(0x3_0000, 1).unwrap();
    assert_eq!(
        copy_to_user_verified(&mut w.proc, &mut w.mm, 0x3_0000, &[1]),
        Err(Status::InvalidArgs)
    );
    assert_eq!(
        copy_to_user_verified(&mut w.proc, &mut w.mm, 0x10, &[1]),
        Err(Status::InvalidArgs),
        "below the user base"
    );
    w.teardown();
}

#[test]
fn read_user_u32_reads_both_halves_of_a_word_whole() {
    use kcore::usercopy::read_user_u32;
    let mut w = world();
    region(&mut w, 0x1_0000, 2, Perms::RW.user());
    // neighbouring words with different values: a read of the wrong half or the wrong word shows
    w.write(0x1_0000, &0x1111_1111u32.to_le_bytes()).unwrap();
    w.write(0x1_0004, &0x2222_2222u32.to_le_bytes()).unwrap();
    w.write(0x1_0008, &0x3333_3333u32.to_le_bytes()).unwrap();
    w.write(0x1_000c, &0x4444_4444u32.to_le_bytes()).unwrap();
    let rd = |w: &mut World, va| read_user_u32(&mut w.proc, &mut w.mm, &mut w.vmos, va);
    assert_eq!(
        rd(&mut w, 0x1_0000),
        Ok(0x1111_1111),
        "low half of a doubleword"
    );
    assert_eq!(
        rd(&mut w, 0x1_0004),
        Ok(0x2222_2222),
        "high half of a doubleword"
    );
    assert_eq!(rd(&mut w, 0x1_0008), Ok(0x3333_3333));
    assert_eq!(rd(&mut w, 0x1_000c), Ok(0x4444_4444));
    // the last word of a page and the first of the next
    w.write(0x1_0ffc, &0xaaaa_bbbbu32.to_le_bytes()).unwrap();
    w.write(0x1_1000, &0xccccddddu32.to_le_bytes()).unwrap();
    assert_eq!(rd(&mut w, 0x1_0ffc), Ok(0xaaaa_bbbb));
    assert_eq!(rd(&mut w, 0x1_1000), Ok(0xcccc_dddd));
    // a lazy page reads as zero and is committed by the read
    region(&mut w, 0x2_0000, 1, Perms::RW.user());
    assert_eq!(rd(&mut w, 0x2_0004), Ok(0));
    // misaligned, outside user memory, unmapped, and a page that cannot be read
    for bad in [
        0x1_0001u64,
        0x1_0002,
        0x1_0003,
        0x10,
        KERNEL_VA,
        TOP,
        0x9_0000,
    ] {
        assert_eq!(rd(&mut w, bad), Err(Status::InvalidArgs), "{bad:#x}");
    }
    let no_read = Perms {
        read: false,
        write: false,
        exec: true,
        user: true,
    };
    region(&mut w, 0x3_0000, 1, no_read);
    assert_eq!(
        rd(&mut w, 0x3_0000),
        Err(Status::InvalidArgs),
        "an unreadable page"
    );
    w.teardown();
}
