//! `udpecho PORT`: bind a UDP port and send every datagram back to its sender (S8, NET-SPEC 7.6). Runs until killed. Prints
//! `udpecho: listening on PORT` once it is bound (the host-side test waits for it).
#![no_std]
#![no_main]

extern crate alloc;

#[path = "../prog.rs"]
mod prog;

use alloc::format;
use rt::fs::Fs;
use rt::net::{Net, NetError};
use rt::sys;

#[no_mangle]
pub fn app_main() -> i32 {
    let p = match prog::start() {
        Ok(p) => p,
        Err(c) => return c,
    };
    let Some(port) = p
        .args()
        .first()
        .and_then(|a| core::str::from_utf8(a).ok())
        .and_then(|a| a.parse::<u16>().ok())
    else {
        let _ = p.complain(b"", "usage: udpecho PORT");
        return 2;
    };
    let Some(svc) = p.env.namespace(b"/svc") else {
        let _ = p.complain(b"", "no /svc");
        return 3;
    };
    let mut fs = Fs::new(svc);
    let name: &[u8] = if port < 1024 { b"net-priv" } else { b"net" };
    let mut net = match Net::connect(&mut fs, name, sys::clock_get() + 10_000) {
        Ok(n) => n,
        Err(e) => {
            let _ = p.complain(b"", &format!("cannot reach the network service: {e:?}"));
            return 4;
        }
    };
    let mut udp = match net.open_udp(port, 0, sys::clock_get() + 5000) {
        Ok(u) => u,
        Err(e) => {
            let _ = p.complain(b"", &format!("cannot bind port {port}: {e:?}"));
            return 5;
        }
    };
    let _ = p.out(format!("udpecho: listening on {port}\n").as_bytes());
    loop {
        match udp.recv(sys::clock_get() + 1000) {
            Ok(d) => {
                let _ = udp.send_to(d.src, d.port, &d.payload);
            }
            Err(NetError::TimedOut) => {}
            Err(_) => return 6, // the stack went away: exit, the shell or the test starts us again
        }
    }
}
