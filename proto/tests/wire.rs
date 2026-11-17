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
    // unknown role (38) and duplicate role (both Stdin)
    let mut r = good.clone();
    let n = r.len();
    r[n - 8] = 38;
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
            is_link: false,
            name: b"sub".to_vec(),
        },
        DirEntry {
            is_dir: false,
            is_link: false,
            name: b"a.txt".to_vec(),
        },
        DirEntry {
            is_dir: false,
            is_link: true,
            name: b"l".to_vec(),
        },
    ];
    let (bytes, n) = file::encode_entries(&es, 1000);
    assert_eq!(n, 3);
    assert_eq!(
        bytes,
        hex("02 03 00 73 75 62  01 05 00 61 2e 74 78 74  03 01 00 6c")
    );
    assert_eq!(file::decode_entries(&bytes).unwrap(), es);
    let (b2, n2) = file::encode_entries(&es[..2], 8);
    assert_eq!(n2, 1, "only what fits");
    assert_eq!(b2.len(), 6);
    assert!(
        file::decode_entries(&hex("04 01 00 61")).is_err(),
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

// ---- S9: stream endpoints (S9-SPEC 8) ----

mod streams {
    use proto::net::{self as pn, EpEvent, EpRequest, OpenKind};

    #[test]
    fn open_kinds_are_classified() {
        assert_eq!(pn::classify_open_kind(pn::kind::UDP), OpenKind::Udp);
        assert_eq!(
            pn::classify_open_kind(pn::kind::ICMP_ECHO),
            OpenKind::IcmpEcho
        );
        assert_eq!(pn::classify_open_kind(3), OpenKind::TcpStream);
        assert_eq!(pn::classify_open_kind(4), OpenKind::TcpListener);
        for k in [0u16, 5, 6, 255, 256, 0xffff] {
            assert_eq!(pn::classify_open_kind(k), OpenKind::Invalid, "{k}");
        }
    }

    #[test]
    fn stream_values_are_the_documented_numbers() {
        assert_eq!(
            [
                pn::status::CONN_REFUSED,
                pn::status::TIMED_OUT,
                pn::status::CONN_RESET,
                pn::status::NOT_CONNECTED,
                pn::status::ALREADY_CONNECTED,
                pn::status::HOST_UNREACHABLE,
                pn::status::BAD_HANDLE
            ],
            [10, 11, 12, 13, 14, 15, 16]
        );
        assert_eq!(
            [
                pn::ep::CONNECT,
                pn::ep::ABORT,
                pn::ep::SET_OPTION,
                pn::ep::ACCEPT
            ],
            [16, 17, 18, 19]
        );
        assert_eq!(
            [
                pn::notice::CONNECTED,
                pn::notice::CONNECT_FAILED,
                pn::notice::PEER_FIN,
                pn::notice::RESET,
                pn::notice::ACCEPTED,
                pn::notice::ACCEPT_STARVED
            ],
            [16, 17, 18, 19, 20, 21]
        );
        assert_eq!(pn::oflags::NODELAY, 4);
        // no S8 value was disturbed
        assert_eq!(
            [
                pn::ep::SEND_UDP,
                pn::ep::SEND_ECHO,
                pn::ep::RECV,
                pn::ep::ECHO_REPLY,
                pn::ep::NOTICE
            ],
            [1, 2, 0x81, 0x82, 0x83]
        );
    }

    #[test]
    fn requests_round_trip_and_match_their_goldens() {
        let c = pn::encode_connect(7, [10, 0, 2, 2], 8080, 16384, 32768);
        assert_eq!(
            c,
            [
                16, 0, 0, 0, 7, 0, 0, 0, 10, 0, 2, 2, 0x90, 0x1f, 0, 0, 0, 0x40, 0, 0, 0, 0x80, 0,
                0
            ]
        );
        assert_eq!(
            pn::decode_ep_request(&c),
            Some(EpRequest::Connect {
                cookie: 7,
                dst: [10, 0, 2, 2],
                port: 8080,
                snd_buf: 16384,
                rcv_buf: 32768
            })
        );
        assert_eq!(pn::encode_abort(), [17, 0, 0, 0]);
        assert_eq!(
            pn::decode_ep_request(&pn::encode_abort()),
            Some(EpRequest::Abort)
        );
        let o = pn::encode_set_option(pn::option::NODELAY, 1);
        assert_eq!(o, [18, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0]);
        assert_eq!(
            pn::decode_ep_request(&o),
            Some(EpRequest::SetOption {
                option: 1,
                value: 1
            })
        );
        let a = pn::encode_accept(9, 0, 0);
        assert_eq!(a, [19, 0, 0, 0, 9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            pn::decode_ep_request(&a),
            Some(EpRequest::Accept {
                cookie: 9,
                snd_buf: 0,
                rcv_buf: 0
            })
        );
    }

    #[test]
    fn the_stream_messages_are_total_over_every_truncation_and_bit_flip() {
        let msgs = [
            pn::encode_connect(1, [1, 2, 3, 4], 5, 6, 7),
            pn::encode_abort(),
            pn::encode_set_option(1, 1),
            pn::encode_accept(1, 2, 3),
            pn::encode_conn_info(
                pn::notice::CONNECTED,
                4,
                ([10, 0, 2, 15], 40000),
                ([10, 0, 2, 2], 80),
            ),
        ];
        for m in &msgs {
            for n in 0..=m.len() {
                let _ = pn::decode_ep_request(&m[..n]);
                let _ = pn::decode_ep_event(&m[..n]);
            }
            for i in 0..m.len() {
                for bit in 0..8 {
                    let mut x = m.clone();
                    x[i] ^= 1 << bit;
                    let _ = pn::decode_ep_request(&x);
                    let _ = pn::decode_ep_event(&x);
                }
            }
        }
        // a wrong length or a non-zero pad is refused, never half-read
        let mut c = pn::encode_connect(1, [1, 2, 3, 4], 5, 6, 7);
        c.push(0);
        assert_eq!(pn::decode_ep_request(&c), None);
        let mut c = pn::encode_connect(1, [1, 2, 3, 4], 5, 6, 7);
        c[2] = 1;
        assert_eq!(pn::decode_ep_request(&c), None);
        assert_eq!(pn::decode_ep_request(&[16, 0, 0, 0]), None);
    }

    #[test]
    fn conn_info_round_trips() {
        let b = pn::encode_conn_info(
            pn::notice::ACCEPTED,
            0xdead_beef,
            ([10, 0, 2, 15], 80),
            ([10, 0, 2, 2], 51234),
        );
        assert_eq!(b.len(), 24);
        assert_eq!(
            pn::decode_ep_event(&b),
            Some(EpEvent::ConnInfo {
                code: pn::notice::ACCEPTED,
                cookie: 0xdead_beef,
                local: ([10, 0, 2, 15], 80),
                remote: ([10, 0, 2, 2], 51234)
            })
        );
    }

    #[test]
    fn notices_and_replies_with_stream_codes_decode() {
        for code in 16..=21u16 {
            let b = pn::encode_notice(code, 5, 6);
            assert_eq!(
                pn::decode_ep_event(&b),
                Some(EpEvent::Notice {
                    code,
                    cookie: 5,
                    detail: 6
                })
            );
        }
        for s in 10..=16 {
            assert_eq!(
                pn::decode_reply(&pn::encode_reply(9, s, &[])),
                Some((9, s, &[][..]))
            );
        }
    }

    #[test]
    fn stream_limits_are_the_documented_ones() {
        assert_eq!(pn::limits::STREAM_BUF_DEFAULT, 16384);
        assert_eq!(
            (pn::limits::STREAM_BUF_MIN, pn::limits::STREAM_BUF_MAX),
            (4096, 65536)
        );
        assert_eq!(pn::limits::STREAM_RCV_BUF_MAX, 65535);
        assert_eq!(pn::limits::POSTED_ACCEPTS_MAX, 16);
    }
}

// --- attributes, queries and symlinks (FS-SPEC 9.1): goldens worked out by hand from the field lists in `proto::file` ---

#[test]
fn attribute_and_index_payload_goldens() {
    // ty 2 (U64), name "age" (len 3), value 7 as 8 little-endian bytes
    let p = file::encode_attr_set(2, b"age", &7u64.to_le_bytes());
    assert_eq!(p, hex("02 03 00 61 67 65 07 00 00 00 00 00 00 00"));
    assert_eq!(
        file::decode_attr_set(&p),
        Ok((2, &b"age"[..], &7u64.to_le_bytes()[..]))
    );
    // an empty value is allowed; the name length may not run past the end
    assert_eq!(
        file::decode_attr_set(&hex("00 01 00 6b")),
        Ok((0, &b"k"[..], &b""[..]))
    );
    assert_eq!(
        file::decode_attr_set(&hex("00 05 00 6b")),
        Err(FrameError::BadLength)
    );
    assert_eq!(
        file::decode_attr_set(&hex("00 05")),
        Err(FrameError::TooShort)
    );
    assert_eq!(file::decode_attr_set(&[]), Err(FrameError::TooShort));
    // MAKE_INDEX: ty 2, name "year"
    let p = file::encode_make_index(2, b"year");
    assert_eq!(p, hex("02 79 65 61 72"));
    assert_eq!(file::decode_make_index(&p), Ok((2, &b"year"[..])));
    assert_eq!(file::decode_make_index(&[]), Err(FrameError::TooShort));
    // ATTR_LIST's reply
    let (bytes, n) = file::encode_names(&[b"a".to_vec(), b"bc".to_vec(), b"def".to_vec()], 100);
    assert_eq!(
        (bytes.clone(), n),
        (hex("01 00 61 02 00 62 63 03 00 64 65 66"), 3)
    );
    assert_eq!(
        file::decode_names(&bytes),
        Ok(vec![b"a".to_vec(), b"bc".to_vec(), b"def".to_vec()])
    );
    let (bytes, n) = file::encode_names(&[b"a".to_vec(), b"bc".to_vec(), b"def".to_vec()], 7);
    assert_eq!(
        (bytes, n),
        (hex("01 00 61 02 00 62 63"), 2),
        "stops before the limit"
    );
    assert_eq!(
        file::decode_names(&hex("03 00 61 62")),
        Err(FrameError::BadLength)
    );
}

#[test]
fn query_goldens_and_rejections() {
    // Eq on index "year": flags 1; payload: index (len 4) "year", value (len 8) 1999 little-endian
    let q = file::Query {
        index: b"year",
        term: file::QueryTerm::Eq(&1999u64.to_le_bytes()),
        after: None,
        max: 50,
    };
    let (flags, payload) = q.encode();
    assert_eq!(flags, 1);
    assert_eq!(
        payload,
        hex("04 00 79 65 61 72 08 00 cf 07 00 00 00 00 00 00")
    );
    assert_eq!(file::decode_query(50, flags, &payload), Ok(q));
    // a range with a lower bound and a position to continue after: flags LO | AFTER = 2 | 8 = 10
    let lo = 5u64.to_le_bytes();
    let key = [0, 0, 0, 0, 0, 0, 7, 0xcf];
    let q = file::Query {
        index: b"year",
        term: file::QueryTerm::Range {
            lo: Some(&lo),
            hi: None,
        },
        after: Some((&key, 5)),
        max: 1000,
    };
    let (flags, payload) = q.encode();
    assert_eq!(flags, 10);
    assert_eq!(
        payload,
        hex("04 00 79 65 61 72  08 00 05 00 00 00 00 00 00 00  08 00 00 00 00 00 00 00 07 cf  05 00 00 00 00 00 00 00")
    );
    assert_eq!(file::decode_query(1000, flags, &payload), Ok(q));
    // both bounds, no position: flags LO | HI = 6
    let hi = 9u64.to_le_bytes();
    let q = file::Query {
        index: b"n",
        term: file::QueryTerm::Range {
            lo: Some(&lo),
            hi: Some(&hi),
        },
        after: None,
        max: 3,
    };
    let (flags, payload) = q.encode();
    assert_eq!(flags, 6);
    assert_eq!(file::decode_query(3, flags, &payload), Ok(q));
    // an open range: flags 0, only the index
    assert_eq!(
        file::decode_query(7, 0, &hex("01 00 6e")),
        Ok(file::Query {
            index: b"n",
            term: file::QueryTerm::Range { lo: None, hi: None },
            after: None,
            max: 7
        })
    );
    // rejections: an undefined flag, Eq together with a bound, a missing field, bytes left over
    assert_eq!(
        file::decode_query(1, 16, &hex("01 00 6e")),
        Err(FrameError::BadBits)
    );
    assert_eq!(
        file::decode_query(1, 1 | 2, &hex("01 00 6e 00 00")),
        Err(FrameError::BadBits)
    );
    assert_eq!(
        file::decode_query(1, 1, &hex("01 00 6e")),
        Err(FrameError::TooShort)
    );
    assert_eq!(
        file::decode_query(1, 0, &hex("01 00 6e 00")),
        Err(FrameError::BadLength)
    );
    assert_eq!(
        file::decode_query(1, 8, &hex("01 00 6e 01 00 6b 05 00")),
        Err(FrameError::TooShort)
    );
}

#[test]
fn query_hit_goldens() {
    // key 1999 big-endian, node 3, path "d/a"
    let h = file::QueryHit {
        key: vec![0, 0, 0, 0, 0, 0, 7, 0xcf],
        id: 3,
        path: b"d/a".to_vec(),
    };
    let (bytes, n) = file::encode_hits(std::slice::from_ref(&h), 100);
    assert_eq!(n, 1);
    assert_eq!(
        bytes,
        hex("08 00 00 00 00 00 00 00 07 cf 03 00 00 00 00 00 00 00 03 00 64 2f 61")
    );
    assert_eq!(file::decode_hits(&bytes), Ok(vec![h.clone()]));
    let two = [h.clone(), h.clone()];
    let (bytes, n) = file::encode_hits(&two, 30);
    assert_eq!((n, bytes.len()), (1, 23), "the second hit does not fit");
    assert_eq!(
        file::decode_hits(&hex("08 00 00")),
        Err(FrameError::BadLength)
    );
    assert_eq!(
        file::decode_hits(&hex("00 00 01 00")),
        Err(FrameError::TooShort)
    );
}

#[test]
fn symlink_payload_and_target_rule() {
    let (a, payload) = file::encode_symlink(b"d/l", b"../x");
    assert_eq!((a, payload.clone()), (3, b"d/l../x".to_vec()));
    assert_eq!(
        file::decode_symlink(a, &payload),
        Ok((&b"d/l"[..], &b"../x"[..]))
    );
    assert_eq!(
        file::decode_symlink(99, &payload),
        Err(FrameError::BadLength)
    );
    // the target rule, with the depth of the link's directory below the connection root
    for (target, depth, ok) in [
        (&b"x"[..], 0, true),
        (b"../x", 0, false),
        (b"../x", 1, true),
        (b"../../x", 1, false),
        (b"a/../../x", 1, true),
        (b"a/../../../x", 1, false),
        (b"./x", 0, true),
        (b"/x", 5, false),
        (b"", 0, false),
        (b"x/", 0, false),
        (b"x//y", 0, false),
        (b"x\0", 0, false),
    ] {
        assert_eq!(
            file::check_link_target(target, depth).is_ok(),
            ok,
            "{:?} at depth {depth}",
            String::from_utf8_lossy(target)
        );
    }
    assert!(
        file::check_link_target(&vec![b'x'; 1024], 0).is_err(),
        "one 1024-byte component is too long as a name"
    );
    assert!(
        file::check_link_target(&[b"x/".repeat(520), b"x".to_vec()].concat(), 0).is_err(),
        "over 1024 bytes"
    );
}

#[test]
fn the_new_operations_have_numbers_handle_counts_and_kinds() {
    use file::op;
    for (o, n) in [
        (op::ATTR_SET, 20),
        (op::ATTR_GET, 21),
        (op::ATTR_REMOVE, 22),
        (op::ATTR_LIST, 23),
        (op::MAKE_INDEX, 24),
        (op::QUERY, 25),
        (op::SYMLINK, 26),
        (op::READLINK, 27),
    ] {
        assert_eq!(o, n);
        assert_eq!(op::handles(o), Some(0));
    }
    for o in [op::MAKE_INDEX, op::QUERY, op::SYMLINK, op::READLINK] {
        assert!(op::is_directory_op(o));
    }
    for o in [op::ATTR_SET, op::ATTR_GET, op::ATTR_REMOVE, op::ATTR_LIST] {
        assert!(
            !op::is_directory_op(o),
            "attribute operations act on any connection's node"
        );
    }
    assert_eq!(op::SYNC, 28);
    assert_eq!(op::handles(op::SYNC), Some(0));
    assert!(
        !op::is_directory_op(op::SYNC),
        "sync is valid on either kind of connection"
    );
    assert_eq!(file::status::IO_ERROR, -114);
    assert_eq!(op::handles(29), None, "29 is not defined");
    assert_eq!(
        [
            file::status::NO_ATTR,
            file::status::NO_INDEX,
            file::status::BAD_VALUE,
            file::status::BAD_TARGET
        ],
        [-110, -111, -112, -113]
    );
}

#[test]
fn the_new_decoders_never_panic_on_any_bytes() {
    let mut r = Rng(0x5eed);
    for _ in 0..20_000 {
        let n = r.below(40) as usize;
        let b: Vec<u8> = (0..n)
            .map(|_| {
                if r.below(3) == 0 {
                    0
                } else {
                    r.below(256) as u8
                }
            })
            .collect();
        let _ = file::decode_attr_set(&b);
        let _ = file::decode_make_index(&b);
        let _ = file::decode_names(&b);
        let _ = file::decode_hits(&b);
        let _ = file::decode_symlink(r.below(50), &b);
        let _ = file::decode_query(r.below(10), r.below(32), &b);
        let _ = file::check_link_target(&b, r.below(4) as usize);
    }
}

#[test]
fn queries_and_hits_survive_a_round_trip() {
    let mut r = Rng(77);
    for _ in 0..2_000 {
        let rnd =
            |r: &mut Rng| -> Vec<u8> { (0..r.below(12)).map(|_| r.below(256) as u8).collect() };
        let index = rnd(&mut r);
        let (lo, hi, eq, key) = (rnd(&mut r), rnd(&mut r), rnd(&mut r), rnd(&mut r));
        let term = match r.below(4) {
            0 => file::QueryTerm::Eq(&eq),
            1 => file::QueryTerm::Range {
                lo: Some(&lo),
                hi: None,
            },
            2 => file::QueryTerm::Range {
                lo: None,
                hi: Some(&hi),
            },
            _ => file::QueryTerm::Range {
                lo: Some(&lo),
                hi: Some(&hi),
            },
        };
        let after = (r.below(2) == 0).then_some((key.as_slice(), r.below(1 << 20) * 0x1_0000_0001));
        let q = file::Query {
            index: &index,
            term,
            after,
            max: r.below(2000),
        };
        let (flags, payload) = q.encode();
        assert_eq!(file::decode_query(q.max, flags, &payload).as_ref(), Ok(&q));
        let hits: Vec<file::QueryHit> = (0..r.below(6))
            .map(|_| file::QueryHit {
                key: rnd(&mut r),
                id: r.below(1 << 30) * 0x1_0000_0001,
                path: rnd(&mut r),
            })
            .collect();
        let (bytes, n) = file::encode_hits(&hits, 4096);
        assert_eq!(file::decode_hits(&bytes).unwrap(), hits[..n].to_vec());
    }
}

#[test]
fn stat_payload_goldens_and_the_advisory_attribute_helper() {
    use file::{StatInfo, ABSENT};
    let s = StatInfo {
        kind: 1,
        size: 27,
        mtime: 0x0102_0304_0506_0708,
        mode: 0o644,
        uid: 1000,
        gid: ABSENT,
    };
    let p = s.encode_payload();
    assert_eq!(
        p,
        hex("01  08 07 06 05 04 03 02 01  a4 01 00 00  e8 03 00 00  ff ff ff ff")
    );
    assert_eq!(StatInfo::decode(27, &p), Ok(s));
    // a wrong length or an unknown kind is an error, never a misread
    assert!(StatInfo::decode(0, &p[..20]).is_err());
    assert!(StatInfo::decode(
        0,
        &hex("09  00 00 00 00 00 00 00 00  00 00 00 00  00 00 00 00  00 00 00 00")
    )
    .is_err());
    for n in 0..p.len() {
        assert!(StatInfo::decode(0, &p[..n]).is_err());
    }
    // the attribute helper: little-endian u64, absent when missing, malformed or too big for 32 bits
    assert_eq!(file::attr_u32(Some(&0o755u64.to_le_bytes())), 0o755);
    assert_eq!(file::attr_u32(None), ABSENT);
    assert_eq!(file::attr_u32(Some(&[1, 2, 3])), ABSENT);
    assert_eq!(file::attr_u32(Some(&(1u64 << 40).to_le_bytes())), ABSENT);
    assert_eq!(
        file::attr_u32(Some(&(u32::MAX as u64).to_le_bytes())),
        ABSENT,
        "the sentinel itself is not a value"
    );
    assert_eq!(file::ATTR_POSIX_MODE, b"posix.mode");
}
