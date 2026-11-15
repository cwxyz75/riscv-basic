//! RFC vectors for the primitives, and the properties of the generator.
use rngcore::chacha;
use rngcore::drbg::{fingerprint, Drbg, MAX_REQUEST};
use rngcore::sha256::Sha256;
use rngcore::{hkdf, hmac_sha256, sha256};

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[test]
fn sha256_matches_fips_180_vectors() {
    assert_eq!(
        hex(&sha256(b"")),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        hex(&sha256(b"abc")),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        hex(&sha256(
            b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
        )),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
    // a million 'a's, fed in awkward pieces, equals one shot
    let mut s = Sha256::new();
    let piece = [b'a'; 997];
    let mut left = 1_000_000usize;
    while left > 0 {
        let n = left.min(piece.len());
        s.update(&piece[..n]);
        left -= n;
    }
    assert_eq!(
        hex(&s.finish()),
        "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
    );
}

#[test]
fn sha256_padding_boundaries_agree_with_a_one_shot() {
    for n in 50..140usize {
        let data: Vec<u8> = (0..n).map(|i| i as u8).collect();
        let one = sha256(&data);
        let mut s = Sha256::new();
        for b in data.chunks(7) {
            s.update(b);
        }
        assert_eq!(s.finish(), one, "length {n}");
    }
}

#[test]
fn hmac_matches_rfc_4231() {
    assert_eq!(
        hex(&hmac_sha256(&[0x0b; 20], &[b"Hi There"])),
        "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
    );
    assert_eq!(
        hex(&hmac_sha256(
            b"Jefe",
            &[b"what do ya want ", b"for nothing?"]
        )),
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
    // a key longer than the block (test case 6)
    assert_eq!(
        hex(&hmac_sha256(
            &[0xaa; 131],
            &[b"Test Using Larger Than Block-Size Key - Hash Key First"]
        )),
        "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
    );
}

#[test]
fn hkdf_matches_rfc_5869_test_case_1() {
    let ikm = [0x0b; 22];
    let salt: Vec<u8> = (0..13u8).collect();
    let info: Vec<u8> = (0xf0..=0xf9u8).collect();
    let mut okm = [0u8; 42];
    assert!(hkdf(&salt, &ikm, &info, &mut okm));
    assert_eq!(
        hex(&okm),
        "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
    );
}

#[test]
fn hkdf_refuses_more_than_the_construction_allows() {
    let mut big = vec![0xaau8; 255 * 32 + 1];
    assert!(!hkdf(b"s", b"k", b"i", &mut big));
    assert!(big.iter().all(|&b| b == 0xaa), "nothing written");
    let mut max = vec![0u8; 255 * 32];
    assert!(hkdf(b"s", b"k", b"i", &mut max));
    assert!(max[8000..].iter().any(|&b| b != 0));
}

#[test]
fn chacha20_block_matches_rfc_8439() {
    let key: Vec<u8> = (0..32u8).collect();
    let mut k = [0u8; 32];
    k.copy_from_slice(&key);
    let nonce = [0, 0, 0, 9, 0, 0, 0, 0x4a, 0, 0, 0, 0];
    let b = chacha::block(&k, 1, &nonce);
    assert_eq!(
        hex(&b),
        "10f1e7e4d13b5915500fdd1fa32071c4c7d1f4c733c068030422aa9ac3d46c4ed2826446079faa0914c2d705d98b02a2b5129cd1de164eb9cbd083e8a2503c4e"
    );
}

fn seeded(tag: u8) -> Drbg {
    let mut d = Drbg::new();
    d.reseed(&[tag; 32]);
    d
}

#[test]
fn an_unseeded_generator_refuses_and_writes_nothing() {
    let mut d = Drbg::new();
    let mut out = [0xaau8; 16];
    assert!(!d.fill(&mut out));
    assert_eq!(out, [0xaa; 16]);
    d.reseed(&[1u8; 31]);
    assert!(
        !d.is_seeded() && !d.fill(&mut out),
        "31 bytes of entropy are not enough"
    );
    d.reseed(&[2u8; 1]);
    assert!(d.fill(&mut out), "32 are");
    assert!(d.derive(b"x").is_some());
    assert!(!Drbg::new().derive(b"x").is_some());
}

#[test]
fn the_same_seed_gives_the_same_stream_and_different_seeds_do_not() {
    let (mut a, mut b, mut c) = (seeded(1), seeded(1), seeded(2));
    let (mut x, mut y, mut z) = ([0u8; 200], [0u8; 200], [0u8; 200]);
    for _ in 0..5 {
        assert!(a.fill(&mut x) && b.fill(&mut y) && c.fill(&mut z));
        assert_eq!(x, y);
        assert_ne!(x, z);
    }
}

#[test]
fn every_request_replaces_the_key_so_output_is_never_repeated() {
    let mut d = seeded(7);
    let mut seen = std::collections::HashSet::new();
    let before = fingerprint(&d);
    for _ in 0..2000 {
        let mut o = [0u8; 16];
        d.fill(&mut o);
        assert!(seen.insert(o), "a repeated 16-byte output");
    }
    assert_ne!(fingerprint(&d), before);
}

#[test]
fn the_state_after_a_request_does_not_reveal_that_request() {
    // two generators, one asked for 1 byte and one for 64: after the call their keys are the same (the key replaced is
    // derived from block 0 only), so the output length cannot leak through the state, and the output differs from the key
    let (mut a, mut b) = (seeded(3), seeded(3));
    let (mut o1, mut o64) = ([0u8; 1], [0u8; 64]);
    a.fill(&mut o1);
    b.fill(&mut o64);
    assert_eq!(fingerprint(&a), fingerprint(&b));
    assert_eq!(o1[0], o64[0], "a short request is a prefix of a longer one");
}

#[test]
fn reseeding_changes_the_stream_and_cannot_be_undone_by_known_input() {
    let (mut a, mut b) = (seeded(4), seeded(4));
    b.reseed(b"more");
    let (mut x, mut y) = ([0u8; 32], [0u8; 32]);
    a.fill(&mut x);
    b.fill(&mut y);
    assert_ne!(x, y);
    // all-zero "entropy" does not reset the state to a known value
    let mut c = seeded(5);
    let f = fingerprint(&c);
    c.reseed(&[0u8; 64]);
    assert_ne!(fingerprint(&c), f);
}

#[test]
fn requests_over_the_limit_are_refused_and_the_stream_is_roughly_uniform() {
    let mut d = seeded(9);
    let mut big = vec![0u8; MAX_REQUEST + 1];
    assert!(!d.fill(&mut big));
    let mut ok = vec![0u8; MAX_REQUEST];
    assert!(d.fill(&mut ok));
    let mut counts = [0usize; 256];
    for &b in &ok {
        counts[b as usize] += 1;
    }
    // 65,536 bytes: each value about 256 times; a chi-square-ish sanity bound, not a proof
    assert!(
        counts.iter().all(|&c| (150..380).contains(&c)),
        "{:?}",
        counts.iter().min()
    );
    let ones: u32 = ok.iter().map(|b| b.count_ones()).sum();
    assert!(
        (ones as i64 - (MAX_REQUEST as i64 * 4)).abs() < 3000,
        "bit balance {ones}"
    );
}

#[test]
fn derived_keys_depend_on_the_label_and_the_state() {
    let (mut a, mut b) = (seeded(6), seeded(6));
    let ka = a.derive(b"tcp isn").unwrap();
    let kb = b.derive(b"tcp ports").unwrap();
    assert_ne!(ka, kb);
    let ka2 = a.derive(b"tcp isn").unwrap();
    assert_ne!(ka, ka2, "each call draws fresh output");
}
