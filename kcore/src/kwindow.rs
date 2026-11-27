//! How many frames a window of the kernel's page table holds (T227): the formulas the leak oracle adds to the free count, for the kernel-stack
//! window (slots made and kept, each with its stack pages at the top of the slot) and the grown heap (pages mapped from the window's base
//! up). Pure arithmetic, proved by Kani against a page-by-page count of the tables those mappings need (`cargo kani -p kcore`, by hand:
//! OD177, OD182), and checked in the kernel against a walk of the real table (`PageTable::frames_in_range`).

/// Bytes a level-0 table spans (512 pages of 4 KiB).
const SPAN0: u64 = 2 * 1024 * 1024;

/// The frames `made` stack slots hold, each `slot_bytes` long with `stack_pages` pages mapped at its top, in a window that one level-1
/// table covers and that starts on a 2 MiB boundary: the stack pages, the level-1 table, and one level-0 table per 2 MiB of slots.
/// `slot_bytes` divides 2 MiB (the kernel's is 32 KiB: 64 slots a table).
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: made is at most the window's slot count, stack_pages a handful, slot_bytes a power of two dividing 2 MiB
pub fn stack_window(made: u64, stack_pages: u64, slot_bytes: u64) -> u64 {
    if made == 0 {
        return 0;
    }
    made * stack_pages + 1 + made.div_ceil(SPAN0 / slot_bytes)
}

/// The frames a grown heap of `pages` pages holds, mapped from the start of a window that one level-1 table covers and that starts on a
/// 2 MiB boundary: the pages, the level-1 table, and one level-0 table per 2 MiB.
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: pages is at most the window's page count
pub fn heap_window(pages: u64) -> u64 {
    if pages == 0 {
        0
    } else {
        pages + 1 + pages.div_ceil(512)
    }
}

#[cfg(kani)]
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: proof harnesses over bounded values (at most 160 slots, 1100 pages) of the kernel's windows
mod proofs {
    use super::*;

    /// The tables a sorted run of page addresses needs, counted the way a walk would find them: a level-0 table for each new 2 MiB
    /// span the run enters, and a level-1 table for each new 1 GiB span (the formulas' independent oracle).
    fn tables_of(pages: impl Iterator<Item = u64>) -> u64 {
        let (mut last0, mut last1, mut n) = (u64::MAX, u64::MAX, 0);
        for va in pages {
            if va >> 21 != last0 {
                last0 = va >> 21;
                n += 1;
            }
            if va >> 30 != last1 {
                last1 = va >> 30;
                n += 1;
            }
        }
        n
    }

    const STACK_BASE: u64 = 0xffff_ffe0_0000_0000; // (abi::layout::KSTACK_WINDOW_BASE)
    const HEAP_BASE: u64 = 0xffff_ffe8_0000_0000; // (abi::layout::HEAP_WINDOW_BASE)

    /// The stack formula, for the kernel's slot geometry (32 KiB slots, four pages at the top), for every slot count up to 160 (two and a
    /// half level-0 tables: none, one, a table's worth, one more, and past the second).
    #[kani::proof]
    #[kani::unwind(642)]
    fn the_stack_formula_counts_every_frame_the_slots_map() {
        let made: u64 = kani::any();
        kani::assume(made <= 160);
        let (slot, pages) = (32 * 1024u64, 4u64);
        let vas = (0..made).flat_map(|s| {
            (0..pages).map(move |i| STACK_BASE + s * slot + slot - (pages - i) * 4096)
        });
        assert_eq!(
            stack_window(made, pages, slot),
            made * pages + tables_of(vas)
        );
    }

    /// The heap formula for every page count up to 1100 (two level-0 tables and a part of a third).
    #[kani::proof]
    #[kani::unwind(1102)]
    fn the_heap_formula_counts_every_frame_the_window_maps() {
        let pages: u64 = kani::any();
        kani::assume(pages <= 1100);
        let vas = (0..pages).map(|k| HEAP_BASE + k * 4096);
        assert_eq!(heap_window(pages), pages + tables_of(vas));
    }
}
