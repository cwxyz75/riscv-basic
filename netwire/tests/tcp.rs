//! H11 and H12 for TCP (S9-SPEC 14): goldens from the real capture, and totality.
use netwire::tcp::{self, Header};
use netwire::WireError;

const PCAP: &[u8] = include_bytes!("../../net/tests/data/slirp-tcp-probe.pcap");

/// Every TCP segment of the capture: (frame number, src ip, dst ip, the IP payload).
type Seg = (usize, [u8; 4], [u8; 4], Vec<u8>);

fn segments() -> Vec<Seg> {
    assert_eq!(
        &PCAP[..4],
        &[0xd4, 0xc3, 0xb2, 0xa1],
        "a little-endian classic pcap"
    );
    let mut out = vec![];
    let (mut i, mut n) = (24usize, 0usize);
    while i + 16 <= PCAP.len() {
        let cl = u32::from_le_bytes(PCAP[i + 8..i + 12].try_into().unwrap()) as usize;
        let f = &PCAP[i + 16..i + 16 + cl];
        i += 16 + cl;
        n += 1;
        // (an IP fragment is not a whole segment: the capture has the two fragments of the reassembly row)
        let frag = u16::from_be_bytes([f[20], f[21]]) & 0x3fff;
        if f.len() >= 54 && f[12..14] == [8, 0] && f[23] == 6 && frag == 0 {
            let ihl = usize::from(f[14] & 15) * 4;
            let total = usize::from(u16::from_be_bytes([f[16], f[17]]));
            out.push((
                n - 1,
                f[26..30].try_into().unwrap(),
                f[30..34].try_into().unwrap(),
                f[14 + ihl..14 + total].to_vec(),
            ));
        }
    }
    out
}

#[test]
fn every_segment_of_the_capture_parses_and_rebuilds_to_the_same_bytes() {
    let segs = segments();
    assert!(
        segs.len() >= 200,
        "the capture has {} TCP segments",
        segs.len()
    );
    let mut syn_with_options = 0;
    let mut data = 0;
    let mut resets = 0;
    for (n, src, dst, bytes) in &segs {
        let t = tcp::parse(bytes, *src, *dst).unwrap_or_else(|e| panic!("frame {n}: {e:?}"));
        let h = Header {
            src_port: t.src_port,
            dst_port: t.dst_port,
            seq: t.seq,
            ack: t.ack,
            flags: t.flags,
            window: t.window,
        };
        let mut buf = vec![0u8; bytes.len()];
        let len = tcp::write(&mut buf, *src, *dst, &h, t.options_raw, t.payload).unwrap();
        assert_eq!(&buf[..len], &bytes[..], "frame {n} rebuilds identically");
        if t.has(tcp::SYN) && !t.options_raw.is_empty() {
            syn_with_options += 1;
        }
        data += usize::from(!t.payload.is_empty());
        resets += usize::from(t.has(tcp::RST));
    }
    assert!(
        syn_with_options >= 5 && data >= 90 && resets >= 3,
        "{syn_with_options} {data} {resets}"
    );
}

#[test]
fn the_syn_with_four_options_and_slirp_s_answer_parse_as_measured() {
    let segs = segments();
    let first_syn = segs
        .iter()
        .map(|(_, s, d, b)| tcp::parse(b, *s, *d).unwrap())
        .find(|t| t.has(tcp::SYN) && !t.has(tcp::ACK) && t.options.window_scale_offered)
        .expect("the guest's SYN offering window scale, SACK and timestamps");
    assert_eq!(first_syn.options.mss, Some(1460));
    assert!(first_syn.options.sack_permitted_offered && first_syn.options.timestamps_offered);
    // slirp's SYN-ACK: only the MSS
    let sa = segs
        .iter()
        .map(|(_, s, d, b)| tcp::parse(b, *s, *d).unwrap())
        .find(|t| t.has(tcp::SYN) && t.has(tcp::ACK) && t.src_port != 0)
        .unwrap();
    assert_eq!(sa.options.mss, Some(1460));
    assert!(
        !sa.options.window_scale_offered
            && !sa.options.sack_permitted_offered
            && !sa.options.timestamps_offered
    );
    assert_eq!(sa.window, 65535);
}

#[test]
fn a_corrupted_byte_anywhere_is_a_bad_checksum_or_a_bad_header() {
    let segs = segments();
    let (_, src, dst, bytes) = segs.iter().find(|s| s.3.len() > 100).unwrap();
    for i in 0..bytes.len() {
        let mut b = bytes.clone();
        b[i] ^= 0x01;
        match tcp::parse(&b, *src, *dst) {
            Err(WireError::BadChecksum | WireError::Malformed | WireError::Truncated) => {}
            Ok(_) => panic!("a flipped bit at byte {i} was accepted"),
            Err(e) => panic!("unexpected {e:?}"),
        }
    }
}

// ---- options ----

#[test]
fn options_follow_the_rules_of_5_1() {
    let o = |raw: &[u8], syn| tcp::parse_options(raw, syn);
    assert_eq!(o(&[2, 4, 5, 0xb4], true).mss, Some(1460));
    assert_eq!(
        o(&[2, 4, 5, 0xb4], false).mss,
        None,
        "the MSS option counts only on a SYN"
    );
    assert_eq!(
        o(&[2, 3, 5, 0xb4], true).mss,
        None,
        "an MSS option of the wrong length"
    );
    assert_eq!(
        o(&[1, 1, 2, 4, 2, 24], true).mss,
        Some(536),
        "no-ops before it"
    );
    assert_eq!(
        o(&[0, 2, 4, 5, 0xb4], true).mss,
        None,
        "end of options stops the parse"
    );
    // a length of 0 or 1 ends the parse; so does one that runs past the header; what came before still counts
    assert_eq!(o(&[2, 4, 5, 0xb4, 99, 0, 3, 3, 7], true).mss, Some(1460));
    assert!(!o(&[99, 1, 3, 3, 7, 0], true).window_scale_offered);
    assert!(
        !o(&[3, 5, 7, 0], true).window_scale_offered,
        "length past the end"
    );
    // unknown kinds are skipped by length; known ones are only noted
    let x = o(
        &[77, 3, 0, 3, 3, 7, 4, 2, 8, 10, 0, 0, 0, 1, 0, 0, 0, 0, 5, 2],
        true,
    );
    assert!(
        x.window_scale_offered && x.sack_permitted_offered && x.timestamps_offered && x.sack_seen
    );
    assert_eq!(x.mss, None);
}

#[test]
fn the_builder_refuses_bad_options_and_small_buffers() {
    let h = Header {
        flags: tcp::SYN,
        ..Header::default()
    };
    let mut b = [0u8; 100];
    assert_eq!(
        tcp::write(&mut b, [1; 4], [2; 4], &h, &[1, 1, 1], b""),
        Err(WireError::Malformed)
    );
    assert_eq!(
        tcp::write(&mut b, [1; 4], [2; 4], &h, &[1; 44], b""),
        Err(WireError::Malformed)
    );
    assert_eq!(
        tcp::write(&mut b[..19], [1; 4], [2; 4], &h, b"", b""),
        Err(WireError::NoSpace)
    );
    assert_eq!(
        tcp::write(&mut b[..24], [1; 4], [2; 4], &h, b"", b"abcde"),
        Err(WireError::NoSpace)
    );
    let n = tcp::write(&mut b, [1; 4], [2; 4], &h, &tcp::mss_option(1460), b"hi").unwrap();
    assert_eq!(n, 26);
    let t = tcp::parse(&b[..n], [1; 4], [2; 4]).unwrap();
    assert_eq!((t.options.mss, t.payload), (Some(1460), &b"hi"[..]));
}

#[test]
fn sequence_space_counts_syn_and_fin() {
    let mut b = [0u8; 64];
    for (flags, data, want) in [
        (tcp::ACK, 5usize, 5u32),
        (tcp::SYN, 0, 1),
        (tcp::FIN | tcp::ACK, 3, 4),
        (tcp::SYN | tcp::FIN, 0, 2),
    ] {
        let h = Header {
            flags,
            ..Header::default()
        };
        let n = tcp::write(&mut b, [1; 4], [2; 4], &h, b"", &vec![7u8; data]).unwrap();
        assert_eq!(tcp::parse(&b[..n], [1; 4], [2; 4]).unwrap().seg_len(), want);
    }
}

// ---- H12: totality ----

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

#[test]
fn random_bytes_truncations_and_mutations_never_panic() {
    let segs = segments();
    let mut r = Rng(0x9e3779b97f4a7c15);
    let mut ok = 0u32;
    for case in 0..200_000u32 {
        let (src, dst) = ([10, 0, 2, 15], [10, 0, 2, 2]);
        let bytes: Vec<u8> = match case % 3 {
            0 => (0..r.next() % 80).map(|_| r.next() as u8).collect(),
            1 => {
                let (_, _, _, b) = &segs[(r.next() as usize) % segs.len()];
                b[..(r.next() as usize) % (b.len() + 1)].to_vec()
            }
            _ => {
                let (_, _, _, b) = &segs[(r.next() as usize) % segs.len()];
                let mut b = b.clone();
                for _ in 0..1 + r.next() % 3 {
                    let i = (r.next() as usize) % b.len();
                    b[i] = r.next() as u8;
                }
                b
            }
        };
        if let Ok(t) = tcp::parse(&bytes, src, dst) {
            ok += 1;
            // whatever parsed is internally consistent
            assert!(
                t.options_raw.len() <= 40
                    && t.options_raw.len() + t.payload.len() + 20 == bytes.len()
            );
            let _ = t.seg_len();
        }
        // the option parser alone, on arbitrary bytes, with and without SYN
        let _ = tcp::parse_options(&bytes, case % 2 == 0);
    }
    assert!(
        ok > 0,
        "some mutants still parse (a mutation of data bytes keeps a valid segment only by luck)"
    );
}
