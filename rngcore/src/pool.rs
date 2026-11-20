//! The kernel's entropy pool, pure half (HD1, KERNEL-V1-SPEC 7.1): how the boot-time inputs become a generator key, and how an unbiased number
//! below `n` is drawn from a byte source. The generator itself is [`crate::Drbg`] (ChaCha20, fast key erasure); the kernel owns the lock and
//! reads the inputs (the device tree's seeds, the `seed` CSR, the clock).

use crate::sha256::{hkdf, Sha256};

/// What the kernel found at boot. An empty slice is an input that was not there.
#[derive(Clone, Copy, Default)]
pub struct Inputs<'a> {
    /// `/chosen/rng-seed` (the firmware's random bytes).
    pub rng_seed: &'a [u8],
    /// `/chosen/kaslr-seed`.
    pub kaslr_seed: &'a [u8],
    /// Bytes read from the `seed` CSR (Zkr), if the harts have it.
    pub zkr: &'a [u8],
    /// The `time` counter at the moment of the derivation: it separates boots but is not trusted as entropy.
    pub time: u64,
}

/// The pool key and whether it rests on a real seed. `seeded` is true when at least one of the three seed inputs has a non-zero byte: the
/// time alone makes a key (the generator still works) but not a seeded one (the kernel then does not randomise addresses). A key from the
/// same inputs is always the same; the kernel scrubs the tree's copy of the seeds after reading them.
pub fn derive_key(inp: &Inputs) -> ([u8; 32], bool) {
    let mut h = Sha256::new();
    for (tag, bytes) in [
        (1u8, inp.rng_seed),
        (2, inp.kaslr_seed),
        (3, inp.zkr),
        (4, &inp.time.to_le_bytes()[..]),
    ] {
        h.update(&[tag]);
        h.update(&(bytes.len() as u32).to_le_bytes());
        h.update(bytes);
    }
    let ikm = h.finish();
    let mut key = [0u8; 32];
    // (a 32-byte output is far below HKDF's limit of 255 blocks: this cannot fail)
    let _ = hkdf(b"kernel entropy v1", &ikm, b"chacha20 key", &mut key);
    let seeded = [inp.rng_seed, inp.kaslr_seed, inp.zkr]
        .iter()
        .any(|s| s.iter().any(|&b| b != 0));
    (key, seeded)
}

/// A uniform number in `0..n` (`None` for `n == 0`) from the byte source `fill`, by rejection sampling: draws that fall in the short last
/// block of the 64-bit range are discarded, so no residue is likelier than another.
pub fn below(mut fill: impl FnMut(&mut [u8]), n: u64) -> Option<u64> {
    if n == 0 {
        return None;
    }
    // draws below `2^64 mod n` would make the low residues likelier: refuse them
    let threshold = n.wrapping_neg() % n;
    loop {
        let mut b = [0u8; 8];
        fill(&mut b);
        let x = u64::from_le_bytes(b);
        if x >= threshold {
            return Some(x % n);
        }
    }
}
