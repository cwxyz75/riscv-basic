//! `dhcpc`: the DHCP client (S8, NET-SPEC 8.2). The state machine is the pure `dhcp` crate; this program gives it packets
//! from a privileged UDP endpoint on port 68 (broadcast and source 0.0.0.0 allowed), the clock, and applies its decisions
//! through the configure connection. It keeps its lease in the configuration blob so a restart resumes (renews) instead of
//! discovering again.
//!
//! Needs `/svc/net-priv` and `/svc/net-config` in its `/svc` namespace entry, and the `NetConfigBlob` handle.
#![no_std]
#![no_main]

extern crate alloc;

#[path = "../netblob.rs"]
mod netblob;
#[path = "../prog.rs"]
mod prog;

use alloc::format;
use dhcp::{Action, Client, Lease};
use proto::startup::Role;
use rt::fs::Fs;
use rt::net::{Net, NetError, Udp};
use rt::sys;

const BLOB_VA: u64 = 0x540_0000;

fn now_ns() -> u64 {
    sys::clock_ns()
}

fn dotted(a: [u8; 4]) -> alloc::string::String {
    format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3])
}

fn apply(
    actions: alloc::vec::Vec<Action>,
    udp: &mut Udp,
    cfg: &mut Net,
    blob: &netblob::Blob,
    acquired_ns: u64,
) {
    for a in actions {
        match a {
            Action::Send { payload, to } => {
                let dst = to.unwrap_or([255; 4]);
                let _ = udp.send_to(dst, netwire::dhcp::PORT_SERVER, &payload);
            }
            Action::Configure(l) => {
                let _ = cfg.set_config(l.addr, l.mask, l.router, l.dns, sys::clock_get() + 3000);
                blob.write_lease(Some(netblob::Lease {
                    server: l.server,
                    addr: l.addr,
                    mask: l.mask,
                    router: l.router,
                    dns: l.dns,
                    lease_s: l.lease_s,
                    t1_s: l.t1_s,
                    t2_s: l.t2_s,
                    acquired_ns,
                }));
                rt::println!(
                    "dhcpc: bound {} mask {} router {} dns {} lease {} s",
                    dotted(l.addr),
                    dotted(l.mask),
                    dotted(l.router),
                    dotted(l.dns),
                    l.lease_s
                );
            }
            Action::Deconfigure => {
                let _ = cfg.clear_config(sys::clock_get() + 3000);
                blob.write_lease(None);
                rt::println!("dhcpc: lease lost");
            }
        }
    }
}

#[no_mangle]
pub fn app_main() -> i32 {
    let p = match prog::start() {
        Ok(p) => p,
        Err(c) => return c,
    };
    let (Some(svc), Some(blob_vmo)) = (p.env.namespace(b"/svc"), p.env.handle(Role::NetConfigBlob))
    else {
        return 3;
    };
    let me = sys::process_self().a1 as u32;
    if sys::process_map(me, blob_vmo, BLOB_VA, 0, 4096, 3).status != 0 {
        return 4;
    }
    let blob = netblob::Blob {
        base: BLOB_VA as usize,
    };
    let mut fs = Fs::new(svc);
    let (Ok(mut priv_net), Ok(mut cfg)) = (
        Net::connect(&mut fs, b"net-priv", sys::clock_get() + 30_000),
        Net::connect(&mut fs, b"net-config", sys::clock_get() + 30_000),
    ) else {
        return 5;
    };
    let Ok(mut udp) = priv_net.open_udp(
        68,
        proto::net::oflags::BROADCAST | proto::net::oflags::SRC_ANY,
        sys::clock_get() + 5000,
    ) else {
        return 6;
    };
    // wait for the link (the driver attaches a moment after the stack starts)
    let mac = loop {
        match cfg.info(sys::clock_get() + 3000) {
            Ok(i) if i.link => break i.mac,
            Ok(_) => {
                let gate = sys::event_create();
                if gate.status == 0 {
                    sys::object_wait(gate.a1 as u32, 1 << 24, sys::clock_get() + 50);
                    sys::handle_close(gate.a1 as u32);
                }
            }
            Err(_) => return 7,
        }
    };
    let mut client = Client::new(mac, sys::clock_get() ^ 0x5eed);
    let mut acquired_ns = now_ns();
    let first = match blob.read_lease() {
        Some(l) => {
            acquired_ns = l.acquired_ns;
            let lease = Lease {
                addr: l.addr,
                mask: l.mask,
                router: l.router,
                dns: l.dns,
                server: l.server,
                lease_s: l.lease_s,
                t1_s: l.t1_s,
                t2_s: l.t2_s,
            };
            rt::println!("dhcpc: resuming a lease for {}", dotted(l.addr));
            client.resume(lease, l.acquired_ns, now_ns())
        }
        None => client.start(now_ns()),
    };
    apply(first, &mut udp, &mut cfg, &blob, acquired_ns);
    loop {
        let now_t = sys::clock_get();
        let deadline = client
            .next_deadline()
            .map_or(now_t + 1000, |ns| (ns / 1_000_000).max(now_t));
        match udp.recv(deadline.min(now_t + 1000)) {
            Ok(d) => {
                let acts = client.on_packet(&d.payload, now_ns());
                if acts.iter().any(|a| matches!(a, Action::Configure(_))) {
                    acquired_ns = now_ns();
                }
                apply(acts, &mut udp, &mut cfg, &blob, acquired_ns);
            }
            Err(NetError::TimedOut) => {}
            Err(_) => return 8, // the stack went away: exit and let the supervisor restart us
        }
        let acts = client.on_timer(now_ns());
        apply(acts, &mut udp, &mut cfg, &blob, acquired_ns);
    }
}
