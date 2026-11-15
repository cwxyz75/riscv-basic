//! The network tests of the boot suite (S8, NET-SPEC 12.2): included by `suite.rs` as a child module so it can use the
//! suite's helpers. Each test starts what it needs (device manager, `netsup` and its children) in jobs of its own and
//! ends with nothing left behind; a machine without a network device skips them (said plainly).
use super::*;
use netring::{ctl, msg, stat, state, Entry, Pool, Ring, RingId, Slot};

#[path = "netshm.rs"]
mod netshm;
use netshm::Shm;

const RING_VA: u64 = 0x18_0000_0000;
const RX_VA: u64 = 0x18_0010_0000;
const TX_VA: u64 = 0x18_0020_0000;
const OUR_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

/// The stack side of a running driver, as the tests play it.
struct Net {
    shm: Shm,
    ring: u32,
    rx: u32,
    tx: u32,
    to_stack: u32,
    to_driver: u32,
    ctl: u32,
    /// `netsup`, the device manager and the channel that keeps the manager's connection.
    sup: u32,
    mgr: u32,
    mgr_chan: u32,
    jobs: [u32; 2],
}

fn spawn_prog(bi: u32, name: &[u8], args: &[&[u8]], roles: Vec<(Role, u32)>, handles: &[u32], job: u32) -> Result<u32, Detail> {
    spawn_prog_env(bi, name, args, roles, Vec::new(), handles, job)
}

fn spawn_prog_env(bi: u32, name: &[u8], args: &[&[u8]], roles: Vec<(Role, u32)>, env: Vec<Vec<u8>>, handles: &[u32], job: u32) -> Result<u32, Detail> {
    let (off, len) = image_entry(bi, name)?;
    let image = rt::spawn::read_image(bi, off, len).ok_or_else(|| msg_at(line!()))?;
    let s = Startup { args: args.iter().map(|a| a.to_vec()).collect(), roles, env, ..Startup::default() };
    let bytes = s.encode().map_err(|_| msg_at(line!()))?;
    rt::spawn::spawn_in(&image, &bytes, handles, job).map_err(|e| {
        let mut d = Detail::new();
        let _ = core::fmt::Write::write_fmt(&mut d, format_args!("spawn: {e:?}"));
        d
    })
}

/// Start the device manager and `netsup` (stackless: the test is the stack). `Ok(None)`: no network device on this machine.
fn start_stack() -> Result<Option<Net>, Detail> {
    let res = ok(sys::resource_root(), "resource_root")?.a1 as u32;
    let bi = boot_image_vmo()?;
    let root = root_job()?;
    let mj = job(root, u64::MAX, u64::MAX, u64::MAX)?;
    let sj = job(root, u64::MAX, u64::MAX, u64::MAX)?;
    sys::handle_close(root);
    // the device manager, and a second connection for `netsup` (a claim lives as long as its connection)
    let (mine, theirs) = channel()?;
    let res_dup = ok(sys::handle_duplicate(res, Rights::SAME.0), "dup resource")?.a1 as u32;
    let bi_dup = ok(sys::handle_duplicate(bi, Rights::SAME.0), "dup image")?.a1 as u32;
    let mgr = spawn_prog(
        bi,
        b"bin/devmgr",
        &[b"devmgr"],
        vec![
            (Role::Resource, 0),
            (Role::BootImage, 1),
            (Role::ServiceDevmgr, 2),
        ],
        &[res_dup, bi_dup, theirs],
        mj,
    )?;
    let (sup_end, mgr_end) = channel()?;
    let req = proto::devmgr::encode_connect(1);
    ensure_eq!(
        st(sys::channel_write(
            mine,
            req.as_ptr(),
            req.len(),
            &mgr_end,
            1
        )),
        Some(S::Ok),
        "connect a second client"
    );
    let w = sys::object_wait(mine, sig::READABLE, sys::clock_get() + 5000);
    ensure_eq!(st(w), Some(S::Ok), "the manager answers the connect");
    let mut rb = [0u8; 16];
    sys::channel_read(mine, rb.as_mut_ptr(), 16, core::ptr::null_mut(), 0);
    let (parent, parent_theirs) = channel()?;
    let bi2 = ok(sys::handle_duplicate(bi, Rights::SAME.0), "dup image")?.a1 as u32;
    let sup = spawn_prog(
        bi,
        b"bin/netsup",
        &[b"netsup", b"stackless"],
        vec![
            (Role::ServiceDevmgr, 0),
            (Role::BootImage, 1),
            (Role::ServiceInit, 2),
        ],
        &[sup_end, bi2, parent_theirs],
        sj,
    )?;
    // the stack's handles arrive in one message
    let w = sys::object_wait(
        parent,
        sig::READABLE | sig::PEER_CLOSED,
        sys::clock_get() + 5000,
    );
    ensure_eq!(st(w), Some(S::Ok), "netsup answers");
    let mut b = [0u8; 16];
    let mut hs = [0u32; 8];
    let r = sys::channel_read(parent, b.as_mut_ptr(), 16, hs.as_mut_ptr(), 8);
    if r.status != 0 {
        // netsup ended without handing anything over: no device (exit code 13)
        wait_ended(sup, 3000)?;
        let code = sys::process_info(sup).a1 as i64;
        for h in [sup, mgr, mine, parent, mj, sj] {
            sys::handle_close(h);
        }
        ensure_eq!(
            code,
            13,
            "netsup's exit code without a network device (NoDevice)"
        );
        return Ok(None);
    }
    ensure_eq!(
        (r.a1, r.a2),
        (6, 6),
        "netsup hands over six bytes and six handles"
    );
    let me = me()?;
    let ring = hs[0];
    ok(
        sys::process_map(me, ring, RING_VA, 0, 2 * 4096, PF_R | PF_W),
        "map the rings",
    )?;
    ok(
        sys::process_map(me, hs[1], RX_VA, 0, 64 * 4096, PF_R),
        "map the receive pool read-only",
    )?;
    ok(
        sys::process_map(me, hs[2], TX_VA, 0, 64 * 4096, PF_R | PF_W),
        "map the transmit pool",
    )?;
    sys::handle_close(parent);
    Ok(Some(Net {
        shm: Shm {
            base: RING_VA as usize,
        },
        ring,
        rx: hs[1],
        tx: hs[2],
        to_stack: hs[3],
        to_driver: hs[4],
        ctl: hs[5],
        sup,
        mgr,
        mgr_chan: mine,
        jobs: [mj, sj],
    }))
}

impl Net {
    fn word(&self, off: usize) -> u32 {
        netring::Mem::load(&self.shm, off)
    }

    fn wait_state(&self, want: u32, ticks: u64) -> bool {
        let until = sys::clock_get() + ticks;
        while sys::clock_get() < until {
            if self.word(ctl::STATE) == want {
                return true;
            }
            sys::thread_yield();
        }
        self.word(ctl::STATE) == want
    }

    fn stat(&self, which: usize) -> u64 {
        self.shm.load64(ctl::STATS + 8 * which)
    }

    fn close(self) {
        let me = me().unwrap_or(0);
        for va in [RING_VA, RX_VA, TX_VA] {
            sys::process_unmap(me, va);
        }
        sys::process_kill(self.sup, -1);
        let _ = sys::object_wait(self.sup, sig::TERMINATED, sys::clock_get() + 3000);
        sys::process_kill(self.mgr, -1);
        let _ = sys::object_wait(self.mgr, sig::TERMINATED, sys::clock_get() + 3000);
        for h in [
            self.ring,
            self.rx,
            self.tx,
            self.to_stack,
            self.to_driver,
            self.ctl,
            self.sup,
            self.mgr,
            self.mgr_chan,
        ] {
            sys::handle_close(h);
        }
        // everything the two jobs held is gone
        let _ = job_use_settles(self.jobs[0], 2, 0, 1000);
        let _ = job_use_settles(self.jobs[1], 2, 0, 1000);
        sys::handle_close(self.jobs[0]);
        sys::handle_close(self.jobs[1]);
    }
}

fn arp_request_frame(target: [u8; 4]) -> Vec<u8> {
    let mut f = vec![0u8; 60];
    netwire::eth::write_header(
        &mut f,
        netwire::BROADCAST_MAC,
        OUR_MAC,
        netwire::eth::ETHERTYPE_ARP,
    )
    .unwrap();
    netwire::arp::write(
        &mut f[14..],
        &netwire::arp::Arp {
            op: 1,
            sha: OUR_MAC,
            spa: [10, 0, 2, 15],
            tha: [0; 6],
            tpa: target,
        },
    )
    .unwrap();
    f
}

fn read_ctl(ch: u32) -> Option<(u32, msg::Msg)> {
    let w = sys::object_wait(ch, sig::READABLE, sys::clock_get() + 5000);
    if w.status != 0 {
        return None;
    }
    let mut b = [0u8; 64];
    let r = sys::channel_read(ch, b.as_mut_ptr(), 64, core::ptr::null_mut(), 0);
    if r.status != 0 {
        return None;
    }
    msg::decode(&b[..r.a1])
}

/// Q1 and the loopback of step 3: the driver attaches, and a real frame goes out through the rings and the real device
/// and its answer comes back (slirp answers an ARP request for its gateway).
pub fn net_driver_attaches_and_exchanges_a_frame() -> Outcome {
    let Some(n) = start_stack()? else {
        sys::debug_write(b"s8: no network device on this machine: net tests skipped\n");
        return Ok(());
    };
    let r = (|| -> Outcome {
        ensure_eq!(
            n.word(ctl::MAGIC),
            ctl::MAGIC_VALUE,
            "the control header's magic"
        );
        ensure_eq!(
            (n.word(ctl::SLOTS), n.word(ctl::SLOT_SIZE)),
            (128, 2048),
            "slots and slot size"
        );
        // the driver says READY with its generation and the MAC the harness gave the device
        let Some((
            _,
            msg::Msg::Ready {
                generation,
                mac,
                link,
            },
        )) = read_ctl(n.ctl)
        else {
            return Err(msg_at(line!()));
        };
        ensure_eq!(generation, 1, "the first generation");
        ensure_eq!(mac, OUR_MAC, "the MAC is the one QEMU was given");
        ensure!(link, "link up");
        ensure_eq!(
            n.shm.load64(ctl::GENERATION),
            1,
            "the header carries the generation"
        );
        ensure!(
            n.wait_state(state::DRIVER_READY, 1000),
            "state DRIVER_READY"
        );
        let at = msg::encode_attach(2, generation);
        ensure_eq!(
            st(sys::channel_write(
                n.ctl,
                at.as_ptr(),
                at.len(),
                core::ptr::null(),
                0
            )),
            Some(S::Ok),
            "ATTACH"
        );
        ensure!(
            n.wait_state(state::STACK_ATTACHED, 1000),
            "state STACK_ATTACHED"
        );
        // send an ARP request for the gateway through a transmit slot
        let frame = arp_request_frame([10, 0, 2, 2]);
        let slot = 5u32;
        let base = TX_VA as usize + slot as usize * 2048;
        for i in 0..12 {
            // SAFETY: TX_VA maps the transmit pool read-write; slot 5's 2048 bytes are inside it.
            unsafe { core::ptr::write_volatile((base + i) as *mut u8, 0) };
        }
        for (i, b) in frame.iter().enumerate() {
            // SAFETY: as above.
            unsafe { core::ptr::write_volatile((base + 12 + i) as *mut u8, *b) };
        }
        let ready = Ring::new(&n.shm, RingId::TxReady);
        ensure!(
            ready
                .push(Entry {
                    slot,
                    len: frame.len() as u16
                })
                .is_ok(),
            "publish the frame"
        );
        sys::event_signal(n.to_driver, 0, 1 << 24);
        // the answer: wait for an rx_used entry holding an ARP reply from the gateway
        let until = sys::clock_get() + 3000;
        let mut got: Option<Vec<u8>> = None;
        let mut done = false;
        let rx_used = Ring::new(&n.shm, RingId::RxUsed);
        let tx_done = Ring::new(&n.shm, RingId::TxDone);
        let rx_free = Ring::new(&n.shm, RingId::RxFree);
        let mut stack_rx = Pool::rx_posted();
        while sys::clock_get() < until && !(got.is_some() && done) {
            if let Ok(Some(e)) = tx_done.pop() {
                ensure_eq!(e.slot, slot, "the transmit completion names our slot");
                done = true;
            }
            if let Ok(Some(e)) = rx_used.pop() {
                let e = netring::validate(RingId::RxUsed, e).map_err(|_| msg_at(line!()))?;
                stack_rx
                    .mv(e.slot, Slot::DeviceHas, Slot::StackHas)
                    .map_err(|_| msg_at(line!()))?;
                let mut f = vec![0u8; usize::from(e.len)];
                for (i, b) in f.iter_mut().enumerate() {
                    // SAFETY: RX_VA maps the receive pool read-only; the entry was validated (slot < 128, len <= 1514).
                    *b = unsafe {
                        core::ptr::read_volatile(
                            (RX_VA as usize + e.slot as usize * 2048 + 12 + i) as *const u8,
                        )
                    };
                }
                // the header the device wrote: no flags, no GSO
                // SAFETY: as above (first 12 bytes of the slot).
                let h0 = unsafe {
                    core::ptr::read_volatile((RX_VA as usize + e.slot as usize * 2048) as *const u8)
                };
                let h1 = unsafe {
                    core::ptr::read_volatile(
                        (RX_VA as usize + e.slot as usize * 2048 + 1) as *const u8,
                    )
                };
                ensure_eq!((h0, h1), (0, 0), "the receive header: no flags, gso none");
                let is_reply = f.len() >= 42 && f[12..14] == [8, 6] && f[20..22] == [0, 2];
                if is_reply {
                    got = Some(f);
                }
                rx_free
                    .push(Entry {
                        slot: e.slot,
                        len: 0,
                    })
                    .map_err(|_| msg_at(line!()))?;
                stack_rx
                    .mv(e.slot, Slot::StackHas, Slot::DeviceHas)
                    .map_err(|_| msg_at(line!()))?;
                sys::event_signal(n.to_driver, 0, 1 << 24);
            } else {
                sys::thread_yield();
            }
        }
        ensure!(done, "the driver reported the transmit done");
        let f = got.ok_or_else(|| msg_at(line!()))?;
        ensure_eq!(f.len(), 64, "slirp's ARP reply is a 64-byte frame (observed in the capture); the ring length is the frame's, without the virtio header");
        let a = netwire::arp::parse(&f[14..]).map_err(|_| msg_at(line!()))?;
        ensure_eq!(
            (a.op, a.spa, a.tpa),
            (2, [10, 0, 2, 2], [10, 0, 2, 15]),
            "slirp answers for its gateway"
        );
        ensure_eq!(a.sha, [0x52, 0x55, 10, 0, 2, 2], "with the gateway's MAC");
        ensure!(
            n.stat(stat::TX_FRAMES) >= 1 && n.stat(stat::RX_FRAMES) >= 1,
            "the counters moved"
        );
        ensure_eq!(n.stat(stat::RING_VIOLATIONS), 0, "no ring violations");
        // the receive pool is read-only here: no write handle exists in this view (Q16)
        let w = sys::process_map(me()?, n.rx, 0x18_0030_0000, 0, 4096, PF_R | PF_W);
        ensure_eq!(
            st(w),
            Some(S::AccessDenied),
            "the receive pool cannot be mapped writable from the stack's handle"
        );
        let p = sys::vmo_phys_addr(n.rx);
        ensure!(
            p.status != 0,
            "the stack's pool handle has no physical address to give"
        );
        Ok(())
    })();
    n.close();
    r
}

// ---- the full stack: netsup supervising netd and netdrv, the registry, clients over /svc ----

/// A running full stack as the tests see it.
struct Full {
    mgr: u32,
    mgr_chan: u32,
    sup: u32,
    /// The channel to `netsup` (Status, Restart).
    ctl: u32,
    reg: u32,
    /// The registry's root connection (it can publish, view).
    root: Fs,
    jobs: [u32; 3],
}

fn start_full() -> Result<Option<Full>, Detail> {
    start_full_with(false)
}

/// `dhcp`: let `netsup` start the DHCP client too (the static-configuration tests leave it out).
fn start_full_with(dhcp: bool) -> Result<Option<Full>, Detail> {
    start_full_opts(dhcp, None)
}

/// `fault`: a deliberate fault for the children to run (`FAULT=name`; the negative controls of NET-SPEC 12.4).
fn start_full_opts(dhcp: bool, fault: Option<&[u8]>) -> Result<Option<Full>, Detail> {
    let res = ok(sys::resource_root(), "resource_root")?.a1 as u32;
    let bi = boot_image_vmo()?;
    let root_j = root_job()?;
    let mj = job(root_j, u64::MAX, u64::MAX, u64::MAX)?;
    let sj = job(root_j, u64::MAX, u64::MAX, u64::MAX)?;
    let rj = job(root_j, u64::MAX, u64::MAX, u64::MAX)?;
    sys::handle_close(root_j);
    let (mine, theirs) = channel()?;
    let res_dup = ok(sys::handle_duplicate(res, Rights::SAME.0), "dup resource")?.a1 as u32;
    let bi_dup = ok(sys::handle_duplicate(bi, Rights::SAME.0), "dup image")?.a1 as u32;
    let mgr = spawn_prog(
        bi,
        b"bin/devmgr",
        &[b"devmgr"],
        vec![
            (Role::Resource, 0),
            (Role::BootImage, 1),
            (Role::ServiceDevmgr, 2),
        ],
        &[res_dup, bi_dup, theirs],
        mj,
    )?;
    let (sup_dm, mgr_end) = channel()?;
    let req = proto::devmgr::encode_connect(1);
    ensure_eq!(
        st(sys::channel_write(
            mine,
            req.as_ptr(),
            req.len(),
            &mgr_end,
            1
        )),
        Some(S::Ok),
        "connect a second client"
    );
    ensure_eq!(
        st(sys::object_wait(
            mine,
            sig::READABLE,
            sys::clock_get() + 5000
        )),
        Some(S::Ok),
        "the manager answers the connect"
    );
    let mut rb = [0u8; 16];
    sys::channel_read(mine, rb.as_mut_ptr(), 16, core::ptr::null_mut(), 0);
    // the registry
    let (off, len) = image_entry(bi, b"bin/registryd")?;
    let image = rt::spawn::read_image(bi, off, len).ok_or_else(|| msg_at(line!()))?;
    let (reg_mine, reg_theirs) = channel()?;
    let s = Startup {
        args: vec![b"registryd".to_vec()],
        roles: vec![(Role::ServiceRegistry, 0)],
        ..Startup::default()
    };
    let reg = rt::spawn::spawn_in(
        &image,
        &s.encode().map_err(|_| msg_at(line!()))?,
        &[reg_theirs],
        rj,
    )
    .map_err(|_| msg_at(line!()))?;
    let mut root = Fs::new(reg_mine);
    let publish_conn = root.clone_conn(prt::ALL).map_err(|e| msg_e(e, line!()))?;
    let (ctl, ctl_theirs) = channel()?;
    let bi2 = ok(sys::handle_duplicate(bi, Rights::SAME.0), "dup image")?.a1 as u32;
    let fault_env: Vec<Vec<u8>> = fault.map(|f| vec![[&b"FAULT="[..], f].concat()]).unwrap_or_default();
    let sup = spawn_prog_env(
        bi,
        b"bin/netsup",
        if dhcp {
            &[b"netsup"]
        } else {
            &[b"netsup", b"nodhcp"]
        },
        vec![
            (Role::ServiceDevmgr, 0),
            (Role::BootImage, 1),
            (Role::ServiceInit, 2),
            (Role::ServiceRegistry, 3),
        ],
        fault_env,
        &[sup_dm, bi2, ctl_theirs, publish_conn.ch],
        sj,
    )?;
    // a machine without a network device: netsup ends with 13 (NoDevice) before saying anything
    let up = sys::object_wait(sup, sig::TERMINATED, sys::clock_get() + 1500);
    if up.status == 0 {
        let code = sys::process_info(sup).a1 as i64;
        for h in [sup, mgr, mine, ctl, reg, mj, sj, rj, reg_mine] {
            sys::handle_close(h);
        }
        ensure_eq!(code, 13, "netsup's exit code without a network device");
        return Ok(None);
    }
    Ok(Some(Full {
        mgr,
        mgr_chan: mine,
        sup,
        ctl,
        reg,
        root,
        jobs: [mj, sj, rj],
    }))
}

impl Full {
    fn view(&mut self, name: &[u8]) -> Result<Fs, Detail> {
        self.root
            .view(&[name], prt::READ | prt::ENUMERATE)
            .map_err(|e| msg_e(e, line!()))
    }

    /// A connection to a published name, waiting until the stack has published it.
    fn connect(&mut self, name: &[u8]) -> Result<rt::net::Net, Detail> {
        let mut v = self.view(name)?;
        rt::net::Net::connect(&mut v, name, sys::clock_get() + 15_000).map_err(|e| {
            let mut d = Detail::new();
            let _ = core::fmt::Write::write_fmt(
                &mut d,
                format_args!(
                    "connect {}: {e:?}",
                    core::str::from_utf8(name).unwrap_or("?")
                ),
            );
            d
        })
    }

    fn close(self) {
        for p in [self.sup, self.mgr, self.reg] {
            sys::process_kill(p, -1);
            let _ = sys::object_wait(p, sig::TERMINATED, sys::clock_get() + 3000);
        }
        for h in [self.sup, self.mgr, self.mgr_chan, self.ctl, self.reg] {
            sys::handle_close(h);
        }
        sys::handle_close(self.root.ch);
        for j in self.jobs {
            let _ = job_use_settles(j, 2, 0, 1500);
            sys::handle_close(j);
        }
    }
}

/// Q2 (static configuration): configure 10.0.2.15/24 through `/svc/net-config` and ping the gateway; the answers are slirp's.
pub fn net_static_config_and_ping_the_gateway() -> Outcome {
    let Some(mut f) = start_full()? else {
        sys::debug_write(b"s8: no network device on this machine: net tests skipped\n");
        return Ok(());
    };
    let r = (|| -> Outcome {
        let t = |n: u64| sys::clock_get() + n;
        let mut cfg = f.connect(b"net-config")?;
        let i = cfg.info(t(3000)).map_err(|_| msg_at(line!()))?;
        ensure!(!i.configured, "unconfigured to begin with");
        ensure_eq!(i.mtu, 1500, "the MTU");
        cfg.set_config(
            [10, 0, 2, 15],
            [255, 255, 255, 0],
            [10, 0, 2, 2],
            [10, 0, 2, 3],
            t(3000),
        )
        .map_err(|_| msg_at(line!()))?;
        // the link comes up when the driver has attached
        let until = t(8000);
        let mut info = cfg.info(t(3000)).map_err(|_| msg_at(line!()))?;
        while !info.link && sys::clock_get() < until {
            nap(20)?;
            info = cfg.info(t(3000)).map_err(|_| msg_at(line!()))?;
        }
        ensure!(info.link && info.configured, "link up and configured");
        ensure_eq!(info.mac, OUR_MAC, "the MAC the driver read from the device");
        ensure_eq!(info.addr, [10, 0, 2, 15], "the address");
        // a normal client pings the gateway
        let mut net = f.connect(b"net")?;
        let mut echo = net.open_echo(t(3000)).map_err(|_| msg_at(line!()))?;
        let mut got = 0;
        for seq in 1..=5u16 {
            let data = [seq as u8; 32];
            if let Ok(r) = echo.ping([10, 0, 2, 2], seq, &data, t(1500)) {
                ensure_eq!(
                    (r.seq, r.src),
                    (seq, [10, 0, 2, 2]),
                    "the reply's sequence and source"
                );
                ensure_eq!(r.payload, data.to_vec(), "the reply echoes the data");
                got += 1;
            }
        }
        ensure!(
            got >= 4,
            "at least four of five echo requests were answered ({got})"
        );
        let arp = cfg.arp(t(3000)).map_err(|_| msg_at(line!()))?;
        ensure!(
            arp.iter()
                .any(|e| e.0 == [10, 0, 2, 2] && e.1 == [0x52, 0x55, 10, 0, 2, 2]),
            "the gateway is in the ARP table with slirp's MAC"
        );
        let stats = cfg.stats(t(3000)).map_err(|_| msg_at(line!()))?;
        ensure!(stats.iter().any(|s| s.1 > 0), "the counters moved");
        Ok(())
    })();
    f.close();
    r
}

/// Q4 (the guest side): `udpecho` on port 7777 through the real stack. The harness's host-side client (the host's own UDP
/// stack, through QEMU's `hostfwd`) sends it datagrams and checks what comes back; this test echoes until the host says stop.
pub fn net_udp_echo_through_hostfwd() -> Outcome {
    let Some(mut f) = start_full()? else {
        sys::debug_write(b"s8: no network device on this machine: net tests skipped\n");
        return Ok(());
    };
    let r = (|| -> Outcome {
        let t = |n: u64| sys::clock_get() + n;
        let mut cfg = f.connect(b"net-config")?;
        cfg.set_config(
            [10, 0, 2, 15],
            [255, 255, 255, 0],
            [10, 0, 2, 2],
            [10, 0, 2, 3],
            t(3000),
        )
        .map_err(|_| msg_at(line!()))?;
        let mut net = f.connect(b"net")?;
        let mut udp = net
            .open_udp(7777, 0, t(3000))
            .map_err(|_| msg_at(line!()))?;
        ensure_eq!(udp.port, 7777, "the port asked for");
        // (until the host has a reason to send, wait for the link: the stack's first frame lets slirp learn our address)
        let until = t(8000);
        loop {
            let i = cfg.info(t(3000)).map_err(|_| msg_at(line!()))?;
            if i.link || sys::clock_get() > until {
                break;
            }
            nap(20)?;
        }
        sys::debug_write(b"s8: udpecho ready\n");
        let end = sys::clock_get() + 60_000;
        let mut echoed = 0u32;
        let mut flood_checked = false;
        while sys::clock_get() < end {
            match udp.recv(t(1000)) {
                Ok(d) if d.payload == b"##stall" => {
                    // Q8: stop reading while the host floods this endpoint: the stack must stay responsive, bound its
                    // memory (drop and count) and never wait for us
                    let mut probe = net.open_echo(t(3000)).map_err(|_| msg_at(line!()))?;
                    let mut answered = 0;
                    for seq in 0..3u16 {
                        if probe.ping([10, 0, 2, 2], seq, b"alive", t(700)).is_ok() {
                            answered += 1;
                        }
                        nap(300)?;
                    }
                    ensure!(answered >= 2, "the stack still answers pings while an endpoint is flooded and stalled ({answered} of 3)");
                    // drain what was queued, then take the host's tail datagram: the count of what was dropped rides on the
                    // next datagram delivered (7.2), which the host sends once the flood is over
                    let (mut n, mut dropped) = (0u32, 0u64);
                    while let Ok(d) = udp.recv(t(8000)) {
                        n += 1;
                        dropped += u64::from(d.dropped);
                        if d.payload == b"##tail" {
                            break;
                        }
                    }
                    ensure!(n > 0 && n < 3000, "some of the flood was delivered and not all ({n} delivered, dropped {dropped})");
                    ensure!(dropped > 0, "the endpoint's dropped count says datagrams were discarded ({dropped}; {n} delivered; tail seen: {})", flood_checked);
                    let stats = cfg.stats(t(3000)).map_err(|_| msg_at(line!()))?;
                    ensure!(
                        counter(&stats, "udp_delivered") >= u64::from(n),
                        "the counters agree"
                    );
                    udp.send_to(d.src, d.port, b"##drained")
                        .map_err(|_| msg_at(line!()))?;
                    flood_checked = true;
                }
                Ok(d) => {
                    let stop = d.payload == b"##stop";
                    udp.send_to(d.src, d.port, &d.payload)
                        .map_err(|_| msg_at(line!()))?;
                    echoed += 1;
                    if stop {
                        nap(200)?; // let the reply leave
                        break;
                    }
                }
                Err(rt::net::NetError::TimedOut) => {}
                Err(_) => return Err(msg_at(line!())),
            }
        }
        ensure!(flood_checked, "the flood test ran");
        ensure!(
            echoed >= 200,
            "the host's datagrams arrived and were echoed ({echoed})"
        );
        let stats = cfg.stats(t(3000)).map_err(|_| msg_at(line!()))?;
        ensure!(stats.iter().any(|s| s.1 > 0), "the counters moved");
        Ok(())
    })();
    f.close();
    r
}

/// The value of the named counter in a `GetStats` answer.
fn counter(stats: &[(u16, u64)], name: &str) -> u64 {
    let id = netcore::Counters::default()
        .pairs()
        .iter()
        .find(|p| p.1 == name)
        .map(|p| p.0);
    id.and_then(|id| stats.iter().find(|s| s.0 == id))
        .map_or(0, |s| s.1)
}

/// Q3: the DHCP client configures the interface from QEMU's own server (an independent implementation), and the S8 "ping the
/// gateway" acceptance then holds over the real flow; a ping to an address nobody answers for gets no reply after exactly three
/// ARP requests.
pub fn net_dhcp_configures_the_interface() -> Outcome {
    let Some(mut f) = start_full_with(true)? else {
        sys::debug_write(b"s8: no network device on this machine: net tests skipped\n");
        return Ok(());
    };
    let r = (|| -> Outcome {
        let t = |n: u64| sys::clock_get() + n;
        let mut net = f.connect(b"net")?;
        let until = t(25_000);
        let mut info = net.info(t(3000)).map_err(|_| msg_at(line!()))?;
        while !info.configured && sys::clock_get() < until {
            nap(100)?;
            info = net.info(t(3000)).map_err(|_| msg_at(line!()))?;
        }
        ensure!(
            info.configured,
            "the interface was configured by DHCP within 25 s"
        );
        ensure_eq!(
            (info.addr, info.mask, info.gateway, info.dns),
            (
                [10, 0, 2, 15],
                [255, 255, 255, 0],
                [10, 0, 2, 2],
                [10, 0, 2, 3]
            ),
            "what QEMU's server leases"
        );
        let mut echo = net.open_echo(t(3000)).map_err(|_| msg_at(line!()))?;
        let mut got = 0;
        for seq in 1..=5u16 {
            if echo
                .ping([10, 0, 2, 2], seq, &[seq as u8; 16], t(1500))
                .is_ok()
            {
                got += 1;
            }
        }
        ensure!(
            got >= 4,
            "the gateway answers pings after DHCP ({got} of 5)"
        );
        ensure!(
            echo.ping([10, 0, 2, 3], 9, b"dns", t(1500)).is_ok(),
            "slirp's DNS address answers pings too"
        );
        // nobody answers for 10.0.2.99: three ARP requests, then NoRoute, and no echo reply
        let mut cfg = f.connect(b"net-config")?;
        let before = counter(
            &cfg.stats(t(3000)).map_err(|_| msg_at(line!()))?,
            "arp_requests_sent",
        );
        let r = echo.ping([10, 0, 2, 99], 1, b"x", t(6000));
        ensure!(r.is_err(), "no reply from an address nobody answers for");
        let after = counter(
            &cfg.stats(t(3000)).map_err(|_| msg_at(line!()))?,
            "arp_requests_sent",
        );
        ensure_eq!(after - before, 3, "exactly three ARP requests");
        Ok(())
    })();
    f.close();
    r
}

// ---- confinement and supervision ----

/// `netsup`'s control channel: `Status` as `(running, restarts, last_exit)` for the driver, the stack and the DHCP client, then whether
/// the network is marked down and the driver generation.
/// What `Status` says: per child (running, restarts, last exit code), then down and the generation.
type SupStatus = ([(bool, u32, i32); 3], bool, u64);

fn sup_status(f: &Full) -> Option<SupStatus> {
    let req = [1, 0, 0, 0, 1, 0, 0, 0];
    sys::channel_write(f.ctl, req.as_ptr(), req.len(), core::ptr::null(), 0);
    if sys::object_wait(f.ctl, sig::READABLE, sys::clock_get() + 3000).status != 0 {
        return None;
    }
    let mut b = [0u8; 64];
    let r = sys::channel_read(f.ctl, b.as_mut_ptr(), 64, core::ptr::null_mut(), 0);
    if r.status != 0 || r.a1 < 8 + 27 + 9 {
        return None;
    }
    let mut kids = [(false, 0u32, 0i32); 3];
    for (i, k) in kids.iter_mut().enumerate() {
        let o = 8 + i * 9;
        *k = (
            b[o] != 0,
            u32::from_le_bytes(b[o + 1..o + 5].try_into().ok()?),
            i32::from_le_bytes(b[o + 5..o + 9].try_into().ok()?),
        );
    }
    Some((
        kids,
        b[35] != 0,
        u64::from_le_bytes(b[36..44].try_into().ok()?),
    ))
}

/// Ask `netsup` to kill and restart a child (0 driver, 1 stack, 2 DHCP client).
fn sup_restart(f: &Full, child: u8) {
    let req = [2, 0, 0, 0, 2, 0, 0, 0, child];
    sys::channel_write(f.ctl, req.as_ptr(), req.len(), core::ptr::null(), 0);
    let _ = sys::object_wait(f.ctl, sig::READABLE, sys::clock_get() + 3000);
    let mut b = [0u8; 16];
    sys::channel_read(f.ctl, b.as_mut_ptr(), 16, core::ptr::null_mut(), 0);
}

fn configure_static(cfg: &mut rt::net::Net) -> Outcome {
    cfg.set_config(
        [10, 0, 2, 15],
        [255, 255, 255, 0],
        [10, 0, 2, 2],
        [10, 0, 2, 3],
        sys::clock_get() + 3000,
    )
    .map_err(|_| msg_at(line!()))?;
    let until = sys::clock_get() + 8000;
    loop {
        let i = cfg
            .info(sys::clock_get() + 3000)
            .map_err(|_| msg_at(line!()))?;
        if i.link || sys::clock_get() > until {
            return Ok(());
        }
        nap(20)?;
    }
}

/// Q6, Q7 and Q10: an account confines a client; authority is the name a program's view contains; an endpoint lives exactly as long
/// as its channel.
pub fn net_clients_are_confined() -> Outcome {
    use rt::net::NetError::Stack;
    let Some(mut f) = start_full()? else {
        sys::debug_write(b"s8: no network device on this machine: net tests skipped\n");
        return Ok(());
    };
    let r = (|| -> Outcome {
        let t = |n: u64| sys::clock_get() + n;
        let refused = pn_status(2);
        let mut cfg = f.connect(b"net-config")?;
        configure_static(&mut cfg)?;
        // Q7: the normal class
        let mut net = f.connect(b"net")?;
        ensure_eq!(
            net.open_udp(80, 0, t(3000)).err(),
            Some(Stack(refused)),
            "port 80 on /svc/net"
        );
        ensure_eq!(
            net.open_udp(7001, proto::net::oflags::BROADCAST, t(3000))
                .err(),
            Some(Stack(refused)),
            "broadcast on /svc/net"
        );
        ensure_eq!(
            net.open_udp(7001, proto::net::oflags::SRC_ANY, t(3000))
                .err(),
            Some(Stack(refused)),
            "source 0.0.0.0 on /svc/net"
        );
        ensure_eq!(
            net.set_config([1, 2, 3, 4], [255, 0, 0, 0], [0; 4], [0; 4], t(3000))
                .err(),
            Some(Stack(refused)),
            "SetConfig on /svc/net"
        );
        ensure_eq!(
            net.arp(t(3000)).err(),
            Some(Stack(refused)),
            "the ARP table needs the configure class"
        );
        ensure!(net.info(t(3000)).is_ok(), "Info is for everyone");
        // the privileged class
        let mut privileged = f.connect(b"net-priv")?;
        let p80 = privileged.open_udp(80, 0, t(3000));
        ensure!(p80.is_ok(), "port 80 on /svc/net-priv");
        ensure!(
            privileged
                .open_udp(
                    7002,
                    proto::net::oflags::BROADCAST | proto::net::oflags::SRC_ANY,
                    t(3000)
                )
                .is_ok(),
            "broadcast and source 0.0.0.0 on /svc/net-priv"
        );
        ensure_eq!(
            privileged
                .set_config([1, 2, 3, 4], [255, 0, 0, 0], [0; 4], [0; 4], t(3000))
                .err(),
            Some(Stack(refused)),
            "but not SetConfig"
        );
        // the configure class opens no endpoints
        ensure_eq!(
            cfg.open_udp(7003, 0, t(3000)).err(),
            Some(Stack(refused)),
            "Open on /svc/net-config"
        );
        ensure_eq!(
            cfg.open_echo(t(3000)).err(),
            Some(Stack(refused)),
            "an echo endpoint on /svc/net-config"
        );
        // a program whose view lacks a name cannot open it
        let mut only_net = f.view(b"net")?;
        ensure!(
            only_net
                .open(b"net-priv", 0, proto::file::rights::READ)
                .is_err(),
            "a view without /svc/net-priv cannot open it"
        );
        // Q6: thirty-one endpoints, then Quota; a second client is unaffected; closing releases exactly
        let mut held = Vec::new();
        let quota = pn_status(6);
        loop {
            match net.open_udp(0, 0, t(3000)) {
                Ok(u) => held.push(u),
                Err(e) => {
                    ensure_eq!(e, Stack(quota), "the endpoint beyond the account's limit");
                    break;
                }
            }
            ensure!(held.len() <= 40, "no limit was enforced");
        }
        ensure_eq!(
            held.len(),
            31,
            "exactly 31 endpoints beside the service connection"
        );
        let mut second = f.connect(b"net")?;
        ensure!(
            second.open_udp(0, 0, t(3000)).is_ok(),
            "a second client's endpoints are unaffected"
        );
        held.pop();
        // (the stack notices the closed channel on its next pass)
        let mut again = None;
        for _ in 0..50 {
            match net.open_udp(0, 0, t(3000)) {
                Ok(u) => {
                    again = Some(u);
                    break;
                }
                Err(_) => nap(20)?,
            }
        }
        ensure!(
            again.is_some(),
            "closing one endpoint freed exactly one place"
        );
        ensure_eq!(
            net.open_udp(0, 0, t(3000)).err(),
            Some(Stack(quota)),
            "and only one"
        );
        // Q10: a closed endpoint frees its port at once
        drop(held);
        drop(again);
        let first = privileged
            .open_udp(7010, 0, t(3000))
            .map_err(|_| msg_at(line!()))?;
        ensure_eq!(
            privileged.open_udp(7010, 0, t(3000)).err(),
            Some(Stack(pn_status(3))),
            "a bound port is in use"
        );
        drop(first);
        let mut ok2 = false;
        for _ in 0..50 {
            if privileged.open_udp(7010, 0, t(3000)).is_ok() {
                ok2 = true;
                break;
            }
            nap(20)?;
        }
        ensure!(
            ok2,
            "the port is free again after the endpoint's channel closed"
        );
        drop(p80);
        Ok(())
    })();
    f.close();
    r
}

fn pn_status(v: i32) -> i32 {
    v
}

/// Q11, Q12 and Q13: killing each of the three processes under traffic leaves a working network within a bounded time.
pub fn net_children_restart() -> Outcome {
    let Some(mut f) = start_full_with(true)? else {
        sys::debug_write(b"s8: no network device on this machine: net tests skipped\n");
        return Ok(());
    };
    let r = (|| -> Outcome {
        let t = |n: u64| sys::clock_get() + n;
        let mut net = f.connect(b"net")?;
        let until = t(25_000);
        while !net.info(t(3000)).map_err(|_| msg_at(line!()))?.configured
            && sys::clock_get() < until
        {
            nap(100)?;
        }
        ensure!(
            net.info(t(3000)).map_err(|_| msg_at(line!()))?.configured,
            "configured by DHCP"
        );
        let mut echo = net.open_echo(t(3000)).map_err(|_| msg_at(line!()))?;
        ensure!(
            echo.ping([10, 0, 2, 2], 1, b"a", t(2000)).is_ok(),
            "the network works to begin with"
        );
        let (_, down, gen0) = sup_status(&f).ok_or_else(|| msg_at(line!()))?;
        ensure!(!down && gen0 >= 1, "the supervisor is up");
        // Q11: kill the driver under a ping loop: pings resume within three seconds, the generation is one higher, the endpoint
        // that was open before still works
        sup_restart(&f, 0);
        let t0 = sys::clock_get();
        let mut seq = 2u16;
        let mut resumed = None;
        while sys::clock_get() < t0 + 6000 {
            if echo.ping([10, 0, 2, 2], seq, b"b", t(700)).is_ok() {
                resumed = Some(sys::clock_get() - t0);
                break;
            }
            seq += 1;
        }
        ensure!(
            resumed.is_some_and(|ms| ms <= 3500),
            "pings resumed within about three seconds of the driver's death ({resumed:?} ms)"
        );
        let (kids, down, gen1) = sup_status(&f).ok_or_else(|| msg_at(line!()))?;
        ensure_eq!(gen1, gen0 + 1, "the generation is one higher");
        ensure!(kids[0].0 && !down, "the driver runs again");
        ensure_eq!(kids[0].1, 1, "one restart of the driver");
        // Q13: kill the DHCP client: the address stays (it was applied already) and a new client resumes from the lease
        sup_restart(&f, 2);
        nap(1500)?;
        let i = net.info(t(3000)).map_err(|_| msg_at(line!()))?;
        ensure!(
            i.configured && i.addr == [10, 0, 2, 15],
            "the address survives a restart of the DHCP client"
        );
        ensure!(
            sup_status(&f).is_some_and(|s| s.0[2].0),
            "the DHCP client runs again"
        );
        // Q12: kill the stack: the client's endpoint is closed, the registry says UNAVAILABLE and then answers, the address is
        // back with no new DISCOVER (the blob), and pings work again
        sup_restart(&f, 1);
        let t1 = sys::clock_get();
        let mut closed = false;
        while sys::clock_get() < t1 + 3000 {
            if let Err(e) = echo.ping([10, 0, 2, 2], 99, b"c", t(300)) {
                if e == rt::net::NetError::Closed {
                    closed = true;
                    break;
                }
            }
        }
        ensure!(
            closed,
            "the old endpoint's channel was closed when the stack died"
        );
        let mut net2 = f.connect(b"net")?;
        let i2 = net2.info(t(3000)).map_err(|_| msg_at(line!()))?;
        ensure!(
            i2.configured && i2.addr == [10, 0, 2, 15],
            "the address is back without waiting for DHCP"
        );
        let mut echo2 = net2.open_echo(t(3000)).map_err(|_| msg_at(line!()))?;
        let mut ok = false;
        let t2 = sys::clock_get();
        while sys::clock_get() < t2 + 8000 {
            if echo2.ping([10, 0, 2, 2], 7, b"d", t(700)).is_ok() {
                ok = true;
                break;
            }
        }
        ensure!(ok, "pings work again after the stack's restart");
        ensure!(
            sup_status(&f).is_some_and(|s| !s.1),
            "the supervisor is not down after three single restarts"
        );
        Ok(())
    })();
    f.close();
    r
}

/// Q14, Q15 and Q17 (the negative controls of NET-SPEC 12.4): each fault is survived as the contract says.
pub fn net_faults_are_survived() -> Outcome {
    // Q15: the driver publishes a slot out of range: the stack counts the violation, refuses the entry, detaches; the driver is
    // restarted (generation + 1) and the network works again
    let Some(mut f) = start_full_opts(false, Some(b"bad-slot"))? else {
        sys::debug_write(b"s8: no network device on this machine: net tests skipped\n");
        return Ok(());
    };
    let r = (|| -> Outcome {
        let t = |n: u64| sys::clock_get() + n;
        let mut cfg = f.connect(b"net-config")?;
        configure_static(&mut cfg)?;
        let mut net = f.connect(b"net")?;
        let mut echo = net.open_echo(t(3000)).map_err(|_| msg_at(line!()))?;
        // traffic: the first received frame makes the faulty driver publish the bad slot
        let mut ok = false;
        let start = sys::clock_get();
        let mut seq = 1u16;
        while sys::clock_get() < start + 12_000 {
            let _ = echo.ping([10, 0, 2, 2], seq, b"x", t(500));
            seq += 1;
            let (kids, down, gen) = sup_status(&f).ok_or_else(|| msg_at(line!()))?;
            ensure!(!down, "the supervisor stays up");
            if gen >= 2 && kids[0].1 >= 1 {
                ok = true;
                break;
            }
        }
        ensure!(ok, "the faulty driver was restarted (a new generation)");
        // (the old stack connection survived the driver's death: same endpoint, pings flow again)
        let mut again = false;
        for _ in 0..20 {
            if echo.ping([10, 0, 2, 2], 900, b"y", t(700)).is_ok() {
                again = true;
                break;
            }
        }
        ensure!(again, "pings flow again after the violation");
        let stats = cfg.stats(t(3000)).map_err(|_| msg_at(line!()))?;
        let violations = stats.last().map(|_| stats[stats.len() - 4].1).unwrap_or(0);
        ensure!(violations >= 1, "the stack counted the ring violation ({violations})");
        Ok(())
    })();
    f.close();
    r?;
    // Q17: the stack never signals the driver: traffic still flows, within the safety poll
    let Some(mut f) = start_full_opts(false, Some(b"no-doorbell"))? else { return Ok(()) };
    let r = (|| -> Outcome {
        let t = |n: u64| sys::clock_get() + n;
        let mut cfg = f.connect(b"net-config")?;
        configure_static(&mut cfg)?;
        let mut net = f.connect(b"net")?;
        let mut echo = net.open_echo(t(3000)).map_err(|_| msg_at(line!()))?;
        let mut got = 0;
        for seq in 1..=10u16 {
            if echo.ping([10, 0, 2, 2], seq, b"z", t(1500)).is_ok() {
                got += 1;
            }
        }
        ensure!(got >= 9, "traffic flows without any doorbell from the stack ({got} of 10): the safety poll covers it");
        Ok(())
    })();
    f.close();
    r?;
    // Q14: a stack that dies on a marked datagram, fed the marker until the supervisor gives up: the network is marked down, and
    // nothing keeps restarting
    let Some(mut f) = start_full_opts(false, Some(b"crash-on-magic"))? else { return Ok(()) };
    let r = (|| -> Outcome {
        let t = |n: u64| sys::clock_get() + n;
        let mut cfg = f.connect(b"net-config")?;
        configure_static(&mut cfg)?;
        drop(cfg);
        for round in 0..6 {
            let Ok(mut net) = f.connect(b"net") else { break };
            let Ok(mut udp) = net.open_udp(0, 0, t(3000)) else { break };
            let _ = udp.send_to([10, 0, 2, 2], 9, b"##crash please");
            nap(600)?;
            let (_, down, _) = sup_status(&f).ok_or_else(|| msg_at(line!()))?;
            if down {
                break;
            }
            ensure!(round < 5, "the supervisor never gave up");
        }
        let (kids, down, _) = sup_status(&f).ok_or_else(|| msg_at(line!()))?;
        ensure!(down, "the supervisor marked the network down after repeated crashes");
        ensure!(kids[1].1 <= 4, "a bounded number of restarts ({})", kids[1].1);
        Ok(())
    })();
    f.close();
    r
}
