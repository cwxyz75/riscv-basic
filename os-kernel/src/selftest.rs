//! The kernel's self-tests (S1: memory). Each reports as a `##TEST` line. Tests
//! that touch lazily committed memory run inside `balanced`, which also checks the
//! frame count returns to what it was: "map/unmap/fault sequences return
//! allocation counts to baseline" (ROADMAP S1).

use crate::{kmem, trap};
use abi::layout::{DIRECT_BASE, KERNEL_OFFSET, LAZY_TEST_BASE};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};
use kcore::aspace::FaultError;
use kcore::pagetable::Perms;

type Outcome = Result<(), String>;

fn ensure(condition: bool, what: &str) -> Outcome {
    if condition {
        Ok(())
    } else {
        Err(String::from(what))
    }
}

fn ensure_eq<T: PartialEq + core::fmt::Debug>(got: T, want: T, what: &str) -> Outcome {
    if got == want {
        Ok(())
    } else {
        Err(format!("{what}: got {got:?}, want {want:?}"))
    }
}

/// Run `body`, then require that no frame leaked.
fn balanced(body: impl FnOnce() -> Outcome) -> Outcome {
    let before = kmem::get().mm.db.free_count();
    body()?;
    let after = kmem::get().mm.db.free_count();
    ensure_eq(
        after,
        before,
        "free frames after the test (leak if smaller)",
    )?;
    kmem::get()
        .mm
        .db
        .audit()
        .map_err(|e| format!("frame database audit: {e}"))
}

const PAGE: u64 = 4096;
const MIB2: u64 = 2 * 1024 * 1024;

fn read(va: u64) -> u64 {
    // SAFETY: deliberately touches a kernel address that may be lazily committed;
    // a missing page faults and the trap handler either maps it or (in the
    // `expect_refused` tests) records the refusal and steps over this load.
    unsafe { read_volatile(va as *const u64) }
}

fn write(va: u64, value: u64) {
    // SAFETY: as for `read`.
    unsafe { write_volatile(va as *mut u64, value) }
}

fn paging_enabled() -> Outcome {
    ensure_eq(kmem::satp() >> 60, 8, "satp mode (8 = Sv39)")
}

fn runs_in_the_upper_half() -> Outcome {
    let pc = runs_in_the_upper_half as *const () as u64;
    let marker = 0u8;
    let sp = core::ptr::addr_of!(marker) as u64;
    ensure(
        (pc as i64) >> 38 == -1,
        "code address is in the Sv39 upper half",
    )?;
    ensure(
        (sp as i64) >> 38 == -1,
        "stack address is in the Sv39 upper half",
    )?;
    ensure(
        pc >= abi::layout::KERNEL_PHYS + KERNEL_OFFSET,
        "code is at the kernel's linked address",
    )
}

fn low_half_is_empty() -> Outcome {
    let k = kmem::get();
    for va in [
        0x1000,
        0x10_0000,
        0x1000_0000,
        0x8000_0000,
        0x8020_0000,
        0x8800_0000,
    ] {
        ensure(
            k.aspace.table().translate(&k.mm, va).is_none(),
            &format!("{va:#x} must not be mapped (the boot identity map is gone)"),
        )?;
    }
    Ok(())
}

fn kernel_mappings_are_w_xor_x() -> Outcome {
    let k = kmem::get();
    let (mut leaves, mut bad) = (0, 0);
    k.aspace.table().for_each_leaf(&k.mm, &mut |_va, t, level| {
        leaves += 1;
        if (t.perms.write && t.perms.exec) || level != 0 || t.perms.user {
            bad += 1;
        }
    });
    ensure(leaves > 100, "the kernel table has leaves")?;
    ensure_eq(bad, 0, "leaves that are W+X, huge or user-accessible")
}

fn kernel_sections_have_the_right_permissions() -> Outcome {
    let k = kmem::get();
    let s = kmem::sections();
    // Every page of every section, not just the first.
    for (name, (start, end), want) in [
        ("text", s.text, Perms::RX),
        ("rodata", s.rodata, Perms::R),
        ("data", s.data, Perms::RW),
    ] {
        let mut va = start;
        while va < end {
            let t = k
                .aspace
                .table()
                .translate(&k.mm, va)
                .ok_or_else(|| format!("{name} page {va:#x} is unmapped"))?;
            ensure_eq(t.perms, want, &format!("{name} page {va:#x}"))?;
            ensure_eq(
                t.pfn,
                (va - KERNEL_OFFSET) >> 12,
                &format!("{name} page {va:#x} maps to its physical load address"),
            )?;
            va += PAGE;
        }
    }
    // Executable pages exist only in the text section.
    let mut stray = 0;
    k.aspace.table().for_each_leaf(&k.mm, &mut |va, t, _| {
        if t.perms.exec && !(s.text.0..s.text.1).contains(&va) {
            stray += 1;
        }
    });
    ensure_eq(stray, 0, "executable pages outside text")
}

/// Every direct-map leaf maps a frame the frame database manages (never the
/// kernel image, whose frames sit below the first managed frame), or is one of
/// the two device pages.
fn direct_map_does_not_alias_the_image() -> Outcome {
    let k = kmem::get();
    let first = kmem::first_managed_frame();
    let image_pa = abi::layout::KERNEL_PHYS;
    ensure(
        k.aspace
            .table()
            .translate(&k.mm, DIRECT_BASE + image_pa)
            .is_none(),
        "the kernel image must not be reachable through the direct map",
    )?;
    let (mut in_map, mut aliases, mut devices) = (0, 0, 0);
    k.aspace.table().for_each_leaf(&k.mm, &mut |va, t, _| {
        if (DIRECT_BASE..DIRECT_BASE + (1 << 36)).contains(&va) {
            in_map += 1;
            if t.pfn >= abi::layout::RAM_BASE >> 12 && t.pfn < first {
                aliases += 1; // a RAM frame below the first managed one: the image or firmware
            }
            if t.pfn < abi::layout::RAM_BASE >> 12 {
                devices += 1;
            }
            if va - DIRECT_BASE != t.pfn << 12 {
                aliases += 1; // not the direct map's own formula
            }
        }
    });
    ensure(in_map > 1000, "the direct map has leaves")?;
    ensure_eq(devices, 2, "device pages in the direct map")?;
    ensure_eq(
        aliases,
        0,
        "direct-map leaves aliasing the image, firmware or a wrong address",
    )
}

fn heap_works() -> Outcome {
    let mut v: Vec<u64> = Vec::new();
    for i in 0..10_000 {
        v.push(i * 3);
    }
    ensure_eq(v.iter().sum::<u64>(), 3 * (9_999 * 10_000 / 2), "sum")?;
    let b = Box::new([7u8; 4096]);
    ensure_eq(
        b.iter().map(|&x| x as u32).sum::<u32>(),
        7 * 4096,
        "boxed array",
    )?;
    drop(v);
    drop(b);
    let strings: Vec<String> = (0..200).map(|i| format!("entry {i}")).collect();
    ensure_eq(strings[199].as_str(), "entry 199", "strings")
}

fn frame_database_is_sane() -> Outcome {
    let db = &kmem::get().mm.db;
    ensure(db.free_count() > 20_000, "plenty of free frames at boot")?;
    db.audit().map_err(|e| format!("audit: {e}"))
}

/// Create a VMO of `pages` pages and one region over all of it at `va`.
fn region(va: u64, pages: u64, perms: Perms) -> u64 {
    let k = kmem::get();
    let id = k.vmos.create(pages);
    k.aspace
        .map_region(&k.vmos, va, pages, id, 0, perms)
        .expect("map_region");
    id
}

fn teardown(vmo: u64, starts: &[u64]) {
    let k = kmem::get();
    for &s in starts {
        k.aspace.unmap_region(&mut k.mm, s).expect("unmap_region");
    }
    trap::flush_tlb();
    k.vmos.destroy(&mut k.mm, vmo).expect("destroy vmo");
}

fn free() -> usize {
    kmem::get().mm.db.free_count()
}

fn lazy_commit_on_read() -> Outcome {
    balanced(|| {
        let before = free();
        let vmo = region(LAZY_TEST_BASE, 4, Perms::RW);
        ensure_eq(free(), before, "creating a region allocates nothing")?;
        ensure_eq(read(LAZY_TEST_BASE + 8), 0, "a fresh page reads as zero")?;
        // one data page, plus the level-1 and level-0 tables under an empty slot
        ensure_eq(before - free(), 3, "frames taken by the first fault")?;
        teardown(vmo, &[LAZY_TEST_BASE]);
        Ok(())
    })
}

fn lazy_commit_on_write() -> Outcome {
    balanced(|| {
        let vmo = region(LAZY_TEST_BASE, 4, Perms::RW);
        write(LAZY_TEST_BASE + PAGE + 16, 0x1122_3344_5566_7788);
        let after_first = free();
        ensure_eq(
            read(LAZY_TEST_BASE + PAGE + 16),
            0x1122_3344_5566_7788,
            "read back",
        )?;
        write(LAZY_TEST_BASE + PAGE + 24, 5);
        ensure_eq(
            free(),
            after_first,
            "touching a mapped page again allocates nothing",
        )?;
        write(LAZY_TEST_BASE + 3 * PAGE, 9); // another page: one more frame, same tables
        ensure_eq(free(), after_first - 1, "a second page costs one frame")?;
        teardown(vmo, &[LAZY_TEST_BASE]);
        Ok(())
    })
}

fn unmapped_region_is_gone() -> Outcome {
    balanced(|| {
        let vmo = region(LAZY_TEST_BASE, 2, Perms::RW);
        write(LAZY_TEST_BASE, 1);
        teardown(vmo, &[LAZY_TEST_BASE]);
        let k = kmem::get();
        ensure(
            k.aspace.table().translate(&k.mm, LAZY_TEST_BASE).is_none(),
            "the page is unmapped",
        )?;
        let refused = trap::expect_refused(|| {
            read(LAZY_TEST_BASE);
        });
        ensure_eq(
            refused.map(|r| r.error),
            Some(FaultError::NoRegion),
            "touching it again is refused",
        )
    })
}

fn fault_outside_region_is_refused() -> Outcome {
    balanced(|| {
        let vmo = region(LAZY_TEST_BASE, 4, Perms::RW);
        let refused = trap::expect_refused(|| {
            read(LAZY_TEST_BASE + 4 * PAGE); // one page past the end
        });
        let r = refused.ok_or("no fault was refused")?;
        ensure_eq(r.error, FaultError::NoRegion, "error")?;
        ensure_eq(r.cause, 13, "scause (load page fault)")?;
        ensure_eq(r.addr, LAZY_TEST_BASE + 4 * PAGE, "stval")?;
        teardown(vmo, &[LAZY_TEST_BASE]);
        Ok(())
    })
}

fn write_to_read_only_region_is_refused() -> Outcome {
    balanced(|| {
        let vmo = region(LAZY_TEST_BASE, 2, Perms::R);
        ensure_eq(read(LAZY_TEST_BASE), 0, "reading is allowed")?;
        let r =
            trap::expect_refused(|| write(LAZY_TEST_BASE + 8, 1)).ok_or("no fault was refused")?;
        ensure_eq(r.error, FaultError::Denied, "error")?;
        ensure_eq(r.cause, 15, "scause (store page fault)")?;
        ensure_eq(
            read(LAZY_TEST_BASE + 8),
            0,
            "the refused write changed nothing",
        )?;
        teardown(vmo, &[LAZY_TEST_BASE]);
        Ok(())
    })
}

fn shared_vmo_is_one_memory() -> Outcome {
    balanced(|| {
        let a = LAZY_TEST_BASE;
        let b = LAZY_TEST_BASE + MIB2;
        let vmo = region(a, 2, Perms::RW);
        {
            let k = kmem::get();
            k.aspace
                .map_region(&k.vmos, b, 2, vmo, 0, Perms::R)
                .map_err(|e| format!("map_region b: {e:?}"))?;
        }
        write(a + 8, 0xfeed_f00d);
        ensure_eq(
            read(b + 8),
            0xfeed_f00d,
            "the write is visible through the other mapping",
        )?;
        let k = kmem::get();
        let pfn = k
            .aspace
            .table()
            .translate(&k.mm, a)
            .ok_or("a unmapped")?
            .pfn;
        ensure_eq(
            k.mm.db.refs(pfn),
            Some(3),
            "one reference for the VMO and one per mapping",
        )?;
        ensure_eq(
            k.aspace
                .table()
                .translate(&k.mm, b)
                .ok_or("b unmapped")?
                .pfn,
            pfn,
            "same frame",
        )?;
        teardown(vmo, &[a, b]);
        Ok(())
    })
}

fn churn_does_not_leak() -> Outcome {
    balanced(|| {
        for round in 0..200u64 {
            let vmo = region(LAZY_TEST_BASE, 8, Perms::RW);
            for p in 0..8 {
                if (round + p) % 3 != 0 {
                    write(LAZY_TEST_BASE + p * PAGE, round * 8 + p);
                }
            }
            for p in 0..8 {
                if (round + p) % 3 != 0 {
                    ensure_eq(read(LAZY_TEST_BASE + p * PAGE), round * 8 + p, "value")?;
                }
            }
            teardown(vmo, &[LAZY_TEST_BASE]);
        }
        Ok(())
    })
}

/// Every test, in order. The boot test requires exactly these names.
type Test = (&'static str, fn() -> Outcome);

pub const TESTS: &[Test] = &[
    ("paging_enabled", paging_enabled),
    ("runs_in_the_upper_half", runs_in_the_upper_half),
    ("low_half_is_empty", low_half_is_empty),
    ("kernel_mappings_are_w_xor_x", kernel_mappings_are_w_xor_x),
    (
        "kernel_sections_have_the_right_permissions",
        kernel_sections_have_the_right_permissions,
    ),
    (
        "direct_map_does_not_alias_the_image",
        direct_map_does_not_alias_the_image,
    ),
    ("heap_works", heap_works),
    ("frame_database_is_sane", frame_database_is_sane),
    ("lazy_commit_on_read", lazy_commit_on_read),
    ("lazy_commit_on_write", lazy_commit_on_write),
    ("unmapped_region_is_gone", unmapped_region_is_gone),
    (
        "fault_outside_region_is_refused",
        fault_outside_region_is_refused,
    ),
    (
        "write_to_read_only_region_is_refused",
        write_to_read_only_region_is_refused,
    ),
    ("shared_vmo_is_one_memory", shared_vmo_is_one_memory),
    ("churn_does_not_leak", churn_does_not_leak),
];
