//! `netsup`: the network stack's supervisor (S8, NET-SPEC 4.2 and 9). It holds everything that must outlive a child
//! (work item T42's rule): the device claim, its three DMA VMOs (the receive pool, the transmit pool, the virtqueue pages), the ring
//! VMO, the doorbell events, and starts and restarts the driver `netdrv` (and, in the full system, `netd` and `dhcpc`) under the
//! restart-intensity policy of `supervise`.
//!
//! Startup: `ServiceDevmgr` (a connection to the device manager: the claim lives as long as this connection), `BootImage` (to find
//! the programs), `Job` (MANAGE: the job it makes the **driver's job** under) and, in `stackless` mode (the self-tests act as the
//! stack), `ServiceInit`: the channel on which the stack-side handles are handed to whoever started us.
//!
//! **The device (IOMMU11; IOMMU-SPEC 4.1, 9, 10; NET-SPEC and NET-DESIGN, amendments of 2026-11-25).** The network function is a
//! `virtio-net-pci` function behind the IOMMU. `netsup` makes the driver's job (a child of `Job`), and claims the function for it
//! (`ClaimNet`): the job pays for the device's domain and the three DMA VMOs, and carries the finite I/O limits the manager sets.
//! It is the network device's **supervisor**: it keeps the domain's control handle (destroy, faults) and the interrupt handles, and
//! **maps nothing** into the domain. The driver is given a duplicate of the domain with IO_MAP and IO_UNMAP only, the receive pool
//! and the queue memory with `DMA`, READ and WRITE, the **transmit pool with `DMA` and READ but no WRITE** (so it can only map it
//! read-only for the device: a device can never be made to overwrite frames the stack is sending), duplicates of the three
//! interrupts, and maps the buffers itself. **A killed driver's domain is destroyed and made anew**: before every driver after the
//! first, `netsup` re-claims the function (`ReclaimNet`: the manager, which alone holds configuration space, turns MSI-X and bus
//! mastering off, resets the function and destroys the old domain, then makes a new one and grants the vectors afresh), so nothing
//! is mapped once and kept across driver lifetimes; the pools themselves (the VMOs and their contents) survive.
//!
//! The stack-side handles (what `netd` gets): the ring VMO, the receive pool **read-only and without the DMA right**, the
//! transmit pool read-write without it, the doorbell events and the stack's end of the driver control channel. The stack
//! never learns a physical address, nor an I/O one, and cannot map a pool for the device.
//!
//! `stackless rig` (a test-only mode, for `net_device_cannot_write_the_transmit_pool`): no driver is started; whoever started us is
//! handed exactly the device-side handles a driver gets (and the stack's two pool views), and plays the driver; the device's faults
//! are read here, with the control handle, and passed on to it as `fault` messages.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use netring::{ctl, state};
use proto::bootimg;
use proto::devmgr::{self, PciClaim, Status as DStatus};
use proto::startup::{Role, Startup};
use rt::abi::rights::Rights;
use rt::abi::signals;
use rt::startup::{self, EXIT_BAD_STARTUP, EXIT_NO_STARTUP};
use rt::sys;
use supervise::{Action, Policy, Supervisor};

#[path = "../netshm.rs"]
mod netshm;
use netshm::Shm;

const PF_RW: u64 = 3;
const RING_VA: u64 = 0x510_0000;

fn dup(h: u64, rights: Rights) -> Option<u64> {
    let r = sys::handle_duplicate(h, rights.0);
    (r.status == 0).then_some(r.a1 as u64)
}

fn read_bin(bi: u64, name: &[u8]) -> Option<Vec<u8>> {
    let total = sys::vmo_get_size(bi).a1;
    let prefix = rt::spawn::read_image(bi, 0, (total as u64).min(128 * 1024))?;
    let entries = bootimg::parse_prefix(&prefix, total).ok()?;
    let e = entries.iter().find(|e| e.name == name)?;
    rt::spawn::read_image(bi, u64::from(e.offset), u64::from(e.size))
}

/// The device's handles of the current claim (new at every re-claim): the BAR VMO, the domain's control handle, the interrupts (in
/// the claim's order: the configuration change, receive, transmit).
struct Device {
    bar: u64,
    domain: u64,
    irqs: Vec<u64>,
    /// No driver has been started on this claim yet (a driver after it needs a re-claim first).
    fresh: bool,
}

/// What the re-claims did, for `Status` (the self-tests' check of the device side of a restart).
#[derive(Default, Clone, Copy)]
struct ReclaimStats {
    reclaims: u32,
    /// The driver job's I/O-mapped pages just after the last re-claim, before the new driver mapped anything: the old domain was
    /// destroyed with the dead driver's mappings, so this is 0.
    io_pages_after: u64,
}

/// Everything held across child lifetimes.
struct Held {
    /// The manager's connection (the claim lives as long as it).
    devmgr: u64,
    /// The driver's job: devmgr limits it and charges it the domain and the DMA VMOs; the driver runs in it.
    drv_job: u64,
    /// The claim's layout (the driver's argument; its DMA sizes are the first claim's).
    claim: devmgr::PciClaim,
    dev: RefCell<Device>,
    stats: Cell<ReclaimStats>,
    /// The function was given back for good (`release_device`).
    released: Cell<bool>,
    rx: u64,
    tx: u64,
    q: u64,
    ring: u64,
    /// The doorbell events: the driver signals `to_stack`, the stack signals `to_driver`.
    to_stack: u64,
    to_driver: u64,
    /// The configuration blob (one page): netd's interface half and dhcpc's lease half.
    blob: u64,
    /// The next driver generation (the supervisor gives it; the driver never takes one from shared memory).
    next_gen: core::cell::Cell<u64>,
    /// A deliberate fault for the negative-control tests (`FAULT=name` in our own environment), passed to the children.
    fault: Option<Vec<u8>>,
    /// The environment entries that give `netd` its TCP secret and say where it came from (`SECRET=<32 hex>`,
    /// `SECRET_SRC=rngd|standin`); empty in the stackless mode, where there is no `netd`.
    secret_env: Vec<Vec<u8>>,
    #[allow(dead_code)] // (read by the status and flight-recorder code of the full supervisor)
    shm: Shm,
}

/// A running child: its process handle.
struct Child {
    proc: Option<u64>,
    /// Which incarnation: the port key of its exit event carries it, so a late event of a killed predecessor is ignored.
    seq: u64,
    name: &'static str,
    restarts: u32,
    last_exit: i32,
}

/// Send `req` with a duplicate of the driver's job, and read the claim reply and then the message that says bus mastering is on:
/// the claim and its handles. Exit codes 10 to 15 (13 NoDevice, 14 Busy).
/// The transaction id of the next request to the manager (each its own, so a late reply can never be taken for another's).
static TXID: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(1);

fn next_txid() -> u32 {
    TXID.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
}

fn claim_request(chan: u64, job: u64, txid: u32, req: &[u8]) -> Result<(PciClaim, Vec<u64>), i32> {
    let Some(j) = dup(job, Rights::SAME) else {
        return Err(10);
    };
    if sys::channel_write(chan, req.as_ptr(), req.len(), &j, 1).status != 0 {
        sys::handle_close(j);
        return Err(10);
    }
    let read = |buf: &mut [u8], hs: &mut [u64]| {
        let w = sys::object_wait(chan, signals::READABLE, sys::clock_get() + 5000);
        if w.status != 0 {
            return Err(11);
        }
        let r = sys::channel_read(chan, buf.as_mut_ptr(), buf.len(), hs.as_mut_ptr(), hs.len());
        if r.status != 0 {
            return Err(12);
        }
        Ok((r.a1, r.a2))
    };
    let mut buf = [0u8; 256];
    let mut hs = [0u64; 2 + devmgr::MAX_CLAIM_DMA + devmgr::MAX_CLAIM_IRQS];
    let (n, nh) = read(&mut buf, &mut hs)?;
    let handles: Vec<u64> = hs[..nh].to_vec();
    let close = |hs: &[u64]| {
        for &h in hs {
            sys::handle_close(h);
        }
    };
    let c = match devmgr::decode_claim_pci_reply(&buf[..n]) {
        Some((t, DStatus::Ok, Some(c))) if t == txid && handles.len() == c.handles() => c,
        Some((t, DStatus::NoDevice, _)) if t == txid => return Err(13),
        Some((t, DStatus::Busy, _)) if t == txid => return Err(14),
        _ => {
            close(&handles);
            return Err(15);
        }
    };
    // the device may start once the manager says bus mastering is on (IOMMU-SPEC 9 step 6)
    let mut b = [0u8; 16];
    match read(&mut b, &mut []) {
        Ok((n, 0)) if devmgr::decode_claim_pci_started(&b[..n]) == Some(txid) => Ok((c, handles)),
        _ => {
            close(&handles);
            Err(15)
        }
    }
}

/// The first claim: the layout, the device's handles, and the DMA VMOs (queue memory, receive pool, transmit pool).
fn claim(chan: u64, job: u64) -> Result<(PciClaim, Device, [u64; 3]), i32> {
    let txid = next_txid();
    let (c, hs) = claim_request(chan, job, txid, &devmgr::encode_claim_net(txid, 0))?;
    let n = c.dma_pages.len();
    if n != 3 || c.irqs.len() != 3 {
        for h in hs {
            sys::handle_close(h);
        }
        return Err(15);
    }
    let dma = [
        hs[2 + devmgr::NET_DMA_QUEUE],
        hs[2 + devmgr::NET_DMA_RX],
        hs[2 + devmgr::NET_DMA_TX],
    ];
    Ok((
        c,
        Device {
            bar: hs[0],
            domain: hs[1],
            irqs: hs[2 + n..].to_vec(),
            fresh: true,
        },
        dma,
    ))
}

/// A re-claim before a new driver (IOMMU-SPEC 10.2 and 10.4): the manager resets the function and destroys the dead driver's
/// domain, then makes a new one with new grants; our old handles to the device are closed and the new ones kept. It is made as soon
/// as a driver dies (IOMMU-SPEC 10.2: the device is quiet and the dead driver's mappings gone before any backoff), and before any
/// driver start on a claim another driver already used. A failure leaves the network down (`release_device`: the connection, and
/// with it whatever the manager holds for us, goes).
fn reclaim(h: &Held) -> Result<(), i32> {
    if h.released.get() {
        return Err(16);
    }
    let txid = next_txid();
    let (c, hs) = claim_request(
        h.devmgr,
        h.drv_job,
        txid,
        &devmgr::encode_reclaim_net(txid, 0),
    )?;
    if !c.dma_pages.is_empty() || c.irqs.len() != 3 || hs.len() != 5 {
        for x in hs {
            sys::handle_close(x);
        }
        return Err(15);
    }
    let mut d = h.dev.borrow_mut();
    for x in [d.bar, d.domain].into_iter().chain(d.irqs.drain(..)) {
        sys::handle_close(x);
    }
    *d = Device {
        bar: hs[0],
        domain: hs[1],
        irqs: hs[2..].to_vec(),
        fresh: true,
    };
    let mut st = h.stats.get();
    st.reclaims += 1;
    st.io_pages_after = sys::job_info(h.drv_job, 12).a1 as u64;
    h.stats.set(st);
    Ok(())
}

/// The network is down for good: give the function back (closing the manager's connection releases the claim: bus mastering off, the
/// function reset, the domain destroyed) and drop our device handles and DMA VMOs, so the driver job's charge returns. Idempotent.
fn release_device(h: &Held) {
    if h.released.replace(true) {
        return;
    }
    sys::handle_close(h.devmgr);
    let mut d = h.dev.borrow_mut();
    for x in [d.bar, d.domain].into_iter().chain(d.irqs.drain(..)) {
        sys::handle_close(x);
    }
    for x in [h.q, h.rx, h.tx] {
        sys::handle_close(x);
    }
}

/// The device-side handles a driver gets, in the order of `DRIVER_ROLES` (see the header comment), each with TRANSFER so it can
/// travel in a startup message.
fn driver_handles(h: &Held) -> Option<Vec<u64>> {
    if h.released.get() {
        return None;
    }
    let d = h.dev.borrow();
    let t = Rights::TRANSFER;
    let mut want = alloc::vec![
        (
            d.bar,
            Rights::READ
                .union(Rights::WRITE)
                .union(Rights::MAP)
                .union(t)
        ),
        (d.domain, Rights::IO_MAP.union(Rights::IO_UNMAP).union(t)),
        (
            h.q,
            Rights::DMA
                .union(Rights::READ)
                .union(Rights::WRITE)
                .union(Rights::MAP)
                .union(t),
        ),
        (
            h.rx,
            Rights::DMA
                .union(Rights::READ)
                .union(Rights::WRITE)
                .union(t)
        ),
        // the transmit pool read-only for the device: no WRITE (IO4), and no MAP (the driver never maps a pool)
        (h.tx, Rights::DMA.union(Rights::READ).union(t)),
    ];
    for &i in &d.irqs {
        want.push((i, Rights::WAIT.union(Rights::MANAGE).union(t)));
    }
    let mut v = Vec::with_capacity(want.len());
    for (x, r) in want {
        match dup(x, r) {
            Some(y) => v.push(y),
            None => {
                for y in v {
                    sys::handle_close(y);
                }
                return None;
            }
        }
    }
    Some(v)
}

/// The roles of `driver_handles`.
const DRIVER_ROLES: [Role; 8] = [
    Role::NetMmio,
    Role::DeviceDomain,
    Role::NetQueueDma,
    Role::NetRxDma,
    Role::NetTxDma,
    Role::DeviceIrqConfig,
    Role::DeviceIrqQueue0,
    Role::DeviceIrqQueue1,
];

/// Start the driver on a fresh control channel; returns its process handle and the stack's end of the channel.
fn spawn_driver(h: &Held, image: &[u8]) -> Result<(u64, u64), i32> {
    // a driver after the first gets the function afresh: the dead one's domain goes, and a new one is made (OD166 point 2)
    if !h.dev.borrow().fresh {
        if let Err(code) = reclaim(h) {
            rt::println!("##NETSUP re-claim failed (code {code})");
            return Err(code);
        }
    }
    let Some(dev_handles) = driver_handles(h) else {
        return Err(22);
    };
    let pair = sys::channel_create();
    if pair.status != 0 {
        return Err(20);
    }
    let (drv_end, stack_end) = (pair.a1 as u64, pair.a2 as u64);
    let sig_wait = Rights::SIGNAL.union(Rights::WAIT).union(Rights::TRANSFER);
    let mut handles: Vec<u64> = Vec::new();
    let mut roles: Vec<(Role, u32)> = Vec::new();
    let mut add = |role: Role, v: Option<u64>, handles: &mut Vec<u64>| -> bool {
        match v {
            Some(x) => {
                roles.push((role, handles.len() as u32));
                handles.push(x);
                true
            }
            None => false,
        }
    };
    for (role, x) in DRIVER_ROLES.into_iter().zip(dev_handles) {
        add(role, Some(x), &mut handles);
    }
    let ok = add(Role::NetRing, dup(h.ring, Rights::SAME), &mut handles)
        & add(
            Role::NetEventToStack,
            dup(h.to_stack, Rights::SIGNAL.union(Rights::TRANSFER)),
            &mut handles,
        )
        & add(
            Role::NetEventToDriver,
            dup(h.to_driver, sig_wait),
            &mut handles,
        )
        & add(Role::NetCtl, Some(drv_end), &mut handles);
    let Some(arg) = devmgr::encode_claim_arg(&h.claim) else {
        for x in handles.into_iter().chain([stack_end]) {
            sys::handle_close(x);
        }
        return Err(21);
    };
    let s = Startup {
        args: alloc::vec![b"netdrv".to_vec(), arg],
        roles,
        env: {
            let mut e = fault_env(h);
            let g = h.next_gen.get();
            h.next_gen.set(g + 1);
            e.push(alloc::format!("GEN={g}").into_bytes());
            e
        },
        ..Startup::default()
    };
    let Ok(bytes) = s.encode() else {
        for x in handles.into_iter().chain([stack_end]) {
            sys::handle_close(x);
        }
        return Err(21);
    };
    if !ok {
        for x in handles.into_iter().chain([stack_end]) {
            sys::handle_close(x);
        }
        return Err(22);
    }
    // in the driver's job: the device's domain and buffers are charged to it, under the limits the manager set (and, as before, our
    // debug resource goes with it: the driver says on the console that it is up)
    let r = rt::spawn::spawn_in(image, &bytes, &handles, h.drv_job);
    h.dev.borrow_mut().fresh = false;
    match r {
        Ok(p) => Ok((p, stack_end)),
        Err(e) => {
            rt::println!("##NETSUP cannot start netdrv: {e:?}");
            sys::handle_close(stack_end);
            Err(23)
        }
    }
}

/// The stack-side handles, in the order `[ring, rx pool (read-only), tx pool, to_stack (wait), to_driver (signal), ctl]`.
fn stack_handles(h: &Held, ctl_end: u64) -> Option<Vec<u64>> {
    let ro = Rights::READ.union(Rights::MAP).union(Rights::TRANSFER);
    let rw = ro.union(Rights::WRITE);
    Some(alloc::vec![
        dup(h.ring, Rights::SAME)?,
        dup(h.rx, ro)?,
        dup(h.tx, rw)?,
        dup(
            h.to_stack,
            Rights::SIGNAL.union(Rights::WAIT).union(Rights::TRANSFER)
        )?,
        dup(h.to_driver, Rights::SIGNAL.union(Rights::TRANSFER))?,
        ctl_end,
    ])
}

#[no_mangle]
pub fn app_main() -> i32 {
    let env = match startup::read() {
        Ok(e) => e,
        Err(startup::EnvError::NoMessage) => return EXIT_NO_STARTUP,
        Err(_) => return EXIT_BAD_STARTUP,
    };
    let (Some(devmgr_ch), Some(bi), Some(job)) = (
        env.handle(Role::ServiceDevmgr),
        env.handle(Role::BootImage),
        env.handle(Role::Job),
    ) else {
        return 3;
    };
    let stackless = env
        .startup
        .args
        .get(1)
        .is_some_and(|a| a.as_slice() == b"stackless");
    let rig = stackless
        && env
            .startup
            .args
            .get(2)
            .is_some_and(|a| a.as_slice() == b"rig");
    let parent = env.handle(Role::ServiceInit);
    let want_dhcp = !env
        .startup
        .args
        .iter()
        .skip(1)
        .any(|a| a.as_slice() == b"nodhcp");

    // the driver's job: the device is claimed for it (it pays, and the manager sets its I/O limits) and the driver runs in it
    let drv_job = sys::job_create(job, &sys::NO_LIMITS);
    if drv_job.status != 0 {
        return 29;
    }
    let drv_job = drv_job.a1 as u64;
    let (claim, dev, [q, rx, tx]) = match claim(devmgr_ch, drv_job) {
        Ok(x) => x,
        Err(code) => {
            rt::println!("##NETSUP no device (code {code})");
            return code;
        }
    };
    // the ring VMO: committed up front, so mapping it in another job is allowed (OD81)
    let ring = sys::vmo_create(2);
    if ring.status != 0 || sys::vmo_commit(ring.a1 as u64, 0, 2 * 4096).status != 0 {
        return 16;
    }
    let ring = ring.a1 as u64;
    let me = sys::process_self().a1 as u64;
    if sys::process_map(me, ring, RING_VA, 0, 2 * 4096, PF_RW).status != 0 {
        return 17;
    }
    let shm = Shm {
        base: RING_VA as usize,
    };
    netring::Mem::store(&shm, ctl::MAGIC, ctl::MAGIC_VALUE);
    netring::Mem::store(&shm, ctl::VERSION, ctl::VERSION_VALUE);
    netring::Mem::store(&shm, ctl::SLOTS, netring::SLOTS as u32);
    netring::Mem::store(&shm, ctl::SLOT_SIZE, netring::SLOT_SIZE as u32);
    netring::Mem::store(&shm, ctl::HDR_LEN, netring::HDR_LEN as u32);
    netring::Mem::store(&shm, ctl::FRAME_MAX, netring::FRAME_MAX as u32);
    netring::Mem::store(&shm, ctl::STATE, state::NONE);
    let (e1, e2) = (sys::event_create(), sys::event_create());
    if e1.status != 0 || e2.status != 0 {
        return 18;
    }
    let blob = sys::vmo_create(1);
    if blob.status != 0 || sys::vmo_commit(blob.a1 as u64, 0, 4096).status != 0 {
        return 28;
    }
    let held = Held {
        next_gen: core::cell::Cell::new(1),
        fault: env.var(b"FAULT").map(|v| v.to_vec()),
        secret_env: if stackless {
            Vec::new()
        } else {
            secret_env(env.handle(Role::ServiceRegistry))
        },
        blob: blob.a1 as u64,
        devmgr: devmgr_ch,
        drv_job,
        claim,
        dev: RefCell::new(dev),
        stats: Cell::new(ReclaimStats::default()),
        released: Cell::new(false),
        rx,
        tx,
        q,
        ring,
        to_stack: e1.a1 as u64,
        to_driver: e2.a1 as u64,
        shm,
    };
    if rig {
        let Some(parent) = parent else { return 3 };
        return rig_mode(&held, parent);
    }
    let Some(drv_image) = read_bin(bi, b"bin/netdrv") else {
        return 19;
    };
    let mut sup = Supervisor::new(Policy::NET);
    let mut drv = Child {
        proc: None,
        seq: 0,
        name: "netdrv",
        restarts: 0,
        last_exit: 0,
    };
    let (p, stack_end) = match spawn_driver(&held, &drv_image) {
        Ok(x) => x,
        Err(code) => return code,
    };
    drv.proc = Some(p);
    if stackless {
        // the self-tests are the stack: hand them its handles and let them drive
        let Some(parent) = parent else { return 3 };
        let Some(hs) = stack_handles(&held, stack_end) else {
            return 24;
        };
        let r = sys::channel_write(parent, b"netsup".as_ptr(), 6, hs.as_ptr(), hs.len());
        if r.status != 0 {
            return 25;
        }
        // supervise nothing more: wait until the driver ends or the parent goes away
        loop {
            let w = sys::object_wait(p, signals::TERMINATED, sys::clock_get() + 200);
            if w.status == 0 && w.a1 as u32 & signals::TERMINATED != 0 {
                let code = sys::process_info(p).a1 as i32;
                rt::println!("##NETSUP child=netdrv exit={code}");
                return 0;
            }
            if sys::object_signals(parent).a1 as u32 & signals::PEER_CLOSED != 0 {
                sys::process_kill(p, -1);
                let _ = sys::object_wait(p, signals::TERMINATED, sys::clock_get() + 2000);
                return 0;
            }
        }
    }
    // ---- the full system: netd as well, and the supervision loop ----
    let (Some(registry), Some(ctl_parent)) = (env.handle(Role::ServiceRegistry), parent) else {
        return 3;
    };
    let Some(netd_image) = read_bin(bi, b"bin/netd") else {
        return 26;
    };
    let pair = sys::channel_create();
    if pair.status != 0 {
        return 27;
    }
    let (mut sup_end, netd_sup_end) = (pair.a1 as u64, pair.a2 as u64);
    let mut dhcpc = Child {
        proc: None,
        seq: 0,
        name: "dhcpc",
        restarts: 0,
        last_exit: 0,
    };
    let mut netd = Child {
        proc: None,
        seq: 0,
        name: "netd",
        restarts: 0,
        last_exit: 0,
    };
    match spawn_netd(&held, &netd_image, stack_end, registry, netd_sup_end) {
        Ok(p) => netd.proc = Some(p),
        Err(code) => return code,
    }
    let port = sys::port_create().a1 as u64;
    let _ = sys::port_bind(
        port,
        ctl_parent,
        signals::READABLE | signals::PEER_CLOSED,
        8,
    );
    // exit events: key = (incarnation << 4) | child index + 1 (children: 0 driver, 1 stack)
    let bind = |p: u64, c: &mut Child, idx: u64| {
        c.seq += 1;
        let _ = sys::port_bind_flags(
            port,
            p,
            signals::TERMINATED,
            c.seq << 4 | (idx + 1),
            rt::abi::port_flags::ONESHOT,
        );
    };
    bind(drv.proc.unwrap_or(0), &mut drv, 0);
    bind(netd.proc.unwrap_or(0), &mut netd, 1);
    let dhcp_image = read_bin(bi, b"bin/dhcpc");
    if want_dhcp {
        if let Some(img) = &dhcp_image {
            match spawn_dhcpc(&held, img, registry) {
                Ok(p) => {
                    dhcpc.proc = Some(p);
                    bind(p, &mut dhcpc, 2);
                }
                Err(code) => rt::println!("##NETSUP dhcpc not started ({code})"),
            }
        }
    }
    rt::println!("##NETSUP up");
    let mut pending: Vec<(u64, usize)> = Vec::new(); // (restart at, child)
    let mut down = false;
    loop {
        if down {
            // nothing restarts any more: the function, its domain and the DMA memory go back (IOMMU-SPEC 10.2)
            release_device(&held);
        }
        let now = sys::clock_get();
        let deadline = pending.iter().map(|p| p.0).min().unwrap_or(u64::MAX);
        let r = sys::port_wait(port, deadline);
        let now2 = sys::clock_get();
        // restarts that are due
        let mut due: Vec<usize> = Vec::new();
        pending.retain(|&(t, c)| {
            if t <= now2 {
                due.push(c);
                false
            } else {
                true
            }
        });
        for c in due {
            if c == 1 && !down {
                // the stack restarted: a fresh driver goes with it (generation + 1) and the new stack gets its control channel
                if let Some(dp) = drv.proc.take() {
                    sys::process_kill(dp, -1);
                    let _ = sys::object_wait(dp, signals::TERMINATED, sys::clock_get() + 2000);
                    sys::handle_close(dp);
                }
                pending.retain(|&(_, c)| c != 0);
                match restart_both(
                    &held,
                    &drv_image,
                    &netd_image,
                    &mut drv,
                    &mut netd,
                    registry,
                    &bind,
                ) {
                    Some(new_sup) => {
                        sys::handle_close(sup_end);
                        sup_end = new_sup;
                    }
                    None => {
                        down = true;
                        rt::println!("##NETSUP down reason=restart");
                    }
                }
            }
            if c == 2 && !down {
                if let Some(img) = &dhcp_image {
                    match spawn_dhcpc(&held, img, registry) {
                        Ok(p) => {
                            dhcpc.proc = Some(p);
                            bind(p, &mut dhcpc, 2);
                        }
                        Err(_) => down = true,
                    }
                }
            }
            if c == 0 && !down && netd.proc.is_some() {
                match spawn_driver(&held, &drv_image) {
                    Ok((p, ctl_end)) => {
                        drv.proc = Some(p);
                        bind(p, &mut drv, 0);
                        // the running stack gets the new control channel's end
                        let hs = [ctl_end];
                        let _ = sys::channel_write(sup_end, b"ctl".as_ptr(), 3, hs.as_ptr(), 1);
                    }
                    Err(code) => {
                        rt::println!("##NETSUP down reason=spawn driver={code}");
                        down = true;
                    }
                }
            }
        }
        if r.status != 0 {
            continue;
        }
        let key = r.a1 as u64;
        let (seq, which) = (key >> 4, key & 15);
        match which {
            1..=3 => {
                let idx = which - 1;
                let child = match idx {
                    0 => &mut drv,
                    1 => &mut netd,
                    _ => &mut dhcpc,
                };
                if child.seq != seq {
                    continue; // a killed predecessor's late event
                }
                let Some(p) = child.proc.take() else { continue };
                let code = sys::process_info(p).a1 as i32;
                sys::handle_close(p);
                child.last_exit = code;
                child.restarts += 1;
                let name = child.name;
                let restarts = child.restarts;
                let gen = held.shm.load64(ctl::GENERATION);
                rt::println!("##NETSUP child={name} exit={code} restarts={restarts} gen={gen}");
                // the flight record (NET-SPEC 9.4, reduced): the shared counters at the moment a child died
                let c = |i: usize| held.shm.load64(ctl::STATS + 8 * i);
                rt::println!(
                    "##NETFR rx={} tx={} rx_ring_full={} rx_bad_header={} violations={} doorbells={}/{}",
                    c(netring::stat::RX_FRAMES),
                    c(netring::stat::TX_FRAMES),
                    c(netring::stat::RX_DROPPED_RING_FULL),
                    c(netring::stat::RX_BAD_HEADER),
                    c(netring::stat::RING_VIOLATIONS),
                    c(netring::stat::DOORBELLS_SENT),
                    c(netring::stat::DOORBELLS_SUPPRESSED)
                );
                if down {
                    continue;
                }
                // the driver is dead: quiet the device and destroy its domain now (IOMMU-SPEC 10.2), not at the next start, so no
                // DMA of the dead driver's outlives it through the backoff; the next driver starts on this fresh claim
                if idx == 0 {
                    if let Err(code) = reclaim(&held) {
                        rt::println!("##NETSUP re-claim failed (code {code})");
                        rt::println!("##NETSUP down reason=reclaim");
                        down = true;
                    }
                }
                // exits that are only the consequence of the stack's death (the driver sees its control channel close, the DHCP
                // client its connections) are restarted at once and do not count against the intensity limit
                let consequence = (idx == 0 && code == 31) || (idx == 2 && code == 8);
                if consequence {
                    pending.push((now2, idx as usize));
                    continue;
                }
                let restartable = !(20..=24).contains(&code);
                match sup.on_exit(idx as usize, now2 * supervise::MS, restartable) {
                    Action::Restart { delay } => {
                        pending.push((now2 + delay / supervise::MS, idx as usize))
                    }
                    Action::Down => {
                        down = true;
                        rt::println!("##NETSUP down reason={:?}", sup.down_reason());
                    }
                }
                if down {
                    // the network is marked down: nothing keeps restarting, and the stack's listeners close (the registry
                    // answers UNAVAILABLE from now on)
                    pending.clear();
                    for c in [&mut drv, &mut netd, &mut dhcpc] {
                        if let Some(p) = c.proc.take() {
                            sys::process_kill(p, -1);
                            let _ =
                                sys::object_wait(p, signals::TERMINATED, sys::clock_get() + 2000);
                            sys::handle_close(p);
                        }
                    }
                }
            }
            8 => {
                // the control channel: Status and Restart
                let mut b = [0u8; 32];
                loop {
                    let rd = sys::channel_read(
                        ctl_parent,
                        b.as_mut_ptr(),
                        b.len(),
                        core::ptr::null_mut(),
                        0,
                    );
                    if rd.status != 0 {
                        break;
                    }
                    if rd.a1 < 8 {
                        continue;
                    }
                    let Some(txid) = b
                        .get(..4)
                        .map(|t| u32::from_le_bytes([t[0], t[1], t[2], t[3]]))
                    else {
                        continue;
                    };
                    let op = u16::from_le_bytes([b[4], b[5]]);
                    let mut reply = alloc::vec![0u8; 8];
                    reply[..4].copy_from_slice(&txid.to_le_bytes());
                    match op {
                        1 => {
                            for c in [&drv, &netd, &dhcpc] {
                                reply.push(u8::from(c.proc.is_some()));
                                reply.extend_from_slice(&c.restarts.to_le_bytes());
                                reply.extend_from_slice(&c.last_exit.to_le_bytes());
                            }
                            reply.push(u8::from(down));
                            reply
                                .extend_from_slice(&held.shm.load64(ctl::GENERATION).to_le_bytes());
                            // the device side (IOMMU11): re-claims, the driver job's I/O-mapped pages just after the last one and now,
                            // its vectors now, and the device work only the safety poll found
                            let st = held.stats.get();
                            reply.extend_from_slice(&st.reclaims.to_le_bytes());
                            reply.extend_from_slice(&st.io_pages_after.to_le_bytes());
                            for which in [12, 13] {
                                reply.extend_from_slice(
                                    &(sys::job_info(held.drv_job, which).a1 as u64).to_le_bytes(),
                                );
                            }
                            reply.extend_from_slice(
                                &held
                                    .shm
                                    .load64(ctl::STATS + 8 * netring::stat::POLL_FOUND)
                                    .to_le_bytes(),
                            );
                        }
                        2 => {
                            let which = if rd.a1 >= 9 { b[8] } else { 255 };
                            let target = match which {
                                0 => drv.proc,
                                1 => netd.proc,
                                _ => dhcpc.proc,
                            };
                            match target {
                                Some(p) => {
                                    sys::process_kill(p, -3000);
                                }
                                None => reply[4..8].copy_from_slice(&(-10i32).to_le_bytes()),
                            }
                        }
                        _ => reply[4..8].copy_from_slice(&(-4i32).to_le_bytes()),
                    }
                    let _ = sys::channel_write(
                        ctl_parent,
                        reply.as_ptr(),
                        reply.len(),
                        core::ptr::null(),
                        0,
                    );
                }
                if sys::object_signals(ctl_parent).a1 as u32 & signals::PEER_CLOSED != 0 {
                    // whoever started us is gone: end the stack and leave
                    for c in [&drv, &netd, &dhcpc] {
                        if let Some(p) = c.proc {
                            sys::process_kill(p, -1);
                            let _ =
                                sys::object_wait(p, signals::TERMINATED, sys::clock_get() + 2000);
                        }
                    }
                    return 0;
                }
                let _ = sys::port_rearm(port, 8);
            }
            _ => {}
        }
        let _ = now;
    }
}

fn spawn_netd(
    h: &Held,
    image: &[u8],
    ctl_end: u64,
    registry: u64,
    sup_end: u64,
) -> Result<u64, i32> {
    let Some(mut hs) = stack_handles(h, ctl_end) else {
        return Err(24);
    };
    // a connection of its own to the registry (a duplicate of the handle would share one channel with us)
    let Ok(reg) = rt::fs::Fs::new(registry).clone_conn(proto::file::rights::ALL) else {
        return Err(25);
    };
    hs.push(reg.ch);
    hs.push(sup_end);
    let Some(blob) = dup(h.blob, Rights::SAME) else {
        return Err(25);
    };
    hs.push(blob);
    let roles = alloc::vec![
        (Role::NetRing, 0),
        (Role::NetRxPool, 1),
        (Role::NetTxPool, 2),
        (Role::NetEventToStack, 3),
        (Role::NetEventToDriver, 4),
        (Role::NetCtl, 5),
        (Role::ServiceRegistry, 6),
        (Role::ServiceInit, 7),
        (Role::NetConfigBlob, 8)
    ];
    let s = Startup {
        args: alloc::vec![b"netd".to_vec()],
        roles,
        env: {
            let mut e = fault_env(h);
            e.extend(h.secret_env.iter().cloned());
            e
        },
        ..Startup::default()
    };
    let Ok(bytes) = s.encode() else {
        return Err(21);
    };
    match rt::spawn::spawn(image, &bytes, &hs) {
        Ok(p) => Ok(p),
        Err(e) => {
            rt::println!("##NETSUP cannot start netd: {e:?}");
            Err(23)
        }
    }
}

/// The TCP secret for `netd` (S9-SPEC 5.10), derived with HKDF-SHA256 from 32 bytes of the system's generator
/// (`/svc/random`, seeded by the entropy device). Without it (no entropy device, or `rngd` not running) a stand-in is used
/// and said so in the entry `SECRET_SRC=standin`: a hash of the clock, which an observer of the boot could guess
/// (work item T99); the debt is loud on purpose. The stack keeps the secret across its own restarts because `netsup` holds it.
fn secret_env(registry: Option<u64>) -> Vec<Vec<u8>> {
    let mut seed = [0u8; 32];
    let mut from_rngd = false;
    if let Some(reg) = registry {
        let mut fs = rt::fs::Fs::new(reg);
        if let Ok(mut r) = rt::random::Random::connect(&mut fs, sys::clock_get() + 15_000) {
            from_rngd = r.fill(&mut seed, sys::clock_get() + 10_000).is_ok();
        }
    }
    if !from_rngd {
        let t = sys::clock_get().to_le_bytes();
        seed = rngcore::sha256(&[&b"netsup stand-in"[..], &t].concat());
    }
    let mut key = [0u8; 16];
    let _ = rngcore::hkdf(b"riscv-os netd v1", &seed, b"tcp isn and ports", &mut key);
    rngcore::drbg::wipe(&mut seed);
    let hex: Vec<u8> = key
        .iter()
        .flat_map(|b| {
            [
                b"0123456789abcdef"[usize::from(b >> 4)],
                b"0123456789abcdef"[usize::from(b & 15)],
            ]
        })
        .collect();
    rt::println!(
        "##NETSUP tcp secret from {}",
        if from_rngd {
            "rngd"
        } else {
            "stand-in (no entropy source)"
        }
    );
    alloc::vec![
        [&b"SECRET="[..], &hex].concat(),
        (if from_rngd {
            &b"SECRET_SRC=rngd"[..]
        } else {
            &b"SECRET_SRC=standin"[..]
        })
        .to_vec()
    ]
}

/// The environment entry that tells a child which deliberate fault to run (empty normally).
fn fault_env(h: &Held) -> Vec<Vec<u8>> {
    match &h.fault {
        Some(f) => {
            let mut e = b"FAULT=".to_vec();
            e.extend_from_slice(f);
            alloc::vec![e]
        }
        None => Vec::new(),
    }
}

/// Start the DHCP client with a view of `/svc` holding only the two names it needs.
fn spawn_dhcpc(h: &Held, image: &[u8], registry: u64) -> Result<u64, i32> {
    let mut reg = rt::fs::Fs::new(registry);
    let Ok(view) = reg.view(&[b"net-priv", b"net-config"], proto::file::rights::READ) else {
        return Err(25);
    };
    let Some(blob) = dup(h.blob, Rights::SAME) else {
        return Err(25);
    };
    let s = Startup {
        args: alloc::vec![b"dhcpc".to_vec()],
        roles: alloc::vec![(Role::NetConfigBlob, 1)],
        namespace: alloc::vec![(b"/svc".to_vec(), 0)],
        ..Startup::default()
    };
    let Ok(bytes) = s.encode() else {
        return Err(21);
    };
    match rt::spawn::spawn(image, &bytes, &[view.ch, blob]) {
        Ok(p) => Ok(p),
        Err(e) => {
            rt::println!("##NETSUP cannot start dhcpc: {e:?}");
            Err(23)
        }
    }
}

/// After the stack died: start a new driver and a new stack together (a new control channel, generation + 1). `false`:
/// something could not be started; else our end of the new supervisor channel.
fn restart_both(
    held: &Held,
    drv_image: &[u8],
    netd_image: &[u8],
    drv: &mut Child,
    netd: &mut Child,
    registry: u64,
    bind: &dyn Fn(u64, &mut Child, u64),
) -> Option<u64> {
    let Ok((dp, ctl_end)) = spawn_driver(held, drv_image) else {
        return None;
    };
    drv.proc = Some(dp);
    bind(dp, drv, 0);
    let pair = sys::channel_create();
    if pair.status != 0 {
        return None;
    }
    // (a fresh supervisor channel for the new stack; the old one died with it)
    match spawn_netd(held, netd_image, ctl_end, registry, pair.a2 as u64) {
        Ok(p) => {
            netd.proc = Some(p);
            bind(p, netd, 1);
            // our end of the new supervisor channel: the caller keeps it (the next driver restart sends the new control channel there)
            Some(pair.a1 as u64)
        }
        Err(_) => None,
    }
}

/// `stackless rig` (see the header comment): the device-side handles a driver gets, and the stack's two pool views, to `parent` in
/// one message (the claim's encoding, then `DRIVER_ROLES`' handles, then the receive and the transmit pool as `netd` gets them);
/// then each fault the device takes, read with the domain's control handle, as a message `fault | word u64 | io_address u64`, until
/// `parent` goes away.
fn rig_mode(h: &Held, parent: u64) -> i32 {
    let Some(mut hs) = driver_handles(h) else {
        return 24;
    };
    let ro = Rights::READ.union(Rights::MAP).union(Rights::TRANSFER);
    let (Some(srx), Some(stx)) = (dup(h.rx, ro), dup(h.tx, ro.union(Rights::WRITE))) else {
        return 24;
    };
    hs.push(srx);
    hs.push(stx);
    let Some(msg) = devmgr::encode_claim_pci_reply(0, &h.claim) else {
        return 24;
    };
    if sys::channel_write(parent, msg.as_ptr(), msg.len(), hs.as_ptr(), hs.len()).status != 0 {
        return 25;
    }
    let domain = h.dev.borrow().domain;
    loop {
        let w = sys::object_wait(domain, signals::DEVICE_FAULT, sys::clock_get() + 100);
        if w.status == 0 {
            loop {
                let r = sys::iodomain_fault_read(domain);
                if r.status != 0 {
                    break;
                }
                let mut m = b"fault".to_vec();
                m.extend_from_slice(&(r.a1 as u64).to_le_bytes());
                m.extend_from_slice(&(r.a2 as u64).to_le_bytes());
                let _ = sys::channel_write(parent, m.as_ptr(), m.len(), core::ptr::null(), 0);
            }
        }
        if sys::object_signals(parent).a1 as u32 & signals::PEER_CLOSED != 0 {
            return 0;
        }
    }
}
