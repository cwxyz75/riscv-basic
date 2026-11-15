//! `rngd`: the random service (S9 step 5). It claims the virtio entropy device from the device manager, seeds a generator
//! with fast key erasure (`rngcore`) from what the device returns, reseeds it from the device from time to time, and serves
//! `/svc/random` (`proto::random`). Until it has 32 bytes from the device it answers `NOT_SEEDED`: it never makes bytes up.
//!
//! Handles: `ServiceDevmgr` (to claim the device), `ServiceRegistry` (a connection with PUBLISH, to publish `random`).
#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use proto::devmgr;
use proto::random as pr;
use proto::startup::Role;
use rngcore::Drbg;
use rt::abi::status::Status as Sys;
use rt::service::{self, Core, Service};
use rt::startup::{self, EXIT_BAD_STARTUP, EXIT_NO_STARTUP};
use rt::sys;
use svc::Limits;

const MMIO_VA: u64 = 0x600_0000;
const DMA_VA: u64 = 0x500_0000;
const PF_RW: u64 = 3;

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
const STATUS: usize = 0x070;
const QUEUE_DESC: usize = 0x080;
const QUEUE_DRIVER: usize = 0x090;
const QUEUE_DEVICE: usize = 0x0a0;

const S_ACK: u32 = 1;
const S_DRIVER: u32 = 2;
const S_DRIVER_OK: u32 = 4;
const S_FEATURES_OK: u32 = 8;

const QUEUE_SIZE: u16 = 4;
const DESC: usize = 0;
const AVAIL: usize = 128;
const USED: usize = 256;
/// The bytes the device fills: the second page.
const DATA: usize = 4096;
/// How much one device read asks for.
const CHUNK: usize = 64;
/// Reseed from the device after this many bytes have been served.
const RESEED_AFTER: u64 = 1 << 20;
const TIMEOUT: u64 = 2000;

fn rd(off: usize) -> u32 {
    // SAFETY: MMIO_VA maps the device's register page for this process (`app_main`).
    unsafe { core::ptr::read_volatile((MMIO_VA as usize + off) as *const u32) }
}

fn wr(off: usize, v: u32) {
    // SAFETY: as in `rd`.
    unsafe { core::ptr::write_volatile((MMIO_VA as usize + off) as *mut u32, v) }
}

fn dma_write(off: usize, b: &[u8]) {
    for (i, x) in b.iter().enumerate() {
        // SAFETY: DMA_VA maps the two DMA pages; callers stay inside them.
        unsafe { core::ptr::write_volatile((DMA_VA as usize + off + i) as *mut u8, *x) };
    }
}

fn dma_read(off: usize, out: &mut [u8]) {
    for (i, x) in out.iter_mut().enumerate() {
        // SAFETY: as in `dma_write`.
        *x = unsafe { core::ptr::read_volatile((DMA_VA as usize + off + i) as *const u8) };
    }
}

fn fence() {
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
}

struct Device {
    pa: u64,
    avail_idx: u16,
    last_used: u16,
}

impl Device {
    fn init(&mut self) -> Result<(), i32> {
        if rd(MAGIC) != 0x7472_6976 || rd(VERSION) != 2 || rd(DEVICE_ID) != 4 {
            return Err(20);
        }
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
        wr(DEVICE_FEATURES_SEL, 1);
        if rd(DEVICE_FEATURES) & 1 == 0 {
            return Err(21);
        }
        wr(DRIVER_FEATURES_SEL, 0);
        wr(DRIVER_FEATURES, 0);
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
        Ok(())
    }

    /// Ask the device for up to `CHUNK` bytes; the count it actually wrote, or `None` if it never answered.
    fn read(&mut self, out: &mut [u8; CHUNK]) -> Option<usize> {
        // one device-writable descriptor
        let mut d = [0u8; 16];
        d[..8].copy_from_slice(&(self.pa + DATA as u64).to_le_bytes());
        d[8..12].copy_from_slice(&(CHUNK as u32).to_le_bytes());
        d[12..14].copy_from_slice(&2u16.to_le_bytes()); // VIRTQ_DESC_F_WRITE
        dma_write(DESC, &d);
        let slot = (self.avail_idx % QUEUE_SIZE) as usize;
        dma_write(AVAIL + 4 + 2 * slot, &0u16.to_le_bytes());
        fence();
        self.avail_idx = self.avail_idx.wrapping_add(1);
        dma_write(AVAIL + 2, &self.avail_idx.to_le_bytes());
        fence();
        wr(QUEUE_NOTIFY, 0);
        let deadline = sys::clock_get() + TIMEOUT;
        loop {
            fence();
            let mut idx = [0u8; 2];
            dma_read(USED + 2, &mut idx);
            if u16::from_le_bytes(idx) != self.last_used {
                break;
            }
            if sys::clock_get() >= deadline {
                return None;
            }
            nap();
        }
        // the used element: id u32, len u32
        let slot = (self.last_used % QUEUE_SIZE) as usize;
        let mut e = [0u8; 8];
        dma_read(USED + 4 + 8 * slot, &mut e);
        self.last_used = self.last_used.wrapping_add(1);
        let n = (u32::from_le_bytes([e[4], e[5], e[6], e[7]]) as usize).min(CHUNK);
        dma_read(DATA, &mut out[..n]);
        // what the device wrote is not kept in the DMA page
        dma_write(DATA, &[0u8; CHUNK]);
        Some(n)
    }
}

fn nap() {
    rt::io::nap(1);
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
            Some(n) if n > 0 => {
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

    fn request(&mut self, key: u64, msg: &[u8], hs: &[u32]) {
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
        if self.served >= RESEED_AFTER {
            self.pull();
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
    let mut dev = None;
    let req = devmgr::encode_claim_rng(1, 0);
    if sys::channel_write(devmgr_ch, req.as_ptr(), req.len(), core::ptr::null(), 0).status == 0 {
        let w = sys::object_wait(
            devmgr_ch,
            rt::abi::signals::READABLE,
            sys::clock_get() + 3000,
        );
        if w.status == 0 {
            let mut b = [0u8; 32];
            let mut hs = [0u32; 4];
            let r = sys::channel_read(
                devmgr_ch,
                b.as_mut_ptr(),
                b.len(),
                hs.as_mut_ptr(),
                hs.len(),
            );
            if r.status == 0 {
                if let Some((_, devmgr::Status::Ok, Some(c))) =
                    devmgr::decode_claim_rng_reply(&b[..r.a1])
                {
                    if r.a2 == devmgr::RNG_CLAIM_HANDLES {
                        let me = sys::process_self().a1 as u32;
                        let pages = u64::from(c.dma_pages) * 4096;
                        let pa = sys::vmo_phys_addr(hs[1]);
                        if sys::process_map(me, hs[0], MMIO_VA, 0, 4096, PF_RW).status == 0
                            && sys::process_map(me, hs[1], DMA_VA, 0, pages, PF_RW).status == 0
                            && pa.status == 0
                        {
                            let mut d = Device {
                                pa: pa.a1 as u64,
                                avail_idx: 0,
                                last_used: 0,
                            };
                            if d.init().is_ok() {
                                dev = Some(d);
                            }
                        }
                    }
                }
            }
        }
    }
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
            p.a1 as u32,
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
    let (mine, theirs) = (c.a1 as u32, c.a2 as u32);
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
