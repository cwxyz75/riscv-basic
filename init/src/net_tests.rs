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
    ring: u64,
    rx: u64,
    tx: u64,
    to_stack: u64,
    to_driver: u64,
    ctl: u64,
    /// `netsup`, the device manager and the channel that keeps the manager's connection.
    sup: u64,
    mgr: u64,
    mgr_chan: u64,
    jobs: [u64; 2],
}

fn spawn_prog(
    bi: u64,
    name: &[u8],
    args: &[&[u8]],
    roles: Vec<(Role, u32)>,
    handles: &[u64],
    job: u64,
) -> Result<u64, Detail> {
    spawn_prog_env(bi, name, args, roles, Vec::new(), handles, job)
}

fn spawn_prog_env(
    bi: u64,
    name: &[u8],
    args: &[&[u8]],
    roles: Vec<(Role, u32)>,
    env: Vec<Vec<u8>>,
    handles: &[u64],
    job: u64,
) -> Result<u64, Detail> {
    let (off, len) = image_entry(bi, name)?;
    let image = rt::spawn::read_image(bi, off, len).ok_or_else(|| msg_at(line!()))?;
    let s = Startup {
        args: args.iter().map(|a| a.to_vec()).collect(),
        roles,
        env,
        ..Startup::default()
    };
    let bytes = s.encode().map_err(|_| msg_at(line!()))?;
    rt::spawn::spawn_in(&image, &bytes, handles, job).map_err(|e| {
        let mut d = Detail::new();
        let _ = core::fmt::Write::write_fmt(&mut d, format_args!("spawn: {e:?}"));
        d
    })
}

/// Start the device manager and `netsup` (stackless: the test is the stack). `Ok(None)`: no network device on this machine.
fn start_stack() -> Result<Option<Net>, Detail> {
    let res = ok(sys::resource_root(), "resource_root")?.a1 as u64;
    let bi = boot_image_vmo()?;
    let root = root_job()?;
    let mj = job(root, u64::MAX, u64::MAX, u64::MAX)?;
    let sj = job(root, u64::MAX, u64::MAX, u64::MAX)?;
    sys::handle_close(root);
    // the device manager, and a second connection for `netsup` (a claim lives as long as its connection)
    let (mine, theirs) = channel()?;
    let res_dup = ok(sys::handle_duplicate(res, Rights::SAME.0), "dup resource")?.a1 as u64;
    let bi_dup = ok(sys::handle_duplicate(bi, Rights::SAME.0), "dup image")?.a1 as u64;
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
    let bi2 = ok(sys::handle_duplicate(bi, Rights::SAME.0), "dup image")?.a1 as u64;
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
    let mut hs = [0u64; 8];
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

fn read_ctl(ch: u64) -> Option<(u32, msg::Msg)> {
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
    mgr: u64,
    mgr_chan: u64,
    sup: u64,
    /// The channel to `netsup` (Status, Restart).
    ctl: u64,
    reg: u64,
    /// The random service (`rngd`).
    rng: u64,
    /// The registry's root connection (it can publish, view).
    root: Fs,
    jobs: [u64; 3],
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
    let res = ok(sys::resource_root(), "resource_root")?.a1 as u64;
    let bi = boot_image_vmo()?;
    let root_j = root_job()?;
    let mj = job(root_j, u64::MAX, u64::MAX, u64::MAX)?;
    let sj = job(root_j, u64::MAX, u64::MAX, u64::MAX)?;
    let rj = job(root_j, u64::MAX, u64::MAX, u64::MAX)?;
    sys::handle_close(root_j);
    let (mine, theirs) = channel()?;
    let res_dup = ok(sys::handle_duplicate(res, Rights::SAME.0), "dup resource")?.a1 as u64;
    let bi_dup = ok(sys::handle_duplicate(bi, Rights::SAME.0), "dup image")?.a1 as u64;
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
    // the random service: a second client of the device manager, and a registry connection that can publish `random`
    let (rng_dm, mgr_end2) = channel()?;
    let req2 = proto::devmgr::encode_connect(2);
    ensure_eq!(
        st(sys::channel_write(
            mine,
            req2.as_ptr(),
            req2.len(),
            &mgr_end2,
            1
        )),
        Some(S::Ok),
        "connect a third client"
    );
    ensure_eq!(
        st(sys::object_wait(
            mine,
            sig::READABLE,
            sys::clock_get() + 5000
        )),
        Some(S::Ok),
        "the manager answers the second connect"
    );
    sys::channel_read(mine, rb.as_mut_ptr(), 16, core::ptr::null_mut(), 0);
    let rng_pub = root.clone_conn(prt::ALL).map_err(|e| msg_e(e, line!()))?;
    let rng = spawn_prog(
        bi,
        b"bin/rngd",
        &[b"rngd"],
        vec![(Role::ServiceDevmgr, 0), (Role::ServiceRegistry, 1)],
        &[rng_dm, rng_pub.ch],
        sj,
    )?;
    let (ctl, ctl_theirs) = channel()?;
    let bi2 = ok(sys::handle_duplicate(bi, Rights::SAME.0), "dup image")?.a1 as u64;
    let fault_env: Vec<Vec<u8>> = fault
        .map(|f| vec![[&b"FAULT="[..], f].concat()])
        .unwrap_or_default();
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
        sys::process_kill(rng, -1);
        for h in [sup, mgr, mine, ctl, reg, rng, mj, sj, rj, reg_mine] {
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
        rng,
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
        for p in [self.sup, self.mgr, self.reg, self.rng] {
            sys::process_kill(p, -1);
            let _ = sys::object_wait(p, sig::TERMINATED, sys::clock_get() + 3000);
        }
        for h in [
            self.sup,
            self.mgr,
            self.mgr_chan,
            self.ctl,
            self.reg,
            self.rng,
        ] {
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
        // S8 follow-up item 1 (kinds 3 and 4 are streams since S9): a request with two handles is BadRequest, an
        // unknown kind is BadRequest, and none of them leaves a handle or a connection behind
        let open = |net: &mut rt::net::Net, kind: u16, n_handles: usize| -> Result<i32, Detail> {
            let mut mine = Vec::new();
            let mut theirs = Vec::new();
            for _ in 0..n_handles {
                let c = sys::channel_create();
                ensure_eq!(c.status, 0, "channel_create");
                mine.push(c.a1 as u64);
                theirs.push(c.a2 as u64);
            }
            let req = proto::net::Request::Open {
                kind,
                local_port: 0,
                local_addr: [0; 4],
                oflags: 0,
            };
            let r = net.call_with(&req, &theirs, t(3000));
            // the stack closed what it was sent: our ends see the peer gone
            for &m in &mine {
                let w = sys::object_wait(m, sig::PEER_CLOSED, t(3000));
                ensure_eq!(w.status, 0, "the stack closed the handle it was sent");
                sys::handle_close(m);
            }
            match r {
                Ok((s, _)) => Ok(s),
                Err(_) => Err(msg_at(line!())),
            }
        };
        let bad = pn_status(1);
        for kind in [0u16, 5, 99] {
            ensure_eq!(open(&mut net, kind, 1)?, bad, "unknown kind {kind}");
        }
        ensure_eq!(open(&mut net, 1, 2)?, bad, "UDP with two handles");
        ensure_eq!(
            open(&mut net, 3, 2)?,
            bad,
            "a stream endpoint with two handles"
        );
        ensure_eq!(open(&mut net, 4, 2)?, bad, "a listener with two handles");
        ensure_eq!(open(&mut net, 4, 1)?, bad, "a listener without a port");
        ensure_eq!(open(&mut net, 1, 0)?, bad, "UDP with no handle");
        let mut once = false;
        for _ in 0..50 {
            if net.open_udp(0, 0, t(3000)).is_ok() {
                once = true;
                break;
            }
            nap(20)?;
        }
        ensure!(once, "the refusals left the account untouched");
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
        // review of S8 (finding 1): after the stack restarted, a later driver restart must still reach the new stack
        sup_restart(&f, 0);
        let t3 = sys::clock_get();
        let mut ok = false;
        while sys::clock_get() < t3 + 8000 {
            if echo2.ping([10, 0, 2, 2], 8, b"e", t(700)).is_ok() {
                ok = true;
                break;
            }
        }
        ensure!(
            ok,
            "pings work after a driver restart that follows a stack restart"
        );
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
        ensure!(
            violations >= 1,
            "the stack counted the ring violation ({violations})"
        );
        Ok(())
    })();
    f.close();
    r?;
    // Q17: the stack never signals the driver: traffic still flows, within the safety poll
    let Some(mut f) = start_full_opts(false, Some(b"no-doorbell"))? else {
        return Ok(());
    };
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
    let Some(mut f) = start_full_opts(false, Some(b"crash-on-magic"))? else {
        return Ok(());
    };
    let r = (|| -> Outcome {
        let t = |n: u64| sys::clock_get() + n;
        let mut cfg = f.connect(b"net-config")?;
        configure_static(&mut cfg)?;
        drop(cfg);
        for round in 0..6 {
            let Ok(mut net) = f.connect(b"net") else {
                break;
            };
            let Ok(mut udp) = net.open_udp(0, 0, t(3000)) else {
                break;
            };
            let _ = udp.send_to([10, 0, 2, 2], 9, b"##crash please");
            nap(600)?;
            let (_, down, _) = sup_status(&f).ok_or_else(|| msg_at(line!()))?;
            if down {
                break;
            }
            ensure!(round < 5, "the supervisor never gave up");
        }
        let (kids, down, _) = sup_status(&f).ok_or_else(|| msg_at(line!()))?;
        ensure!(
            down,
            "the supervisor marked the network down after repeated crashes"
        );
        ensure!(
            kids[1].1 <= 4,
            "a bounded number of restarts ({})",
            kids[1].1
        );
        Ok(())
    })();
    f.close();
    r
}

/// The network benchmark (NET-SPEC 12.3, tier 2 data): the round trip of a ping to the gateway, median and 99th percentile of 200
/// (`##BENCH` lines; measurements, not verdicts). The first numbers are the load the scheduler tasks 11 and 12 will be judged on.
pub fn net_ping_round_trip_is_measured() -> Outcome {
    let Some(mut f) = start_full()? else {
        sys::debug_write(b"s8: no network device on this machine: net tests skipped\n");
        return Ok(());
    };
    let r = (|| -> Outcome {
        let t = |n: u64| sys::clock_get() + n;
        let mut cfg = f.connect(b"net-config")?;
        configure_static(&mut cfg)?;
        let mut net = f.connect(b"net")?;
        let mut echo = net.open_echo(t(3000)).map_err(|_| msg_at(line!()))?;
        // warm up (ARP, link)
        for seq in 0..5u16 {
            let _ = echo.ping([10, 0, 2, 2], seq, b"w", t(1500));
        }
        let mut rtts: Vec<u64> = Vec::new();
        for seq in 0..200u16 {
            let c0 = sys::debug_stat(6);
            if echo
                .ping([10, 0, 2, 2], 100 + seq, &[7u8; 32], t(1500))
                .is_ok()
            {
                rtts.push(counter_ns(sys::debug_stat(6) - c0));
            }
        }
        ensure!(
            rtts.len() >= 150,
            "most pings were answered ({} of 200)",
            rtts.len()
        );
        rtts.sort_unstable();
        let (p50, p99) = (rtts[rtts.len() / 2], rtts[rtts.len() * 99 / 100]);
        println!("##BENCH net_ping_rtt_p50 {p50} ns");
        println!("##BENCH net_ping_rtt_p99 {p99} ns");
        Ok(())
    })();
    f.close();
    r
}

// ---------------------------------------------------------------------------------------------------- S9 streams

use rt::net::NetError;
use rt::net::NetError::Stack;

fn pattern(seed: u32, i: usize) -> u8 {
    (i as u32)
        .wrapping_mul(2654435761)
        .wrapping_add(seed)
        .wrapping_shr(24) as u8
}

/// Read exactly `buf.len()` bytes from `s` (until `deadline`).
fn read_exact(s: &mut rt::net::Stream, buf: &mut [u8], deadline: u64) -> Result<(), NetError> {
    let mut got = 0;
    while got < buf.len() {
        let n = s.read(&mut buf[got..], deadline)?;
        if n == 0 {
            return Err(NetError::Closed);
        }
        got += n;
    }
    Ok(())
}

/// A connected loopback pair on `port`: (client, server, the listener that must stay open).
fn pair(
    net: &mut rt::net::Net,
    port: u16,
) -> Result<(rt::net::Stream, rt::net::Stream, rt::net::Listener), Detail> {
    let t = |n: u64| sys::clock_get() + n;
    let mut l = net
        .listen_stream(port, 4, 0, t(3000))
        .map_err(|_| msg_at(line!()))?;
    let c = net
        .connect_stream([10, 0, 2, 15], port, 0, 0, 0, t(5000))
        .map_err(|e| {
            let mut d = Detail::new();
            let _ = core::fmt::Write::write_fmt(&mut d, format_args!("connect: {e:?}"));
            d
        })?;
    let s = l.accept(t(5000)).map_err(|_| msg_at(line!()))?;
    Ok((c, s, l))
}

/// Streams over loopback: echo in lockstep, a megabyte one way with both ends pumped from this thread, half-close, abort,
/// refusal, the end-of-file rule, bad handles, and the quotas.
pub fn net_streams_over_loopback() -> Outcome {
    let Some(mut f) = start_full()? else {
        sys::debug_write(b"s9: no network device on this machine: net tests skipped\n");
        return Ok(());
    };
    let r = (|| -> Outcome {
        let t = |n: u64| sys::clock_get() + n;
        let mut cfg = f.connect(b"net-config")?;
        configure_static(&mut cfg)?;
        let mut net = f.connect(b"net")?;
        // ---- lockstep echo: 64 blocks of 4 KiB through the stack and back
        let (mut c, mut s, _l) = pair(&mut net, 7000)?;
        ensure_eq!(
            c.remote(),
            Some(([10, 0, 2, 15], 7000)),
            "the client's view of the connection"
        );
        ensure_eq!(s.local(), Some(([10, 0, 2, 15], 7000)), "the server's view");
        let mut block = [0u8; 4096];
        let mut back = [0u8; 4096];
        let echo0 = sys::debug_stat(6);
        for i in 0..64usize {
            for (k, b) in block.iter_mut().enumerate() {
                *b = pattern(i as u32, k);
            }
            c.write_all(&block, t(5000)).map_err(|_| msg_at(line!()))?;
            read_exact(&mut s, &mut back, t(5000)).map_err(|_| msg_at(line!()))?;
            ensure!(back == block, "block {i} arrived intact at the server");
            s.write_all(&back, t(5000)).map_err(|_| msg_at(line!()))?;
            read_exact(&mut c, &mut back, t(5000)).map_err(|_| msg_at(line!()))?;
            ensure!(back == block, "block {i} came back intact");
        }
        // (14.3: the lockstep round trip of 4 KiB through the stack and back, per round trip)
        println!(
            "##BENCH tcp_echo_rt_4k {} ns",
            counter_ns(sys::debug_stat(6) - echo0) / 64
        );
        // ---- a megabyte one way, both ends pumped from here with immediate deadlines
        let total = 1024 * 1024usize;
        let (mut sent, mut got) = (0usize, 0usize);
        let mut chunk = [0u8; 8192];
        let mut rbuf = [0u8; 8192];
        let until = sys::clock_get() + 60_000;
        let mb0 = sys::debug_stat(6);
        while got < total {
            ensure!(
                sys::clock_get() < until,
                "the megabyte moved in a minute ({sent} sent, {got} received)"
            );
            if sent < total {
                let n = (total - sent).min(chunk.len());
                for (k, b) in chunk[..n].iter_mut().enumerate() {
                    *b = pattern(9, sent + k);
                }
                if let Ok(w) = c.write(&chunk[..n], sys::clock_get()) {
                    sent += w;
                    // (a partial write: the unwritten tail is rewritten next time from the same offset)
                }
            }
            if let Ok(n) = s.read(&mut rbuf, sys::clock_get()) {
                for (k, &b) in rbuf[..n].iter().enumerate() {
                    if b != pattern(9, got + k) {
                        return Err(msg_at(line!()));
                    }
                }
                got += n;
            } else {
                nap(1)?;
            }
        }
        // (14.3: the whole megabyte through loopback, the stack and the IPC with no device)
        println!(
            "##BENCH tcp_loopback_1mib {} ns",
            counter_ns(sys::debug_stat(6) - mb0)
        );
        // ---- half-close both ways: end-of-file is clean only because PeerFin came first
        c.shutdown_write();
        let mut one = [0u8; 8];
        let r = s.read(&mut one, t(5000));
        if !matches!(r, Ok(0)) {
            let mut d = Detail::new();
            let _ = core::fmt::Write::write_fmt(
                &mut d,
                format_args!("server read after the client's shutdown: {r:?}"),
            );
            return Err(d);
        }
        s.write_all(b"still open", t(3000))
            .map_err(|_| msg_at(line!()))?;
        let mut ten = [0u8; 10];
        read_exact(&mut c, &mut ten, t(3000)).map_err(|_| msg_at(line!()))?;
        ensure_eq!(
            &ten,
            b"still open",
            "a half-closed connection still carries the other direction"
        );
        s.shutdown_write();
        ensure_eq!(
            c.read(&mut one, t(5000)).map_err(|_| msg_at(line!()))?,
            0,
            "and the client reads the server's"
        );
        drop((c, s));
        // ---- abort: the peer sees a reset, never a clean end
        let (mut c, mut s, _l2) = pair(&mut net, 7001)?;
        c.abort();
        ensure_eq!(
            s.read(&mut one, t(5000)).err(),
            Some(Stack(proto::net::status::CONN_RESET)),
            "an abort is a reset at the peer"
        );
        let _ = &mut c;
        drop((c, s));
        // ---- write and leave: the stack keeps what was queued and finishes it as an orphan (review 2026-11-15, finding 3)
        let (mut c, mut s, _l3) = pair(&mut net, 7002)?;
        let blob: Vec<u8> = (0..60_000usize).map(|i| pattern(5, i)).collect();
        c.write_all(&blob, t(5000)).map_err(|_| msg_at(line!()))?;
        drop(c);
        let mut got = Vec::new();
        let mut piece = [0u8; 4096];
        loop {
            match s.read(&mut piece, t(5000)) {
                Ok(0) => break,
                Ok(n) => got.extend_from_slice(&piece[..n]),
                Err(_) => return Err(msg_at(line!())),
            }
        }
        ensure!(
            got == blob,
            "all 60,000 bytes arrived, and then a clean end of stream ({} bytes)",
            got.len()
        );
        drop(s);
        // ---- refused: nothing listens
        ensure_eq!(
            net.connect_stream([10, 0, 2, 15], 7999, 0, 0, 0, t(5000))
                .err(),
            Some(Stack(proto::net::status::CONN_REFUSED)),
            "connect to a closed port"
        );
        // ---- an unconfigured destination and a port in use
        ensure_eq!(
            net.listen_stream(7000, 1, 0, t(3000)).err(),
            Some(Stack(pn_status(3))),
            "a second listener on the port"
        );
        ensure_eq!(
            net.listen_stream(80, 1, 0, t(3000)).err(),
            Some(Stack(pn_status(2))),
            "a low port needs /svc/net-priv"
        );
        Ok(())
    })();
    f.close();
    r
}

/// The quotas of the stream path (S9-SPEC 6): `stream_bytes` per account, the connection limit, and everything coming back.
pub fn net_stream_quotas_and_bad_handles() -> Outcome {
    let Some(mut f) = start_full()? else {
        sys::debug_write(b"s9: no network device on this machine: net tests skipped\n");
        return Ok(());
    };
    let r = (|| -> Outcome {
        let t = |n: u64| sys::clock_get() + n;
        let mut cfg = f.connect(b"net-config")?;
        configure_static(&mut cfg)?;
        let mut server = f.connect(b"net")?;
        let mut client = f.connect(b"net")?;
        let mut l = server
            .listen_stream(7100, 16, 0, t(3000))
            .map_err(|_| msg_at(line!()))?;
        // sixteen default streams (32 KiB each) fill the client's 512 KiB; the seventeenth is Quota
        let mut held = Vec::new();
        for i in 0..16 {
            match client.connect_stream([10, 0, 2, 15], 7100, 0, 0, 0, t(5000)) {
                Ok(s) => held.push(s),
                Err(e) => {
                    let mut d = Detail::new();
                    let _ = core::fmt::Write::write_fmt(&mut d, format_args!("stream {i}: {e:?}"));
                    return Err(d);
                }
            }
        }
        ensure_eq!(
            client
                .connect_stream([10, 0, 2, 15], 7100, 0, 0, 0, t(5000))
                .err(),
            Some(Stack(pn_status(6))),
            "the seventeenth default stream is over the account's stream_bytes"
        );
        // another client is not affected: it gets the posted accepts that are left over (none: the server's are used up), so
        // its connect is dropped at the listener and ends by refusal or timeout rather than Quota
        let mut other = f.connect(b"net")?;
        let e = other
            .connect_stream([10, 0, 2, 15], 7100, 0, 0, 0, t(2500))
            .err();
        ensure!(
            e != Some(Stack(pn_status(6))),
            "a second client has its own account: {e:?}"
        );
        // the accepted side: all sixteen connections arrived at the listener
        let mut accepted = Vec::new();
        for _ in 0..16 {
            accepted.push(l.accept(t(5000)).map_err(|_| msg_at(line!()))?);
        }
        // everything comes back: close it all, and a fresh connect works again
        drop(held);
        drop(accepted);
        let mut again = None;
        for _ in 0..100 {
            match client.connect_stream([10, 0, 2, 15], 7100, 0, 0, 0, t(5000)) {
                Ok(s) => {
                    again = Some(s);
                    break;
                }
                Err(Stack(6)) => nap(30)?,
                Err(_) => nap(30)?,
            }
        }
        ensure!(
            again.is_some(),
            "the reservations came back when the streams were closed"
        );
        drop(again);
        // ---- accepts reserve at the SYN, not at the post: six big accepts post fine, and only as many as the account can pay for
        // are taken (4 of about 128 KiB in 512 KiB); the fifth connect gets no answer until a stream is gone
        drop(l); // (its posted accepts and the connection `again` took go with it)
        nap(1500)?; // (the orphans of the streams above finish and give their reservations back)
        let mut big = server
            .listen_stream(7101, 0, 0, t(3000))
            .map_err(|_| msg_at(line!()))?;
        for _ in 0..6 {
            big.post(65536, 65535).map_err(|_| msg_at(line!()))?;
        }
        let mut cs = Vec::new();
        for i in 0..4 {
            match client.connect_stream([10, 0, 2, 15], 7101, 0, 4096, 4096, t(5000)) {
                Ok(c) => cs.push(c),
                Err(e) => {
                    let mut d = Detail::new();
                    let _ =
                        core::fmt::Write::write_fmt(&mut d, format_args!("big accept {i}: {e:?}"));
                    return Err(d);
                }
            }
        }
        let mut ss = Vec::new();
        for _ in 0..4 {
            ss.push(big.accept(t(5000)).map_err(|_| msg_at(line!()))?);
        }
        ensure!(
            client
                .connect_stream([10, 0, 2, 15], 7101, 0, 4096, 4096, t(2500))
                .is_err(),
            "a fifth big accept does not fit the server's account: its SYN is dropped"
        );
        drop(ss.pop());
        drop(cs.pop());
        let mut fourth = None;
        for _ in 0..4 {
            if let Ok(c) = client.connect_stream([10, 0, 2, 15], 7101, 0, 4096, 4096, t(9000)) {
                fourth = Some(c);
                break;
            }
        }
        ensure!(
            fourth.is_some(),
            "once a stream is gone the next SYN is taken"
        );
        drop((fourth, cs, ss, big));
        // ---- bad handles: a channel where a socket belongs, one socket used twice, both ends of one pair
        let _lb = server
            .listen_stream(7102, 2, 0, t(3000))
            .map_err(|_| msg_at(line!()))?;
        let open_raw = |net: &mut rt::net::Net| -> Result<u64, Detail> {
            let c = sys::channel_create();
            ensure_eq!(c.status, 0, "an endpoint channel");
            let (mine, theirs) = (c.a1 as u64, c.a2 as u64);
            let req = proto::net::Request::Open {
                kind: 3,
                local_port: 0,
                local_addr: [0; 4],
                oflags: 0,
            };
            let (st, _) = net
                .call_with(&req, &[theirs], t(3000))
                .map_err(|_| msg_at(line!()))?;
            ensure_eq!(st, 0, "a stream endpoint opened");
            Ok(mine)
        };
        // sends Connect with `h` and returns what the stack answered first: Ok(true) Connected, Ok(false) failed with `status`
        let try_connect = |mine: u64, h: u64| -> Result<Result<(), i32>, Detail> {
            let msg = proto::net::encode_connect(1, [10, 0, 2, 15], 7102, 0, 0);
            let w = sys::channel_write(mine, msg.as_ptr(), msg.len(), [h].as_ptr(), 1);
            ensure_eq!(w.status, 0, "the connect request was sent");
            let wait = sys::object_wait(mine, sig::READABLE | sig::PEER_CLOSED, t(10_000));
            ensure_eq!(wait.status, 0, "an answer");
            let mut b = [0u8; 64];
            let r = sys::channel_read(mine, b.as_mut_ptr(), 64, core::ptr::null_mut(), 0);
            ensure_eq!(r.status, 0, "the notice");
            match proto::net::decode_ep_event(&b[..r.a1]) {
                Some(proto::net::EpEvent::ConnInfo { code, .. })
                    if code == proto::net::notice::CONNECTED =>
                {
                    Ok(Ok(()))
                }
                Some(proto::net::EpEvent::Notice { code, detail, .. })
                    if code == proto::net::notice::CONNECT_FAILED =>
                {
                    Ok(Err(detail as i32))
                }
                other => {
                    let mut d = Detail::new();
                    let _ =
                        core::fmt::Write::write_fmt(&mut d, format_args!("unexpected {other:?}"));
                    Err(d)
                }
            }
        };
        let bad = proto::net::status::BAD_HANDLE;
        // a channel
        let chan = sys::channel_create();
        ensure_eq!(chan.status, 0, "a channel to pass by mistake");
        let e1 = open_raw(&mut client)?;
        ensure_eq!(
            try_connect(e1, chan.a2 as u64)?,
            Err(bad),
            "a channel instead of a socket"
        );
        // one socket used twice
        let pr = sys::socket_create();
        ensure_eq!(pr.status, 0, "a socket pair");
        let (pa, pb) = (pr.a1 as u64, pr.a2 as u64);
        let dup = sys::handle_duplicate(pa, 1 << 31);
        ensure_eq!(dup.status, 0, "a duplicate of the same socket");
        let e2 = open_raw(&mut client)?;
        ensure_eq!(
            try_connect(e2, pa)?,
            Ok(()),
            "the first use of a socket is fine"
        );
        let e3 = open_raw(&mut client)?;
        ensure_eq!(
            try_connect(e3, dup.a1 as u64)?,
            Err(bad),
            "the same socket again"
        );
        // the peer of a socket the stack holds
        let e4 = open_raw(&mut client)?;
        ensure_eq!(try_connect(e4, pb)?, Err(bad), "both ends of one pair");
        for h in [e1, e2, e3, e4] {
            sys::handle_close(h);
        }
        Ok(())
    })();
    f.close();
    r
}

/// Kill the stack under live streams: each client sees a reset, never end-of-file; the stack comes back and streams work again.
pub fn net_streams_survive_a_stack_restart() -> Outcome {
    let Some(mut f) = start_full()? else {
        sys::debug_write(b"s9: no network device on this machine: net tests skipped\n");
        return Ok(());
    };
    let r = (|| -> Outcome {
        let t = |n: u64| sys::clock_get() + n;
        let mut cfg = f.connect(b"net-config")?;
        configure_static(&mut cfg)?;
        let mut net = f.connect(b"net")?;
        let (mut c, mut s, _l) = pair(&mut net, 7200)?;
        c.write_all(b"before", t(3000))
            .map_err(|_| msg_at(line!()))?;
        let mut six = [0u8; 6];
        read_exact(&mut s, &mut six, t(3000)).map_err(|_| msg_at(line!()))?;
        sup_restart(&f, 1);
        let mut one = [0u8; 8];
        ensure_eq!(
            s.read(&mut one, t(5000)).err(),
            Some(Stack(proto::net::status::CONN_RESET)),
            "the server end sees a reset when the stack dies, not end-of-file"
        );
        ensure_eq!(
            c.read(&mut one, t(5000)).err(),
            Some(Stack(proto::net::status::CONN_RESET)),
            "and so does the client end"
        );
        drop((c, s));
        // the stack is back: a new connection works (the address came back from the blob)
        let mut net2 = f.connect(b"net")?;
        let (mut c, mut s, _l2) = pair(&mut net2, 7201)?;
        c.write_all(b"after", t(3000))
            .map_err(|_| msg_at(line!()))?;
        let mut five = [0u8; 5];
        read_exact(&mut s, &mut five, t(3000)).map_err(|_| msg_at(line!()))?;
        ensure_eq!(&five, b"after", "streams work after the restart");
        Ok(())
    })();
    f.close();
    r
}

/// S9 step 4c: the host's own TCP (through `hostfwd=tcp`) as the independent client of a guest listener. The guest echoes until
/// the host's end of stream, then ends its own side; the harness checks 200,000 bytes came back byte for byte and cleanly.
pub fn net_tcp_echo_for_the_host() -> Outcome {
    let Some(mut f) = start_full()? else {
        sys::debug_write(b"s9: no network device on this machine: net tests skipped\n");
        return Ok(());
    };
    let r = (|| -> Outcome {
        let t = |n: u64| sys::clock_get() + n;
        let mut cfg = f.connect(b"net-config")?;
        configure_static(&mut cfg)?;
        let mut net = f.connect(b"net")?;
        let mut l = net
            .listen_stream(7778, 2, 0, t(3000))
            .map_err(|_| msg_at(line!()))?;
        sys::debug_write(b"s9: tcpecho ready\n");
        let Ok(mut s) = l.accept(t(40_000)) else {
            sys::debug_write(
                b"s9: no host TCP client came (the harness did not start one): skipped\n",
            );
            return Ok(());
        };
        let mut buf = [0u8; 8192];
        let mut total = 0usize;
        loop {
            match s.read(&mut buf, t(30_000)) {
                Ok(0) => break,
                Ok(n) => {
                    total += n;
                    s.write_all(&buf[..n], t(30_000))
                        .map_err(|_| msg_at(line!()))?;
                }
                Err(_) => return Err(msg_at(line!())),
            }
        }
        s.shutdown_write();
        ensure_eq!(total, 200_000, "the host's 200,000 bytes were echoed");
        nap(drain_ms(300))?;
        Ok(())
    })();
    f.close();
    r
}

/// How long a test lets the last bytes and the close drain after its final write before it stops the stack: a short time on the
/// clean link, long enough for a few retransmission timeouts when the harness runs the guest behind the lossy relay (Q30).
fn drain_ms(clean: u64) -> u64 {
    if sys::debug_stat(9) != 0 {
        12_000
    } else {
        clean
    }
}

/// The other direction: the guest connects to a TCP server on the host (10.0.2.2 through slirp), reads a greeting and 300,000
/// bytes, sends 100,000 bytes back and shuts its side down. The harness checks what the server received.
pub fn net_tcp_to_the_host_server() -> Outcome {
    let Some(mut f) = start_full()? else {
        sys::debug_write(b"s9: no network device on this machine: net tests skipped\n");
        return Ok(());
    };
    let r = (|| -> Outcome {
        let t = |n: u64| sys::clock_get() + n;
        let mut cfg = f.connect(b"net-config")?;
        configure_static(&mut cfg)?;
        let mut net = f.connect(b"net")?;
        let mut s = match net.connect_stream([10, 0, 2, 2], 47802, 0, 0, 0, t(8000)) {
            Ok(s) => s,
            Err(Stack(10)) | Err(NetError::TimedOut) => {
                sys::debug_write(
                    b"s9: no host TCP server on 47802 (the harness did not start one): skipped\n",
                );
                return Ok(());
            }
            Err(_) => return Err(msg_at(line!())),
        };
        let mut greeting = [0u8; 16];
        read_exact(&mut s, &mut greeting, t(10_000)).map_err(|_| msg_at(line!()))?;
        ensure_eq!(&greeting, b"HELLO-FROM-HOST\n", "the server's greeting");
        let mut got = 0usize;
        let mut buf = [0u8; 8192];
        while got < 300_000 {
            let n = s.read(&mut buf, t(30_000)).map_err(|_| msg_at(line!()))?;
            ensure!(n > 0, "the stream did not end early");
            for (k, &b) in buf[..n].iter().enumerate() {
                if b != pattern(3, got + k) {
                    return Err(msg_at(line!()));
                }
            }
            got += n;
        }
        ensure_eq!(
            s.read(&mut buf, t(30_000)).map_err(|_| msg_at(line!()))?,
            0,
            "a clean end of stream after the 300,000 bytes"
        );
        let out: Vec<u8> = (0..100_000usize).map(|i| pattern(4, i)).collect();
        if let Err(e) = s.write_all(&out, t(30_000)) {
            let mut d = Detail::new();
            let _ = core::fmt::Write::write_fmt(
                &mut d,
                format_args!("writing to the host server: {e:?}"),
            );
            return Err(d);
        }
        s.shutdown_write();
        nap(drain_ms(500))?;
        sys::debug_write(b"s9: host-tcp-server done\n");
        Ok(())
    })();
    f.close();
    r
}

/// A connect that waits out Quota: a connection the client just closed keeps its reservation until its orphan has drained (up to
/// 20 s, N22), so a churning client sees Quota for a moment. Anything else is an error.
fn connect_patiently(net: &mut rt::net::Net, port: u16) -> Result<rt::net::Stream, Detail> {
    for _ in 0..400 {
        match net.connect_stream([10, 0, 2, 15], port, 0, 4096, 4096, sys::clock_get() + 8000) {
            Ok(s) => return Ok(s),
            Err(Stack(6)) => nap(50)?,
            Err(e) => {
                let mut d = Detail::new();
                let _ = core::fmt::Write::write_fmt(&mut d, format_args!("connect: {e:?}"));
                return Err(d);
            }
        }
    }
    Err(msg_at(line!()))
}

/// Churn (Q32, tier 1 in a short form, the full 2,000 connections with `bench=full`): connections opened and closed in every way,
/// in sequence over one listener and then 32 at a time over two, with nothing left behind. The leak check is by capacity: after the
/// churn the client account takes exactly its full complement of streams again (the account arithmetic is exact, so a leaked
/// reservation anywhere shows as a Quota too early), and a closed connection's port can be listened on afresh.
pub fn net_tcp_churn() -> Outcome {
    let Some(mut f) = start_full()? else {
        sys::debug_write(b"s9: no network device on this machine: net tests skipped\n");
        return Ok(());
    };
    let r = (|| -> Outcome {
        let t = |n: u64| sys::clock_get() + n;
        let full = sys::debug_stat(7) != 0;
        let mut cfg = f.connect(b"net-config")?;
        configure_static(&mut cfg)?;
        let mut sa = f.connect(b"net")?;
        let mut sb = f.connect(b"net")?;
        let mut sc = f.connect(b"net")?;
        let mut sd = f.connect(b"net")?;
        let mut client = f.connect(b"net")?;
        // (an account holds at most 32 connections, its service connection among them: 32 streams at once need two clients)
        let mut client2 = f.connect(b"net")?;
        let t0 = sys::clock_get();
        // ---- in sequence: one listener, N connections, four ways to end
        let n_seq = if full { 2000 } else { 100 };
        let mut l = sa
            .listen_stream(7200, 4, 0, t(3000))
            .map_err(|_| msg_at(line!()))?;
        for i in 0..n_seq {
            let mut c = connect_patiently(&mut client, 7200)?;
            let mut s = l.accept(t(8000)).map_err(|_| msg_at(line!()))?;
            let msg = [pattern(11, i), pattern(12, i), 7, 8];
            c.write_all(&msg, t(5000)).map_err(|_| msg_at(line!()))?;
            let mut got = [0u8; 4];
            read_exact(&mut s, &mut got, t(5000)).map_err(|_| msg_at(line!()))?;
            ensure_eq!(got, msg, "connection {i}: the bytes arrived");
            match i % 4 {
                0 => drop((c, s)),
                1 => {
                    drop(s);
                    drop(c);
                }
                2 => {
                    c.abort();
                    drop((c, s));
                }
                _ => {
                    s.write_all(&got, t(5000)).map_err(|_| msg_at(line!()))?;
                    drop(s);
                    let mut back = [0u8; 4];
                    read_exact(&mut c, &mut back, t(5000)).map_err(|_| msg_at(line!()))?;
                    drop(c);
                }
            }
        }
        drop(l);
        // ---- 32 at a time over four listeners (eight posted accepts each), rounds of connect, exchange and a mixed close
        let rounds = if full { 40 } else { 3 };
        // (four server accounts with eight posted accepts each: an accepted stream holds 32 KiB, so a account with sixteen of
        // them has no room for the next round's SYNs until its streams have drained)
        let mut ls = Vec::new();
        for (i, srv) in [&mut sa, &mut sb, &mut sc, &mut sd].into_iter().enumerate() {
            ls.push(
                srv.listen_stream(7201 + i as u16, 8, 0, t(3000))
                    .map_err(|_| msg_at(line!()))?,
            );
        }
        for round in 0..rounds {
            let mut clients = Vec::new();
            for k in 0..32usize {
                let port = 7201 + (k % 4) as u16;
                let who = if k < 16 { &mut client } else { &mut client2 };
                let c = connect_patiently(who, port)?;
                clients.push(c);
            }
            let mut servers = Vec::new();
            for k in 0..32usize {
                servers.push(ls[k % 4].accept(t(8000)).map_err(|_| msg_at(line!()))?);
            }
            // (accepts come in arrival order per listener, which is the order of the connects)
            for (k, c) in clients.iter_mut().enumerate() {
                c.write_all(&[k as u8; 100], t(5000))
                    .map_err(|_| msg_at(line!()))?;
            }
            let mut buf = [0u8; 100];
            for (k, s) in servers.iter_mut().enumerate() {
                read_exact(s, &mut buf, t(5000)).map_err(|_| msg_at(line!()))?;
                ensure!(
                    buf.iter().all(|&b| b == k as u8),
                    "round {round} stream {k}: the right bytes on the right connection"
                );
            }
            for (k, (c, s)) in clients.into_iter().zip(servers).enumerate() {
                match k % 4 {
                    0 => drop(c),
                    1 => drop(s),
                    2 => {
                        let mut c = c;
                        c.abort();
                    }
                    _ => {}
                }
            }
        }
        drop(ls);
        // ---- nothing left behind: the account takes streams again (eight, the listener's backlog), once the
        // orphans and the closing connections have gone (a bounded time: 20 s for an orphan, TIME-WAIT is not charged)
        let mut l = sa
            .listen_stream(7210, 8, 0, t(3000))
            .map_err(|_| msg_at(line!()))?;
        let mut settled = false;
        for _ in 0..60 {
            let mut held = Vec::new();
            let mut ok = true;
            for _ in 0..8 {
                match client.connect_stream([10, 0, 2, 15], 7210, 0, 4096, 4096, t(5000)) {
                    Ok(s) => held.push(s),
                    Err(_) => {
                        ok = false;
                        break;
                    }
                }
            }
            drop(held);
            for _ in 0..8 {
                let _ = l.accept(t(200));
            }
            if ok {
                settled = true;
                break;
            }
            nap(500)?;
        }
        ensure!(settled, "the account's streams came back after the churn");
        let secs = (sys::clock_get() - t0) / 1000;
        let msg = alloc::format!(
            "s9: churn done ({n_seq} in sequence, {rounds} rounds of 32) in {secs} s\n"
        );
        sys::debug_write(msg.as_bytes());
        Ok(())
    })();
    f.close();
    r
}

/// The random service (S9 step 5): bytes from a generator seeded by the entropy device, never the same twice, with the
/// balance of a good source; and refusal rather than invention when there is no device.
pub fn random_service_gives_seeded_distinct_bytes() -> Outcome {
    let Some(mut f) = start_full()? else {
        sys::debug_write(b"s9: no network device on this machine: net tests skipped\n");
        return Ok(());
    };
    let r = (|| -> Outcome {
        let t = |n: u64| sys::clock_get() + n;
        let mut v = f.view(b"random")?;
        let mut r = rt::random::Random::connect(&mut v, t(8000)).map_err(|_| msg_at(line!()))?;
        let mut a = [0u8; 4096];
        let mut b = [0u8; 4096];
        match r.fill(&mut a, t(5000)) {
            Ok(()) => {}
            Err(rt::random::RandomError::NotSeeded) => {
                sys::debug_write(
                    b"s9: no entropy device: the random service refuses, as it should\n",
                );
                return Ok(());
            }
            Err(_) => return Err(msg_at(line!())),
        }
        r.fill(&mut b, t(5000)).map_err(|_| msg_at(line!()))?;
        ensure!(a != b, "two reads differ");
        ensure!(a.iter().any(|&x| x != a[0]), "not constant");
        let ones: u32 = a.iter().chain(b.iter()).map(|x| x.count_ones()).sum();
        ensure!(
            (ones as i64 - 4 * 8192).abs() < 1000,
            "bit balance of 8 KiB: {ones} ones in 65536 bits"
        );
        let mut counts = [0u32; 256];
        for &x in a.iter().chain(b.iter()) {
            counts[x as usize] += 1;
        }
        ensure!(counts.iter().all(|&c| c < 80), "no byte value dominates");
        // a long request is served in pieces; a zero-length one is refused by the protocol, not answered with nothing
        let mut big = [0u8; 10_000];
        r.fill(&mut big, t(5000)).map_err(|_| msg_at(line!()))?;
        ensure!(big.iter().any(|&x| x != 0), "a long read is filled");
        sys::debug_write(b"s9: random service ok\n");
        Ok(())
    })();
    f.close();
    r
}

/// S9 step 6, Q28: a C program using BSD sockets (`ctest sockets`, `libc/tests/c/sockets.c`) over loopback and the stack:
/// refusals with the right errno, TCP echo of 100,000 bytes through nonblocking calls and `poll`, `select`, a connect in
/// progress and its `SO_ERROR`, a reset by `SO_LINGER {1,0}`, UDP, and `SO_RCVTIMEO`. Its output goes to the serial log; the
/// test fails if any line says FAIL or the exit code is not 0.
pub fn c_sockets_over_the_stack() -> Outcome {
    let Some(mut f) = start_full()? else {
        sys::debug_write(b"s9: no network device on this machine: net tests skipped\n");
        return Ok(());
    };
    let r = (|| -> Outcome {
        let mut cfg = f.connect(b"net-config")?;
        configure_static(&mut cfg)?;
        // wait until the stack serves `net`
        let _warm = f.connect(b"net")?;
        let bi = boot_image_vmo()?;
        let (off, len) = image_entry(bi, b"bin/ctest")?;
        let image = rt::spawn::read_image(bi, off, len).ok_or_else(|| msg_at(line!()))?;
        let (out_mine, out_theirs) = socket()?;
        let svc_view = f.view(b"net")?;
        let s = Startup {
            args: vec![b"ctest".to_vec(), b"sockets".to_vec()],
            namespace: vec![(b"/svc".to_vec(), 1)],
            roles: vec![(Role::Stdout, 0)],
            env: vec![b"RESOLVER=10.0.2.2:47803".to_vec()],
        };
        let bytes = s.encode().map_err(|_| msg_at(line!()))?;
        let p = rt::spawn::spawn(&image, &bytes, &[out_theirs, svc_view.ch]).map_err(|e| {
            let mut d = Detail::new();
            let _ = core::fmt::Write::write_fmt(&mut d, format_args!("spawn ctest: {e:?}"));
            d
        })?;
        let mut got = Vec::new();
        let mut buf = [0u8; 2048];
        let limit = sys::clock_get() + 40_000;
        let mut timed_out = false;
        loop {
            if sys::clock_get() >= limit {
                timed_out = true;
                sys::process_kill(p, -1);
                break;
            }
            let w = sys::object_wait(
                out_mine,
                sig::READABLE | sig::PEER_CLOSED,
                sys::clock_get() + 500,
            );
            if w.status != 0 {
                continue;
            }
            match rt::io::read(out_mine, &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => got.extend_from_slice(&buf[..n]),
            }
        }
        wait_ended(p, 5000)?;
        let code = if timed_out {
            -9999
        } else {
            sys::process_info(p).a1 as i64
        };
        sys::handle_close(p);
        sys::handle_close(out_mine);
        ensure!(
            code != 99,
            "ctest is a stub: the build had no RISC-V C compiler (set CC_RISCV)"
        );
        let mut failed = 0;
        for line in got.split(|&b| b == b'\n') {
            if line.starts_with(b"FAIL") {
                failed += 1;
            }
            if code != 0 || line.starts_with(b"FAIL") || line.starts_with(b"sockets:") {
                let mut l = b"s9: c|".to_vec();
                l.extend_from_slice(&line[..line.len().min(300)]);
                sys::debug_write(&l);
            }
        }
        ensure_eq!(failed, 0, "no C socket check failed");
        ensure_eq!(code, 0, "ctest sockets exits 0");
        ensure!(got.windows(8).any(|w| w == b"ok: bind"), "the checks ran");
        Ok(())
    })();
    f.close();
    r
}

// ---------------------------------------------------------------------------------------------------- S9 step 7b

/// Run a program from the boot image with `/svc` = a view of `net` (and extra namespace entries), collect its standard output
/// until it ends or `limit` ticks pass, and return `(exit code, output)`; the code is -9999 if it had to be killed.
fn run_with_svc(
    f: &mut Full,
    name: &[u8],
    args: &[&[u8]],
    extra_ns: Vec<(Vec<u8>, u64)>,
    limit: u64,
) -> Result<(i64, Vec<u8>), Detail> {
    let bi = boot_image_vmo()?;
    let (off, len) = image_entry(bi, name)?;
    let image = rt::spawn::read_image(bi, off, len).ok_or_else(|| msg_at(line!()))?;
    let (out_mine, out_theirs) = socket()?;
    let svc_view = f.view(b"net")?;
    let mut handles = vec![out_theirs, svc_view.ch];
    let mut namespace = vec![(b"/svc".to_vec(), 1u32)];
    for (i, (n, h)) in extra_ns.into_iter().enumerate() {
        namespace.push((n, 2 + i as u32));
        handles.push(h);
    }
    let s = Startup {
        args: args.iter().map(|a| a.to_vec()).collect(),
        namespace,
        roles: vec![(Role::Stdout, 0)],
        ..Startup::default()
    };
    let bytes = s.encode().map_err(|_| msg_at(line!()))?;
    let p = rt::spawn::spawn(&image, &bytes, &handles).map_err(|e| {
        let mut d = Detail::new();
        let _ = core::fmt::Write::write_fmt(&mut d, format_args!("spawn: {e:?}"));
        d
    })?;
    let mut got = Vec::new();
    let mut buf = [0u8; 2048];
    let until = sys::clock_get() + limit;
    let mut timed_out = false;
    loop {
        if sys::clock_get() >= until {
            timed_out = true;
            sys::process_kill(p, -1);
            break;
        }
        let w = sys::object_wait(
            out_mine,
            sig::READABLE | sig::PEER_CLOSED,
            sys::clock_get() + 500,
        );
        if w.status != 0 {
            continue;
        }
        match rt::io::read(out_mine, &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => got.extend_from_slice(&buf[..n]),
        }
    }
    wait_ended(p, 5000)?;
    let code = if timed_out {
        -9999
    } else {
        sys::process_info(p).a1 as i64
    };
    sys::handle_close(p);
    sys::handle_close(out_mine);
    Ok((code, got))
}

fn relay(tag: &str, out: &[u8]) {
    for line in out.split(|&b| b == b'\n') {
        if line.starts_with(b"##FETCH") {
            let mut l = alloc::format!("s9: fetch|{tag}|").into_bytes();
            l.extend_from_slice(&line[..line.len().min(200)]);
            sys::debug_write(&l);
        }
    }
}

/// `fetch` against the host's own HTTP server (10.0.2.2:47804, started by the harness): the digest of a 256 KiB file comes out
/// the same as the host computed, by address, by name through the harness's DNS server (A, CNAME, a dropped first query that
/// the resolver retries), and the failure exits: 404, no such name, connection refused.
pub fn net_fetch_from_a_host_server() -> Outcome {
    let Some(mut f) = start_full()? else {
        sys::debug_write(b"s9: no network device on this machine: net tests skipped\n");
        return Ok(());
    };
    let r = (|| -> Outcome {
        let mut cfg = f.connect(b"net-config")?;
        configure_static(&mut cfg)?;
        let _warm = f.connect(b"net")?;
        // is there a host server? (a harness without one skips this test, saying so)
        let probe = run_with_svc(
            &mut f,
            b"bin/fetch",
            &[b"fetch", b"-m", b"4", b"http://10.0.2.2:47804/file.bin"],
            vec![],
            12_000,
        )?;
        if probe.0 == 4 || probe.0 == 7 {
            sys::debug_write(b"s9: no host HTTP server on 47804 (the harness did not start one): fetch tests skipped\n");
            return Ok(());
        }
        ensure_eq!(probe.0, 0, "a plain fetch by address");
        relay("addr", &probe.1);
        let dns: &[u8] = b"10.0.2.2:47803";
        for (tag, url) in [
            ("name", &b"http://files.test:47804/file.bin"[..]),
            ("cname", b"http://alias.test:47804/file.bin"),
            ("retry", b"http://slow.test:47804/file.bin"),
        ] {
            let (code, out) = run_with_svc(
                &mut f,
                b"bin/fetch",
                &[b"fetch", b"--sha256", b"--dns", dns, url],
                vec![],
                30_000,
            )?;
            relay(tag, &out);
            ensure_eq!(code, 0, "fetch by name ({tag})");
        }
        let (code, out) = run_with_svc(
            &mut f,
            b"bin/fetch",
            &[b"fetch", b"--sha256", b"http://10.0.2.2:47804/file.bin"],
            vec![],
            30_000,
        )?;
        relay("addr", &out);
        ensure_eq!(code, 0, "fetch by address with a digest");
        let (code, _) = run_with_svc(
            &mut f,
            b"bin/fetch",
            &[b"fetch", b"http://10.0.2.2:47804/nosuch"],
            vec![],
            15_000,
        )?;
        ensure_eq!(code, 5, "a 404 is exit 5");
        let (code, _) = run_with_svc(
            &mut f,
            b"bin/fetch",
            &[b"fetch", b"--dns", dns, b"http://nosuch.test/"],
            vec![],
            15_000,
        )?;
        ensure_eq!(code, 3, "NXDOMAIN is exit 3");
        let (code, _) = run_with_svc(
            &mut f,
            b"bin/fetch",
            &[b"fetch", b"http://10.0.2.2:47899/"],
            vec![],
            20_000,
        )?;
        ensure_eq!(code, 4, "a refused connection is exit 4");
        let (code, _) = run_with_svc(
            &mut f,
            b"bin/fetch",
            &[b"fetch", b"https://10.0.2.2/"],
            vec![],
            5000,
        )?;
        ensure_eq!(code, 2, "https is refused");
        sys::debug_write(b"s9: fetch tests done\n");
        Ok(())
    })();
    f.close();
    r
}

/// `httpd` serving a directory of the memory filesystem for the host's own HTTP client (through `hostfwd=tcp`): the harness
/// downloads a 200,000-byte file, reads the index, gets 404, 405, 400 (`..`, an encoded `..`, a body), 414, 431 and 505, sends
/// HEAD, hangs up mid-head, and downloads four files at once; then `httpd` quits after its 16 connections.
pub fn net_httpd_for_the_host() -> Outcome {
    let Some(mut f) = start_full()? else {
        sys::debug_write(b"s9: no network device on this machine: net tests skipped\n");
        return Ok(());
    };
    let r = (|| -> Outcome {
        let mut cfg = f.connect(b"net-config")?;
        configure_static(&mut cfg)?;
        let _warm = f.connect(b"net")?;
        // the files: a memory filesystem's /tmp is the web root
        let (_fsd, mut root, _bi) = start_memfsd()?;
        let mut tmp = root
            .open(b"tmp", pfl::DIRECTORY, RW_DIR)
            .map_err(|e| msg_e(e, line!()))?;
        let blob: Vec<u8> = (0..200_000usize).map(|i| pattern(13, i)).collect();
        let mut file = tmp
            .open(b"file.bin", pfl::CREATE, prt::READ | prt::WRITE)
            .map_err(|e| msg_e(e, line!()))?;
        let mut off = 0u64;
        for c in blob.chunks(8192) {
            file.write(off, c).map_err(|e| msg_e(e, line!()))?;
            off += c.len() as u64;
        }
        let mut idx = tmp
            .open(b"index.html", pfl::CREATE, prt::READ | prt::WRITE)
            .map_err(|e| msg_e(e, line!()))?;
        idx.write(0, b"<html>hello from the OS</html>")
            .map_err(|e| msg_e(e, line!()))?;
        let www = tmp.clone_conn(RO_DIR).map_err(|e| msg_e(e, line!()))?;
        sys::debug_write(b"s9: httpd ready\n");
        let (code, _) = run_with_svc(
            &mut f,
            b"bin/httpd",
            &[b"httpd", b"-p", b"7778", b"--quit-after", b"16"],
            vec![(b"/www".to_vec(), www.ch)],
            60_000,
        )?;
        ensure_eq!(code, 0, "httpd served its 16 connections and quit");
        Ok(())
    })();
    f.close();
    r
}

/// Nanoseconds in `cycles` of the `time` counter, at the rate the device tree gave the kernel (`debug_stat(105)`).
fn counter_ns(cycles: u64) -> u64 {
    (u128::from(cycles) * 1_000_000_000 / u128::from(sys::debug_stat(105).max(1))) as u64
}
