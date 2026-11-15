use http1::*;

fn req(s: &str) -> Result<Request, Reject> {
    parse_request(s.as_bytes())
}

#[test]
fn good_requests_parse() {
    let r = req("GET /index.html HTTP/1.1\r\nHost: x\r\nUser-Agent: curl/8\r\nAccept: */*\r\n\r\n")
        .unwrap();
    assert_eq!(
        (r.method, r.target.as_slice()),
        (Method::Get, &b"/index.html"[..])
    );
    assert_eq!(
        req("HEAD /a?b=c HTTP/1.0\r\n\r\n").unwrap().method,
        Method::Head
    );
    assert!(
        req("GET / HTTP/1.1\r\nContent-Length: 0\r\n\r\n").is_ok(),
        "an announced empty body is fine"
    );
    assert_eq!(head_end(b"GET / HTTP/1.0\r\n\r\nBODY"), Some(18));
    assert_eq!(head_end(b"GET / HTTP/1.0\r\n"), None);
}

#[test]
fn every_kind_of_bad_request_gets_its_status() {
    let cases: &[(&str, Reject)] = &[
        ("POST / HTTP/1.1\r\n\r\n", Reject::MethodNotAllowed),
        ("DELETE / HTTP/1.1\r\n\r\n", Reject::MethodNotAllowed),
        ("GET / HTTP/2.0\r\n\r\n", Reject::VersionNotSupported),
        ("GET / HTTP/0.9\r\n\r\n", Reject::VersionNotSupported),
        ("GET / FTP/1.1\r\n\r\n", Reject::BadRequest),
        ("GET /\r\n\r\n", Reject::BadRequest),
        ("GET  / HTTP/1.1\r\n\r\n", Reject::BadRequest),
        ("GET / HTTP/1.1 x\r\n\r\n", Reject::BadRequest),
        ("GET http://x/ HTTP/1.1\r\n\r\n", Reject::BadRequest),
        ("GET *  HTTP/1.1\r\n\r\n", Reject::BadRequest),
        ("GET /a b HTTP/1.1\r\n\r\n", Reject::BadRequest),
        ("GET /\u{1}x HTTP/1.1\r\n\r\n", Reject::BadRequest),
        (
            "GET / HTTP/1.1\r\nbad header line\r\n\r\n",
            Reject::BadRequest,
        ),
        ("GET / HTTP/1.1\r\n: nameless\r\n\r\n", Reject::BadRequest),
        (
            "GET / HTTP/1.1\r\nContent-Length: 5\r\n\r\n",
            Reject::BadRequest,
        ),
        (
            "GET / HTTP/1.1\r\nContent-Length: x\r\n\r\n",
            Reject::BadRequest,
        ),
        (
            "GET / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n",
            Reject::BadRequest,
        ),
        ("GET / HTTP/1.1\nHost: x\n\n", Reject::BadRequest),
        (
            "GET / HTTP/1.1\r\nHost: x\rEvil: y\r\n\r\n",
            Reject::BadRequest,
        ),
        ("GE T / HTTP/1.1\r\n\r\n", Reject::BadRequest),
    ];
    for (r, want) in cases {
        assert_eq!(req(r).err(), Some(*want), "{r:?}");
    }
    let long = format!("GET /{} HTTP/1.1\r\n\r\n", "a".repeat(MAX_TARGET));
    assert_eq!(req(&long).err(), Some(Reject::TargetTooLong));
    let many: String = (0..=MAX_HEADERS).map(|i| format!("H{i}: v\r\n")).collect();
    assert_eq!(
        req(&format!("GET / HTTP/1.1\r\n{many}\r\n")).err(),
        Some(Reject::HeadTooLarge)
    );
    let big = format!("GET / HTTP/1.1\r\nX: {}\r\n\r\n", "a".repeat(MAX_HEAD));
    assert_eq!(req(&big).err(), Some(Reject::HeadTooLarge));
    assert_eq!(
        [
            Reject::BadRequest.status(),
            Reject::MethodNotAllowed.status(),
            Reject::TargetTooLong.status(),
            Reject::HeadTooLarge.status(),
            Reject::VersionNotSupported.status()
        ],
        [400, 405, 414, 431, 505]
    );
}

fn segs(t: &str) -> Result<Vec<String>, Reject> {
    path_segments(t.as_bytes()).map(|v| {
        v.into_iter()
            .map(|s| String::from_utf8_lossy(&s).into_owned())
            .collect()
    })
}

#[test]
fn targets_become_safe_segments() {
    assert_eq!(segs("/").unwrap(), Vec::<String>::new());
    assert_eq!(segs("/a/b/c.txt").unwrap(), ["a", "b", "c.txt"]);
    assert_eq!(segs("//a///b/./c/").unwrap(), ["a", "b", "c"]);
    assert_eq!(segs("/a?q=1#frag").unwrap(), ["a"]);
    assert_eq!(segs("/a%20b/%41").unwrap(), ["a b", "A"]);
    assert_eq!(
        segs("/%2e/a").unwrap(),
        ["a"],
        "an encoded dot is a dot segment"
    );
    for bad in [
        "/..",
        "/a/../b",
        "/a/..",
        "/%2e%2e/etc",
        "/%2E%2e",
        "/a%00b",
        "/a%2fb",
        "/a%2Fb",
        "/a%5cb",
        "/a\\b",
        "/a%",
        "/a%4",
        "/a%zz",
        "..",
        "a/b",
    ] {
        assert_eq!(segs(bad).err(), Some(Reject::BadRequest), "{bad:?}");
    }
    // an escape is decoded once: %252e%252e is the literal text %2e%2e, not a dot segment
    assert_eq!(segs("/%252e%252e").unwrap(), ["%2e%2e"]);
}

#[test]
fn content_types_by_extension() {
    assert_eq!(content_type(b"index.html"), "text/html; charset=utf-8");
    assert_eq!(content_type(b"A.TXT"), "text/plain; charset=utf-8");
    assert_eq!(content_type(b"x.PNG"), "image/png");
    assert_eq!(content_type(b"noext"), "application/octet-stream");
    assert_eq!(
        content_type(b"weird.verylongextension"),
        "application/octet-stream"
    );
    assert_eq!(content_type(b"a.tar.gz"), "application/octet-stream");
}

#[test]
fn response_heads_are_exact() {
    assert_eq!(
        String::from_utf8(response_head(200, "text/plain; charset=utf-8", 1234)).unwrap(),
        "HTTP/1.1 200 OK\r\nContent-Length: 1234\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\nServer: riscv-os-httpd\r\n\r\n"
    );
    let h = String::from_utf8(response_head(405, "text/plain", 0)).unwrap();
    assert!(
        h.starts_with("HTTP/1.1 405 Method Not Allowed\r\n") && h.contains("Allow: GET, HEAD\r\n")
    );
    assert!(
        String::from_utf8(response_head(404, "text/plain", u64::MAX))
            .unwrap()
            .contains("Content-Length: 18446744073709551615\r\n")
    );
    // what we build, we parse
    let built = response_head(200, "text/html", 77);
    let p = parse_response_head(&built).unwrap();
    assert_eq!(
        (p.status, p.content_length, p.head_len),
        (200, Some(77), built.len())
    );
}

#[test]
fn response_heads_from_other_servers_parse_and_bad_ones_do_not() {
    // what python's http.server sends
    let py = b"HTTP/1.0 200 OK\r\nServer: SimpleHTTP/0.6 Python/3.8.10\r\nDate: Sat, 14 Nov 2026 00:00:00 GMT\r\nContent-type: application/octet-stream\r\nContent-Length: 262144\r\nLast-Modified: x\r\n\r\nBODY";
    let p = parse_response_head(py).unwrap();
    assert_eq!((p.status, p.content_length), (200, Some(262144)));
    assert_eq!(&py[p.head_len..], b"BODY");
    assert_eq!(
        parse_response_head(b"HTTP/1.1 404 Not Found\r\n\r\n")
            .unwrap()
            .content_length,
        None
    );
    assert_eq!(
        parse_response_head(b"HTTP/1.0 200 OK\r\nContent-Le"),
        Err(ResponseError::Incomplete)
    );
    for bad in [
        &b"HTTP/2 200 OK\r\n\r\n"[..],
        b"HTTP/1.1 20 OK\r\n\r\n",
        b"HTTP/1.1 2x0 OK\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length: -1\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nno colon here\r\n\r\n",
        b"GARBAGE\r\n\r\n",
    ] {
        assert_eq!(
            parse_response_head(bad),
            Err(ResponseError::Bad),
            "{:?}",
            String::from_utf8_lossy(bad)
        );
    }
    assert_eq!(
        parse_response_head(&vec![b'a'; MAX_RESPONSE_HEAD]),
        Err(ResponseError::Bad),
        "a head that never ends"
    );
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
fn random_truncated_and_mutated_input_never_panics() {
    let good = b"GET /a/b%20c?x=1 HTTP/1.1\r\nHost: h\r\nAccept: */*\r\nContent-Length: 0\r\n\r\n"
        .to_vec();
    let resp = response_head(200, "text/html", 5);
    let mut r = Rng(0x9e3779b97f4a7c15);
    for case in 0..200_000u32 {
        let mut b: Vec<u8> = match case % 4 {
            0 => (0..r.next() % 200).map(|_| r.next() as u8).collect(),
            1 => good[..(r.next() as usize) % (good.len() + 1)].to_vec(),
            2 => resp[..(r.next() as usize) % (resp.len() + 1)].to_vec(),
            _ => good.clone(),
        };
        if case % 4 >= 2 && !b.is_empty() {
            for _ in 0..1 + r.next() % 3 {
                let i = (r.next() as usize) % b.len();
                b[i] = r.next() as u8;
            }
        }
        if let Ok(rq) = parse_request(&b) {
            let _ = path_segments(&rq.target);
        }
        let _ = path_segments(&b);
        let _ = parse_response_head(&b);
        let _ = content_type(&b);
    }
}
