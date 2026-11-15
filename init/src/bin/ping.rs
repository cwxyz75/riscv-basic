//! `ping [-c N] ADDR`: ICMP echo through the network stack (S8, NET-SPEC 7.6). One structured line per reply and a summary.
//! Uses `/svc/net` from the program's `/svc` namespace entry.
#![no_std]
#![no_main]

extern crate alloc;

#[path = "../prog.rs"]
mod prog;

use alloc::format;
use rt::fs::Fs;
use rt::net::Net;
use rt::sys;

fn parse_ip(s: &[u8]) -> Option<[u8; 4]> {
    let t = core::str::from_utf8(s).ok()?;
    let mut out = [0u8; 4];
    let mut parts = t.split('.');
    for o in &mut out {
        *o = parts.next()?.parse().ok()?;
    }
    parts.next().is_none().then_some(out)
}

#[no_mangle]
pub fn app_main() -> i32 {
    let p = match prog::start() {
        Ok(p) => p,
        Err(c) => return c,
    };
    let mut count = 4u32;
    let mut addr = None;
    let mut args = p.args().iter();
    while let Some(a) = args.next() {
        if a == b"-c" {
            count = args
                .next()
                .and_then(|n| core::str::from_utf8(n).ok())
                .and_then(|n| n.parse().ok())
                .unwrap_or(4);
        } else {
            addr = parse_ip(a);
        }
    }
    let Some(dst) = addr else {
        let _ = p.complain(b"", "usage: ping [-c N] A.B.C.D");
        return 2;
    };
    let Some(svc) = p.env.namespace(b"/svc") else {
        let _ = p.complain(b"", "no /svc");
        return 3;
    };
    let mut fs = Fs::new(svc);
    let mut net = match Net::connect(&mut fs, b"net", sys::clock_get() + 10_000) {
        Ok(n) => n,
        Err(e) => {
            let _ = p.complain(b"", &format!("cannot reach the network service: {e:?}"));
            return 4;
        }
    };
    let mut echo = match net.open_echo(sys::clock_get() + 5000) {
        Ok(e) => e,
        Err(e) => {
            let _ = p.complain(b"", &format!("cannot open an echo endpoint: {e:?}"));
            return 5;
        }
    };
    let data = [0x5au8; 32];
    let mut got = 0;
    for seq in 1..=count {
        let t0 = sys::clock_get();
        match echo.ping(dst, seq as u16, &data, t0 + 1000) {
            Ok(r) if r.payload == data => {
                got += 1;
                let line = format!(
                    "ping: reply from {}.{}.{}.{} seq={} ttl={} time={}ms\n",
                    r.src[0],
                    r.src[1],
                    r.src[2],
                    r.src[3],
                    r.seq,
                    r.ttl,
                    sys::clock_get() - t0
                );
                let _ = p.out(line.as_bytes());
            }
            Ok(_) => {
                let _ = p.out(b"ping: reply with the wrong data\n");
            }
            Err(e) => {
                let _ = p.out(format!("ping: seq={seq} no reply ({e:?})\n").as_bytes());
            }
        }
    }
    let _ = p.out(format!("ping: {count} sent, {got} received\n").as_bytes());
    if got > 0 {
        0
    } else {
        1
    }
}
