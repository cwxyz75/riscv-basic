//! Tests added after the independent review of S4a: ambiguity, unreferenced authority, encoder
//! and decoder being duals, stricter framing, and round-trip properties for accepted input.

use proto::bootimg::{self, BootImageError};
use proto::file::{self, FrameError, Request};
use proto::launch::{self, Spawn};
use proto::startup::{Role, Startup, StartupError};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn startup_refuses_ambiguous_namespaces_and_nul() {
    let enc = |s: &Startup| s.encode();
    let dup = Startup {
        namespace: vec![(b".".to_vec(), 0), (b".".to_vec(), 1)],
        ..Startup::default()
    };
    assert_eq!(enc(&dup), Err(StartupError::BadNamespace));
    let odd = Startup {
        namespace: vec![(b"../x".to_vec(), 0)],
        ..Startup::default()
    };
    assert_eq!(enc(&odd), Err(StartupError::BadNamespace));
    let empty = Startup {
        namespace: vec![(b"".to_vec(), 0)],
        ..Startup::default()
    };
    assert_eq!(enc(&empty), Err(StartupError::BadNamespace));
    let nul = Startup {
        args: vec![b"a\0b".to_vec()],
        ..Startup::default()
    };
    assert_eq!(enc(&nul), Err(StartupError::NulInString));
    // the decoder refuses the same bytes even if a hostile encoder produced them
    let good = Startup {
        namespace: vec![(b".".to_vec(), 0), (b"/".to_vec(), 1)],
        roles: vec![(Role::Stdin, 2)],
        ..Startup::default()
    };
    let mut bytes = good.encode().unwrap();
    assert_eq!(Startup::decode(&bytes, 3).unwrap(), good);
    let at = bytes.iter().position(|&b| b == b'/').unwrap();
    bytes[at] = b'.';
    assert_eq!(Startup::decode(&bytes, 3), Err(StartupError::BadNamespace));
}

#[test]
fn startup_refuses_handles_nobody_refers_to() {
    let s = Startup {
        roles: vec![(Role::Stdin, 0)],
        ..Startup::default()
    };
    let bytes = s.encode().unwrap();
    assert!(Startup::decode(&bytes, 1).is_ok());
    assert_eq!(
        Startup::decode(&bytes, 2),
        Err(StartupError::UnreferencedHandle)
    );
    assert_eq!(
        Startup::decode(&bytes, 9),
        Err(StartupError::UnreferencedHandle)
    );
}

#[test]
fn the_startup_encoder_is_the_decoders_dual() {
    let dup_role = Startup {
        roles: vec![(Role::Stdin, 0), (Role::Stdin, 1)],
        ..Startup::default()
    };
    assert_eq!(dup_role.encode(), Err(StartupError::DuplicateRole));
    let dup_idx = Startup {
        roles: vec![(Role::Stdin, 0), (Role::Stdout, 0)],
        ..Startup::default()
    };
    assert_eq!(dup_idx.encode(), Err(StartupError::DuplicateHandle));
    let ns_vs_role = Startup {
        namespace: vec![(b".".to_vec(), 0)],
        roles: vec![(Role::Stdin, 0)],
        ..Startup::default()
    };
    assert_eq!(ns_vs_role.encode(), Err(StartupError::DuplicateHandle));
}

#[test]
fn startup_golden_with_environment_pins_the_order() {
    // args ["a"], env ["E=1"], no namespace, roles [(Stdout, 0)], one handle
    let want: Vec<u8> = "52 41 54 53 01 00 00 00 01 00 00 00 01 00 00 00 00 00 00 00 01 00 00 00 \
         01 00 61 \
         03 00 45 3d 31 \
         02 00 00 00 00 00 00 00"
        .split_whitespace()
        .map(|b| u8::from_str_radix(b, 16).unwrap())
        .collect();
    let s = Startup {
        args: vec![b"a".to_vec()],
        env: vec![b"E=1".to_vec()],
        namespace: vec![],
        roles: vec![(Role::Stdout, 0)],
    };
    assert_eq!(s.encode().unwrap(), want);
    let d = Startup::decode(&want, 1).unwrap();
    assert_eq!(d.args, vec![b"a".to_vec()]);
    assert_eq!(d.env, vec![b"E=1".to_vec()]);
}

#[test]
fn boot_images_refuse_overlap_and_odd_names() {
    let files: [(&[u8], &[u8]); 2] = [(b"a", b"hi"), (b"bc", b"xyz")];
    let mut img = bootimg::build(&files).unwrap();
    img[23..27].copy_from_slice(&0x20u32.to_le_bytes()); // "bc" now shares "a"'s extent
    assert_eq!(bootimg::parse(&img), Err(BootImageError::Overlap));
    for bad in [
        &b"../x"[..],
        b"/abs",
        b"a/../b",
        b"a//b",
        b"n\0ul",
        b".",
        b"a/",
    ] {
        let one: [(&[u8], &[u8]); 1] = [(bad, b"1")];
        assert_eq!(
            bootimg::build(&one),
            Err(BootImageError::BadName),
            "{bad:?}"
        );
    }
    let ok: [(&[u8], &[u8]); 2] = [(b"fixtures/hello.txt", b"1"), (b"init", b"2")];
    assert!(bootimg::parse(&bootimg::build(&ok).unwrap()).is_ok());
}

#[test]
fn file_framing_is_strict() {
    assert_eq!(file::MAX_MESSAGE, 33 * 1024);
    let big = vec![0u8; file::MAX_IO + 1];
    let req = Request {
        txid: 1,
        op: file::op::WRITE,
        flags: 0,
        rights: 0,
        a: 0,
        b: 0,
        payload: &big,
    };
    assert_eq!(req.encode(), Err(FrameError::TooBig));
    let max = vec![0u8; file::MAX_IO];
    let ok = Request {
        payload: &max,
        ..req.clone()
    };
    let bytes = ok.encode().unwrap();
    assert!(bytes.len() <= file::MAX_MESSAGE);
    assert!(file::decode_request(&bytes).is_ok());
    let bad_flags = Request {
        flags: 0x10,
        payload: b"",
        ..req.clone()
    }
    .encode()
    .unwrap();
    assert_eq!(file::decode_request(&bad_flags), Err(FrameError::BadBits));
    let bad_rights = Request {
        rights: 0x10,
        payload: b"",
        ..req.clone()
    }
    .encode()
    .unwrap();
    assert_eq!(file::decode_request(&bad_rights), Err(FrameError::BadBits));
}

#[test]
fn accepted_input_round_trips() {
    let mut rng = Rng(99);
    let seed_req = Request {
        txid: 3,
        op: file::op::OPEN,
        flags: 1,
        rights: 3,
        a: 5,
        b: 6,
        payload: b"dir/file",
    }
    .encode()
    .unwrap();
    let seed_spawn = Spawn {
        txid: 2,
        image_offset: 16,
        image_len: 256,
        startup: b"abcd",
    }
    .encode()
    .unwrap();
    let seed_img = bootimg::build(&[(b"a", b"hi"), (b"bc/d", b"xyz")]).unwrap();
    for _ in 0..30_000 {
        let mutate = |seed: &[u8], rng: &mut Rng| {
            let mut b = seed.to_vec();
            for _ in 0..1 + rng.below(3) {
                let i = rng.below(b.len() as u64) as usize;
                b[i] = rng.next() as u8;
            }
            b
        };
        let b = mutate(&seed_req, &mut rng);
        if let Ok(r) = file::decode_request(&b) {
            assert_eq!(r.encode().unwrap(), b, "an accepted request is canonical");
        }
        let b = mutate(&seed_spawn, &mut rng);
        if let Ok(s) = launch::decode_spawn(&b) {
            // the pad word must be zero and the lengths exact, so the encoding is canonical
            assert_eq!(s.encode().unwrap(), b);
        }
        let b = mutate(&seed_img, &mut rng);
        if let Ok(entries) = bootimg::parse(&b) {
            for e in &entries {
                assert!(bootimg::find(&b, &entries, &e.name).is_some());
            }
        }
    }
}
