//! `netinfo`: what the network stack says about itself (the `Info` call), one line.
#![no_std]
#![no_main]

extern crate alloc;

#[path = "../prog.rs"]
mod prog;

use alloc::format;
use rt::fs::Fs;
use rt::net::Net;
use rt::sys;

#[no_mangle]
pub fn app_main() -> i32 {
    let p = match prog::start() {
        Ok(p) => p,
        Err(c) => return c,
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
    match net.info(sys::clock_get() + 5000) {
        Ok(i) => {
            let ip = |a: [u8; 4]| format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3]);
            let line = format!(
                "netinfo: addr={} mask={} gateway={} dns={} mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} mtu={} link={} configured={}\n",
                ip(i.addr), ip(i.mask), ip(i.gateway), ip(i.dns), i.mac[0], i.mac[1], i.mac[2], i.mac[3], i.mac[4], i.mac[5], i.mtu, i.link, i.configured
            );
            let _ = p.out(line.as_bytes());
            0
        }
        Err(e) => {
            let _ = p.complain(b"", &format!("Info failed: {e:?}"));
            5
        }
    }
}
