//! `rngd`: the random service (S9 step 5). It claims the entropy device from the device manager, seeds a generator
//! with fast key erasure (`rngcore`) from what the device returns, reseeds it from the device from time to time, and serves
//! `/svc/random` (`proto::random`). Until it has 32 bytes from the device it answers `NOT_SEEDED`: it never makes bytes up.
//!
//! **The device (IOMMU11; IOMMU-SPEC 9).** It is a `virtio-rng-pci` function behind the IOMMU, and `rngd` is its driver: `ClaimRng`
//! claims it for **the job `rngd` runs in**: its starter makes that job for `rngd` alone, starts it there, and connects it to the
//! manager with the connection bound to that job (`Connect` with a job, `proto::devmgr`), so the claim names no job and cannot name
//! another. That job pays for the domain and the two DMA pages, and the manager sets its finite limits on I/O-mapped pages and
//! vectors, so they bind the driver itself; `rngd` holds no handle to any job and cannot loosen them. The claim hands over a VMO over the BAR with the virtio structures, the domain with IO_MAP and IO_UNMAP only, the queue
//! page and the page the device fills (DMA VMOs), and the interrupt of its one queue. `rngd` maps the two pages **into the device's
//! domain itself**, read-write for the device, and programs the queue with the I/O addresses that came back; it never learns a
//! physical address. A read completes by the queue's interrupt. The manager is the device's supervisor: it keeps the domain's control
//! handle, and when `rngd`'s connection closes it turns bus mastering off, resets the function and destroys the domain.
//!
//! Handles: `ServiceDevmgr` (a connection bound to the job `rngd` runs in, to claim the device), `ServiceRegistry` (a connection with
//! PUBLISH, to publish `random`).
#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use proto::devmgr;
use proto::random as pr;
use proto::startup::Role;
use rngcore::Drbg;
use rt::abi::signals;
use rt::abi::status::Status as Sys;
use rt::service::{self, Core, Service};
use rt::startup::{self, EXIT_BAD_STARTUP, EXIT_NO_STARTUP};
use rt::sys;
use svc::Limits;
use virtio::Transport;

/// Where the BAR and the two DMA pages are mapped in this process.
const BAR_VA: u64 = 0x600_0000;
const DMA_VA: u64 = 0x500_0000;
const PF_RW: u64 = 3;
const PAGE: u64 = 4096;
/// The largest BAR this driver maps (a virtio-rng-pci BAR is 16 KiB on QEMU).
const MAX_BAR: u64 = 1 << 20;
/// `iodomain_map`'s access: the device may read and write.
const IO_RW: u64 = 3;

const QUEUE_SIZE: u16 = 4;
/// The bytes the device fills: the second page (the first is the queue).
const DATA: usize = 4096;
/// How much one device read asks for.
const CHUNK: usize = 64;
/// Reseed from the device after this many bytes have been served.
const RESEED_AFTER: u64 = 1 << 20;
const TIMEOUT: u64 = 2000;
/// How long a reset may take to read back 0 (ticks).
const RESET_MS: u64 = 1000;

/// The BAR's registers, mapped at BAR_VA for `span` bytes: an access outside it, or not aligned, is not made (a read gives all
/// ones).
struct Bar {
    span: u64,
}

impl Bar {
    fn at(&self, off: u64, width: u64) -> Option<u64> {
        (off.is_multiple_of(width) && off.checked_add(width)? <= self.span).then_some(BAR_VA + off)
    }
}

impl virtio::Regs for Bar {
    fn read8(&self, off: u64) -> u8 {
        // SAFETY: BAR_VA maps the BAR VMO (`span` bytes, device memory) read-write for as long as the service runs (`app_main`),
        // and `at` gives an address only when the access lies inside it and is aligned.
        self.at(off, 1).map_or(u8::MAX, |a| unsafe {
            core::ptr::read_volatile(a as *const u8)
        })
    }
    fn read16(&self, off: u64) -> u16 {
        // SAFETY: as `read8`.
        self.at(off, 2).map_or(u16::MAX, |a| unsafe {
            core::ptr::read_volatile(a as *const u16)
        })
    }
    fn read32(&self, off: u64) -> u32 {
        // SAFETY: as `read8`.
        self.at(off, 4).map_or(u32::MAX, |a| unsafe {
            core::ptr::read_volatile(a as *const u32)
        })
    }
    fn write8(&mut self, off: u64, v: u8) {
        if let Some(a) = self.at(off, 1) {
            // SAFETY: as `read8`.
            unsafe { core::ptr::write_volatile(a as *mut u8, v) }
        }
    }
    fn write16(&mut self, off: u64, v: u16) {
        if let Some(a) = self.at(off, 2) {
            // SAFETY: as `read8`.
            unsafe { core::ptr::write_volatile(a as *mut u16, v) }
        }
    }
    fn write32(&mut self, off: u64, v: u32) {
        if let Some(a) = self.at(off, 4) {
            // SAFETY: as `read8`.
            unsafe { core::ptr::write_volatile(a as *mut u32, v) }
        }
    }
}

/// The queue page, mapped read-write at DMA_VA: every offset is kept inside it.
struct Rings;

impl Rings {
    fn at(off: u64) -> u64 {
        DMA_VA + (off % PAGE)
    }
}

impl virtio::Mem for Rings {
    fn read16(&self, off: u64) -> u16 {
        // SAFETY: `at` keeps the address inside the queue page mapped read-write at DMA_VA (`app_main`); the layout aligns it.
        unsafe { core::ptr::read_volatile(Self::at(off & !1) as *const u16) }
    }
    fn read32(&self, off: u64) -> u32 {
        // SAFETY: as `read16`.
        unsafe { core::ptr::read_volatile(Self::at(off & !3) as *const u32) }
    }
    fn write16(&mut self, off: u64, v: u16) {
        // SAFETY: as `read16`.
        unsafe { core::ptr::write_volatile(Self::at(off & !1) as *mut u16, v) }
    }
    fn write32(&mut self, off: u64, v: u32) {
        // SAFETY: as `read16`.
        unsafe { core::ptr::write_volatile(Self::at(off & !3) as *mut u32, v) }
    }
    fn write64(&mut self, off: u64, v: u64) {
        // SAFETY: as `read16`.
        unsafe { core::ptr::write_volatile(Self::at(off & !7) as *mut u64, v) }
    }
}

fn dma_write(off: usize, b: &[u8]) {
    for (i, x) in b.iter().enumerate() {
        // SAFETY: DMA_VA maps the two DMA pages read-write (`claim_device`); callers stay inside them.
        unsafe { core::ptr::write_volatile((DMA_VA as usize + off + i) as *mut u8, *x) };
    }
}

fn dma_read(off: usize, out: &mut [u8]) {
    for (i, x) in out.iter_mut().enumerate() {
        // SAFETY: as in `dma_write`.
        *x = unsafe { core::ptr::read_volatile((DMA_VA as usize + off + i) as *const u8) };
    }
}

struct Device {
    t: virtio::PciTransport<Bar>,
    q: virtio::SplitQueue,
    /// The I/O addresses of the queue page and of the page the device fills (`iodomain_map`'s answers).
    queue_io: u64,
    data_io: u64,
    /// The interrupt of the queue, and its MSI-X entry.
    irq: u64,
    entry: u16,
    /// A request timed out: the queue's state is unknown, so the next read resets the device and starts clean.
    broken: bool,
}

impl Device {
    /// Reset the device and bring it up: the features (only those every driver requires), the queue with its vector, DRIVER_OK.
    fn init(&mut self) -> Result<(), i32> {
        self.broken = false;
        let until = sys::clock_get() + RESET_MS;
        self.t
            .reset(&mut || sys::clock_get() < until)
            .map_err(|_| 24)?;
        dma_write(0, &[0u8; DATA]);
        self.t.negotiate(0).map_err(|_| 21)?;
        if self.t.queue_max(0) < QUEUE_SIZE {
            return Err(23);
        }
        self.q = virtio::SplitQueue::new(QUEUE_SIZE, self.queue_io, PAGE).map_err(|_| 23)?;
        self.t
            .setup_queue(0, QUEUE_SIZE, self.q.addresses(), self.entry)
            .map_err(|_| 23)?;
        self.t.driver_ok();
        Ok(())
    }

    /// Ask the device for up to `CHUNK` bytes; the count it actually wrote, or `None` if it never answered.
    fn read(&mut self, out: &mut [u8; CHUNK]) -> Option<usize> {
        if self.broken && self.init().is_err() {
            return None;
        }
        // one device-writable buffer
        let chain = [virtio::Buf {
            addr: self.data_io,
            len: CHUNK as u32,
            device_writes: true,
        }];
        let head = self.q.add(&mut Rings, &chain, true).ok()?;
        self.t.notify(0);
        // the used ring is read once the queue's interrupt fired (a stale one finds nothing and waits on)
        let deadline = sys::clock_get() + TIMEOUT;
        let n = loop {
            match self.q.take_used(&Rings) {
                Ok(Some((id, len))) if id == head => break len as usize,
                Ok(Some(_)) | Err(_) => {
                    self.broken = true;
                    return None;
                }
                Ok(None) => {}
            }
            let w = sys::object_wait(self.irq, signals::INTERRUPT, deadline);
            if w.status() != Some(Sys::Ok) {
                // whatever the device does later, it must not find a half-used queue: reset and start over next time
                self.broken = true;
                return None;
            }
            sys::interrupt_ack(self.irq);
        };
        let n = n.min(CHUNK);
        dma_read(DATA, &mut out[..n]);
        // what the device wrote is not kept in the DMA page
        dma_write(DATA, &[0u8; CHUNK]);
        Some(n)
    }
}

/// What a connection of this service is.
enum Conn {
    /// The published name's listener: forwarded opens arrive here.
    Listener,
    Client,
}

struct Rngd {
    core: Core<Conn>,
    drbg: Drbg,
    dev: Option<Device>,
    served: u64,
}

impl Rngd {
    /// Mix 64 bytes from the device into the generator. `true` if the device answered.
    fn pull(&mut self) -> bool {
        let Some(dev) = self.dev.as_mut() else {
            return false;
        };
        let mut buf = [0u8; CHUNK];
        match dev.read(&mut buf) {
            // an all-zero block is not entropy (a stuck device): it is not counted
            Some(n) if n > 0 && buf[..n].iter().any(|&b| b != 0) => {
                self.drbg.reseed(&buf[..n]);
                buf.fill(0);
                true
            }
            _ => false,
        }
    }
}

impl Service for Rngd {
    type Conn = Conn;

    fn core(&mut self) -> &mut Core<Conn> {
        &mut self.core
    }

    fn request(&mut self, key: u64, msg: &[u8], hs: &[u64]) {
        if matches!(self.core.conn(key), Some(Conn::Listener)) {
            // a forwarded Open: the client's endpoint becomes a connection in an account of its own
            let ok = proto::file::decode_request(msg)
                .is_ok_and(|r| r.op == proto::file::op::OPEN && r.payload.is_empty())
                && hs.len() == 1;
            if !ok {
                for &h in hs {
                    sys::handle_close(h);
                }
                return;
            }
            let limits = Limits {
                conns: 4,
                held_bytes: 16 * 1024,
                inflight: 4,
                stream_bytes: 0,
            };
            let Ok(account) = self.core.accounts().open(svc::ROOT, limits) else {
                sys::handle_close(hs[0]);
                return;
            };
            if self.core.add_conn(hs[0], account, Conn::Client).is_none() {
                sys::handle_close(hs[0]);
                let _ = self.core.accounts().close(account);
            }
            return;
        }
        for &h in hs {
            sys::handle_close(h);
        }
        let (txid, n) = match pr::decode_request(msg) {
            Ok(x) => x,
            Err(Some(t)) => {
                let _ =
                    self.core
                        .send(key, pr::encode_reply(t, pr::status::BAD_REQUEST, &[]), None);
                return;
            }
            Err(None) => return self.core.violation(key),
        };
        if self.served >= RESEED_AFTER && self.pull() {
            self.served = 0;
        }
        // not yet seeded: try the device again (it may have been slow at boot)
        while !self.drbg.is_seeded() && self.pull() {}
        let mut out: Vec<u8> = alloc::vec![0u8; n];
        let reply = if self.drbg.fill(&mut out) {
            self.served += n as u64;
            pr::encode_reply(txid, pr::status::OK, &out)
        } else {
            pr::encode_reply(txid, pr::status::NOT_SEEDED, &[])
        };
        out.fill(0);
        let _ = self.core.send(key, reply, None);
    }

    fn closed(&mut self, _conn: Conn) {}
}

#[no_mangle]
pub fn app_main() -> i32 {
    let env = match startup::read() {
        Ok(e) => e,
        Err(startup::EnvError::NoMessage) => return EXIT_NO_STARTUP,
        Err(_) => return EXIT_BAD_STARTUP,
    };
    let (Some(devmgr_ch), Some(registry)) = (
        env.handle(Role::ServiceDevmgr),
        env.handle(Role::ServiceRegistry),
    ) else {
        return 3;
    };
    // claim the entropy device (none: the service still runs and answers NOT_SEEDED)
    let dev = claim_device(devmgr_ch);
    let p = sys::port_create();
    if p.status != 0 {
        return 5;
    }
    let pool = Limits {
        conns: 64,
        held_bytes: 64 * 1024,
        inflight: 64,
        stream_bytes: 0,
    };
    let mut d = Rngd {
        core: Core::new(
            p.a1 as u64,
            pool,
            rt::abi::limits::MAX_MSG_BYTES,
            rt::abi::limits::MAX_MSG_HANDLES,
            30_000,
        ),
        drbg: Drbg::new(),
        dev,
        served: 0,
    };
    // seed now: two reads of 64 bytes from the device (the generator wants 32 and takes what it is given)
    let seeded = d.pull() && d.pull();
    // publish `random`
    let mut fs = rt::fs::Fs::new(registry);
    let c = sys::channel_create();
    if c.status != 0 {
        return 6;
    }
    let (mine, theirs) = (c.a1 as u64, c.a2 as u64);
    if fs.publish(b"random", theirs).is_err() {
        return 7;
    }
    // (a published name is served as a listener: forwarded opens arrive on `mine`; each becomes a connection)
    if d.core.add_conn(mine, svc::ROOT, Conn::Listener).is_none() {
        return 8;
    }
    rt::println!(
        "rngd: up, seeded={}",
        u8::from(seeded && d.drbg.is_seeded())
    );
    let _ = Sys::Ok;
    service::run(&mut d)
}

/// Claim the entropy device on our bound connection (for the job we run in) and bring it up; `None` if there is none or it does not
/// work.
fn claim_device(devmgr_ch: u64) -> Option<Device> {
    let req = devmgr::encode_claim_rng(1, 0);
    if sys::channel_write(devmgr_ch, req.as_ptr(), req.len(), core::ptr::null(), 0).status != 0 {
        return None;
    }
    let read = |b: &mut [u8], hs: &mut [u64]| {
        let w = sys::object_wait(devmgr_ch, signals::READABLE, sys::clock_get() + 3000);
        if w.status != 0 {
            return None;
        }
        let r = sys::channel_read(
            devmgr_ch,
            b.as_mut_ptr(),
            b.len(),
            hs.as_mut_ptr(),
            hs.len(),
        );
        (r.status == 0).then_some((r.a1, r.a2))
    };
    let mut b = [0u8; 256];
    let mut hs = [0u64; 2 + devmgr::MAX_CLAIM_DMA + devmgr::MAX_CLAIM_IRQS];
    let (n, nh) = read(&mut b, &mut hs)?;
    let close = |hs: &[u64]| {
        for &h in hs {
            sys::handle_close(h);
        }
    };
    let c = match devmgr::decode_claim_pci_reply(&b[..n]) {
        Some((1, devmgr::Status::Ok, Some(c)))
            if nh == c.handles()
                && c.dma_pages.len() == 2
                && (c.vendor, c.device) == (0x1af4, 0x1044) =>
        {
            c
        }
        _ => {
            close(&hs[..nh]);
            return None;
        }
    };
    // (the device starts only once the manager says bus mastering is on)
    let mut s = [0u8; 16];
    match read(&mut s, &mut []) {
        Some((n, 0)) if devmgr::decode_claim_pci_started(&s[..n]) == Some(1) => {}
        _ => {
            close(&hs[..nh]);
            return None;
        }
    }
    let up = (|| -> Option<Device> {
        let (bar, dom, queue, data) = (hs[0], hs[1], hs[2], hs[3]);
        let (irq_at, entry) = c.irq(0)?;
        let irq = hs[irq_at];
        if c.bar_bytes == 0 || c.bar_bytes > MAX_BAR || !c.bar_bytes.is_multiple_of(PAGE) {
            return None;
        }
        let me = sys::process_self().a1 as u64;
        let mapped = sys::process_map(me, bar, BAR_VA, 0, c.bar_bytes, PF_RW).status == 0
            && sys::process_map(me, queue, DMA_VA, 0, PAGE, PF_RW).status == 0
            && sys::process_map(me, data, DMA_VA + PAGE, 0, PAGE, PF_RW).status == 0;
        if !mapped {
            return None;
        }
        // the two pages go into the device's domain here, read-write for the device, at addresses the kernel chooses
        let (q_io, d_io) = (
            sys::iodomain_map(dom, queue, 0, 1, IO_RW),
            sys::iodomain_map(dom, data, 0, 1, IO_RW),
        );
        if q_io.status != 0 || d_io.status != 0 {
            return None;
        }
        let t = virtio::PciTransport::new(
            Bar { span: c.bar_bytes },
            virtio::PciLocations {
                common: u64::from(c.common.0),
                notify: u64::from(c.notify.0),
                notify_len: u64::from(c.notify.1),
                notify_multiplier: c.notify_multiplier,
                isr: u64::from(c.isr.0),
                device: (c.device_cfg.1 != 0)
                    .then(|| (u64::from(c.device_cfg.0), u64::from(c.device_cfg.1))),
            },
        );
        let mut d = Device {
            t,
            q: virtio::SplitQueue::new(QUEUE_SIZE, q_io.a1 as u64, PAGE).ok()?,
            queue_io: q_io.a1 as u64,
            data_io: d_io.a1 as u64,
            irq,
            entry,
            broken: false,
        };
        d.init().ok()?;
        Some(d)
    })();
    if up.is_none() {
        // a claim we cannot use goes back at once: the connection's end releases it (the function reset, the domain destroyed with
        // whatever was mapped in it), then our mappings and handles go, so nothing stays charged to the job
        sys::handle_close(devmgr_ch);
        let me = sys::process_self().a1 as u64;
        for va in [BAR_VA, DMA_VA, DMA_VA + PAGE] {
            sys::process_unmap(me, va);
        }
        sys::handle_close(me);
        close(&hs[..nh]);
    }
    up
}
