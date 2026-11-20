//! `devmgr`: the device manager (S6c, S6-SPEC section 2; slice DM). It reads the device tree through the
//! root resource, finds *every* virtio block device (a table, in device-tree order), and starts `blkd`
//! for a client that asks for device `index` (lowest address first), handing the driver only what it needs: that slot's MMIO
//! window, one DMA buffer and (on a PLIC machine) the slot's interrupt. Each device has its own driver
//! slot, so two devices can be in use at once.
//!
//! The manager is the *supervisor* of the driver in the sense of work item T42: it holds its own
//! handles to the MMIO window and the DMA buffer for as long as it runs, so killing or losing a
//! driver frees nothing the device may still write to, and it resets the device before every new
//! driver gets the buffer.
//!
//! Handles: `Resource` (the root resource), `BootImage` (to find `bin/blkd`), `ServiceDevmgr`
//! (the channel it serves: `proto::devmgr`). It exits 0 when its client closes the service channel.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use proto::bootimg;
use proto::devmgr::{self, Status};
use proto::startup::{Role, Startup};
use rt::abi::rights::Rights;
use rt::abi::signals;
use rt::service::{self, Core, Service};
use rt::startup::{self, EXIT_BAD_STARTUP, EXIT_NO_STARTUP};
use rt::sys;

const PF_R: u64 = 1;
const PF_RW: u64 = 3;
/// Where the device tree and the probed slot are mapped in this process.
const DTB_VA: u64 = 0x300_0000;
/// Where device `i`'s register page stays mapped in this process (for the reset): one page each.
const MMIO_VA: u64 = 0x600_0000;

fn mmio_va(i: usize) -> u64 {
    MMIO_VA + (i as u64) * 4096
}

const STATUS: usize = 0x070;
const INTERRUPT_STATUS: usize = 0x060;
const INTERRUPT_ACK: usize = 0x064;
const MAGIC: usize = 0x000;
const DEVICE_ID: usize = 0x008;
const ID_BLOCK: u32 = 2;
const ID_NET: u32 = 1;
const ID_RNG: u32 = 4;
/// The entropy device's DMA: one page for the queue and one for the bytes it fills.
const RNG_DMA_PAGES: u64 = 2;
/// The network device's DMA (NET-SPEC 4.3, 5.1): two 64-page packet pools and a two-page virtqueue area.
const NET_RX_PAGES: u64 = 64;
const NET_TX_PAGES: u64 = 64;
const NET_QUEUE_PAGES: u64 = 2;

struct Block {
    /// Where the slot's MMIO page is mapped in this process (for the reset).
    va: u64,
    /// The slot's MMIO VMO.
    mmio: u32,
    dma: u32,
    irq: Option<u32>,
    /// The driver process handle, once one was started.
    driver: Option<u32>,
}

fn rd(va: u64, off: usize) -> u32 {
    // SAFETY: `va` maps a block device's 4 KiB register page in this process.
    unsafe { core::ptr::read_volatile((va as usize + off) as *const u32) }
}

fn wr(va: u64, off: usize, v: u32) {
    // SAFETY: as in `rd`.
    unsafe { core::ptr::write_volatile((va as usize + off) as *mut u32, v) }
}

/// Put the device back to its reset state and quiet its interrupt line.
fn reset(va: u64) {
    wr(va, STATUS, 0);
    for _ in 0..1_000_000 {
        if rd(va, STATUS) == 0 {
            break;
        }
    }
    wr(va, INTERRUPT_ACK, rd(va, INTERRUPT_STATUS));
}

fn read_dtb(res: u32, me: u32) -> Option<Vec<u8>> {
    let r = sys::resource_dtb(res);
    if r.status != 0 {
        return None;
    }
    let (vmo, len) = (r.a1 as u32, r.a2);
    let ok =
        sys::process_map(me, vmo, DTB_VA, 0, (len as u64).div_ceil(4096) * 4096, PF_R).status == 0;
    sys::handle_close(vmo);
    if !ok {
        return None;
    }
    // SAFETY: DTB_VA was just mapped read-only for `len` bytes (rounded up to pages).
    let blob = unsafe { core::slice::from_raw_parts(DTB_VA as *const u8, len) }.to_vec();
    sys::process_unmap(me, DTB_VA);
    Some(blob)
}

/// Is the slot at `pa` a virtio device of `want` (a device id)? If so its page stays mapped at `va`.
fn probe(res: u32, me: u32, pa: u64, va: u64, want: u32) -> Option<u32> {
    let vmo = sys::vmo_create_physical(res, pa, 4096);
    if vmo.status != 0 {
        return None;
    }
    let vmo = vmo.a1 as u32;
    let mapped = sys::process_map(me, vmo, va, 0, 4096, PF_RW).status == 0;
    let found = mapped && rd(va, MAGIC) == 0x7472_6976 && rd(va, DEVICE_ID) == want;
    if mapped && !found {
        sys::process_unmap(me, va);
    }
    if found {
        Some(vmo) // stays mapped at `va`
    } else {
        sys::handle_close(vmo);
        None
    }
}

/// A virtio network device, claimable by one client at a time (S8). The DMA memory exists only while it is claimed.
struct Net {
    va: u64,
    mmio: u32,
    irq: Option<u32>,
    /// While claimed: the claimant's connection key and the manager's own handles to the three DMA VMOs.
    claim: Option<(u64, [u32; 3])>,
}

impl Net {
    fn release(&mut self) {
        if let Some((_, dmas)) = self.claim.take() {
            // the claimant is gone: quiet the device before its memory can be freed or handed out again (T42)
            reset(self.va);
            for h in dmas {
                sys::handle_close(h);
            }
        }
    }
}

struct Rng {
    va: u64,
    mmio: u32,
    claim: Option<(u64, u32)>,
}

impl Rng {
    fn release(&mut self) {
        if let Some((_, dma)) = self.claim.take() {
            reset(self.va);
            sys::handle_close(dma);
        }
    }
}

fn blkd_image(bi: u32) -> Option<Vec<u8>> {
    let total = sys::vmo_get_size(bi).a1;
    let prefix = rt::spawn::read_image(bi, 0, (total as u64).min(128 * 1024))?;
    let entries = bootimg::parse_prefix(&prefix, total).ok()?;
    let e = entries.iter().find(|e| e.name == b"bin/blkd")?;
    rt::spawn::read_image(bi, u64::from(e.offset), u64::from(e.size))
}

impl Block {
    fn driver_alive(&self) -> bool {
        match self.driver {
            None => false,
            Some(p) => sys::object_signals(p).a1 as u32 & signals::TERMINATED == 0,
        }
    }

    /// Start a driver on a fresh channel; `Err` is the status to answer. The client's end moves
    /// into the reply.
    fn open(&mut self, image: &[u8]) -> Result<u32, Status> {
        if self.driver_alive() {
            return Err(Status::Busy);
        }
        if let Some(p) = self.driver.take() {
            sys::handle_close(p);
        }
        reset(self.va);
        let pair = sys::channel_create();
        if pair.status != 0 {
            return Err(Status::Failed);
        }
        let (mine, theirs) = (pair.a1 as u32, pair.a2 as u32);
        let dup = |h: u32| {
            let r = sys::handle_duplicate(h, Rights::SAME.0);
            (r.status == 0).then_some(r.a1 as u32)
        };
        let (m, d) = (dup(self.mmio), dup(self.dma));
        let (Some(mmio), Some(dma)) = (m, d) else {
            for h in [m, d].into_iter().flatten().chain([mine, theirs]) {
                sys::handle_close(h);
            }
            return Err(Status::Failed);
        };
        let mut handles = alloc::vec![mmio, dma, theirs];
        let mut roles = alloc::vec![(Role::BlkMmio, 0), (Role::BlkDma, 1), (Role::ServiceBlk, 2)];
        if let Some(irq) = self.irq.and_then(dup) {
            handles.push(irq);
            roles.push((Role::BlkIrq, 3));
        }
        let s = Startup {
            args: alloc::vec![b"blkd".to_vec()],
            roles,
            ..Startup::default()
        };
        let Ok(bytes) = s.encode() else {
            for h in handles.into_iter().chain([mine]) {
                sys::handle_close(h);
            }
            return Err(Status::Failed);
        };
        match rt::spawn::spawn(image, &bytes, &handles) {
            Ok(p) => {
                self.driver = Some(p);
                Ok(mine)
            }
            Err(_) => {
                sys::handle_close(mine);
                Err(Status::Failed)
            }
        }
    }
}

/// The service's total pool (OD84): a supervisor and a few more.
const POOL: svc::Limits = svc::Limits {
    conns: 16,
    held_bytes: 64 * 1024,
    inflight: 16,
    stream_bytes: 0,
};
/// A reply the client has no room for is kept this long (ticks) before the connection is dropped.
const HOLD_TIMEOUT: u64 = 30_000;

struct DevMgr {
    core: Core<u64>,
    res: u32,
    blocks: Vec<Block>,
    nets: Vec<Net>,
    rngs: Vec<Rng>,
    /// The real-time clocks: one VMO of each register page (claimed read-only, by any number of clients).
    rtcs: Vec<u32>,
    image: Option<Vec<u8>>,
}

impl DevMgr {
    fn reply(&mut self, key: u64, txid: u32, status: Status, handle: Option<u32>) {
        let _ = self
            .core
            .send(key, devmgr::encode_reply(txid, status), handle);
    }
}

impl Service for DevMgr {
    /// A connection's own key (so `closed` can tell which claim to release).
    type Conn = u64;

    fn core(&mut self) -> &mut Core<u64> {
        &mut self.core
    }

    fn request(&mut self, key: u64, msg: &[u8], hs: &[u32]) {
        let txid_of = |m: &[u8]| {
            m.get(..4)
                .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        };
        // Connect carries exactly one handle (the new client's end); OpenBlock carries none. Anything else
        // is refused, and whatever came is closed (authority we did not ask for).
        if devmgr::op_of(msg) == Some(devmgr::OP_CONNECT) {
            let Ok(txid) = devmgr::decode_connect(msg) else {
                for &h in hs {
                    sys::handle_close(h);
                }
                return self.reply(key, txid_of(msg), Status::BadRequest, None);
            };
            if hs.len() != 1 {
                for &h in hs {
                    sys::handle_close(h);
                }
                return self.reply(key, txid, Status::BadRequest, None);
            }
            let account = self.core.account_of(key).unwrap_or(svc::ROOT);
            return match self.core.add_conn(hs[0], account, 0) {
                Some(k) => {
                    if let Some(c) = self.core.conn_mut(k) {
                        *c = k;
                    }
                    self.reply(key, txid, Status::Ok, None)
                }
                None => {
                    sys::handle_close(hs[0]);
                    self.reply(key, txid, Status::Quota, None)
                }
            };
        }
        for &h in hs {
            sys::handle_close(h);
        }
        if !hs.is_empty() {
            return self.reply(key, txid_of(msg), Status::BadRequest, None);
        }
        if devmgr::op_of(msg) == Some(devmgr::OP_CLAIM_RTC) {
            return match devmgr::decode_claim_rtc(msg) {
                Ok((txid, index)) => {
                    let Some(&vmo) = self.rtcs.get(usize::from(index)) else {
                        return self.reply(key, txid, Status::NoDevice, None);
                    };
                    // read-only: the claimant can map and read the time, nothing else
                    let d = sys::handle_duplicate(
                        vmo,
                        Rights::READ.0 | Rights::MAP.0 | Rights::TRANSFER.0,
                    );
                    if d.status != 0 {
                        return self.reply(key, txid, Status::Failed, None);
                    }
                    self.reply(key, txid, Status::Ok, Some(d.a1 as u32))
                }
                Err(Some(t)) => self.reply(key, t, Status::BadRequest, None),
                Err(None) => {}
            };
        }
        if devmgr::op_of(msg) == Some(devmgr::OP_CLAIM_RNG) {
            return match devmgr::decode_claim_rng(msg) {
                Ok((txid, index)) => self.claim_rng(key, txid, index),
                Err(Some(t)) => self.reply(key, t, Status::BadRequest, None),
                Err(None) => {}
            };
        }
        if devmgr::op_of(msg) == Some(devmgr::OP_CLAIM_NET) {
            return match devmgr::decode_claim_net(msg) {
                Ok((txid, index)) => self.claim_net(key, txid, index),
                Err(Some(t)) => self.reply(key, t, Status::BadRequest, None),
                Err(None) => {}
            };
        }
        match devmgr::decode_request(msg) {
            Err(Some(t)) => self.reply(key, t, Status::BadRequest, None),
            Err(None) => {}
            Ok((txid, index)) => {
                let (st, handle) = match (self.blocks.get_mut(usize::from(index)), &self.image) {
                    (None, _) => (Status::NoDevice, None),
                    (Some(_), None) => (Status::Failed, None),
                    (Some(b), Some(img)) => match b.open(img) {
                        Ok(h) => (Status::Ok, Some(h)),
                        Err(s) => (s, None),
                    },
                };
                self.reply(key, txid, st, handle);
            }
        }
    }

    fn closed(&mut self, conn: u64) {
        self.drop_claims_of(conn);
    }
}

impl DevMgr {
    /// `ClaimNet`: hand the claimant the device and its DMA memory (NET-SPEC 4.3). The DMA VMOs are made here (only the
    /// root resource can) and are charged to this manager (work item T84).
    fn claim_net(&mut self, key: u64, txid: u32, index: u16) {
        let fail = |this: &mut DevMgr, st: Status| {
            let _ = this.core.send(key, devmgr::encode_reply(txid, st), None);
        };
        let res = self.res;
        let Some(n) = self.nets.get_mut(usize::from(index)) else {
            return fail(self, Status::NoDevice);
        };
        if n.claim.is_some() {
            return fail(self, Status::Busy);
        }
        let mk = |pages: u64| {
            let r = sys::vmo_create_dma(res, pages);
            (r.status == 0).then_some(r.a1 as u32)
        };
        let (rx, tx, q) = (mk(NET_RX_PAGES), mk(NET_TX_PAGES), mk(NET_QUEUE_PAGES));
        let (Some(rx), Some(tx), Some(q)) = (rx, tx, q) else {
            for h in [rx, tx, q].into_iter().flatten() {
                sys::handle_close(h);
            }
            return fail(self, Status::Failed);
        };
        reset(n.va);
        let dup = |h: u32| {
            let r = sys::handle_duplicate(h, Rights::SAME.0);
            (r.status == 0).then_some(r.a1 as u32)
        };
        let mut give: Vec<u32> = Vec::new();
        let mut ok = true;
        for h in [n.mmio, rx, tx, q].into_iter().chain(n.irq) {
            match dup(h) {
                Some(d) => give.push(d),
                None => ok = false,
            }
        }
        if !ok {
            for h in give.into_iter().chain([rx, tx, q]) {
                sys::handle_close(h);
            }
            return fail(self, Status::Failed);
        }
        let flags = if n.irq.is_some() {
            devmgr::NET_FLAG_IRQ
        } else {
            0
        };
        n.claim = Some((key, [rx, tx, q]));
        let reply = devmgr::encode_claim_net_reply(
            txid,
            Status::Ok,
            &devmgr::NetClaim {
                flags,
                rx_pages: NET_RX_PAGES as u32,
                tx_pages: NET_TX_PAGES as u32,
                queue_pages: NET_QUEUE_PAGES as u32,
            },
        );
        // (the handles travel in one message: the channel carries up to MAX_MSG_HANDLES)
        let _ = self.core.send_handles(key, reply, give);
    }

    /// `ClaimRng`: hand the claimant the entropy device and two pages of DMA (S9 step 5).
    fn claim_rng(&mut self, key: u64, txid: u32, index: u16) {
        let fail = |this: &mut DevMgr, st: Status| {
            let _ = this.core.send(key, devmgr::encode_reply(txid, st), None);
        };
        let res = self.res;
        let Some(r) = self.rngs.get_mut(usize::from(index)) else {
            return fail(self, Status::NoDevice);
        };
        if r.claim.is_some() {
            return fail(self, Status::Busy);
        }
        let dma = sys::vmo_create_dma(res, RNG_DMA_PAGES);
        if dma.status != 0 {
            return fail(self, Status::Failed);
        }
        let dma = dma.a1 as u32;
        reset(r.va);
        let dup = |h: u32| {
            let d = sys::handle_duplicate(h, Rights::SAME.0);
            (d.status == 0).then_some(d.a1 as u32)
        };
        let (Some(m), Some(d)) = (dup(r.mmio), dup(dma)) else {
            sys::handle_close(dma);
            return fail(self, Status::Failed);
        };
        r.claim = Some((key, dma));
        let reply = devmgr::encode_claim_rng_reply(
            txid,
            Status::Ok,
            &devmgr::RngClaim {
                flags: 0,
                dma_pages: RNG_DMA_PAGES as u32,
            },
        );
        let _ = self.core.send_handles(key, reply, alloc::vec![m, d]);
    }

    /// A connection ended: if it held a network claim, release it.
    fn drop_claims_of(&mut self, key: u64) {
        for r in &mut self.rngs {
            if r.claim.as_ref().is_some_and(|(k, _)| *k == key) {
                r.release();
            }
        }
        for n in &mut self.nets {
            if n.claim.as_ref().is_some_and(|(k, _)| *k == key) {
                n.release();
            }
        }
    }
}

#[no_mangle]
pub fn app_main() -> i32 {
    let env = match startup::read() {
        Ok(e) => e,
        Err(startup::EnvError::NoMessage) => return EXIT_NO_STARTUP,
        Err(_) => return EXIT_BAD_STARTUP,
    };
    let (Some(res), Some(bi), Some(chan)) = (
        env.handle(Role::Resource),
        env.handle(Role::BootImage),
        env.handle(Role::ServiceDevmgr),
    ) else {
        return 3;
    };
    let me = sys::process_self().a1 as u32;
    let Some(blob) = read_dtb(res, me) else {
        return 4;
    };
    let Ok(nodes) = kcore::fdt::parse(&blob) else {
        return 5;
    };
    // every virtio slot that really is a block device, lowest address first (the device tree lists the
    // slots in no promised order)
    let mut blocks: Vec<Block> = Vec::new();
    let mut nets: Vec<Net> = Vec::new();
    let mut rngs: Vec<Rng> = Vec::new();
    let mut slots = kcore::fdt::virtio_mmio(&nodes);
    slots.sort_by_key(|s| s.base);
    for slot in slots {
        if slot.size != 4096 {
            continue;
        }
        let va = mmio_va(blocks.len() + nets.len() + rngs.len());
        // the device's interrupt (the AIA is required, PL6); a source the kernel will not give (taken already) leaves the device without one
        let irq_of = || {
            let i = sys::interrupt_create(res, slot.irq);
            (i.status == 0).then_some(i.a1 as u32)
        };
        if let Some(mmio) = probe(res, me, slot.base, va, ID_BLOCK) {
            let dma = sys::vmo_create_dma(res, 4);
            if dma.status != 0 {
                return 6;
            }
            blocks.push(Block {
                va,
                mmio,
                dma: dma.a1 as u32,
                irq: irq_of(),
                driver: None,
            });
        } else if let Some(mmio) = probe(res, me, slot.base, va, ID_NET) {
            nets.push(Net {
                va,
                mmio,
                irq: irq_of(),
                claim: None,
            });
        } else if let Some(mmio) = probe(res, me, slot.base, va, ID_RNG) {
            rngs.push(Rng {
                va,
                mmio,
                claim: None,
            });
        }
    }
    // the real-time clocks (a goldfish RTC on QEMU `virt`): a VMO of each one's register page, kept for claims
    let mut rtcs: Vec<u32> = Vec::new();
    for n in nodes
        .iter()
        .filter(|n| n.is_compatible("google,goldfish-rtc"))
    {
        if let Some((base, size)) = n.reg() {
            if size >= 4096 && base % 4096 == 0 {
                let v = sys::vmo_create_physical(res, base, 4096);
                if v.status == 0 {
                    rtcs.push(v.a1 as u32);
                }
            }
        }
    }
    let image = blkd_image(bi);
    let p = sys::port_create();
    if p.status != 0 {
        return 7;
    }
    let mut mgr = DevMgr {
        core: Core::new(
            p.a1 as u32,
            POOL,
            rt::abi::limits::MAX_MSG_BYTES,
            rt::abi::limits::MAX_MSG_HANDLES,
            HOLD_TIMEOUT,
        ),
        res,
        blocks,
        nets,
        rngs,
        rtcs,
        image,
    };
    match mgr.core.add_conn(chan, svc::ROOT, 0) {
        Some(k) => {
            if let Some(c) = mgr.core.conn_mut(k) {
                *c = k;
            }
        }
        None => return 3,
    }
    let code = service::run(&mut mgr);
    for n in &mut mgr.nets {
        n.release();
    }
    for r in &mut mgr.rngs {
        r.release();
    }
    let blocks = &mut mgr.blocks;
    // the client is gone (or the manager failed): end the driver, wait until it is really gone,
    // and only then leave the device quiet (T42: reset after the driver has stopped)
    for b in blocks.iter_mut() {
        if let Some(p) = b.driver {
            sys::process_kill(p, -1);
            let _ = sys::object_wait(p, signals::TERMINATED, sys::clock_get() + 2000);
        }
        reset(b.va);
    }
    code
}
