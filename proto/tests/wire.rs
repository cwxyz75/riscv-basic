//! Wire-format tests. The golden byte strings were worked out by hand from S4-SPEC.md, not
//! produced by running the encoder.

use proto::bootimg::{self, BootImageError};
use proto::console::{self, Control, ControlError};
use proto::file::{self, DirEntry, FrameError, PathError, Reply, Request};
use proto::launch::{self, LaunchError, Spawn};
use proto::startup::{Role, Startup, StartupError};

fn hex(s: &str) -> Vec<u8> {
    s.split_whitespace()
        .map(|b| u8::from_str_radix(b, 16).unwrap())
        .collect()
}

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

// --- startup message ---

fn sample_startup() -> Startup {
    Startup {
        args: vec![b"ls".to_vec(), b"-l".to_vec()],
        env: vec![],
        namespace: vec![(b".".to_vec(), 0)],
        roles: vec![(Role::Stdin, 1), (Role::Stdout, 2)],
    }
}

const STARTUP_HEX: &str =
    "52 41 54 53  01 00 00 00  02 00 00 00  00 00 00 00  01 00 00 00  02 00 00 00 \
     02 00 6c 73  02 00 2d 6c \
     01 00 2e  00 00 00 00 \
     01 00 00 00 01 00 00 00  02 00 00 00 02 00 00 00";

#[test]
fn startup_golden_bytes() {
    let want = hex(STARTUP_HEX);
    assert_eq!(want.len(), 55);
    assert_eq!(sample_startup().encode().unwrap(), want);
    assert_eq!(Startup::decode(&want, 3).unwrap(), sample_startup());
}

#[test]
fn startup_rejects_every_kind_of_malformation() {
    let good = hex(STARTUP_HEX);
    let d = |b: &[u8], n| Startup::decode(b, n);
    // too few handles for the indices
    assert_eq!(d(&good, 2), Err(StartupError::BadIndex));
    // truncated anywhere
    for cut in 0..good.len() {
        assert!(d(&good[..cut], 3).is_err(), "cut at {cut}");
    }
    // trailing bytes
    let mut t = good.clone();
    t.push(0);
    assert_eq!(d(&t, 3), Err(StartupError::TrailingBytes));
    // magic and version
    let mut m = good.clone();
    m[0] ^= 1;
    assert_eq!(d(&m, 3), Err(StartupError::BadMagic));
    let mut v = good.clone();
    v[4] = 2;
    assert_eq!(d(&v, 3), Err(StartupError::BadVersion));
    // counts beyond the limits (argc = 65)
    let mut c = good.clone();
    c[8] = 65;
    assert_eq!(d(&c, 3), Err(StartupError::TooMany));
    // a count larger than the bytes available
    let mut c = good.clone();
    c[8] = 9;
    assert!(d(&c, 3).is_err());
    // unknown role (9) and duplicate role (both Stdin)
    let mut r = good.clone();
    let n = r.len();
    r[n - 8] = 9;
    assert_eq!(d(&r, 3), Err(StartupError::UnknownRole));
    let mut r = good.clone();
    r[n - 8] = 1;
    assert_eq!(d(&r, 3), Err(StartupError::DuplicateRole));
    // the same handle index twice (stdout also uses index 1)
    let mut x = good.clone();
    x[n - 4] = 1;
    assert_eq!(d(&x, 3), Err(StartupError::DuplicateHandle));
    // too many handles claimed, message too big
    assert_eq!(d(&good, 33), Err(StartupError::TooBig));
    assert_eq!(d(&vec![0u8; 16 * 1024 + 1], 3), Err(StartupError::TooBig));
    // a string longer than 1024
    let mut s = Startup::default();
    s.args.push(vec![b'x'; 1025]);
    assert_eq!(s.encode(), Err(StartupError::StringTooLong));
}

#[test]
fn startup_encoder_refuses_what_the_decoder_would() {
    let s = Startup {
        args: vec![b"a".to_vec(); 65],
        ..Startup::default()
    };
    assert_eq!(s.encode(), Err(StartupError::Limit));
    let s = Startup {
        args: vec![vec![b'y'; 1000]; 20],
        ..Startup::default()
    };
    assert_eq!(s.encode(), Err(StartupError::TooBig));
    let max = Startup {
        args: vec![vec![b'z'; 1024]],
        ..Startup::default()
    };
    assert!(Startup::decode(&max.encode().unwrap(), 0).is_ok());
}

#[test]
fn startup_mutation_fuzz_never_panics_and_round_trips_what_it_accepts() {
    let good = hex(STARTUP_HEX);
    let mut rng = Rng(0x1234_5678_9abc_def1);
    for _ in 0..20_000 {
        let mut b = good.clone();
        for _ in 0..1 + rng.below(4) {
            let i = rng.below(b.len() as u64) as usize;
            b[i] = rng.next() as u8;
        }
        if rng.below(4) == 0 {
            b.truncate(rng.below(b.len() as u64 + 1) as usize);
        }
        let nh = rng.below(6) as usize;
        if let Ok(s) = Startup::decode(&b, nh) {
            // whatever it accepts is canonical: encoding it gives back the same bytes
            assert_eq!(s.encode().unwrap(), b);
        }
    }
}

// --- boot image ---

#[test]
fn bootimg_golden_bytes() {
    let want = hex("42 49 4d 47 02 00 00 00 \
         01 00 61 20 00 00 00 02 00 00 00 \
         02 00 62 63 30 00 00 00 03 00 00 00 \
         00 \
         68 69 00 00 00 00 00 00 00 00 00 00 00 00 00 00 \
         78 79 7a 00 00 00 00 00 00 00 00 00 00 00 00 00");
    assert_eq!(want.len(), 64);
    let files: [(&[u8], &[u8]); 2] = [(b"a", b"hi"), (b"bc", b"xyz")];
    assert_eq!(bootimg::build(&files).unwrap(), want);
    let entries = bootimg::parse(&want).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(bootimg::find(&want, &entries, b"a"), Some(&b"hi"[..]));
    assert_eq!(bootimg::find(&want, &entries, b"bc"), Some(&b"xyz"[..]));
    assert_eq!(bootimg::find(&want, &entries, b"zz"), None);
}

#[test]
fn bootimg_rejects_malformed_images() {
    let files: [(&[u8], &[u8]); 2] = [(b"a", b"hi"), (b"bc", b"xyz")];
    let good = bootimg::build(&files).unwrap();
    let mut m = good.clone();
    m[0] = b'X';
    assert_eq!(bootimg::parse(&m), Err(BootImageError::BadMagic));
    for cut in 0..good.len() {
        // a truncated image is refused (51 bytes is the end of the last file: the padding after
        // it is not required)
        if cut < 51 {
            assert!(bootimg::parse(&good[..cut]).is_err(), "cut at {cut}");
        }
    }
    // offset pointing into the table (not 16-aligned and before its end)
    let mut o = good.clone();
    o[11] = 8;
    assert_eq!(bootimg::parse(&o), Err(BootImageError::BadExtent));
    // size past the end
    let mut s = good.clone();
    s[15] = 0xff;
    assert_eq!(bootimg::parse(&s), Err(BootImageError::BadExtent));
    // duplicate names
    // (the builder refuses them now, so make the bytes: rename the second entry "b" to "a")
    let two: [(&[u8], &[u8]); 2] = [(b"a", b"1"), (b"b", b"2")];
    let mut d = bootimg::build(&two).unwrap();
    d[21] = b'a';
    assert_eq!(bootimg::parse(&d), Err(BootImageError::DuplicateName));
    // empty name, too many entries
    let empty: [(&[u8], &[u8]); 1] = [(b"", b"1")];
    assert_eq!(bootimg::build(&empty), Err(BootImageError::BadName));
    let mut c = good.clone();
    c[5] = 1;
    assert_eq!(bootimg::parse(&c), Err(BootImageError::TooMany));
}

#[test]
fn bootimg_fuzz_never_panics() {
    let files: [(&[u8], &[u8]); 3] = [(b"init", b"\x7fELF.."), (b"cat", b"c"), (b"x/y", b"zz")];
    let good = bootimg::build(&files).unwrap();
    let mut rng = Rng(77);
    for _ in 0..20_000 {
        let mut b = good.clone();
        for _ in 0..1 + rng.below(3) {
            let i = rng.below(b.len() as u64) as usize;
            b[i] = rng.next() as u8;
        }
        if let Ok(entries) = bootimg::parse(&b) {
            for e in &entries {
                assert!(bootimg::find(&b, &entries, &e.name).is_some());
            }
        }
    }
}

// --- file protocol ---

#[test]
fn file_request_and_reply_golden_bytes() {
    let req = Request {
        txid: 7,
        op: file::op::READ,
        flags: 0,
        rights: 1,
        a: 16,
        b: 100,
        payload: b"",
    };
    let want = hex("07 00 00 00  0a 00  00 00  01 00 00 00 \
         10 00 00 00 00 00 00 00  64 00 00 00 00 00 00 00  00 00 00 00");
    assert_eq!(want.len(), 32);
    assert_eq!(req.encode().unwrap(), want);
    assert_eq!(file::decode_request(&want).unwrap(), req);
    let rep = Reply {
        txid: 7,
        status: file::status::NOT_FOUND,
        a: 5,
        payload: b"ok",
    };
    let want = hex("07 00 00 00  9c ff ff ff  05 00 00 00 00 00 00 00  02 00 00 00  6f 6b");
    assert_eq!(rep.encode().unwrap(), want);
    assert_eq!(file::decode_reply(&want).unwrap(), rep);
}

#[test]
fn file_framing_errors() {
    let req = Request {
        txid: 1,
        op: file::op::WRITE,
        flags: 0,
        rights: 0,
        a: 0,
        b: 0,
        payload: b"abc",
    };
    let good = req.encode().unwrap();
    assert_eq!(file::decode_request(&good[..31]), Err(FrameError::TooShort));
    let mut long = good.clone();
    long.push(0);
    assert_eq!(file::decode_request(&long), Err(FrameError::BadLength));
    assert_eq!(
        file::decode_request(&good[..good.len() - 1]),
        Err(FrameError::BadLength)
    );
    assert_eq!(
        file::decode_request(&vec![0u8; file::MAX_MESSAGE + 1]),
        Err(FrameError::TooBig)
    );
    assert_eq!(file::peek_txid(&good[..4]), Some(1));
    assert_eq!(file::peek_txid(&good[..3]), None);
    assert_eq!(file::op::handles(file::op::OPEN), Some(1));
    assert_eq!(file::op::handles(file::op::READ), Some(0));
    assert_eq!(file::op::handles(99), None);
}

#[test]
fn path_validation() {
    use PathError::*;
    assert_eq!(file::split_path(b"a"), Ok(vec![&b"a"[..]]));
    assert_eq!(file::split_path(b"a/b/c"), Ok(vec![&b"a"[..], b"b", b"c"]));
    assert_eq!(file::split_path(b""), Err(Malformed));
    assert_eq!(file::split_path(b"/a"), Err(Malformed), "a leading slash");
    assert_eq!(file::split_path(b"a/"), Err(Malformed), "a trailing slash");
    assert_eq!(file::split_path(b"a//b"), Err(Malformed));
    assert_eq!(file::split_path(b"a\0b"), Err(Malformed));
    assert_eq!(file::split_path(b".."), Err(Dots));
    assert_eq!(file::split_path(b"a/../b"), Err(Dots), "no parent, ever");
    assert_eq!(file::split_path(b"./a"), Err(Dots));
    assert_eq!(file::split_path(&vec![b'x'; 256]), Err(TooLong));
    assert!(file::split_path(&vec![b'x'; 255]).is_ok());
    let deep = vec!["a"; 33].join("/");
    assert_eq!(file::split_path(deep.as_bytes()), Err(TooDeep));
    assert!(file::split_path(vec!["a"; 32].join("/").as_bytes()).is_ok());
    let long = vec!["abcdefgh"; 129].join("/");
    assert_eq!(file::split_path(long.as_bytes()), Err(TooLong));
}

#[test]
fn directory_entries_pack_and_unpack() {
    let es = vec![
        DirEntry {
            is_dir: true,
            name: b"sub".to_vec(),
        },
        DirEntry {
            is_dir: false,
            name: b"a.txt".to_vec(),
        },
    ];
    let (bytes, n) = file::encode_entries(&es, 1000);
    assert_eq!(n, 2);
    assert_eq!(bytes, hex("02 03 00 73 75 62  01 05 00 61 2e 74 78 74"));
    assert_eq!(file::decode_entries(&bytes).unwrap(), es);
    let (b2, n2) = file::encode_entries(&es, 8);
    assert_eq!(n2, 1, "only what fits");
    assert_eq!(b2.len(), 6);
    assert!(
        file::decode_entries(&hex("03 01 00 61")).is_err(),
        "unknown kind"
    );
    assert!(
        file::decode_entries(&hex("01 05 00 61")).is_err(),
        "short name"
    );
    assert!(
        file::decode_entries(&hex("01 02 00 2e 2e")).is_err(),
        "dotdot"
    );
}

// --- launcher and console ---

#[test]
fn launch_golden_and_errors() {
    let sp = Spawn {
        txid: 1,
        image_offset: 0x10,
        image_len: 0x100,
        startup: b"ab",
    };
    let want = hex("01 00 00 00  01 00  00 00  10 00 00 00 00 00 00 00 \
         00 01 00 00 00 00 00 00  02 00 00 00  61 62");
    assert_eq!(want.len(), 30);
    assert_eq!(sp.encode().unwrap(), want);
    assert_eq!(launch::decode_spawn(&want).unwrap(), sp);
    assert_eq!(
        launch::decode_spawn(&want[..27]),
        Err(LaunchError::TooShort)
    );
    assert_eq!(
        launch::decode_spawn(&want[..29]),
        Err(LaunchError::BadLength)
    );
    let mut bad_op = want.clone();
    bad_op[4] = 2;
    assert_eq!(launch::decode_spawn(&bad_op), Err(LaunchError::BadOp));
    let mut zero = want.clone();
    zero[16..24].fill(0);
    assert_eq!(launch::decode_spawn(&zero), Err(LaunchError::BadImage));
    let mut huge = want.clone();
    huge[16..24].copy_from_slice(&(launch::MAX_IMAGE + 1).to_le_bytes());
    assert_eq!(launch::decode_spawn(&huge), Err(LaunchError::BadImage));
    let mut wrap = want.clone();
    wrap[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
    assert_eq!(
        launch::decode_spawn(&wrap),
        Err(LaunchError::BadImage),
        "offset + len overflows"
    );
    let mut big = want.clone();
    big[24..28].copy_from_slice(&(16 * 1024 + 1u32).to_le_bytes());
    assert_eq!(launch::decode_spawn(&big), Err(LaunchError::BadStartup));
    assert_eq!(
        launch::decode_reply(&launch::encode_reply(9, -108)),
        Some((9, -108))
    );
    assert_eq!(launch::decode_reply(&[0u8; 7]), None);
    assert_eq!(launch::MAX_HANDLES, 33);
}

#[test]
fn console_control_golden_and_errors() {
    assert_eq!(
        console::encode(5, Control::SetForeground),
        hex("05 00 00 00 01 00 00 00")
    );
    assert_eq!(
        console::encode(5, Control::ClearForeground),
        hex("05 00 00 00 02 00 00 00")
    );
    let set = hex("05 00 00 00 01 00 00 00");
    let clear = hex("05 00 00 00 02 00 00 00");
    assert_eq!(console::decode(&set, 1), Ok((5, Control::SetForeground)));
    assert_eq!(console::decode(&set, 16), Ok((5, Control::SetForeground)));
    assert_eq!(console::decode(&set, 0), Err(ControlError::BadHandles));
    assert_eq!(console::decode(&set, 17), Err(ControlError::BadHandles));
    assert_eq!(
        console::decode(&clear, 0),
        Ok((5, Control::ClearForeground))
    );
    assert_eq!(console::decode(&clear, 1), Err(ControlError::BadHandles));
    assert_eq!(console::decode(&set[..7], 1), Err(ControlError::TooShort));
    assert_eq!(
        console::decode(&hex("05 00 00 00 09 00 00 00"), 1),
        Err(ControlError::BadOp)
    );
    assert_eq!(
        console::decode(&hex("05 00 00 00 01 00 01 00"), 1),
        Err(ControlError::BadOp)
    );
}

#[test]
fn request_fuzz_never_panics() {
    let req = Request {
        txid: 3,
        op: file::op::OPEN,
        flags: 1,
        rights: 3,
        a: 0,
        b: 0,
        payload: b"dir/file",
    };
    let good = req.encode().unwrap();
    let mut rng = Rng(4242);
    for _ in 0..20_000 {
        let mut b = good.clone();
        for _ in 0..1 + rng.below(3) {
            let i = rng.below(b.len() as u64) as usize;
            b[i] = rng.next() as u8;
        }
        b.truncate(rng.below(b.len() as u64 + 1) as usize);
        if let Ok(r) = file::decode_request(&b) {
            let _ = file::split_path(r.payload);
            let _ = file::op::handles(r.op);
        }
        let _ = file::decode_reply(&b);
        let _ = file::decode_entries(&b);
        let _ = launch::decode_spawn(&b);
        let _ = console::decode(&b, 1);
    }
}
