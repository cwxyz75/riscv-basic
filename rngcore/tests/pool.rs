//! The entropy pool's pure half (HD1): the key derivation against an independent computation (Python's hashlib and hmac, the same
//! construction written out there), `seeded`, and `below(n)` by a chi-squared test with a fixed seed.
use rngcore::pool::{below, derive_key, Inputs};
use rngcore::Drbg;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[test]
fn the_key_matches_an_independent_hkdf_computation() {
    let seed: Vec<u8> = (0..32).collect();
    let kaslr: Vec<u8> = (100..108).collect();
    let (key, seeded) = derive_key(&Inputs {
        rng_seed: &seed,
        kaslr_seed: &kaslr,
        zkr: &[],
        time: 0x0123_4567_89ab_cdef,
    });
    assert_eq!(
        hex(&key),
        "f8ba69d90d235e35431509b209da04d4e62bb2a2f91bf3db197d1583dae1c3ea"
    );
    assert!(seeded);
    // no seed at all: a key from the time alone, and not seeded
    let (key, seeded) = derive_key(&Inputs {
        time: 5,
        ..Inputs::default()
    });
    assert_eq!(
        hex(&key),
        "991e453e5b1bc340a6d2b1bd1f548c2ca731154a485ece26ddb9da82ae355432"
    );
    assert!(!seeded);
}

#[test]
fn every_input_changes_the_key_and_an_all_zero_seed_is_not_a_seed() {
    let base = Inputs {
        rng_seed: &[1; 32],
        kaslr_seed: &[2; 8],
        zkr: &[3; 8],
        time: 7,
    };
    let (k0, s0) = derive_key(&base);
    assert!(s0);
    for variant in [
        Inputs {
            rng_seed: &[9; 32],
            ..base
        },
        Inputs {
            kaslr_seed: &[9; 8],
            ..base
        },
        Inputs {
            zkr: &[9; 8],
            ..base
        },
        Inputs { time: 8, ..base },
        // the same bytes moved between inputs (the framing keeps them apart)
        Inputs {
            rng_seed: &[1; 31],
            kaslr_seed: &[1, 2, 2, 2, 2, 2, 2, 2, 2],
            ..base
        },
    ] {
        assert_ne!(derive_key(&variant).0, k0);
    }
    let zero = Inputs {
        rng_seed: &[0; 32],
        kaslr_seed: &[0; 8],
        zkr: &[],
        time: 1,
    };
    assert!(!derive_key(&zero).1, "a scrubbed seed is not a seed");
    assert!(
        !derive_key(&Inputs {
            zkr: &[0, 0, 4],
            ..zero
        })
        .1,
        "three bytes are not a seed"
    );
}

fn drbg(seed: u8) -> Drbg {
    let mut d = Drbg::new();
    d.reseed(&[seed; 32]);
    d
}

#[test]
fn below_is_in_range_and_refuses_zero() {
    let mut d = drbg(1);
    assert_eq!(below(|b| d.fill(b), 0), None);
    for n in [1u64, 2, 3, 7, 1000, u64::MAX, (1 << 63) + 1] {
        for _ in 0..200 {
            let x = below(|b| d.fill(b), n).unwrap();
            assert!(x < n);
        }
    }
}

#[test]
fn below_rejects_draws_that_would_bias_the_low_residues() {
    // n = 2^63 + 1: 2^64 mod n = 2^63 - 1, so every draw below that is refused; scripted draws show the refusal
    let n = (1u64 << 63) + 1;
    let mut script = vec![5u64, 1 << 62, (1 << 63) - 2, (1 << 63) - 1].into_iter();
    let got = below(
        |b| {
            b.copy_from_slice(&script.next().expect("a draw left").to_le_bytes());
            true
        },
        n,
    );
    assert_eq!(
        got,
        Some(((1u64 << 63) - 1) % n),
        "the first draw not below the threshold"
    );
    assert_eq!(
        script.next(),
        None,
        "exactly the three refused draws and the accepted one"
    );
}

#[test]
fn below_is_uniform_by_a_chi_squared_test_with_a_fixed_seed() {
    // 7 is not a power of two (a plain modulo would be slightly biased); 140_000 draws, 20_000 expected per bucket. The 0.999 point of the
    // chi-squared distribution with 6 degrees of freedom is 22.46; the seed is fixed, so this is deterministic
    for (n, bound) in [(7u64, 22.46f64), (10, 27.88)] {
        let mut d = drbg(42);
        let draws = 20_000 * n;
        let mut count = vec![0u64; n as usize];
        for _ in 0..draws {
            count[below(|b| d.fill(b), n).unwrap() as usize] += 1;
        }
        let expect = (draws / n) as f64;
        let chi2: f64 = count
            .iter()
            .map(|&c| (c as f64 - expect).powi(2) / expect)
            .sum();
        assert!(
            chi2 < bound,
            "n = {n}: chi-squared {chi2} over {bound}: {count:?}"
        );
    }
}

#[test]
fn a_small_seed_is_not_enough_and_sixteen_bytes_are() {
    let seeded = |rng: &[u8], kaslr: &[u8], zkr: &[u8]| {
        derive_key(&Inputs {
            rng_seed: rng,
            kaslr_seed: kaslr,
            zkr,
            time: 0,
        })
        .1
    };
    assert!(!seeded(&[1], &[], &[]), "one byte");
    assert!(!seeded(&[], &[], &[7, 7]), "one 16-bit seed CSR read");
    assert!(!seeded(&[1; 15], &[], &[]), "fifteen bytes");
    assert!(seeded(&[1; 16], &[], &[]), "sixteen bytes");
    assert!(seeded(&[1; 8], &[2; 8], &[]), "two sources add up");
    assert!(
        !seeded(&[0; 32], &[2; 8], &[]),
        "zero bytes count for nothing"
    );
}

#[test]
fn below_gives_up_when_the_source_refuses() {
    assert_eq!(below(|_| false, 7), None);
}
