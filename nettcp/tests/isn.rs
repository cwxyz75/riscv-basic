//! H20: SipHash-2-4 against the published vectors, the ISN formula of S9-SPEC 5.10, and port selection.
use nettcp::siphash::{ephemeral_port, isn, siphash24, tuple_hash};

fn key() -> [u8; 16] {
    let mut k = [0u8; 16];
    for (i, b) in k.iter_mut().enumerate() {
        *b = i as u8;
    }
    k
}

#[test]
fn siphash_matches_the_published_vectors() {
    // the paper's appendix: key 00..0f, message 00..0e (15 bytes)
    let m: Vec<u8> = (0..15u8).collect();
    assert_eq!(siphash24(&key(), &m), 0xa129_ca61_49be_45e5);
    // the reference implementation's vector table: the empty message and the 1-byte message
    assert_eq!(siphash24(&key(), &[]), 0x726f_db47_dd0e_0e31);
    assert_eq!(siphash24(&key(), &[0]), 0x74f8_39c5_93dc_67fd);
    // a message crossing the 8-byte block boundary exactly
    let m8: Vec<u8> = (0..8u8).collect();
    assert_eq!(siphash24(&key(), &m8), 0x93f5_f579_9a93_2462);
}

const A: [u8; 4] = [10, 0, 2, 15];
const B: [u8; 4] = [10, 0, 2, 2];

#[test]
fn isns_move_forward_with_the_clock_differ_by_tuple_and_secret_and_survive_a_restart() {
    let k = key();
    let t0 = 1_000_000_000u64;
    let a = isn(&k, A, 40000, B, 80, t0);
    // later: the same tuple gets a larger number, by exactly the elapsed time in 4 us units
    let b = isn(&k, A, 40000, B, 80, t0 + 400_000);
    assert_eq!(b.wrapping_sub(a), 100);
    // another tuple or another secret: unrelated
    assert_ne!(isn(&k, A, 40001, B, 80, t0), a);
    assert_ne!(isn(&k, A, 40000, B, 81, t0), a);
    let mut k2 = k;
    k2[0] ^= 1;
    assert_ne!(isn(&k2, A, 40000, B, 80, t0), a);
    // a restarted stack keeps the secret and sees the same clock: the same sequence, so it never goes backwards
    let again = isn(&k, A, 40000, B, 80, t0 + 400_000);
    assert_eq!(again, b);
    assert_ne!(
        tuple_hash(&k, A, 1, B, 2),
        tuple_hash(&k, B, 2, A, 1),
        "direction matters"
    );
}

#[test]
fn ports_cover_the_range_skip_used_ones_and_depend_on_the_destination() {
    let k = key();
    let used = |p: u16| p.is_multiple_of(2);
    let mut seen = std::collections::HashSet::new();
    for c in 0..2000u32 {
        let p = ephemeral_port(&k, A, B, 80, 49152, 65535, c, used).unwrap();
        assert!((49152..=65535).contains(&p) && !used(p));
        seen.insert(p);
    }
    assert!(
        seen.len() >= 900,
        "the choice is spread over the range: {}",
        seen.len()
    );
    let p1 = ephemeral_port(&k, A, B, 80, 49152, 65535, 0, |_| false).unwrap();
    let p2 = ephemeral_port(&k, A, B, 81, 49152, 65535, 0, |_| false).unwrap();
    let p3 = ephemeral_port(&k, A, [10, 0, 2, 3], 80, 49152, 65535, 0, |_| false).unwrap();
    assert!(
        p1 != p2 || p1 != p3,
        "different destinations start at different places"
    );
    assert_eq!(
        ephemeral_port(&k, A, B, 80, 100, 102, 0, |p| p != 101),
        Some(101),
        "the one free port is found"
    );
    assert_eq!(
        ephemeral_port(&k, A, B, 80, 100, 102, 0, |_| true),
        None,
        "none free"
    );
}
