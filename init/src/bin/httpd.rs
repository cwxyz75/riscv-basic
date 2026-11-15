//! `httpd [-p PORT]` (S9-SPEC 10.1): the demo web server. It serves the directory of its `/www` namespace entry on `PORT`
//! (default 8080) through the network service (`/svc/net`), one thread, one listener and up to eight connections as small
//! state machines. HTTP/1.0 semantics (one request per connection, `Connection: close`), `GET` and `HEAD` only, every failure a
//! status and a close, a file that cannot be finished after its head was sent is **aborted** (a reset), never left looking
//! complete. The parser and the path sanitising are the pure `http1` crate.
#![no_std]
#![no_main]

extern crate alloc;

#[path = "../prog.rs"]
mod prog;

use alloc::vec::Vec;
use http1::{content_type, parse_request, path_segments, response_head, Method};
use proto::file::rights;
use rt::fs::Fs;
use rt::net::{Net, NetError, Stream};
use rt::sys;

const MAX_CONNS: usize = 8;
const HEAD_TIMEOUT: u64 = 5000;
const EXCHANGE_TIMEOUT: u64 = 30_000;
const DRAIN: u64 = 1000;
const PIECE: usize = 16 * 1024;

enum Phase {
    /// Reading the request head.
    Head,
    /// Sending: `out` holds what remains of the head and the current piece; `file` the source, `off` the next file offset.
    Send {
        out: Vec<u8>,
        file: Option<Fs>,
        off: u64,
        left: u64,
    },
    /// All sent and our side shut down: read until the peer closes or `DRAIN` ms pass.
    Drain { until: u64 },
}

struct Conn {
    s: Stream,
    started: u64,
    buf: Vec<u8>,
    phase: Phase,
}

fn status_response(status: u16) -> Vec<u8> {
    let body = alloc::format!("{status}\n");
    let mut h = response_head(status, "text/plain; charset=utf-8", body.len() as u64);
    h.extend_from_slice(body.as_bytes());
    h
}

/// Work out the response for a parsed request: the head (and for a file, its handle and size).
fn respond(www: &mut Fs, method: Method, target: &[u8]) -> (Vec<u8>, Option<(Fs, u64)>) {
    let segs = match path_segments(target) {
        Ok(s) => s,
        Err(r) => return (status_response(r.status()), None),
    };
    let mut rel: Vec<u8> = Vec::new();
    for s in &segs {
        if !rel.is_empty() {
            rel.push(b'/');
        }
        rel.extend_from_slice(s);
    }
    // a directory (or the root) serves its index.html
    let st = if rel.is_empty() {
        Some(rt::fs::Stat {
            is_dir: true,
            size: 0,
        })
    } else {
        www.stat(&rel).ok()
    };
    let Some(st) = st else {
        return (status_response(404), None);
    };
    if st.is_dir {
        if !rel.is_empty() {
            rel.push(b'/');
        }
        rel.extend_from_slice(b"index.html");
    }
    let Ok(stat) = www.stat(&rel) else {
        return (status_response(404), None);
    };
    if stat.is_dir {
        return (status_response(404), None);
    }
    let name = rel.rsplit(|&c| c == b'/').next().unwrap_or(&rel);
    let head = response_head(200, content_type(name), stat.size);
    if method == Method::Head {
        return (head, None);
    }
    match www.open(&rel, 0, rights::READ) {
        Ok(f) => (head, Some((f, stat.size))),
        Err(_) => (status_response(500), None),
    }
}

#[no_mangle]
pub fn app_main() -> i32 {
    let p = match prog::start() {
        Ok(p) => p,
        Err(c) => return c,
    };
    let mut port = 8080u16;
    // `--quit-after N` (the tests): exit 0 once N connections have finished
    let mut quit_after = u64::MAX;
    let mut finished = 0u64;
    let mut args = p.args().iter();
    while let Some(a) = args.next() {
        if a == b"-p" {
            port = args
                .next()
                .and_then(|n| core::str::from_utf8(n).ok())
                .and_then(|n| n.parse().ok())
                .unwrap_or(8080);
        } else if a == b"--quit-after" {
            quit_after = args
                .next()
                .and_then(|n| core::str::from_utf8(n).ok())
                .and_then(|n| n.parse().ok())
                .unwrap_or(u64::MAX);
        }
    }
    let (Some(svc), Some(www_h)) = (p.env.namespace(b"/svc"), p.env.namespace(b"/www")) else {
        let _ = p.complain(b"", "needs /svc and /www in its view");
        return 3;
    };
    let mut www = Fs::new_at(www_h, 1 << 16);
    let mut net = match Net::connect(&mut Fs::new(svc), b"net", sys::clock_get() + 10_000) {
        Ok(n) => n,
        Err(_) => return 4,
    };
    let mut listener = match net.listen_stream(port, MAX_CONNS, 0, sys::clock_get() + 5000) {
        Ok(l) => l,
        Err(e) => {
            let _ = p.out(alloc::format!("httpd: cannot listen on {port}: {e:?}\n").as_bytes());
            return 5;
        }
    };
    rt::println!("httpd: listening on {}", listener.port);
    let mut conns: Vec<Conn> = Vec::new();
    loop {
        let now = sys::clock_get();
        let mut busy = false;
        // new connections
        while conns.len() < MAX_CONNS {
            match listener.accept(now) {
                Ok(s) => {
                    conns.push(Conn {
                        s,
                        started: now,
                        buf: Vec::new(),
                        phase: Phase::Head,
                    });
                    busy = true;
                }
                Err(NetError::TimedOut) => break,
                Err(NetError::Closed) => return 6, // the stack died: the supervisor restarts us
                Err(_) => break,
            }
        }
        let mut i = 0;
        while i < conns.len() {
            let c = &mut conns[i];
            let mut done = false;
            match &mut c.phase {
                Phase::Head => {
                    let mut tmp = [0u8; 1024];
                    match c.s.read(&mut tmp, now) {
                        Ok(0) => done = true,
                        Ok(n) => {
                            busy = true;
                            c.buf.extend_from_slice(&tmp[..n]);
                        }
                        Err(NetError::TimedOut) => {}
                        Err(_) => done = true,
                    }
                    if !done {
                        let have_head = http1::head_end(&c.buf).is_some();
                        if have_head || c.buf.len() > http1::MAX_HEAD {
                            let (head, file) = match parse_request(&c.buf) {
                                Ok(r) => respond(&mut www, r.method, &r.target),
                                Err(r) => (status_response(r.status()), None),
                            };
                            let (f, left) = match file {
                                Some((f, size)) => (Some(f), size),
                                None => (None, 0),
                            };
                            c.phase = Phase::Send {
                                out: head,
                                file: f,
                                off: 0,
                                left,
                            };
                            busy = true;
                        } else if now.saturating_sub(c.started) > HEAD_TIMEOUT {
                            c.phase = Phase::Send {
                                out: status_response(408),
                                file: None,
                                off: 0,
                                left: 0,
                            };
                        }
                    }
                }
                Phase::Send {
                    out,
                    file,
                    off,
                    left,
                } => {
                    // refill from the file when the piece is gone
                    if out.is_empty() && *left > 0 {
                        if let Some(f) = file.as_mut() {
                            match f.read(*off, PIECE.min(*left as usize)) {
                                Ok(d) if !d.is_empty() => {
                                    *off += d.len() as u64;
                                    *left -= d.len() as u64;
                                    *out = d;
                                }
                                _ => {
                                    // the file cannot be finished: a reset, so the body is never taken for complete
                                    c.s.abort();
                                    done = true;
                                }
                            }
                        }
                    }
                    if !done && !out.is_empty() {
                        match c.s.write(out, now) {
                            Ok(n) => {
                                out.drain(..n);
                                busy = true;
                            }
                            Err(NetError::TimedOut) => {}
                            Err(_) => done = true,
                        }
                    }
                    if !done && out.is_empty() && *left == 0 {
                        c.s.shutdown_write();
                        c.phase = Phase::Drain { until: now + DRAIN };
                    }
                }
                Phase::Drain { until } => {
                    let mut tmp = [0u8; 512];
                    match c.s.read(&mut tmp, now) {
                        Ok(0) | Err(NetError::Stack(_)) => done = true,
                        Ok(_) => {}
                        Err(_) => {}
                    }
                    if now >= *until {
                        done = true;
                    }
                }
            }
            if now.saturating_sub(c.started) > EXCHANGE_TIMEOUT {
                done = true;
            }
            if done {
                conns.swap_remove(i);
                finished += 1;
            } else {
                i += 1;
            }
        }
        if finished >= quit_after && conns.is_empty() {
            rt::println!("httpd: served {finished} connections, quitting");
            return 0;
        }
        if !busy {
            rt::io::nap(2);
        }
    }
}
