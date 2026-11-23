//! HD3: the slide and the direct map's move are a pure function of the seed (the early boot code is assembly: this is the model it is checked against).

use kcore::kaslr::{choose, seed, DIRECT_POSITIONS, IMAGE_SLOTS};

#[test]
fn the_seed_is_kaslr_seed_else_rng_seed() {
    let k = 0x0123_4567_89ab_cdefu64.to_le_bytes();
    let r: Vec<u8> = (1u8..=32).collect();
    assert_eq!(seed(Some(&k), Some(&r)), 0x0123_4567_89ab_cdef);
    let expect = u64::from_le_bytes(r[0..8].try_into().unwrap())
        ^ u64::from_le_bytes(r[8..16].try_into().unwrap());
    assert_eq!(seed(None, Some(&r)), expect);
    assert_eq!(
        seed(Some(&[0u8; 8]), Some(&r)),
        expect,
        "an all-zero kaslr-seed is no seed"
    );
    assert_eq!(seed(Some(&k[..7]), None), 0, "too short");
    assert_eq!(
        seed(None, Some(&r[..12])),
        u64::from_le_bytes(r[0..8].try_into().unwrap()),
        "a short rng-seed has only its first word"
    );
    assert_eq!(seed(None, None), 0);
}

#[test]
fn no_seed_means_no_slide_and_no_move() {
    assert_eq!(choose(0, 5), (0, 0));
}

#[test]
fn the_image_always_stays_inside_its_slot_and_the_direct_map_below_the_stack_window() {
    for pages in [1u64, 5, 40, 509, 510] {
        for i in 0..2000u64 {
            let seed = i.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
            let (s, k) = choose(seed, pages);
            assert!(
                1 + s + pages <= 512,
                "pages {pages} seed {seed:#x}: the image runs past EARLY_L1"
            );
            assert!(k < DIRECT_POSITIONS);
        }
    }
    // an image with no room is not slid; the direct map still moves
    assert_eq!(choose(0x0000_0007_0000_0003, IMAGE_SLOTS), (0, 7));
}

#[test]
fn the_choice_has_the_entropy_the_spec_states() {
    let mut slides = std::collections::BTreeSet::new();
    let mut moves = std::collections::BTreeSet::new();
    for i in 0..20_000u64 {
        let (s, k) = choose(i.wrapping_mul(0xD6E8_FEB8_6659_FD93).rotate_left(17) | 1, 8);
        slides.insert(s);
        moves.insert(k);
    }
    assert!(
        slides.len() > 450,
        "about nine bits of slide: {}",
        slides.len()
    );
    assert_eq!(
        moves.len() as u64,
        DIRECT_POSITIONS,
        "every direct-map position is reachable"
    );
}
