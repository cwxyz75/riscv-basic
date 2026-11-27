//! The kernel's self-tests (S1: memory). Each reports as a `##TEST` line. Tests
//! that touch lazily committed memory run inside `balanced`, which also checks the
//! frame count returns to what it was: "map/unmap/fault sequences return
//! allocation counts to baseline" (the old roadmap, S1).

use crate::{kmem, trap};
use abi::layout::LAZY_TEST_BASE;
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
    let before = kmem::free_frames_accounted();
    body()?;
    let after = kmem::free_frames_accounted();
    ensure_eq(
        after,
        before,
        "free frames after the test (leak if smaller)",
    )?;
    kmem::get_as()
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
    ensure_eq(
        crate::arch::aspace_current() >> 60,
        8,
        "address-space mode (8 = Sv39)",
    )
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
        pc >= abi::layout::KERNEL_VIRT_BASE,
        "code is at the kernel's linked address",
    )
}

fn low_half_is_empty() -> Outcome {
    let k = kmem::get_as();
    for va in [
        0x1000,
        0x10_0000,
        0x1000_0000,
        0x8000_0000,
        0x8020_0000,
        0x8800_0000,
        kmem::stats().image_first << 12, // wherever the image actually is
    ] {
        ensure(
            k.aspace.table().translate(k.mm, va).is_none(),
            &format!("{va:#x} must not be mapped (the boot identity map is gone)"),
        )?;
    }
    Ok(())
}

fn kernel_mappings_are_w_xor_x() -> Outcome {
    let k = kmem::get_as();
    let (mut leaves, mut bad) = (0, 0);
    k.aspace.table().for_each_leaf(k.mm, &mut |va, t, level| {
        leaves += 1;
        // a large leaf is legal only in the direct map (MM3) and only as plain data: read-write, never executable, never user
        let large_ok = level == 0
            || ((crate::kmem::direct_base()..crate::kmem::direct_base() + (1 << 36)).contains(&va)
                && t.perms == Perms::RW);
        if (t.perms.write && t.perms.exec) || !large_ok || t.perms.user {
            bad += 1;
        }
    });
    ensure(leaves > 100, "the kernel table has leaves")?;
    ensure_eq(bad, 0, "leaves that are W+X, huge or user-accessible")
}

#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
fn kernel_sections_have_the_right_permissions() -> Outcome {
    let k = kmem::get_as();
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
                .translate(k.mm, va)
                .ok_or_else(|| format!("{name} page {va:#x} is unmapped"))?;
            ensure_eq(t.perms, want, &format!("{name} page {va:#x}"))?;
            ensure_eq(
                t.pfn,
                (va - crate::arch::kernel_delta()) >> 12,
                &format!("{name} page {va:#x} maps to its physical load address"),
            )?;
            va += PAGE;
        }
    }
    // Executable pages exist only in the text section.
    let mut stray = 0;
    k.aspace.table().for_each_leaf(k.mm, &mut |va, t, _| {
        if t.perms.exec && !(s.text.0..s.text.1).contains(&va) {
            stray += 1;
        }
    });
    ensure_eq(stray, 0, "executable pages outside text")
}

/// Every direct-map leaf maps usable RAM (the frames the database manages and the metadata the database lives in; never the kernel image
/// or the firmware's reserved ranges), or is one of the device pages or the tree's own frames.
#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
fn direct_map_does_not_alias_the_image() -> Outcome {
    let k = kmem::get_as();
    let st = kmem::stats();
    // (the whole image, page by page: nothing of it is reachable through the direct map, whatever the leaf sizes. The firmware's own
    // reservation is the tree's business; the RAM between it and the image is ordinary usable memory now)
    for pfn in st.image_first..st.image_end {
        ensure(
            k.aspace
                .table()
                .translate(k.mm, crate::kmem::direct_base() + (pfn << 12))
                .is_none(),
            "a page of the kernel image or firmware is reachable through the direct map",
        )?;
    }
    let usable = kmem::usable_ranges();
    let (mut in_map, mut aliases, mut devices, mut outside) = (0, 0, 0, 0);
    let (mut leaves, mut level1) = (0u64, 0u64);
    k.aspace.table().for_each_leaf(k.mm, &mut |va, t, level| {
        if (crate::kmem::direct_base()..crate::kmem::direct_base() + (1 << 36)).contains(&va) {
            let pages = 1u64 << (9 * level);
            in_map += pages; // pages, whatever the leaf's size
            leaves += 1;
            if level >= 1 {
                level1 += pages; // pages covered by large leaves (2 MiB, and 1 GiB on a big machine)
            }
            if t.pfn < st.image_end && t.pfn + pages > st.image_first {
                aliases += 1; // a leaf that covers part of the image
            }
            if t.pfn < st.ram_first {
                devices += 1;
            }
            if va - crate::kmem::direct_base() != t.pfn << 12 {
                aliases += 1; // not the direct map's own formula
            }
            // a read-write leaf lies wholly inside a usable range; anything else is a device page (below RAM) or one of the tree's
            // read-only pages
            let inside = usable
                .iter()
                .any(|&(f, c)| t.pfn >= f && t.pfn + pages <= f + c as u64);
            if t.perms.write && !inside && t.pfn >= st.ram_first {
                outside += 1;
            }
            if !t.perms.write && t.pfn >= st.ram_first && !(t.perms.read && pages == 1) {
                outside += 1; // only the tree's pages are read-only RAM, one frame each
            }
        }
    });
    ensure(in_map > 1000, "the direct map has pages")?;
    // MM3: RAM is mapped with 2 MiB pages (a 256 MiB machine has about 126 of them) and 4 KiB pages only at the edges and around the
    // tree, not tens of thousands of small leaves
    ensure(
        level1 >= st.usable_frames / 2,
        "most of the direct map is large pages",
    )?;
    // leaves: large pages are few; the small ones are the edges and the tree's pages
    ensure(leaves < 3000, "the direct map has few leaves")?;
    ensure_eq(
        devices,
        2 + crate::aia::pages().count() + crate::iommu::page().iter().count(),
        "device pages in the direct map",
    )?;
    ensure_eq(
        outside,
        0,
        "direct-map leaves that are not usable memory, a device page or a tree page",
    )?;
    ensure_eq(
        aliases,
        0,
        "direct-map leaves aliasing the image, firmware or a wrong address",
    )
}

/// PL4: the frame count matches the machine. What does not need the harness's word is checked always: the allocator's total is the managed
/// frames, and the reserved part of RAM covers at least the kernel image (a tree that reserved nothing would hand out the image's frames)
/// and is the image plus a little (firmware, the tree). The harness says how much RAM it gave QEMU (`ram=<size>` on the kernel line:
/// `128M`, `4G`, or bytes); the tree must describe exactly that.
#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
fn ram_matches_the_machine() -> Outcome {
    let st = kmem::stats();
    let image = st.image_end - st.image_first;
    let reserved = st.memory_frames - st.usable_frames;
    ensure_eq(
        k_total_frames(),
        st.managed_frames,
        "the allocator's total is the managed frames",
    )?;
    ensure(
        reserved >= image,
        "the reserved part of RAM covers the image",
    )?;
    ensure(
        st.meta_frames * 4096 >= st.managed_frames * kcore::buddy::META_BYTES as u64,
        "the frames set aside for the metadata hold one entry per managed frame",
    )?;
    ensure(
        reserved < image + 2048,
        "the reserved part of RAM is the image and a little more",
    )?;
    let Some(want) = crate::dtb::boot_arg_value("ram=") else {
        return Ok(()); // a boot without the harness (the interactive targets): nothing to compare
    };
    let (num, shift) = match want.as_bytes().last() {
        Some(b'M') => (&want[..want.len() - 1], 20),
        Some(b'G') => (&want[..want.len() - 1], 30),
        _ => (want.as_str(), 0),
    };
    let bytes = num.parse::<u64>().map_err(|_| "ram= is not a number")? << shift;
    ensure_eq(
        st.memory_frames,
        bytes.min(abi::layout::DIRECT_MAP_PHYS_LIMIT - st.ram_first * 4096) / 4096,
        "the frames of RAM the device tree describes",
    )
}

/// PL5b: the console fallback proves itself. Under the boot word `console=sbi` (`make os-boot-test-sbi-console`) output must really go through the
/// firmware: the switch happened, bytes went that way (this test's own line included) and the firmware refused none. Without the word the
/// kernel prints on the UART the tree named, and nothing went through the firmware.
fn the_console_is_where_the_boot_line_says() -> Outcome {
    let (bytes, refused) = crate::console::sbi_counts();
    if crate::dtb::boot_arg_present("console=sbi") {
        ensure(
            crate::console::sbi_forced(),
            "console=sbi switched the console",
        )?;
        ensure(
            bytes > 0,
            "output went through the firmware's debug console",
        )?;
        ensure_eq(refused, 0, "bytes the firmware's debug console refused")
    } else {
        ensure(
            !crate::console::sbi_forced(),
            "the console was not switched without the word",
        )?;
        ensure(
            crate::console::base().is_some(),
            "the tree named a UART and the kernel found it",
        )?;
        ensure_eq(bytes, 0, "bytes sent through the firmware without the word")
    }
}

/// PL4c: the image is where the loader put it, and the kernel found out. The harness names the address it built the image for
/// (`load=0x...` on the kernel line; only the load-address boot test does) and the image's physical start, worked out by the early boot
/// code from its own address, must be that: a kernel that quietly ran from the default address would pass every other test.
/// HD1: the pool is seeded exactly when the tree has a seed (a stripped tree is booted with `-dtb`), and what it produces is not constant.
#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
fn the_entropy_pool_is_seeded_as_the_tree_says() -> Outcome {
    let blob = crate::dtb::dtb_blob().ok_or_else(|| String::from("no device tree"))?;
    let seeds = kcore::fdt::chosen_seeds(blob).unwrap_or_default();
    let nonzero = |r: Option<(usize, usize)>| {
        r.and_then(|(o, l)| blob.get(o..o + l))
            .is_some_and(|b| b.iter().any(|&x| x != 0))
    };
    let want = nonzero(seeds.rng_seed) || nonzero(seeds.kaslr_seed);
    ensure_eq(
        crate::entropy::seeded(),
        want,
        "seeded exactly when the tree has a non-zero seed (this machine has no Zkr)",
    )?;
    let (mut a, mut b) = ([0u8; 16], [0u8; 16]);
    ensure(
        crate::entropy::fill(&mut a) && crate::entropy::fill(&mut b),
        "the pool fills",
    )?;
    ensure(a != b && a != [0; 16], "two draws differ and are not zero")?;
    ensure(crate::entropy::below(0).is_none(), "below(0) is refused")?;
    ensure(
        (0..50).all(|_| crate::entropy::below(7).is_some_and(|x| x < 7)),
        "below(7) stays below 7",
    )?;
    ensure(
        crate::entropy::first_output() != 0,
        "the first output was recorded",
    )
}

/// HD3: the slide and the direct map's move that the early boot code (assembly, reading the tree itself) chose are what `kcore::kaslr` says the tree's seed gives, or
/// nothing under `kaslr=off` or with no seed; the slide is a multiple of 2 MiB and the image still lies inside its slot; and the direct map starts where the
/// move says.
#[allow(clippy::large_stack_frames)] // (a boot self-test on the boot stack: the tree parse and the failure messages)
fn kaslr_choice_follows_the_seed() -> Outcome {
    let blob = crate::dtb::dtb_blob().ok_or("no device tree")?;
    let seeds = kcore::fdt::chosen_seeds(blob).ok_or("the tree has no /chosen")?;
    let part = |r: Option<(usize, usize)>| -> Option<&[u8]> {
        r.and_then(|(o, l)| blob.get(o..o.checked_add(l)?))
    };
    let seed = kcore::kaslr::seed(part(seeds.kaslr_seed), part(seeds.rng_seed));
    let sec = kmem::sections();
    let pages = (sec.data.1 - sec.text.0).div_ceil(2 << 20);
    let off = crate::dtb::boot_arg_value("kaslr=").as_deref() == Some("off");
    let (s, k) = if off {
        (0, 0)
    } else {
        kcore::kaslr::choose(seed, pages)
    };
    ensure_eq(crate::arch::slide() >> 21, s, "the slide, in 2 MiB pages")?;
    ensure(
        crate::arch::slide().is_multiple_of(2 << 20),
        "the slide is a multiple of 2 MiB",
    )?;
    ensure_eq(
        crate::arch::direct_shift(),
        k,
        "the direct map's move, in GiB",
    )?;
    ensure_eq(
        crate::kmem::direct_base(),
        abi::layout::DIRECT_BASE + (k << 30),
        "the direct map starts where the move says",
    )?;
    ensure(
        sec.text.0 >= abi::layout::KERNEL_VIRT_BASE && sec.data.1 <= 0xffff_ffff_c000_0000,
        "the image lies inside its top-level slot",
    )
}

fn the_image_is_where_the_loader_put_it() -> Outcome {
    let st = kmem::stats();
    let delta = crate::arch::kernel_delta();
    ensure_eq(
        abi::layout::KERNEL_VIRT_BASE + crate::arch::slide() - delta,
        st.image_first << 12,
        "the image's physical start is its link address plus the slide minus the delta",
    )?;
    ensure(
        (st.image_first << 12).is_multiple_of(2 << 20),
        "the image is loaded at a multiple of 2 MiB",
    )?;
    let Some(want) = crate::dtb::boot_arg_value("load=") else {
        // a boot without the harness (the interactive targets) says nothing; the harness always does (`ram=` is its mark)
        return if crate::dtb::boot_arg_value("ram=").is_some() {
            Err("the harness did not say where it loaded the image (load=)".into())
        } else {
            Ok(())
        };
    };
    let want =
        u64::from_str_radix(want.trim_start_matches("0x"), 16).map_err(|_| "load= is not hex")?;
    ensure_eq(
        st.image_first << 12,
        want,
        "the image's physical start is where the harness loaded it",
    )
}

fn k_total_frames() -> u64 {
    crate::irq::without(|| kmem::get().mm.db.total() as u64)
}

#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
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
    let db = &kmem::get_as().mm.db;
    ensure(db.free_count() > 20_000, "plenty of free frames at boot")?;
    db.audit().map_err(|e| format!("audit: {e}"))
}

/// Create a VMO of `pages` pages and one region over all of it at `va`.
fn region(va: u64, pages: u64, perms: Perms) -> u64 {
    let k = kmem::get_as();
    let id = k.vmos.create(k.jobs, pages);
    k.aspace
        .map_region(k.vmos, va, pages, id, 0, perms)
        .expect("map_region");
    id
}

fn teardown(vmo: u64, starts: &[u64]) {
    let k = kmem::get_as();
    for &s in starts {
        k.aspace
            .unmap_region(k.mm, k.vmos, k.jobs, s)
            .expect("unmap_region");
    }
    trap::flush_tlb();
    k.vmos.release(k.mm, k.jobs, vmo).expect("destroy vmo");
}

fn free() -> usize {
    kmem::free_frames_accounted()
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
        let unmapped = {
            let k = kmem::get_as(); // (dropped before the access below: the fault handler takes the same locks)
            k.aspace.table().translate(k.mm, LAZY_TEST_BASE).is_none()
        };
        ensure(unmapped, "the page is unmapped")?;
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
        ensure_eq(r.access, kcore::aspace::Access::Read, "the access (a load)")?;
        ensure_eq(r.addr, LAZY_TEST_BASE + 4 * PAGE, "the faulting address")?;
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
        ensure_eq(
            r.access,
            kcore::aspace::Access::Write,
            "the access (a store)",
        )?;
        ensure_eq(
            read(LAZY_TEST_BASE + 8),
            0,
            "the refused write changed nothing",
        )?;
        teardown(vmo, &[LAZY_TEST_BASE]);
        Ok(())
    })
}

#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
fn shared_vmo_is_one_memory() -> Outcome {
    balanced(|| {
        let a = LAZY_TEST_BASE;
        let b = LAZY_TEST_BASE + MIB2;
        let vmo = region(a, 2, Perms::RW);
        {
            let k = kmem::get_as();
            k.aspace
                .map_region(k.vmos, b, 2, vmo, 0, Perms::R)
                .map_err(|e| format!("map_region b: {e:?}"))?;
        }
        write(a + 8, 0xfeed_f00d);
        ensure_eq(
            read(b + 8),
            0xfeed_f00d,
            "the write is visible through the other mapping",
        )?;
        {
            let k = kmem::get_as(); // (dropped before `teardown`, which takes the locks again)
            let pfn = k.aspace.table().translate(k.mm, a).ok_or("a unmapped")?.pfn;
            ensure_eq(
                k.mm.db.refs(pfn),
                Some(3),
                "one reference for the VMO and one per mapping",
            )?;
            ensure_eq(
                k.aspace.table().translate(k.mm, b).ok_or("b unmapped")?.pfn,
                pfn,
                "same frame",
            )?;
        }
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

/// The heap grows at a safe point when it runs low: fill the boot heap's free space, call `service`, and a block
/// that no longer fits the boot heap is allocated in the grown one (in the window), then everything is freed.
#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
fn heap_grows_at_a_safe_point() -> Outcome {
    use alloc::vec::Vec;
    let grown_before = crate::heap::grown_bytes();
    let frames_before = crate::kmem::free_frames_accounted();
    let window = abi::layout::HEAP_WINDOW_BASE as usize
        ..(abi::layout::HEAP_WINDOW_BASE + abi::layout::HEAP_WINDOW_SIZE) as usize;
    // take nearly everything that is free in both heaps, in 256 KiB blocks
    let mut hold: Vec<Vec<u8>> = Vec::new();
    while crate::heap::free_bytes() > 600 * 1024 {
        let mut v = Vec::new();
        if v.try_reserve_exact(256 * 1024).is_err() {
            break;
        }
        v.resize(256 * 1024, 1);
        hold.push(v);
    }
    ensure(
        crate::heap::free_bytes() < crate::heap::LOW_WATER,
        "the heap is low",
    )?;
    // the safe point: the heap grows
    irq_service();
    ensure(
        crate::heap::grown_bytes() > grown_before,
        "the heap grew at the safe point",
    )?;
    ensure(
        crate::heap::free_bytes() >= 1024 * 1024,
        "and has room again",
    )?;
    // (the frames it took are accounted for: held by the heap, not lost)
    ensure(
        crate::kmem::free_frames_accounted() == frames_before,
        "growth moves frames to the heap without losing any",
    )?;
    let big: Vec<u8> = alloc::vec![7u8; 1024 * 1024];
    ensure(
        window.contains(&(big.as_ptr() as usize)),
        "a block taken after the boot heap ran out lives in the window",
    )?;
    ensure(big.iter().all(|&b| b == 7), "and holds what was written")?;
    drop(big);
    drop(hold);
    Ok(())
}

/// L16: this hart's frame cache hands out frames with one reference, refills in batches (the database's free count drops by whole
/// batches, not per frame), takes frames back without the database, and the leak oracle counts what it holds as free.
#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
fn frame_cache_serves_and_accounts() -> Outcome {
    use alloc::vec::Vec;
    irq_service();
    let accounted = kmem::free_frames_accounted();
    let db_free = || kmem::get().mm.db.free_count();
    let db_before = db_free();
    let mut got: Vec<u64> = Vec::new();
    for _ in 0..40 {
        got.push(kmem::frame_alloc().ok_or("a frame")?);
    }
    let mut sorted = got.clone();
    sorted.sort_unstable();
    sorted.dedup();
    ensure_eq(sorted.len(), 40, "forty different frames")?;
    ensure_eq(
        kmem::free_frames_accounted(),
        accounted - 40,
        "forty frames are out",
    )?;
    let taken = db_before - db_free();
    ensure(
        taken % kcore::fcache::BATCH == 0 && taken <= 40 + kcore::fcache::BATCH,
        "the database was asked in batches",
    )?;
    {
        let k = kmem::get();
        for &p in &got {
            ensure_eq(k.mm.db.refs(p), Some(1), "one reference each")?;
        }
    }
    for &p in &got {
        kmem::frame_free_sole(p);
    }
    ensure_eq(
        kmem::free_frames_accounted(),
        accounted,
        "the frames are back (the cache counts as free)",
    )?;
    ensure(
        kmem::cached_frames() > 0,
        "the cache keeps frames for the next allocation",
    )?;
    kmem::get()
        .mm
        .db
        .audit()
        .map_err(|e| format!("frame database audit: {e}"))
}

/// MM4: small objects come from the slabs (in the direct map, aligned to their class), keep what is written, and when they are all freed the
/// frames and bytes the heap holds are back where they were: the churn test of T-SPEC section 8 for the slab path.
#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
fn slab_serves_small_objects_and_everything_comes_back() -> Outcome {
    use alloc::boxed::Box;
    use alloc::vec::Vec;
    irq_service(); // (the pool stocked, the magazines settled)
    let frames_before = crate::kmem::free_frames_accounted();
    let bytes_before = crate::thread::heap_free();
    let direct = crate::kmem::direct_base() as usize
        ..(crate::kmem::direct_base() + abi::layout::DIRECT_MAP_PHYS_LIMIT) as usize;
    // many objects of every size class, written with a pattern
    let mut held: Vec<(Box<[u8]>, u8)> = Vec::new();
    for round in 0..6000usize {
        let size = [8usize, 16, 24, 48, 100, 200, 400, 1000, 2000][round % 9];
        let pattern = (round % 251) as u8;
        if round % 40 == 0 {
            irq_service(); // (a safe point: the pool is stocked again; a burst without one falls back to the free-list heap, which is allowed)
        }
        let b: Box<[u8]> = alloc::vec![pattern; size].into_boxed_slice();
        let addr = b.as_ptr() as usize;
        ensure(
            direct.contains(&addr),
            "a small object lives in the direct map",
        )?;
        ensure(
            addr.is_multiple_of(16),
            "and is aligned to its class (at least 16)",
        )?;
        held.push((b, pattern));
    }
    ensure(
        held.iter().all(|(b, p)| b.iter().all(|x| x == p)),
        "every object holds what was written",
    )?;
    // something big goes to the free-list heaps, not the slabs
    let big: Vec<u8> = alloc::vec![3u8; 8192];
    ensure(
        !direct.contains(&(big.as_ptr() as usize)),
        "a large block is not a slab object",
    )?;
    ensure(
        crate::thread::heap_free() != bytes_before,
        "the live objects show in the leak oracle",
    )?;
    drop(big);
    // free in a different order than allocated (every other one first), so slabs go from full to partial to empty
    let mut later = Vec::new();
    for (i, o) in held.drain(..).enumerate() {
        if i % 2 == 0 {
            drop(o);
        } else {
            later.push(o);
        }
    }
    drop(later);
    drop(held); // (the vector's own buffer is heap too)
    irq_service(); // (the pool above its high-water mark goes back to the frame database)
    ensure_eq(
        crate::thread::heap_free(),
        bytes_before,
        "the heap holds the same bytes once everything is freed",
    )?;
    ensure_eq(
        crate::heap::slab_violations(),
        0,
        "the slab refused no free",
    )?;
    ensure(
        crate::kmem::free_frames_accounted() == frames_before,
        "and the frames are all accounted for",
    )?;
    Ok(())
}

fn irq_service() {
    crate::irq::without(crate::heap::service);
}

/// IOMMU5 (A11): QEMU's own walker, through the IOMMU's debug translation interface, agrees with `iommucore`'s tables; every frame
/// comes back (`iommu::selftest`).
fn iommu_translation_matches_the_tables() -> Outcome {
    balanced(crate::iommu::selftest)
}

pub const TESTS: &[Test] = &[
    (
        "iommu_translation_matches_the_tables",
        iommu_translation_matches_the_tables,
    ),
    ("heap_grows_at_a_safe_point", heap_grows_at_a_safe_point),
    (
        "frame_cache_serves_and_accounts",
        frame_cache_serves_and_accounts,
    ),
    (
        "slab_serves_small_objects_and_everything_comes_back",
        slab_serves_small_objects_and_everything_comes_back,
    ),
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
    ("ram_matches_the_machine", ram_matches_the_machine),
    (
        "the_console_is_where_the_boot_line_says",
        the_console_is_where_the_boot_line_says,
    ),
    (
        "the_entropy_pool_is_seeded_as_the_tree_says",
        the_entropy_pool_is_seeded_as_the_tree_says,
    ),
    (
        "the_image_is_where_the_loader_put_it",
        the_image_is_where_the_loader_put_it,
    ),
    (
        "kaslr_choice_follows_the_seed",
        kaslr_choice_follows_the_seed,
    ),
    ("heap_works", heap_works),
    ("frame_database_is_sane", frame_database_is_sane),
    (
        "kernel_windows_hold_what_their_formulas_say",
        crate::user_tests::windows_hold_what_their_formulas_say,
    ),
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
