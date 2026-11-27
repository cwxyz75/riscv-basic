//! `place_anywhere` (MM7, HD2) stays inside the mapping arena `[MAP_ARENA_BASE, STACK_ARENA_BASE)` whatever the random draw and however full the
//! arena is. The boot suite's fixed-address mappings rely on it (T272): they live above the stack arena, where nothing the kernel places may land,
//! after a fixed 2 GiB in the mapping arena collided now and then with the randomly placed image or library.
use abi::user::{MAP_ARENA_BASE, STACK_ARENA_BASE, STACK_ARENA_END, TOP};
use kcore::aspace::{AddressSpace, Vmos};
use kcore::frame::FrameDb;
use kcore::pagetable::{PageTable, Perms};
use kcore::Mm;
use ksys::place_anywhere;

mod common;
use common::FakeMem;

const PERMS: Perms = Perms {
    read: true,
    write: false,
    exec: false,
    user: true,
};

/// A small deterministic generator (xorshift), so each case is the same on every run.
struct Rng(u64);
impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

fn inside_the_arena(at: u64, bytes: u64) -> bool {
    at >= MAP_ARENA_BASE
        && at.is_multiple_of(4096)
        && at
            .checked_add(bytes)
            .is_some_and(|end| end <= STACK_ARENA_BASE)
}

#[test]
fn the_fixed_window_above_the_stack_arena_is_never_placed_into() {
    // (the window the suite uses is real: room above the stack arena, below the top of the user half)
    const {
        assert!(
            STACK_ARENA_END < TOP
                && MAP_ARENA_BASE < STACK_ARENA_BASE
                && STACK_ARENA_BASE < STACK_ARENA_END
        )
    };
    let mut mm = Mm {
        db: FrameDb::new(4000, 300),
        mem: FakeMem::default(),
    };
    let mut vmos = Vmos::new();
    let mut jobs = kcore::job::Jobs::new();
    let mut space = AddressSpace::new(PageTable::new(&mut mm).unwrap());
    let arena = STACK_ARENA_BASE - MAP_ARENA_BASE;
    let sizes = [4096, 2 << 20, 2 << 30, arena / 2, arena - 4096, arena];
    // the draws at both ends and in between, on an empty arena
    let draws: [fn(u64) -> u64; 3] = [|_| 0, |n| n - 1, |n| n / 2];
    for &bytes in &sizes {
        for mut draw in draws {
            let at = place_anywhere(&mut draw, &space, bytes).expect("an empty arena has room");
            assert!(inside_the_arena(at, bytes), "{bytes:#x} at {at:#x}");
        }
    }
    assert_eq!(
        place_anywhere(&mut |_| 0, &space, arena + 4096),
        None,
        "bigger than the arena"
    );
    // an arena with room for 2 GiB only at its top end: the random tries and the search find that room, never the window above the stack arena
    let v = vmos.create(&mut jobs, 1);
    let gib2 = 2u64 << 30;
    let hole = STACK_ARENA_BASE - gib2;
    let mut at = MAP_ARENA_BASE;
    while at < hole {
        // (a page at the start of every 2 GiB: the gaps between are a page short of 2 GiB; above the last one, below the hole, 4 GiB less a page are free)
        space.map_region(&mut vmos, at, 1, v, 0, PERMS).unwrap();
        at += gib2;
    }
    let mut rng = Rng(0x7272_7272);
    for _ in 0..200 {
        let mut draw = |n: u64| rng.below(n);
        let got = place_anywhere(&mut draw, &space, gib2).expect("the hole fits");
        assert!(inside_the_arena(got, gib2), "{got:#x}");
        assert!(
            !space.has_region_in(got, got + gib2),
            "{got:#x} overlaps a mapping"
        );
    }
    // nothing more fits: NO room, not a place outside the arena
    space.map_region(&mut vmos, hole, 1, v, 0, PERMS).unwrap();
    assert_eq!(
        place_anywhere(&mut |n| n - 1, &space, gib2),
        None,
        "no room left in the arena"
    );
    space
        .unmap_region(&mut mm, &mut vmos, &mut jobs, hole)
        .unwrap();
}
