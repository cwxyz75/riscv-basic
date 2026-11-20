//! Memory discovery (PL4): the device tree's RAM and reserved ranges, the memory reservation block, the timebase frequency, and the
//! frames the allocator is finally given. The scenarios are the ones the item names (RAM at 0x4000_0000, two banks, a reserved range in the
//! middle, a tree placed low in RAM, a 24 MHz timebase) on synthetic trees, plus a property test of the range arithmetic against a bitmap,
//! and the real QEMU trees as an independent check.
mod common;
use common::{Dt, Rng, SEEDS};
use kcore::fdt;
use kcore::memmap::usable_frames;

/// A machine: RAM banks, reserved children, an optional reservation block, a timebase.
fn machine(
    banks: &[(u64, u64)],
    reserved: &[(u64, u64)],
    rsv: &[(u64, u64)],
    timebase: u32,
) -> Vec<u8> {
    let mut d = Dt::new();
    d.rsv = rsv.to_vec();
    d.begin("");
    d.u32s("#address-cells", &[2]);
    d.u32s("#size-cells", &[2]);
    d.begin("cpus");
    d.u32s("timebase-frequency", &[timebase]);
    d.end();
    for (i, b) in banks.iter().enumerate() {
        d.begin(&format!("memory@{:x}", b.0));
        d.prop("device_type", b"memory\0");
        d.reg(&[*b]);
        d.end();
        let _ = i;
    }
    d.begin("reserved-memory");
    d.u32s("#address-cells", &[2]);
    d.u32s("#size-cells", &[2]);
    d.prop("ranges", &[]);
    for r in reserved {
        d.begin(&format!("resv@{:x}", r.0));
        d.reg(&[*r]);
        d.end();
    }
    d.end();
    d.end();
    d.finish()
}

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

#[test]
fn ram_at_0x4000_0000_with_a_reserved_range_in_the_middle_and_a_24_mhz_timebase() {
    let blob = machine(
        &[(0x4000_0000, 512 * MIB)],
        &[(0x4800_0000, 2 * MIB)],
        &[],
        24_000_000,
    );
    let nodes = fdt::parse(&blob).unwrap();
    assert_eq!(fdt::memory_ranges(&nodes), vec![(0x4000_0000, 512 * MIB)]);
    assert_eq!(fdt::reserved_ranges(&nodes), vec![(0x4800_0000, 2 * MIB)]);
    assert_eq!(fdt::timebase_frequency(&nodes), Some(24_000_000));
    let got = usable_frames(&fdt::memory_ranges(&nodes), &fdt::reserved_ranges(&nodes));
    // frames 0x40000 .. 0x60000 minus 0x48000 .. 0x48200
    assert_eq!(got, vec![(0x40000, 0x8000), (0x48200, 0x60000 - 0x48200)]);
}

#[test]
fn two_banks_the_reservation_block_and_a_tree_placed_low_in_ram() {
    let tree_at = 0x8000_0000u64 + 4096;
    let blob = machine(
        &[(0x8000_0000, 256 * MIB), (0x1_0000_0000, GIB)],
        &[(0x8000_0000, 2 * MIB)], // firmware at the bottom of the first bank
        &[(0x8010_0000, 3 * 4096)], // the loader's reservation block
        10_000_000,
    );
    let nodes = fdt::parse(&blob).unwrap();
    assert_eq!(
        fdt::memory_ranges(&nodes),
        vec![(0x8000_0000, 256 * MIB), (0x1_0000_0000, GIB)]
    );
    assert_eq!(fdt::reservation_block(&blob), vec![(0x8010_0000, 3 * 4096)]);
    // the kernel also keeps out of the tree's own extent (the boot code adds it as a reserved range)
    let mut reserved = fdt::reserved_ranges(&nodes);
    reserved.extend(fdt::reservation_block(&blob));
    reserved.push((tree_at, blob.len() as u64));
    let got = usable_frames(&fdt::memory_ranges(&nodes), &reserved);
    // bank one loses its first 2 MiB (frames 0x80000..0x80200: the tree at 0x80001000 lies inside); bank two is whole
    assert_eq!(
        got,
        vec![(0x80200, 0x90000 - 0x80200), (0x10_0000, 0x40000)]
    );
}

#[test]
fn reservations_cut_to_whole_frames_outward_and_memory_inward() {
    // memory not frame aligned at either end loses its partial frames; a one-byte reservation takes a whole frame
    let got = usable_frames(&[(0x1001, 0x4000)], &[(0x3000, 1)]);
    // memory 0x1001..0x5001 -> frames 2..5; reserved frame 3 -> [2,3) and [4,5)
    assert_eq!(got, vec![(2, 1), (4, 1)]);
    assert_eq!(
        usable_frames(&[(0x1000, 0x1000)], &[(0x1000, 0x1000)]),
        vec![]
    );
    assert_eq!(
        usable_frames(&[(0x10, 0x20)], &[]),
        vec![],
        "less than a frame is nothing"
    );
    // overlapping memory ranges are one; a zero-size reservation reserves nothing
    assert_eq!(
        usable_frames(&[(0, 0x4000), (0x2000, 0x4000)], &[(0x100, 0)]),
        vec![(0, 6)]
    );
}

#[test]
fn the_range_arithmetic_agrees_with_a_frame_bitmap() {
    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed);
        let mut free = vec![false; 500];
        let mut mem = vec![];
        for _ in 0..rng.below(4) + 1 {
            let (a, s) = (
                rng.below(300 * 4096) + rng.below(2) * 7,
                rng.below(120 * 4096),
            );
            mem.push((a, s));
        }
        let mut res = vec![];
        for _ in 0..rng.below(4) {
            res.push((rng.below(300 * 4096), rng.below(60 * 4096)));
        }
        for &(a, s) in &mem {
            // frames wholly inside
            for f in 0..500u64 {
                if f * 4096 >= a && (f + 1) * 4096 <= a + s {
                    free[f as usize] = true;
                }
            }
        }
        for &(a, s) in &res {
            for f in 0..500u64 {
                // frames touched at all
                if s > 0 && f * 4096 < a + s && (f + 1) * 4096 > a {
                    free[f as usize] = false;
                }
            }
        }
        let got = usable_frames(&mem, &res);
        let mut seen = vec![false; 500];
        let mut last_end = 0u64;
        for (k, &(first, n)) in got.iter().enumerate() {
            assert!(n > 0, "seed {seed}");
            assert!(
                k == 0 || first > last_end,
                "seed {seed}: sorted and not touching"
            );
            last_end = first + n as u64;
            for f in first..first + n as u64 {
                seen[f as usize] = true;
            }
        }
        assert_eq!(seen, free, "seed {seed}: mem {mem:?} reserved {res:?}");
    }
}

#[test]
fn the_real_qemu_tree_gives_the_ram_and_the_timebase() {
    // `qemu-system-riscv64 -machine virt,aia=aplic-imsic,dumpdtb -m 128M`: the tree QEMU builds, before OpenSBI adds its own
    // `/reserved-memory` node (so there is none here; the kernel sees it in the tree it is handed, and a boot test checks the RAM
    // it ends up with)
    let nodes = fdt::parse(include_bytes!("fixtures/virt-rva23s64.dtb")).unwrap();
    let mem = fdt::memory_ranges(&nodes);
    assert_eq!(mem, vec![(0x8000_0000, 128 * MIB)]);
    assert_eq!(fdt::timebase_frequency(&nodes), Some(10_000_000));
    assert_eq!(fdt::reserved_ranges(&nodes), vec![]);
    assert_eq!(usable_frames(&mem, &[]), vec![(0x80000, 0x8000)]);
}

/// A machine written by hand with the odd shapes real trees have: one-cell addresses and sizes, several pairs in one `reg`, a node that
/// looks like memory but has no `device_type`, a reserved node with no `reg`, grandchildren of `/reserved-memory`, an eight-byte timebase.
#[test]
fn odd_tree_shapes_are_read_as_the_specification_says() {
    let mut d = Dt::new();
    d.rsv = vec![(0x4010_0000, 0x1000)];
    d.begin("");
    d.u32s("#address-cells", &[1]);
    d.u32s("#size-cells", &[1]);
    d.begin("cpus");
    d.prop("timebase-frequency", &24_000_000u64.to_be_bytes()); // two cells
    d.end();
    d.begin("memory@40000000"); // two banks in one node, one-cell form
    d.prop("device_type", b"memory\0");
    let mut reg = vec![];
    for x in [0x4000_0000u32, 0x0080_0000, 0x5000_0000, 0x0010_0000] {
        reg.extend(x.to_be_bytes());
    }
    d.raw_reg(&reg);
    d.end();
    d.begin("memory@60000000"); // no device_type: not memory
    d.reg_1cell(0x6000_0000, 0x1000_0000);
    d.end();
    d.begin("reserved-memory");
    d.u32s("#address-cells", &[1]);
    d.u32s("#size-cells", &[1]);
    d.prop("ranges", &[]);
    d.begin("dynamic"); // a request with no reg: nothing to name
    d.prop("size", &[0, 0, 0x10, 0]);
    d.end();
    d.begin("fw@40400000");
    d.reg_1cell(0x4040_0000, 0x0010_0000);
    d.end();
    d.end();
    d.end();
    let blob = d.finish();
    let nodes = fdt::parse(&blob).unwrap();
    assert_eq!(
        fdt::memory_ranges(&nodes),
        vec![(0x4000_0000, 0x80_0000), (0x5000_0000, 0x10_0000)]
    );
    assert_eq!(fdt::reserved_ranges(&nodes), vec![(0x4040_0000, 0x10_0000)]);
    assert_eq!(fdt::timebase_frequency(&nodes), Some(24_000_000));
    let mut reserved = fdt::reserved_ranges(&nodes);
    reserved.extend(fdt::reservation_block(&blob));
    // bank one 0x40000..0x40800 minus the reservation block's frame 0x40100 and the firmware's 0x40400..0x40500; bank two 0x50000..0x50100
    assert_eq!(
        usable_frames(&fdt::memory_ranges(&nodes), &reserved),
        vec![
            (0x40000, 0x100),
            (0x40101, 0x2ff),
            (0x40500, 0x300),
            (0x50000, 0x100)
        ]
    );
}

#[test]
fn ranges_at_the_top_of_the_address_space_are_cut_there_not_dropped_or_wrapped() {
    // memory running past u64::MAX is cut at it (the same rule the reserved side already had), not discarded
    let top = !0xfffu64;
    let got = usable_frames(&[(top - 0x3000, 0x10_0000)], &[]);
    assert_eq!(got, vec![((top - 0x3000) / 4096, 3)]);
    assert_eq!(
        usable_frames(&[(0, 0x4000)], &[(u64::MAX - 8, 100)]),
        vec![(0, 4)]
    );
}

#[test]
fn a_reservation_block_without_a_terminator_or_with_too_many_entries_is_bounded() {
    let mut d = Dt::new();
    d.rsv = (0..100).map(|i| (0x1000 * (i + 1), 0x1000)).collect();
    d.begin("");
    d.end();
    let blob = d.finish();
    let got = fdt::reservation_block(&blob);
    assert_eq!(got.len(), 64, "capped");
    assert_eq!(got[0], (0x1000, 0x1000));
    // a header whose block starts outside the blob gives nothing
    let mut bad = blob.clone();
    bad[16..20].copy_from_slice(&0x7fff_0000u32.to_be_bytes());
    assert_eq!(fdt::reservation_block(&bad), vec![]);
}
