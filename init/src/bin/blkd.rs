//! `blkd`: the virtio-blk driver (S6b, S6-SPEC section 4), a user-mode server for the block protocol (`proto::blk`), on a
//! `virtio-blk-pci` function behind the IOMMU (IOMMU10; IOMMU-SPEC 9 and 10).
//!
//! Authority, all of it handed over in the startup message by its supervisor (`devmgr`, which claimed the function for this driver's
//! job): a VMO over the BAR that holds the virtio structures (`BlkMmio`), a duplicate of the device's domain handle with IO_MAP and
//! IO_UNMAP only (`DeviceDomain`), its queue memory and its data buffer (`BlkQueueDma`, `BlkDma`: DMA VMOs with the `DMA` right,
//! charged to its job), the interrupt of the configuration change and of the request queue (`DeviceIrqConfig`, `DeviceIrqQueue0`:
//! wait and acknowledge, nothing else), and the channel it serves (`ServiceBlk`). Its second argument is the claim
//! (`proto::devmgr::encode_claim_arg`): where the virtio structures are inside the BAR and what each interrupt is for. No address
//! travels: the driver maps its two buffers into the domain itself and programs the device with the I/O addresses `iodomain_map`
//! returns, every time it starts (a restarted driver has a new domain, whose addresses it learns afresh). It holds no control right
//! on the domain, no MSI-X table, no configuration space and no physical address (Q-IOMMU-MODEL 3, Q-IOMMU-MSI 6).
//!
//! A request completes by **interrupt** only: the used ring is read after the queue's interrupt fired (there is no polling mode,
//! PL6). One request in flight. A device that does not answer within two seconds gets TIMEOUT back to the client and the driver
//! exits, so its supervisor tears the claim down and a new driver gets the device afresh (IOMMU-SPEC 10.2).
#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use proto::blk::{self, Op, Status};
use proto::devmgr::{self, PciClaim};
use proto::startup::Role;
use rt::abi::signals;
use rt::abi::status::Status as Sys;
use rt::service::{self, Core, Service};
use rt::startup::{self, EXIT_BAD_STARTUP, EXIT_NO_STARTUP};
use rt::sys;
use virtio::Transport;

/// Where the BAR, the queue page and the data buffer are mapped in this process.
const BAR_VA: u64 = 0x600_0000;
const RINGS_VA: u64 = 0x5f0_0000;
const DMA_VA: u64 = 0x500_0000;
const PF_RW: u64 = 3;
/// The largest BAR this driver maps (a virtio-blk-pci BAR is 16 KiB on QEMU).
const MAX_BAR: u64 = 1 << 20;
/// `iodomain_map`'s access: the device may read and write.
const IO_RW: u64 = 3;

// the data buffer's layout: the request header and the status byte in page 0, the data in the pages after it
const HDR: u64 = 0;
const STATUS_BYTE: u64 = 512;
const DATA: u64 = 4096;
/// The data buffer must hold the header page and the largest request.
const DMA_PAGES_MIN: u64 = 1 + (blk::MAX_IO_SECTORS as u64 * blk::SECTOR as u64).div_ceil(4096);

const QUEUE_SIZE: u16 = 8;
const PAGE: u64 = 4096;

/// VIRTIO_BLK_F_RO and VIRTIO_BLK_F_FLUSH (VIRTIO 1.x 5.2.3).
const F_RO: u64 = 1 << 5;
const F_FLUSH: u64 = 1 << 9;
const T_FLUSH: u32 = 4;

/// How long the device may take (ticks).
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
        // SAFETY: BAR_VA maps the BAR VMO (`span` bytes, device memory) read-write for as long as the driver runs (`app_main`), and
        // `at` gives an address only when the access lies inside it and is aligned.
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

/// The queue page, mapped read-write at RINGS_VA: every offset is kept inside it.
struct Rings;

impl Rings {
    fn at(off: u64) -> u64 {
        RINGS_VA + (off % PAGE)
    }
}

impl virtio::Mem for Rings {
    fn read16(&self, off: u64) -> u16 {
        // SAFETY: `at` keeps the address inside the page mapped read-write at RINGS_VA (`app_main`); the queue layout aligns it.
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

/// The data buffer, mapped read-write at DMA_VA for `pages` pages: every access stays inside it.
fn dma_write(off: u64, bytes: &[u8], pages: u64) {
    for (i, b) in bytes.iter().enumerate() {
        let at = off + i as u64;
        if at < pages * PAGE {
            // SAFETY: DMA_VA maps the data buffer read-write for `pages` pages (`app_main`), and `at` is inside it.
            unsafe { core::ptr::write_volatile((DMA_VA + at) as *mut u8, *b) };
        }
    }
}

fn dma_read(off: u64, out: &mut [u8], pages: u64) {
    for (i, b) in out.iter_mut().enumerate() {
        let at = off + i as u64;
        *b = if at < pages * PAGE {
            // SAFETY: as `dma_write`.
            unsafe { core::ptr::read_volatile((DMA_VA + at) as *const u8) }
        } else {
            0
        };
    }
}

struct Dev {
    t: virtio::PciTransport<Bar>,
    q: virtio::SplitQueue,
    /// The data buffer's I/O address (what `iodomain_map` returned) and its pages.
    data_io: u64,
    data_pages: u64,
    config_irq: u64,
    queue_irq: u64,
    sectors: u64,
    read_only: bool,
    flush: bool,
}

impl Dev {
    /// The capacity, re-read when the device said its configuration changed (its interrupt fired since the last request).
    fn config_changed(&mut self) {
        if sys::object_signals(self.config_irq).a1 as u32 & signals::INTERRUPT != 0 {
            sys::interrupt_ack(self.config_irq);
            self.sectors = capacity(&self.t);
        }
    }

    /// Run one request: `kind` with `data_len` bytes of data already in (writes) or to come back (reads) at `DATA`. `Err` only for
    /// a device that never answered, or answered with something that is not this request.
    fn run(&mut self, kind: u32, sector: u64, data_len: u64) -> Result<Status, ()> {
        let pages = self.data_pages;
        dma_write(HDR, &virtio::blk::header(kind, sector), pages);
        dma_write(STATUS_BYTE, &[0xff], pages);
        let mut chain = vec![virtio::Buf {
            addr: self.data_io + HDR,
            len: virtio::blk::HEADER_BYTES,
            device_writes: false,
        }];
        if data_len > 0 {
            chain.push(virtio::Buf {
                addr: self.data_io + DATA,
                len: data_len as u32,
                device_writes: kind == virtio::blk::T_IN,
            });
        }
        chain.push(virtio::Buf {
            addr: self.data_io + STATUS_BYTE,
            len: 1,
            device_writes: true,
        });
        let head = self.q.add(&mut Rings, &chain, true).map_err(|_| ())?;
        self.t.notify(0);
        // the used ring is read only once the queue's interrupt fired (a spurious one, or a stale one, finds nothing and waits on)
        let deadline = sys::clock_get() + TIMEOUT;
        loop {
            let r = sys::object_wait(self.queue_irq, signals::INTERRUPT, deadline);
            if r.status() != Some(Sys::Ok) {
                return Err(());
            }
            sys::interrupt_ack(self.queue_irq);
            match self.q.take_used(&Rings) {
                Ok(Some((id, _))) if id == head => break,
                Ok(Some(_)) | Err(_) => return Err(()),
                Ok(None) => {}
            }
        }
        let mut st = [0xffu8];
        dma_read(STATUS_BYTE, &mut st, pages);
        Ok(if st[0] == virtio::blk::S_OK {
            Status::Ok
        } else {
            Status::IoError
        })
    }

    fn serve(&mut self, req: &blk::Request<'_>) -> Result<(Status, Vec<u8>), ()> {
        self.config_changed();
        match &req.op {
            Op::Info => {
                let info = blk::Info {
                    sectors: self.sectors,
                    sector_size: blk::SECTOR as u32,
                    read_only: self.read_only,
                    max_io: blk::MAX_IO_SECTORS,
                };
                Ok((Status::Ok, blk::encode_info(&info)))
            }
            Op::Flush => {
                if self.read_only {
                    return Ok((Status::ReadOnly, Vec::new()));
                }
                if !self.flush {
                    return Ok((Status::Ok, Vec::new())); // a device with no cache to flush
                }
                Ok((self.run(T_FLUSH, 0, 0)?, Vec::new()))
            }
            Op::Read { sector, count } => {
                let st = blk::check(self.sectors, self.read_only, false, *sector, *count);
                if st != Status::Ok {
                    return Ok((st, Vec::new()));
                }
                let len = u64::from(*count) * blk::SECTOR as u64;
                let st = self.run(virtio::blk::T_IN, *sector, len)?;
                if st != Status::Ok {
                    return Ok((st, Vec::new()));
                }
                let mut data = vec![0u8; len as usize];
                dma_read(DATA, &mut data, self.data_pages);
                Ok((Status::Ok, data))
            }
            Op::Write {
                sector,
                count,
                data,
            } => {
                let st = blk::check(self.sectors, self.read_only, true, *sector, *count);
                if st != Status::Ok {
                    return Ok((st, Vec::new()));
                }
                dma_write(DATA, data, self.data_pages);
                Ok((
                    self.run(virtio::blk::T_OUT, *sector, data.len() as u64)?,
                    Vec::new(),
                ))
            }
        }
    }
}

/// The device's capacity in sectors (its configuration's first field).
fn capacity(t: &virtio::PciTransport<Bar>) -> u64 {
    let lo = t.config_read32(virtio::blk::CONFIG_CAPACITY).unwrap_or(0);
    let hi = t
        .config_read32(virtio::blk::CONFIG_CAPACITY + 4)
        .unwrap_or(0);
    u64::from(lo) | (u64::from(hi) << 32)
}

fn map(process: u64, vmo: u64, va: u64, bytes: u64) -> bool {
    sys::process_map(process, vmo, va, 0, bytes, PF_RW).status() == Some(Sys::Ok)
}

/// The pages of a VMO, if it is a whole number of them.
fn pages_of(vmo: u64) -> Option<u64> {
    let r = sys::vmo_get_size(vmo);
    let bytes = r.a1 as u64;
    (r.status() == Some(Sys::Ok) && bytes > 0 && bytes.is_multiple_of(PAGE)).then_some(bytes / PAGE)
}

/// Bring the device up from its handles (see the header comment); `Err` is the exit code.
fn bring_up(
    claim: &PciClaim,
    (bar, dom, rings, data, config_irq, queue_irq): (u64, u64, u64, u64, u64, u64),
) -> Result<Dev, i32> {
    if (claim.vendor, claim.device) != (0x1af4, 0x1042) {
        return Err(20); // not a modern virtio-blk-pci function
    }
    let (Some((_, config_entry)), Some((_, queue_entry))) =
        (claim.irq(devmgr::IRQ_CONFIG), claim.irq(0))
    else {
        return Err(3);
    };
    if claim.bar_bytes == 0 || claim.bar_bytes > MAX_BAR || !claim.bar_bytes.is_multiple_of(PAGE) {
        return Err(3);
    }
    let (Some(rings_pages), Some(data_pages)) = (pages_of(rings), pages_of(data)) else {
        return Err(3);
    };
    // (the data buffer must fit below the queue page's mapping)
    if !(DMA_PAGES_MIN..=(RINGS_VA - DMA_VA) / PAGE).contains(&data_pages) {
        return Err(3);
    }
    let me = sys::process_self().a1 as u64;
    let mapped = map(me, bar, BAR_VA, claim.bar_bytes)
        && map(me, rings, RINGS_VA, PAGE)
        && map(me, data, DMA_VA, data_pages * PAGE);
    sys::handle_close(me);
    if !mapped || rings_pages < 1 {
        return Err(4);
    }
    // the buffers go into the domain here, at addresses the kernel chooses: the device is programmed with what came back
    let rings_io = sys::iodomain_map(dom, rings, 0, 1, IO_RW);
    let data_io = sys::iodomain_map(dom, data, 0, data_pages, IO_RW);
    if rings_io.status() != Some(Sys::Ok) || data_io.status() != Some(Sys::Ok) {
        return Err(5);
    }
    let (rings_io, data_io) = (rings_io.a1 as u64, data_io.a1 as u64);
    let mut t = virtio::PciTransport::new(
        Bar {
            span: claim.bar_bytes,
        },
        virtio::PciLocations {
            common: u64::from(claim.common.0),
            notify: u64::from(claim.notify.0),
            notify_len: u64::from(claim.notify.1),
            notify_multiplier: claim.notify_multiplier,
            isr: u64::from(claim.isr.0),
            device: (claim.device_cfg.1 != 0)
                .then(|| (u64::from(claim.device_cfg.0), u64::from(claim.device_cfg.1))),
        },
    );
    // the supervisor reset the device at the claim; reset again (a driver starts from a known state) and wait for it: only then is
    // nothing of an earlier driver's queue in use
    let until = sys::clock_get() + RESET_MS;
    if t.reset(&mut || sys::clock_get() < until).is_err() {
        return Err(24);
    }
    // zero the queue page: the ring indexes start at 0
    for i in 0..PAGE / 8 {
        virtio::Mem::write64(&mut Rings, 8 * i, 0);
    }
    let features = t.negotiate(F_RO | F_FLUSH).map_err(|_| 21)?;
    t.set_config_vector(config_entry).map_err(|_| 22)?;
    let q = virtio::SplitQueue::new(QUEUE_SIZE, rings_io, PAGE).map_err(|_| 23)?;
    if t.queue_max(0) < QUEUE_SIZE {
        return Err(23);
    }
    t.setup_queue(0, QUEUE_SIZE, q.addresses(), queue_entry)
        .map_err(|_| 23)?;
    t.driver_ok();
    let sectors = capacity(&t);
    Ok(Dev {
        t,
        q,
        data_io,
        data_pages,
        config_irq,
        queue_irq,
        sectors,
        read_only: features & F_RO != 0,
        flush: features & F_FLUSH != 0,
    })
}

#[no_mangle]
pub fn app_main() -> i32 {
    let env = match startup::read() {
        Ok(e) => e,
        Err(startup::EnvError::NoMessage) => return EXIT_NO_STARTUP,
        Err(_) => return EXIT_BAD_STARTUP,
    };
    let Some(claim) = env
        .startup
        .args
        .get(1)
        .and_then(|a| devmgr::decode_claim_arg(a))
    else {
        return 3;
    };
    let h = |r| env.handle(r);
    let (
        Some(bar),
        Some(dom),
        Some(rings),
        Some(data),
        Some(config_irq),
        Some(queue_irq),
        Some(chan),
    ) = (
        h(Role::BlkMmio),
        h(Role::DeviceDomain),
        h(Role::BlkQueueDma),
        h(Role::BlkDma),
        h(Role::DeviceIrqConfig),
        h(Role::DeviceIrqQueue0),
        h(Role::ServiceBlk),
    )
    else {
        return 3;
    };
    let dev = match bring_up(&claim, (bar, dom, rings, data, config_irq, queue_irq)) {
        Ok(d) => d,
        Err(c) => return c,
    };
    // room for any message the kernel can queue, handles included: nothing a client sends can be left stuck at the head of the
    // queue
    let p = sys::port_create();
    if p.status != 0 {
        return 7;
    }
    let mut server = BlkServer {
        core: Core::new(
            p.a1 as u64,
            POOL,
            rt::abi::limits::MAX_MSG_BYTES,
            rt::abi::limits::MAX_MSG_HANDLES,
            HOLD_TIMEOUT,
        ),
        dev,
    };
    if server.core.add_conn(chan, svc::ROOT, ()).is_none() {
        return 9;
    }
    service::run(&mut server)
}

/// The service's total pool: one client, with room for a supervisor's probe.
const POOL: svc::Limits = svc::Limits {
    conns: 4,
    held_bytes: 1024 * 1024,
    inflight: 1,
    stream_bytes: 0,
};
/// A reply the client has no room for is kept this long (ticks) before the connection is dropped.
const HOLD_TIMEOUT: u64 = 30_000;

struct BlkServer {
    core: Core<()>,
    dev: Dev,
}

impl Service for BlkServer {
    type Conn = ();

    fn core(&mut self) -> &mut Core<()> {
        &mut self.core
    }

    fn request(&mut self, key: u64, msg: &[u8], hs: &[u64]) {
        // a request carries no handles: close whatever came (authority we did not ask for)
        for &h in hs {
            sys::handle_close(h);
        }
        let decoded = if hs.is_empty() {
            blk::decode_request(msg)
        } else {
            Err(blk::Malformed {
                txid: msg
                    .get(..4)
                    .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])),
            })
        };
        let reply = match decoded {
            Ok(req) => match self.dev.serve(&req) {
                Ok((st, body)) => blk::encode_reply(req.txid, st, &body),
                Err(()) => {
                    let _ = self.core.send(
                        key,
                        blk::encode_reply(req.txid, Status::Timeout, &[]),
                        None,
                    );
                    // the device is not answering: end, so the supervisor tears the claim down and restarts us
                    self.core.exit_with(10);
                    return;
                }
            },
            Err(m) => match m.txid {
                Some(t) => blk::encode_reply(t, Status::BadRequest, &[]),
                None => return,
            },
        };
        let _ = self.core.send(key, reply, None);
    }

    fn closed(&mut self, _conn: ()) {}
}
