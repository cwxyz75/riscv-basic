//! The kernel's address-space randomisation choice (HD3): how the seed the device tree gives becomes the image's slide and the direct map's move. The
//! early boot code (`kernel/src/arch/riscv64/boot.rs`, assembly, before there is a stack or a page table) makes the same choice with the same arithmetic; the
//! boot self-test `kaslr_choice_follows_the_seed` checks the two against each other, and this module is what that check and the host tests run.

/// The 2 MiB slots of the table that maps the image (`EARLY_L1`): 512, of which the first is left empty (the link address is 2 MiB above the slot's start).
pub const IMAGE_SLOTS: u64 = 511;
/// The positions of the direct map: it moves up by 0 to 64 GiB (65 positions), so that its 64 slots always end below the kernel-stack window.
pub const DIRECT_POSITIONS: u64 = 65;

/// The 64-bit number the early boot code takes from the seed properties: the first eight bytes of `kaslr-seed` when that holds at least eight, else
/// the first eight of `rng-seed` xor the next eight (when it holds sixteen). 0 means there is no seed.
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: the offsets are the constants 0 and 8
pub fn seed(kaslr_seed: Option<&[u8]>, rng_seed: Option<&[u8]>) -> u64 {
    let word = |b: &[u8], at: usize| {
        b.get(at..at + 8)
            .map_or(0, |w| u64::from_le_bytes(w.try_into().unwrap_or([0; 8])))
    };
    let k = kaslr_seed.map_or(0, |b| word(b, 0));
    if k != 0 {
        return k;
    }
    rng_seed.map_or(0, |b| word(b, 0) ^ word(b, 8))
}

/// The choice for a seed and an image of `image_pages` 2 MiB pages: (the slide in 2 MiB pages, the direct map's move in GiB). No seed (0) means no
/// slide and no move; an image that leaves no room for a slide is not slid, but the direct map still moves.
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: the divisors are constants above zero (`IMAGE_SLOTS - image_pages` is checked positive just before)
pub fn choose(seed: u64, image_pages: u64) -> (u64, u64) {
    if seed == 0 {
        return (0, 0);
    }
    let slide = if image_pages < IMAGE_SLOTS {
        (seed & 0xffff_ffff) % (IMAGE_SLOTS - image_pages)
    } else {
        0
    };
    (slide, (seed >> 32) % DIRECT_POSITIONS)
}
