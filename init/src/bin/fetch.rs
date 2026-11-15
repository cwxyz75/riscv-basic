//! `fetch [--sha256] [-m SECONDS] [--dns ADDR:PORT] URL` (S9-SPEC 10.2): an HTTP/1.0 client for `http://host[:port]/path`.
//! It resolves the host (a dotted quad is used as it is), connects, sends `GET`, reads the head (at most 8 KiB) and the body
//! (exactly `Content-Length` bytes if the head has one, else until a clean end of stream) and prints one structured line:
//! `##FETCH status=200 bytes=N sha256=<64 hex>`. Redirects are reported, not followed.
//!
//! Exit codes: 0 a complete 2xx body; 2 usage; 3 resolve failure; 4 connect failure; 5 a non-2xx status; 6 truncated or
//! reset; 7 timeout; 8 the head was bad; 9 no network name in the view.
#![no_std]
#![no_main]

extern crate alloc;

#[path = "../prog.rs"]
mod prog;

use alloc::format;
use alloc::vec::Vec;
use http1::{parse_response_head, ResponseError};
use rngcore::sha256::Sha256;
use rt::fs::Fs;
use rt::net::{Net, NetError};
use rt::resolve::{resolve, ResolveError};
use rt::sys;

fn parse_addr_port(s: &[u8]) -> Option<([u8; 4], u16)> {
    let t = core::str::from_utf8(s).ok()?;
    let (a, p) = t.rsplit_once(':')?;
    let mut out = [0u8; 4];
    let mut parts = a.split('.');
    for o in &mut out {
        *o = parts.next()?.parse().ok()?;
    }
    parts.next().is_none().then_some((out, p.parse().ok()?))
}

fn hex(d: &[u8; 32]) -> Vec<u8> {
    d.iter()
        .flat_map(|b| {
            [
                b"0123456789abcdef"[usize::from(b >> 4)],
                b"0123456789abcdef"[usize::from(b & 15)],
            ]
        })
        .collect()
}

#[no_mangle]
pub fn app_main() -> i32 {
    let p = match prog::start() {
        Ok(p) => p,
        Err(c) => return c,
    };
    let (mut want_sha, mut secs, mut dns, mut url) = (false, 30u64, None, None);
    let mut args = p.args().iter();
    while let Some(a) = args.next() {
        match a.as_slice() {
            b"--sha256" => want_sha = true,
            b"-m" => {
                secs = args
                    .next()
                    .and_then(|n| core::str::from_utf8(n).ok())
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(30)
            }
            b"--dns" => dns = args.next().and_then(|a| parse_addr_port(a)),
            _ => url = Some(a.clone()),
        }
    }
    let Some(url) = url else {
        let _ = p.complain(
            b"",
            "usage: fetch [--sha256] [-m SECONDS] [--dns ADDR:PORT] http://host[:port]/path",
        );
        return 2;
    };
    let Some(rest) = url.strip_prefix(b"http://") else {
        let _ = p.complain(&url, "only http:// URLs are supported");
        return 2;
    };
    let (hostport, path) = match rest.iter().position(|&c| c == b'/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, &b"/"[..]),
    };
    let (host, port) = match hostport.iter().position(|&c| c == b':') {
        Some(i) => (
            &hostport[..i],
            core::str::from_utf8(&hostport[i + 1..])
                .ok()
                .and_then(|n| n.parse::<u16>().ok())
                .unwrap_or(0),
        ),
        None => (hostport, 80),
    };
    if host.is_empty() || port == 0 {
        let _ = p.complain(&url, "bad URL");
        return 2;
    }
    let overall = sys::clock_get() + secs * 1000;
    let Some(svc) = p.env.namespace(b"/svc") else {
        let _ = p.complain(b"", "no /svc in this program's view");
        return 9;
    };
    let mut fs = Fs::new(svc);
    let mut net = match Net::connect(&mut fs, b"net", sys::clock_get() + 10_000) {
        Ok(n) => n,
        Err(_) => {
            let _ = p.complain(b"", "cannot reach the network service");
            return 9;
        }
    };
    let addr = match resolve(&mut net, host, dns, overall.min(sys::clock_get() + 10_000)) {
        Ok(a) => a,
        Err(e) => {
            let _ = p.out(
                format!(
                    "fetch: cannot resolve {}: {e:?}\n",
                    core::str::from_utf8(host).unwrap_or("?")
                )
                .as_bytes(),
            );
            return if e == ResolveError::Timeout { 7 } else { 3 };
        }
    };
    let mut s = match net.connect_stream(addr, port, 0, 0, 0, overall) {
        Ok(s) => s,
        Err(e) => {
            let _ = p.out(format!("fetch: cannot connect: {e:?}\n").as_bytes());
            return if e == NetError::TimedOut { 7 } else { 4 };
        }
    };
    let mut req = Vec::new();
    req.extend_from_slice(b"GET ");
    req.extend_from_slice(path);
    req.extend_from_slice(b" HTTP/1.0\r\nHost: ");
    req.extend_from_slice(hostport);
    req.extend_from_slice(b"\r\nUser-Agent: riscv-os-fetch\r\nAccept: */*\r\n\r\n");
    if s.write_all(&req, overall).is_err() {
        let _ = p.out(b"fetch: the request could not be sent\n");
        return 6;
    }
    // the head
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 4096];
    let head = loop {
        match parse_response_head(&buf) {
            Ok(h) => break h,
            Err(ResponseError::Bad) => {
                let _ = p.out(b"fetch: a bad response head\n");
                return 8;
            }
            Err(ResponseError::Incomplete) => {}
        }
        match s.read(&mut tmp, overall) {
            Ok(0) => {
                let _ = p.out(b"fetch: the connection ended before the response head\n");
                return 6;
            }
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(NetError::TimedOut) => return 7,
            Err(_) => {
                let _ = p.out(b"fetch: the connection was reset\n");
                return 6;
            }
        }
    };
    // the body
    let mut sha = Sha256::new();
    let mut bytes = 0u64;
    let first = &buf[head.head_len..];
    let take = |data: &[u8], bytes: &mut u64, sha: &mut Sha256| {
        sha.update(data);
        *bytes += data.len() as u64;
    };
    take(first, &mut bytes, &mut sha);
    let mut exit = 0;
    loop {
        if let Some(len) = head.content_length {
            if bytes >= len {
                break;
            }
        }
        match s.read(&mut tmp, overall) {
            Ok(0) => {
                if head.content_length.is_some() {
                    exit = 6; // fewer bytes than announced
                }
                break;
            }
            Ok(n) => take(&tmp[..n], &mut bytes, &mut sha),
            Err(NetError::TimedOut) => {
                exit = 7;
                break;
            }
            Err(_) => {
                exit = 6;
                break;
            }
        }
    }
    if let Some(len) = head.content_length {
        if bytes > len && exit == 0 {
            exit = 6; // more than announced: a framing problem
        }
    }
    let digest = sha.finish();
    let mut line = format!("##FETCH status={} bytes={}", head.status, bytes).into_bytes();
    if want_sha {
        line.extend_from_slice(b" sha256=");
        line.extend_from_slice(&hex(&digest));
    }
    if exit != 0 {
        line.extend_from_slice(if exit == 7 {
            b" timeout"
        } else {
            b" truncated"
        });
    }
    line.push(b'\n');
    let _ = p.out(&line);
    if exit != 0 {
        return exit;
    }
    if !(200..300).contains(&head.status) {
        return 5;
    }
    0
}
