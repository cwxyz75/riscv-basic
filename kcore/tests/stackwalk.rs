//! The panic trace's stack bounds (T266, T233-b): from `sp` alone, the live part of the stack, never the guard half of a kernel-stack
//! slot and never a neighbouring slot. Exhaustive over every 4-byte step of the first slots, one in the middle, the last two and the
//! window's edges (the arithmetic is the same in every slot).
use abi::layout::{KSTACK_BYTES, KSTACK_SLOT_BYTES, KSTACK_WINDOW_BASE, KSTACK_WINDOW_SIZE};
use kcore::stackwalk::{live_stack, window_top};

fn check_slot(slot: u64) {
    let base = KSTACK_WINDOW_BASE + slot * KSTACK_SLOT_BYTES;
    let top = base + KSTACK_SLOT_BYTES;
    for sp in (base..top).step_by(4) {
        let got = live_stack(sp, &[]);
        if sp < top - KSTACK_BYTES {
            assert_eq!(got, None, "sp {sp:#x} is in slot {slot}'s guard");
        } else {
            let (s, t) = got.unwrap_or_else(|| panic!("sp {sp:#x} is on slot {slot}'s stack"));
            assert_eq!(s, sp & !7);
            assert_eq!(t, top, "the top is the slot's own");
            assert!(s >= top - KSTACK_BYTES && s < t && t - s <= KSTACK_BYTES);
        }
    }
}

#[test]
fn every_word_of_a_slot_maps_to_its_own_stack_or_to_nothing() {
    let last = KSTACK_WINDOW_SIZE / KSTACK_SLOT_BYTES - 1;
    for slot in [0, 1, 2, 777, last - 1, last] {
        check_slot(slot);
    }
}

#[test]
fn outside_the_window_only_the_given_stacks_count() {
    assert_eq!(window_top(KSTACK_WINDOW_BASE - 8), None);
    assert_eq!(window_top(KSTACK_WINDOW_BASE + KSTACK_WINDOW_SIZE), None);
    assert_eq!(live_stack(KSTACK_WINDOW_BASE - 8, &[]), None);
    let boot = (0xffff_ffff_8020_0000u64, 0xffff_ffff_8024_0000u64);
    let emergency = (0xffff_ffff_8030_0000u64, 0xffff_ffff_8030_2000u64);
    let others = [boot, emergency];
    assert_eq!(live_stack(boot.0, &others), Some((boot.0, boot.1)));
    assert_eq!(live_stack(boot.1 - 1, &others), Some((boot.1 - 8, boot.1)));
    assert_eq!(
        live_stack(boot.1, &others),
        None,
        "one past the top is not the stack"
    );
    assert_eq!(
        live_stack(emergency.0 + 0x100, &others),
        Some((emergency.0 + 0x100, emergency.1))
    );
    assert_eq!(live_stack(0x8000_0000, &others), None);
    // an empty or reversed range is never an answer
    assert_eq!(live_stack(0x1000, &[(0x1000, 0x1000)]), None);
    assert_eq!(live_stack(0x1000, &[(0x2000, 0x1000)]), None);
}

#[test]
fn a_window_address_ignores_the_ranges() {
    // (a range that overlaps the window must not widen a slot's stack into its guard)
    let base = KSTACK_WINDOW_BASE + 5 * KSTACK_SLOT_BYTES;
    let wide = [(KSTACK_WINDOW_BASE, KSTACK_WINDOW_BASE + KSTACK_WINDOW_SIZE)];
    assert_eq!(live_stack(base + 8, &wide), None);
    assert_eq!(
        live_stack(base + KSTACK_SLOT_BYTES - 8, &wide),
        Some((base + KSTACK_SLOT_BYTES - 8, base + KSTACK_SLOT_BYTES))
    );
}
