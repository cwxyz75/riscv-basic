use sockshim::errno::*;
use sockshim::*;

#[test]
fn the_errno_table_is_the_one_in_the_spec() {
    // S9-SPEC 9.5: statuses 1 to 16
    let want = [
        (1, EINVAL),
        (2, EACCES),
        (3, EADDRINUSE),
        (4, EMSGSIZE),
        (5, ENETUNREACH),
        (6, EMFILE),
        (7, ENETDOWN),
        (8, ENOBUFS),
        (9, EOPNOTSUPP),
        (10, ECONNREFUSED),
        (11, ETIMEDOUT),
        (12, ECONNRESET),
        (13, ENOTCONN),
        (14, EISCONN),
        (15, EHOSTUNREACH),
        (16, EINVAL),
        (-109, ENETDOWN),
    ];
    for (s, e) in want {
        assert_eq!(errno_of_status(s), e, "status {s}");
    }
    assert_eq!(errno_of_kernel(-6), EAGAIN);
    assert_eq!(errno_of_kernel(-5), ENOBUFS);
    assert_eq!(errno_of_kernel(-7), EPIPE);
    // the Linux numbers the headers publish
    assert_eq!(
        [
            ECONNRESET,
            ECONNREFUSED,
            EADDRINUSE,
            EINPROGRESS,
            EAFNOSUPPORT,
            ENOTSOCK
        ],
        [104, 111, 98, 115, 97, 88]
    );
}

#[test]
fn socket_arguments_are_checked_as_the_spec_says() {
    assert_eq!(
        check_socket_args(2, SOCK_STREAM, 0),
        Ok((SOCK_STREAM, false))
    );
    assert_eq!(
        check_socket_args(2, SOCK_DGRAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0),
        Ok((SOCK_DGRAM, true))
    );
    assert_eq!(
        check_socket_args(2, SOCK_STREAM, 6),
        Ok((SOCK_STREAM, false))
    );
    assert_eq!(check_socket_args(2, SOCK_STREAM, 17), Err(EPROTONOSUPPORT));
    assert_eq!(
        check_socket_args(10, SOCK_STREAM, 0),
        Err(EAFNOSUPPORT),
        "IPv6"
    );
    assert_eq!(
        check_socket_args(1, SOCK_STREAM, 0),
        Err(EAFNOSUPPORT),
        "AF_UNIX"
    );
    assert_eq!(check_socket_args(2, 3, 0), Err(EPROTONOSUPPORT), "SOCK_RAW");
}

#[test]
fn sockaddr_in_round_trips_and_rejects_the_wrong_family() {
    let b = format_sockaddr_in([10, 0, 2, 15], 8080);
    assert_eq!(
        &b[2..8],
        &[0x1f, 0x90, 10, 0, 2, 15],
        "port in network order, then the address"
    );
    assert_eq!(parse_sockaddr_in(&b), Ok(([10, 0, 2, 15], 8080)));
    let mut v6 = b;
    v6[0] = 10;
    assert_eq!(parse_sockaddr_in(&v6), Err(EAFNOSUPPORT));
    assert_eq!(parse_sockaddr_in(&b[..15]), Err(EINVAL));
}

#[test]
fn inet_pton_and_ntop_agree_and_are_strict() {
    for a in [
        [0, 0, 0, 0],
        [127, 0, 0, 1],
        [255, 255, 255, 255],
        [10, 0, 2, 15],
        [192, 168, 1, 100],
    ] {
        let mut buf = [0u8; 16];
        let n = inet_ntop4(a, &mut buf).unwrap();
        assert_eq!(inet_pton4(&buf[..n]), Some(a), "{:?}", &buf[..n]);
    }
    let mut buf = [0u8; 16];
    let n = inet_ntop4([1, 22, 133, 4], &mut buf).unwrap();
    assert_eq!(&buf[..n], b"1.22.133.4");
    assert_eq!(
        inet_ntop4([255, 255, 255, 255], &mut [0u8; 14]),
        None,
        "a buffer one byte short"
    );
    for bad in [
        "",
        "1.2.3",
        "1.2.3.4.5",
        "1.2.3.256",
        "01.2.3.4",
        "1..3.4",
        "a.b.c.d",
        "1.2.3.4 ",
        " 1.2.3.4",
        "1.2.3.-4",
        "1.2.3.4.",
    ] {
        assert_eq!(inet_pton4(bad.as_bytes()), None, "{bad:?}");
    }
    assert_eq!(inet_pton4(b"0.0.0.0"), Some([0; 4]));
}

fn int(v: i32) -> Vec<u8> {
    v.to_ne_bytes().to_vec()
}

fn timeval(s: i64, us: i64) -> Vec<u8> {
    [s.to_ne_bytes(), us.to_ne_bytes()].concat()
}

#[test]
fn options_are_accepted_or_refused_as_the_spec_says() {
    use opt::*;
    assert_eq!(
        set_option(SOL_SOCKET, SO_REUSEADDR, &int(1)),
        Ok(SetOpt::Accept)
    );
    assert_eq!(
        set_option(SOL_SOCKET, SO_RCVBUF, &int(32768)),
        Ok(SetOpt::RcvBuf(32768))
    );
    assert_eq!(
        set_option(SOL_SOCKET, SO_SNDBUF, &int(-5)),
        Ok(SetOpt::SndBuf(0))
    );
    assert_eq!(set_option(SOL_SOCKET, SO_RCVBUF, &[1, 2]), Err(EINVAL));
    assert_eq!(
        set_option(IPPROTO_TCP, TCP_NODELAY, &int(1)),
        Ok(SetOpt::NoDelay(true))
    );
    assert_eq!(
        set_option(IPPROTO_TCP, TCP_NODELAY, &int(0)),
        Ok(SetOpt::NoDelay(false))
    );
    assert_eq!(
        set_option(SOL_SOCKET, SO_RCVTIMEO, &timeval(2, 500_000)),
        Ok(SetOpt::RcvTimeoutMs(2500))
    );
    assert_eq!(
        set_option(SOL_SOCKET, SO_SNDTIMEO, &timeval(0, 0)),
        Ok(SetOpt::SndTimeoutMs(0))
    );
    assert_eq!(
        set_option(SOL_SOCKET, SO_RCVTIMEO, &timeval(-1, 0)),
        Err(EINVAL)
    );
    assert_eq!(
        set_option(SOL_SOCKET, SO_RCVTIMEO, &timeval(0, 1_000_000)),
        Err(EINVAL)
    );
    // linger: off, abort ({1,0}), anything else is EINVAL
    assert_eq!(
        set_option(SOL_SOCKET, SO_LINGER, &[int(0), int(0)].concat()),
        Ok(SetOpt::LingerAbort(false))
    );
    assert_eq!(
        set_option(SOL_SOCKET, SO_LINGER, &[int(1), int(0)].concat()),
        Ok(SetOpt::LingerAbort(true))
    );
    assert_eq!(
        set_option(SOL_SOCKET, SO_LINGER, &[int(1), int(5)].concat()),
        Err(EINVAL)
    );
    // refused, never silently
    assert_eq!(
        set_option(SOL_SOCKET, SO_KEEPALIVE, &int(1)),
        Err(ENOPROTOOPT)
    );
    assert_eq!(
        set_option(SOL_SOCKET, SO_REUSEPORT, &int(1)),
        Err(ENOPROTOOPT)
    );
    assert_eq!(
        set_option(SOL_SOCKET, SO_BROADCAST, &int(1)),
        Err(ENOPROTOOPT)
    );
    assert_eq!(
        set_option(SOL_SOCKET, SO_OOBINLINE, &int(1)),
        Err(EOPNOTSUPP)
    );
    assert_eq!(
        set_option(0, 1, &int(1)),
        Err(ENOPROTOOPT),
        "IP_TOS and the like"
    );
    assert_eq!(
        set_option(IPPROTO_TCP, 4, &int(1)),
        Err(ENOPROTOOPT),
        "TCP_KEEPIDLE and the like"
    );
}

#[test]
fn poll_reports_what_the_table_says() {
    // stream: readable data, room to write, a failure
    assert_eq!(
        poll_stream(POLLIN | POLLOUT, true, true, false),
        POLLIN | POLLOUT
    );
    assert_eq!(
        poll_stream(POLLIN, true, true, false),
        POLLIN,
        "only what was asked for"
    );
    assert_eq!(poll_stream(POLLOUT, false, false, false), 0);
    assert_eq!(
        poll_stream(POLLIN, false, false, true),
        POLLERR | POLLHUP,
        "errors are always reported"
    );
    // connecting: writable when the outcome is known; a failure adds POLLERR; never readable
    assert_eq!(poll_connecting(POLLOUT, false, false), 0);
    assert_eq!(poll_connecting(POLLOUT, true, false), POLLOUT);
    assert_eq!(poll_connecting(POLLOUT, true, true), POLLOUT | POLLERR);
    assert_eq!(poll_connecting(POLLIN, true, false), 0);
}
