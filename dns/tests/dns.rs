use dns::*;

fn hdr(id: u16, flags: u16, qd: u16, an: u16) -> Vec<u8> {
    let mut m = id.to_be_bytes().to_vec();
    m.extend_from_slice(&flags.to_be_bytes());
    m.extend_from_slice(&qd.to_be_bytes());
    m.extend_from_slice(&an.to_be_bytes());
    m.extend_from_slice(&[0, 0, 0, 0]);
    m
}

fn name_bytes(n: &str) -> Vec<u8> {
    let mut b = vec![];
    for l in n.split('.') {
        b.push(l.len() as u8);
        b.extend_from_slice(l.as_bytes());
    }
    b.push(0);
    b
}

fn question(n: &str) -> Vec<u8> {
    let mut q = name_bytes(n);
    q.extend_from_slice(&[0, 1, 0, 1]);
    q
}

/// A record whose owner is a pointer to offset 12 (the question's name).
fn rr_ptr(ty: u16, ttl: u32, rdata: &[u8]) -> Vec<u8> {
    let mut r = vec![0xc0, 12];
    r.extend_from_slice(&ty.to_be_bytes());
    r.extend_from_slice(&[0, 1]);
    r.extend_from_slice(&ttl.to_be_bytes());
    r.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
    r.extend_from_slice(rdata);
    r
}

fn response(id: u16, qname: &str, rcode: u16, answers: &[Vec<u8>]) -> Vec<u8> {
    let mut m = hdr(id, 0x8180 | rcode, 1, answers.len() as u16);
    m.extend(question(qname));
    for a in answers {
        m.extend_from_slice(a);
    }
    m
}

#[test]
fn a_query_is_built_as_the_rfc_says() {
    let q = build_query(0x1234, b"Example.com").unwrap();
    assert_eq!(&q[..12], &[0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
    assert_eq!(
        &q[12..],
        &[7, b'E', b'x', b'a', b'm', b'p', b'l', b'e', 3, b'c', b'o', b'm', 0, 0, 1, 0, 1]
    );
    assert_eq!(build_query(1, b"a.b.").unwrap().len(), 12 + 5 + 4);
    for bad in [
        "",
        ".",
        "a..b",
        "-.",
        "a b.com",
        &"x".repeat(64),
        &format!("{}.com", "a".repeat(250)),
    ] {
        assert_eq!(
            build_query(1, bad.as_bytes()),
            Err(Error::BadName),
            "{bad:?}"
        );
    }
    assert!(valid_name(b"my_host-1.example.org."));
}

#[test]
fn a_plain_answer_gives_its_addresses() {
    let m = response(
        7,
        "example.com",
        0,
        &[
            rr_ptr(1, 300, &[93, 184, 216, 34]),
            rr_ptr(1, 60, &[93, 184, 216, 35]),
        ],
    );
    assert_eq!(
        parse_response(&m, 7, b"example.com"),
        Ok(Some(Reply::Addresses {
            addrs: vec![[93, 184, 216, 34], [93, 184, 216, 35]],
            ttl: 60
        }))
    );
    // the name compares without regard to case
    assert!(matches!(
        parse_response(&m, 7, b"EXAMPLE.com."),
        Ok(Some(Reply::Addresses { .. }))
    ));
}

#[test]
fn answers_that_are_not_ours_are_ignored() {
    let good = response(7, "example.com", 0, &[rr_ptr(1, 300, &[1, 2, 3, 4])]);
    assert_eq!(
        parse_response(&good, 8, b"example.com"),
        Ok(None),
        "another identifier"
    );
    assert_eq!(
        parse_response(&good, 7, b"other.com"),
        Ok(None),
        "another question"
    );
    let mut query = good.clone();
    query[2] &= 0x7f; // QR clear: a query
    assert_eq!(parse_response(&query, 7, b"example.com"), Ok(None));
    let mut two = good.clone();
    two[5] = 2; // two questions
    assert_eq!(parse_response(&two, 7, b"example.com"), Ok(None));
    assert_eq!(
        parse_response(&good[..11], 7, b"example.com"),
        Err(Error::Malformed)
    );
}

#[test]
fn nxdomain_servfail_and_no_data() {
    assert_eq!(
        parse_response(&response(1, "a.b", 3, &[]), 1, b"a.b"),
        Ok(Some(Reply::NoSuchName))
    );
    assert_eq!(
        parse_response(&response(1, "a.b", 2, &[]), 1, b"a.b"),
        Ok(Some(Reply::ServerFailure))
    );
    assert_eq!(
        parse_response(&response(1, "a.b", 5, &[]), 1, b"a.b"),
        Ok(Some(Reply::ServerFailure))
    );
    assert_eq!(
        parse_response(&response(1, "a.b", 0, &[]), 1, b"a.b"),
        Ok(Some(Reply::NoData))
    );
    // an AAAA only
    assert_eq!(
        parse_response(
            &response(1, "a.b", 0, &[rr_ptr(28, 60, &[0; 16])]),
            1,
            b"a.b"
        ),
        Ok(Some(Reply::NoData))
    );
}

#[test]
fn a_cname_chain_is_followed_inside_the_answer() {
    // www.example.com CNAME host.example.net ; host.example.net A 10.0.0.1 (owner spelled out, not a pointer)
    let mut cname_rdata = name_bytes("host.example.net");
    let cname = rr_ptr(5, 100, &cname_rdata);
    let mut a = name_bytes("host.example.net");
    a.extend_from_slice(&[0, 1, 0, 1, 0, 0, 0, 50, 0, 4, 10, 0, 0, 1]);
    let m = response(9, "www.example.com", 0, &[cname, a]);
    assert_eq!(
        parse_response(&m, 9, b"www.example.com"),
        Ok(Some(Reply::Addresses {
            addrs: vec![[10, 0, 0, 1]],
            ttl: 50
        }))
    );
    // an A record for an unrelated owner is not taken
    cname_rdata = name_bytes("elsewhere.test");
    let mut stray = name_bytes("unrelated.test");
    stray.extend_from_slice(&[0, 1, 0, 1, 0, 0, 0, 50, 0, 4, 6, 6, 6, 6]);
    let m = response(
        9,
        "www.example.com",
        0,
        &[rr_ptr(5, 100, &cname_rdata), stray],
    );
    assert_eq!(
        parse_response(&m, 9, b"www.example.com"),
        Ok(Some(Reply::NoData))
    );
    // more than eight links is refused
    let links: Vec<Vec<u8>> = (0..9)
        .map(|i| {
            let mut r = name_bytes(&format!("n{i}.t"));
            r.extend_from_slice(&[0, 5, 0, 1, 0, 0, 0, 9]);
            let t = name_bytes(&format!("n{}.t", i + 1));
            r.extend_from_slice(&(t.len() as u16).to_be_bytes());
            r.extend(t);
            r
        })
        .collect();
    let mut first = rr_ptr(5, 9, &name_bytes("n0.t"));
    first.truncate(first.len());
    let mut all = vec![first];
    all.extend(links);
    assert_eq!(
        parse_response(&response(3, "start.t", 0, &all), 3, b"start.t"),
        Err(Error::Malformed)
    );
}

#[test]
fn compression_pointers_cannot_loop_or_point_forward() {
    // a name that is a pointer to itself
    let mut m = hdr(1, 0x8180, 1, 0);
    m.extend_from_slice(&[0xc0, 12, 0, 1, 0, 1]);
    assert_eq!(parse_response(&m, 1, b"x"), Err(Error::Malformed));
    // a pointer to a later offset
    let mut m = hdr(1, 0x8180, 1, 0);
    m.extend_from_slice(&[0xc0, 40, 0, 1, 0, 1]);
    assert_eq!(parse_response(&m, 1, b"x"), Err(Error::Malformed));
    // a pointer to a later offset that holds a perfectly good name is still refused (pointers go backwards only)
    let mut m = hdr(1, 0x8180, 1, 0);
    m.extend_from_slice(&[0xc0, 18, 0, 1, 0, 1]); // offset 12: pointer to 18
    m.extend_from_slice(&name_bytes("x")); // offset 18: the name
    assert_eq!(read_name(&m, 12), Err(Error::Malformed));
    // a long chain of backward pointers stops at the jump limit
    let mut m = vec![0u8; 12];
    m.extend_from_slice(&name_bytes("a"));
    let mut at = 12;
    for _ in 0..40 {
        let here = m.len();
        m.extend_from_slice(&[0xc0 | (at >> 8) as u8, at as u8]);
        at = here;
    }
    assert_eq!(read_name(&m, at), Err(Error::Malformed));
    // reserved label types
    assert_eq!(read_name(&[0x80, 0], 0), Err(Error::Malformed));
}

#[test]
fn a_truncated_answer_uses_a_record_it_already_holds_and_otherwise_says_so() {
    let mut m = response(
        5,
        "big.test",
        0,
        &[rr_ptr(1, 30, &[1, 1, 1, 1]), rr_ptr(1, 30, &[2, 2, 2, 2])],
    );
    m[2] |= 0x02; // TC
    m.truncate(m.len() - 6); // the second record is cut off
    assert_eq!(
        parse_response(&m, 5, b"big.test"),
        Ok(Some(Reply::Addresses {
            addrs: vec![[1, 1, 1, 1]],
            ttl: 30
        }))
    );
    let mut empty = response(5, "big.test", 0, &[]);
    empty[2] |= 0x02;
    assert_eq!(
        parse_response(&empty, 5, b"big.test"),
        Ok(Some(Reply::Truncated))
    );
}

#[test]
fn the_cache_expires_evicts_and_ignores_case() {
    let mut c = Cache::new();
    c.put(b"Example.COM.", &[[1, 2, 3, 4]], 10, 0);
    assert_eq!(c.get(b"example.com", 9_999), Some(vec![[1, 2, 3, 4]]));
    assert_eq!(c.get(b"example.com", 10_000), None, "expired after the TTL");
    c.put(b"long.test", &[[9, 9, 9, 9]], 100_000, 0);
    assert!(
        c.get(b"long.test", 299_999).is_some() && c.get(b"long.test", 300_000).is_none(),
        "TTL capped at 300 s"
    );
    for i in 0..20u8 {
        c.put(format!("h{i}.test").as_bytes(), &[[i, 0, 0, 1]], 60, 1000);
    }
    assert!(c.get(b"h0.test", 2000).is_none(), "the oldest entries went");
    assert!(c.get(b"h19.test", 2000).is_some());
    c.put(b"none.test", &[], 60, 0);
    c.put(b"zero.test", &[[1, 1, 1, 1]], 0, 0);
    assert!(
        c.get(b"none.test", 1).is_none() && c.get(b"zero.test", 1).is_none(),
        "nothing to cache"
    );
}

#[test]
fn a_query_sends_at_0_1_and_3_seconds_and_fails_at_7() {
    let mut q = Query::new(b"example.com", 100);
    let mut sends = vec![];
    let mut t = 5_000u64; // (an arbitrary start on the caller's clock)
    let end = loop {
        match q.poll(t) {
            Step::Send(m) => sends.push((t - 5_000, u16::from_be_bytes([m[0], m[1]]))),
            Step::Done(r) => break (t - 5_000, r),
            Step::Wait => {}
        }
        t = q.next_deadline().max(t + 1).min(t + 500);
    };
    assert_eq!(
        sends,
        vec![(0, 100), (1000, 101), (3000, 102)],
        "three sends, each with a new identifier"
    );
    assert_eq!(end, (7000, Err(Failure::Timeout)));
}

#[test]
fn a_late_answer_to_an_earlier_send_still_counts_and_a_servfail_does_not_end_the_wait() {
    let mut q = Query::new(b"example.com", 200);
    q.poll(0);
    q.poll(1000); // the second send (id 201)
    let servfail = response(201, "example.com", 2, &[]);
    assert_eq!(q.on_response(&servfail), None, "SERVFAIL: keep waiting");
    let late = response(200, "example.com", 0, &[rr_ptr(1, 60, &[7, 7, 7, 7])]);
    assert_eq!(
        q.on_response(&late),
        Some(Step::Done(Ok(vec![[7, 7, 7, 7]])))
    );
    assert_eq!(q.on_response(&late), None, "settled once");
    let mut n = Query::new(b"nope.test", 1);
    n.poll(0);
    assert_eq!(
        n.on_response(&response(1, "nope.test", 3, &[])),
        Some(Step::Done(Err(Failure::NoSuchName)))
    );
    let mut b = Query::new(b"bad name", 1);
    assert_eq!(b.poll(0), Step::Done(Err(Failure::BadName)));
}

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
fn random_truncated_and_mutated_messages_never_panic_or_hang() {
    let base = response(
        7,
        "www.example.com",
        0,
        &[rr_ptr(5, 9, &name_bytes("a.example.net")), {
            let mut a = name_bytes("a.example.net");
            a.extend_from_slice(&[0, 1, 0, 1, 0, 0, 0, 5, 0, 4, 1, 2, 3, 4]);
            a
        }],
    );
    let mut r = Rng(0x1234_5678_9abc_def1);
    for case in 0..200_000u32 {
        let msg: Vec<u8> = match case % 3 {
            0 => (0..r.next() % 100).map(|_| r.next() as u8).collect(),
            1 => base[..(r.next() as usize) % (base.len() + 1)].to_vec(),
            _ => {
                let mut b = base.clone();
                for _ in 0..1 + r.next() % 4 {
                    let i = (r.next() as usize) % b.len();
                    b[i] = r.next() as u8;
                }
                b
            }
        };
        let _ = parse_response(&msg, 7, b"www.example.com");
        let _ = read_name(&msg, (r.next() as usize) % (msg.len() + 1));
    }
}
