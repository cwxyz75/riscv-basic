//! `blkd`: the virtio-blk driver (S6b, S6-SPEC section 4), a user-mode server for the block
//! protocol (`proto::blk`).
//!
//! Authority, all of it handed over in the startup message: the device's virtio-mmio slot
//! (`BlkMmio`, a physical VMO), one DMA buffer (`BlkDma`, with the DMA right), optionally the
//! slot's interrupt (`BlkIrq`: absent on machines without a PLIC, then it polls), and the channel
//! it serves (`ServiceBlk`). It never owns the memory: its supervisor keeps its own handles to the
//! DMA buffer and the MMIO slot, so killing the driver frees nothing the device may still write
//! to; the supervisor resets the device before it hands the buffer to the next driver (TIDY-UP T42).
//!
//! One request in flight. A device that does not answer within two seconds gets TIMEOUT back to
//! the client and the driver exits, so a supervisor can restart it.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use proto::blk::{self, Op, Status};
use proto::startup::Role;
use rt::abi::signals;
use rt::abi::status::Status as Sys;
use rt::startup::{self, EXIT_BAD_STARTUP, EXIT_NO_STARTUP};
use rt::sys;

const MMIO_VA: u64 = 0x600_0000;
const DMA_VA: u64 = 0x500_0000;
const PF_RW: u64 = 3;

// virtio-mmio registers (modern transport, version 2)
const MAGIC: usize = 0x000;
const VERSION: usize = 0x004;
const DEVICE_ID: usize = 0x008;
const DEVICE_FEATURES: usize = 0x010;
const DEVICE_FEATURES_SEL: usize = 0x014;
const DRIVER_FEATURES: usize = 0x020;
const DRIVER_FEATURES_SEL: usize = 0x024;
const QUEUE_SEL: usize = 0x030;
const QUEUE_NUM_MAX: usize = 0x034;
const QUEUE_NUM: usize = 0x038;
const QUEUE_READY: usize = 0x044;
const QUEUE_NOTIFY: usize = 0x050;
const INTERRUPT_STATUS: usize = 0x060;
const INTERRUPT_ACK: usize = 0x064;
const STATUS: usize = 0x070;
const QUEUE_DESC: usize = 0x080;
const QUEUE_DRIVER: usize = 0x090;
const QUEUE_DEVICE: usize = 0x0a0;
const CONFIG: usize = 0x100;

const S_ACK: u32 = 1;
const S_DRIVER: u32 = 2;
const S_DRIVER_OK: u32 = 4;
const S_FEATURES_OK: u32 = 8;

const F_RO: u32 = 1 << 5;
const F_FLUSH: u32 = 1 << 9;

// the DMA buffer's layout (see S6-SPEC): the queue and one request header in page 0, the data
// area in the pages after it
const QUEUE_SIZE: u16 = 8;
const DESC: usize = 0;
const AVAIL: usize = 128;
const USED: usize = 256;
const HDR: usize = 512;
const STATUS_BYTE: usize = 528;
const DATA: usize = 4096;
const DMA_PAGES: u64 = 4;

const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;

/// How long the device may take (ticks).
const TIMEOUT: u64 = 2000;

struct Dev {
    pa: u64,
    irq: Option<u32>,
    /// A private event only ever waited on with a deadline: the polling mode's sleep.
    nap: u32,
    avail_idx: u16,
    last_used: u16,
    sectors: u64,
    read_only: bool,
    flush: bool,
}

fn rd(off: usize) -> u32 {
    // SAFETY: MMIO_VA maps the device's 4 KiB register page for this process (`app_main`).
    unsafe { core::ptr::read_volatile((MMIO_VA as usize + off) as *const u32) }
}

fn wr(off: usize, v: u32) {
    // SAFETY: as in `rd`.
    unsafe { core::ptr::write_volatile((MMIO_VA as usize + off) as *mut u32, v) }
}

fn dma_write(off: usize, bytes: &[u8]) {
    for (i, b) in bytes.iter().enumerate() {
        // SAFETY: DMA_VA maps the DMA buffer (DMA_PAGES pages); callers stay inside it.
        unsafe { core::ptr::write_volatile((DMA_VA as usize + off + i) as *mut u8, *b) };
    }
}

fn dma_read(off: usize, out: &mut [u8]) {
    for (i, b) in out.iter_mut().enumerate() {
        // SAFETY: as in `dma_write`.
        *b = unsafe { core::ptr::read_volatile((DMA_VA as usize + off + i) as *const u8) };
    }
}

fn dma_u16(off: usize) -> u16 {
    let mut b = [0u8; 2];
    dma_read(off, &mut b);
    u16::from_le_bytes(b)
}

fn fence() {
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
}

impl Dev {
    /// Bring the device up (the supervisor has reset it): features, one queue, DRIVER_OK.
    fn init(&mut self) -> Result<(), i32> {
        if rd(MAGIC) != 0x7472_6976 || rd(VERSION) != 2 || rd(DEVICE_ID) != 2 {
            return Err(20);
        }
        // the buffer may hold a dead driver's queue: start from a clean one
        // reset first (a supervisor normally has), and wait for the device to say it is done: only
        // then is nothing writing the buffer any more and zeroing it safe
        wr(STATUS, 0);
        let mut spins = 0u32;
        while rd(STATUS) != 0 {
            spins += 1;
            if spins > 1_000_000 {
                return Err(24);
            }
        }
        dma_write(0, &[0u8; DATA]);
        wr(STATUS, S_ACK);
        wr(STATUS, S_ACK | S_DRIVER);
        wr(DEVICE_FEATURES_SEL, 0);
        let low = rd(DEVICE_FEATURES);
        wr(DEVICE_FEATURES_SEL, 1);
        if rd(DEVICE_FEATURES) & 1 == 0 {
            return Err(21); // no VIRTIO_F_VERSION_1
        }
        self.read_only = low & F_RO != 0;
        self.flush = low & F_FLUSH != 0;
        wr(DRIVER_FEATURES_SEL, 0);
        wr(DRIVER_FEATURES, if self.flush { F_FLUSH } else { 0 });
        wr(DRIVER_FEATURES_SEL, 1);
        wr(DRIVER_FEATURES, 1);
        wr(STATUS, S_ACK | S_DRIVER | S_FEATURES_OK);
        if rd(STATUS) & S_FEATURES_OK == 0 {
            return Err(22);
        }
        wr(QUEUE_SEL, 0);
        if rd(QUEUE_READY) != 0 || rd(QUEUE_NUM_MAX) < u32::from(QUEUE_SIZE) {
            return Err(23);
        }
        wr(QUEUE_NUM, u32::from(QUEUE_SIZE));
        for (reg, at) in [
            (QUEUE_DESC, DESC),
            (QUEUE_DRIVER, AVAIL),
            (QUEUE_DEVICE, USED),
        ] {
            let pa = self.pa + at as u64;
            wr(reg, pa as u32);
            wr(reg + 4, (pa >> 32) as u32);
        }
        wr(QUEUE_READY, 1);
        wr(STATUS, S_ACK | S_DRIVER | S_FEATURES_OK | S_DRIVER_OK);
        self.sectors = u64::from(rd(CONFIG)) | u64::from(rd(CONFIG + 4)) << 32;
        Ok(())
    }

    /// Is a completed request waiting in the used ring?
    fn done(&self) -> bool {
        fence();
        dma_u16(USED + 2) != self.last_used
    }

    /// Wait for the device (interrupt or polling); false on timeout.
    fn wait(&mut self) -> bool {
        let deadline = sys::clock_get() + TIMEOUT;
        loop {
            if self.done() {
                return true;
            }
            let now = sys::clock_get();
            if now >= deadline {
                return false;
            }
            match self.irq {
                Some(irq) => {
                    let r = sys::object_wait(irq, signals::INTERRUPT, deadline);
                    if r.status() == Some(Sys::Ok) {
                        // quiet the device's line, then let the kernel unmask the source
                        // (spurious interrupts are possible and harmless: `done` decides)
                        wr(INTERRUPT_ACK, rd(INTERRUPT_STATUS));
                        sys::interrupt_ack(irq);
                    }
                }
                None => {
                    sys::object_wait(self.nap, 1 << 24, now + 1);
                }
            }
        }
    }

    /// Run one request: `kind` with `data_len` bytes of data already in (writes) or to come back
    /// (reads) at `DATA`. `Err` only for a device that never answered.
    fn run(&mut self, kind: u32, sector: u64, data_len: usize) -> Result<Status, ()> {
        let mut h = [0u8; 16];
        h[..4].copy_from_slice(&kind.to_le_bytes());
        h[8..].copy_from_slice(&sector.to_le_bytes());
        dma_write(HDR, &h);
        dma_write(STATUS_BYTE, &[0xff]);
        let d = |addr: u64, len: u32, flags: u16, next: u16| {
            let mut e = [0u8; 16];
            e[..8].copy_from_slice(&addr.to_le_bytes());
            e[8..12].copy_from_slice(&len.to_le_bytes());
            e[12..14].copy_from_slice(&flags.to_le_bytes());
            e[14..].copy_from_slice(&next.to_le_bytes());
            e
        };
        const NEXT: u16 = 1;
        const WRITE: u16 = 2;
        if data_len > 0 {
            let dir = if kind == T_IN { WRITE } else { 0 };
            dma_write(DESC, &d(self.pa + HDR as u64, 16, NEXT, 1));
            dma_write(
                DESC + 16,
                &d(self.pa + DATA as u64, data_len as u32, NEXT | dir, 2),
            );
            dma_write(DESC + 32, &d(self.pa + STATUS_BYTE as u64, 1, WRITE, 0));
        } else {
            dma_write(DESC, &d(self.pa + HDR as u64, 16, NEXT, 1));
            dma_write(DESC + 16, &d(self.pa + STATUS_BYTE as u64, 1, WRITE, 0));
        }
        let slot = (self.avail_idx % QUEUE_SIZE) as usize;
        dma_write(AVAIL + 4 + 2 * slot, &0u16.to_le_bytes());
        fence();
        self.avail_idx = self.avail_idx.wrapping_add(1);
        dma_write(AVAIL + 2, &self.avail_idx.to_le_bytes());
        fence();
        wr(QUEUE_NOTIFY, 0);
        if !self.wait() {
            return Err(());
        }
        self.last_used = self.last_used.wrapping_add(1);
        let mut st = [0xffu8];
        dma_read(STATUS_BYTE, &mut st);
        Ok(if st[0] == 0 {
            Status::Ok
        } else {
            Status::IoError
        })
    }

    fn serve(&mut self, req: &blk::Request<'_>) -> Result<(Status, Vec<u8>), ()> {
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
                let len = *count as usize * blk::SECTOR;
                let st = self.run(T_IN, *sector, len)?;
                if st != Status::Ok {
                    return Ok((st, Vec::new()));
                }
                let mut data = vec![0u8; len];
                dma_read(DATA, &mut data);
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
                dma_write(DATA, data);
                Ok((self.run(T_OUT, *sector, data.len())?, Vec::new()))
            }
        }
    }
}

fn map(process: u32, vmo: u32, va: u64, pages: u64) -> bool {
    sys::process_map(process, vmo, va, 0, pages * 4096, PF_RW).status() == Some(Sys::Ok)
}

#[no_mangle]
pub fn app_main() -> i32 {
    let env = match startup::read() {
        Ok(e) => e,
        Err(startup::EnvError::NoMessage) => return EXIT_NO_STARTUP,
        Err(_) => return EXIT_BAD_STARTUP,
    };
    let (Some(mmio), Some(dma), Some(chan)) = (
        env.handle(Role::BlkMmio),
        env.handle(Role::BlkDma),
        env.handle(Role::ServiceBlk),
    ) else {
        return 3;
    };
    let irq = env.handle(Role::BlkIrq);
    let me = sys::process_self().a1 as u32;
    if !map(me, mmio, MMIO_VA, 1) || !map(me, dma, DMA_VA, DMA_PAGES) {
        return 4;
    }
    let pa = sys::vmo_phys_addr(dma);
    if pa.status() != Some(Sys::Ok) {
        return 5;
    }
    let nap = sys::event_create();
    if nap.status != 0 {
        return 6;
    }
    let mut dev = Dev {
        pa: pa.a1 as u64,
        irq,
        nap: nap.a1 as u32,
        avail_idx: 0,
        last_used: 0,
        sectors: 0,
        read_only: false,
        flush: false,
    };
    if let Err(c) = dev.init() {
        return c;
    }
    // room for any message the kernel can queue, handles included: nothing a client sends can be
    // left stuck at the head of the queue
    let mut rx = vec![0u8; abi_max_msg()];
    let mut hs = [0u32; rt::abi::limits::MAX_MSG_HANDLES];
    loop {
        let w = sys::object_wait(chan, signals::READABLE | signals::PEER_CLOSED, u64::MAX);
        if w.status() != Some(Sys::Ok) {
            return 7;
        }
        let r = sys::channel_read(chan, rx.as_mut_ptr(), rx.len(), hs.as_mut_ptr(), hs.len());
        match r.status() {
            Some(Sys::Ok) => {}
            Some(Sys::ShouldWait) => continue,
            Some(Sys::PeerClosed) => return 0, // the client is gone and nothing is queued
            _ => return 8,
        }
        // a request carries no handles: close whatever came (authority we did not ask for)
        let nh = r.a2;
        for &h in &hs[..nh] {
            sys::handle_close(h);
        }
        let decoded = if nh == 0 {
            blk::decode_request(&rx[..r.a1])
        } else {
            Err(blk::Malformed {
                txid: rx[..r.a1]
                    .get(..4)
                    .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])),
            })
        };
        let reply = match decoded {
            Ok(req) => match dev.serve(&req) {
                Ok((st, body)) => blk::encode_reply(req.txid, st, &body),
                Err(()) => {
                    let _ = send(chan, &blk::encode_reply(req.txid, Status::Timeout, &[]));
                    return 10; // the device is not answering: end, so a supervisor restarts us
                }
            },
            Err(m) => match m.txid {
                Some(t) => blk::encode_reply(t, Status::BadRequest, &[]),
                None => continue,
            },
        };
        if send(chan, &reply).is_err() {
            // the client's queue is full or gone: drop the reply
            if sys::object_info(chan).status != 0 {
                return 9;
            }
        }
    }
}

fn abi_max_msg() -> usize {
    rt::abi::limits::MAX_MSG_BYTES
}

fn send(chan: u32, bytes: &[u8]) -> Result<(), i32> {
    let r = sys::channel_write(chan, bytes.as_ptr(), bytes.len(), core::ptr::null(), 0);
    if r.status == 0 {
        Ok(())
    } else {
        Err(r.status)
    }
}
