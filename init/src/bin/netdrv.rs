//! `netdrv`: the virtio-net driver (S8, NET-SPEC 5). It moves frames between the device and the stack through the
//! shared rings and **never maps a packet pool** (the stack owns packet memory; the driver knows only slot numbers and
//! lengths, and the device's physical addresses). It negotiates only VERSION_1 and MAC.
//!
//! Handles (startup message): `NetMmio`, `NetRxDma`, `NetTxDma`, `NetQueueDma` (DMA right), optionally `NetIrq`,
//! `NetRing`, the two doorbell events and the control channel `NetCtl`.
//!
//! Lifecycle (5.6): reset the device, bring it up, zero the rings, bump the generation, post every receive slot, set
//! `state = DRIVER_READY` and send READY; the stack answers ATTACH{generation} and `state` becomes STACK_ATTACHED. A DETACH or
//! the control channel closing ends the driver (exit 0): the supervisor starts a fresh one, which is a re-attach with
//! `generation + 1`. A violation by the peer ends it with code 30; a device that is not usable with 20 to 24 (no restart).
#![no_std]
#![no_main]

extern crate alloc;

#[path = "../netshm.rs"]
mod netshm;

use netring::{
    ctl, msg, must_signal, stat, validate, Entry, Pool, Ring, RingId, Slot, FRAME_MAX, HDR_LEN,
    SLOTS, SLOT_SIZE,
};
use netshm::Shm;
use proto::startup::Role;
use rt::abi::signals;
use rt::abi::status::Status as Sys;
use rt::startup::{self, EXIT_BAD_STARTUP, EXIT_NO_STARTUP};
use rt::sys;

const MMIO_VA: u64 = 0x600_0000;
const QUEUE_VA: u64 = 0x500_0000;
const RING_VA: u64 = 0x510_0000;
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
const F_MAC: u32 = 1 << 5;

// virtqueue layout inside each queue's page
const QSIZE: u16 = 128;
const DESC: usize = 0;
const AVAIL: usize = 2048;
const USED: usize = 2560;
const DESC_WRITE: u16 = 2;

/// Exit codes the supervisor understands: 20 to 24 say the device cannot be used (do not restart).
const EXIT_NO_DEVICE: i32 = 20;
const EXIT_NO_VERSION_1: i32 = 21;
const EXIT_FEATURES: i32 = 22;
const EXIT_QUEUE: i32 = 23;
const EXIT_RESET: i32 = 24;
const EXIT_VIOLATION: i32 = 30;
const EXIT_CTL: i32 = 31;

/// The safety poll (5.5): the loops re-poll at least this often (ticks of 1 ms).
const POLL_TICKS: u64 = 10;

fn rd(off: usize) -> u32 {
    // SAFETY: MMIO_VA maps the device's 4 KiB register page for this process.
    unsafe { core::ptr::read_volatile((MMIO_VA as usize + off) as *const u32) }
}
fn wr(off: usize, v: u32) {
    // SAFETY: as in `rd`.
    unsafe { core::ptr::write_volatile((MMIO_VA as usize + off) as *mut u32, v) }
}
fn q16(off: usize) -> u16 {
    // SAFETY: QUEUE_VA maps the two-page queue area; callers pass offsets inside it.
    unsafe { core::ptr::read_volatile((QUEUE_VA as usize + off) as *const u16) }
}
fn q32(off: usize) -> u32 {
    // SAFETY: as in `q16`.
    unsafe { core::ptr::read_volatile((QUEUE_VA as usize + off) as *const u32) }
}
fn w16(off: usize, v: u16) {
    // SAFETY: as in `q16`.
    unsafe { core::ptr::write_volatile((QUEUE_VA as usize + off) as *mut u16, v) }
}
fn w32(off: usize, v: u32) {
    // SAFETY: as in `q16`.
    unsafe { core::ptr::write_volatile((QUEUE_VA as usize + off) as *mut u32, v) }
}
fn w64(off: usize, v: u64) {
    w32(off, v as u32);
    w32(off + 4, (v >> 32) as u32);
}
fn fence() {
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
}

/// Queue `q`'s base offset in the queue area.
fn qbase(q: usize) -> usize {
    q * 4096
}

struct Drv {
    shm: Shm,
    tx_pa: u64,
    irq: Option<u64>,
    ev_stack: u64,
    ev_me: u64,
    ctl: u64,
    port: u64,
    generation: u64,
    attached: bool,
    rx_avail: u16,
    rx_seen: u16,
    tx_avail: u16,
    tx_seen: u16,
    rx: Pool,
    tx: Pool,
    txid: u32,
    /// Negative control (`FAULT=bad-slot`, first generation only): publish a slot number out of range, once.
    fault_bad_slot: bool,
}

impl Drv {
    fn ring(&self, id: RingId) -> Ring<'_, Shm> {
        Ring::new(&self.shm, id)
    }

    fn violation(&self, why: &str) -> i32 {
        self.shm.count(stat::RING_VIOLATIONS);
        netring::Mem::store(&self.shm, ctl::STATE, netring::state::NONE);
        rt::println!("netdrv: ring violation: {why}");
        EXIT_VIOLATION
    }

    /// Hand a receive slot to the device.
    fn post_rx(&mut self, slot: u32) {
        let i = usize::from(self.rx_avail % QSIZE);
        w16(qbase(0) + AVAIL + 4 + 2 * i, slot as u16);
        fence();
        self.rx_avail = self.rx_avail.wrapping_add(1);
        w16(qbase(0) + AVAIL + 2, self.rx_avail);
    }

    fn kick(&self, q: u32) {
        fence();
        wr(QUEUE_NOTIFY, q);
    }

    /// One pass over everything that can move; `Err(code)` is an exit code.
    fn work(&mut self) -> Result<bool, i32> {
        let mut moved = false;
        let rx_used_ring = Ring::new(&self.shm, RingId::RxUsed);
        let tx_done_ring = Ring::new(&self.shm, RingId::TxDone);
        let was_empty = rx_used_ring.is_empty() && tx_done_ring.is_empty();
        let mut published = false;
        let mut posted = false;
        // the device's completed receives
        fence();
        while q16(qbase(0) + USED + 2) != self.rx_seen {
            let at = usize::from(self.rx_seen % QSIZE);
            let id = q32(qbase(0) + USED + 4 + 8 * at);
            let len = q32(qbase(0) + USED + 4 + 8 * at + 4) as usize;
            self.rx_seen = self.rx_seen.wrapping_add(1);
            moved = true;
            if id as usize >= SLOTS {
                return Err(self.violation("the device used a descriptor out of range"));
            }
            self.rx
                .mv(id, Slot::DeviceHas, Slot::DriverHas)
                .map_err(|_| self.violation("a receive slot used twice"))?;
            let frame = len.saturating_sub(HDR_LEN);
            let good = len >= HDR_LEN + 14 && frame <= FRAME_MAX;
            if !good {
                self.shm.count(stat::RX_BAD_LEN);
            }
            if self.attached && good {
                match self.ring(RingId::RxUsed).push(Entry {
                    slot: id,
                    len: frame as u16,
                }) {
                    Ok(()) => {
                        if self.fault_bad_slot {
                            // negative control: a slot number the stack must refuse
                            self.fault_bad_slot = false;
                            let _ = self.ring(RingId::RxUsed).push(Entry { slot: 200, len: 60 });
                        }
                        self.rx
                            .mv(id, Slot::DriverHas, Slot::InRing(RingId::RxUsed))
                            .map_err(|_| EXIT_VIOLATION)?;
                        self.shm.count(stat::RX_FRAMES);
                        published = true;
                        continue;
                    }
                    Err(_) => self.shm.count(stat::RX_DROPPED_RING_FULL),
                }
            }
            // not attached, or not usable: give the slot straight back to the device
            self.rx
                .mv(id, Slot::DriverHas, Slot::DeviceHas)
                .map_err(|_| EXIT_VIOLATION)?;
            self.post_rx(id);
            posted = true;
        }
        // the device's completed transmits
        while q16(qbase(1) + USED + 2) != self.tx_seen {
            let at = usize::from(self.tx_seen % QSIZE);
            let id = q32(qbase(1) + USED + 4 + 8 * at);
            self.tx_seen = self.tx_seen.wrapping_add(1);
            moved = true;
            if id as usize >= SLOTS {
                return Err(self.violation("the device completed a transmit out of range"));
            }
            self.tx
                .mv(id, Slot::DeviceHas, Slot::StackFree)
                .map_err(|_| self.violation("a transmit slot completed twice"))?;
            if self
                .ring(RingId::TxDone)
                .push(Entry { slot: id, len: 0 })
                .is_err()
            {
                return Err(self.violation("tx_done overflowed"));
            }
            published = true;
        }
        if self.attached {
            // receive slots the stack is finished with go back to the device
            loop {
                let e = match self.ring(RingId::RxFree).pop() {
                    Ok(Some(e)) => e,
                    Ok(None) => break,
                    Err(_) => return Err(self.violation("rx_free indices are nonsense")),
                };
                let e = validate(RingId::RxFree, e)
                    .map_err(|_| self.violation("a bad entry in rx_free"))?;
                self.rx
                    .mv(e.slot, Slot::InRing(RingId::RxUsed), Slot::DeviceHas)
                    .map_err(|_| self.violation("rx_free named a slot the stack did not hold"))?;
                self.post_rx(e.slot);
                posted = true;
                moved = true;
            }
            // frames the stack wants sent
            let mut sent = false;
            loop {
                let e = match self.ring(RingId::TxReady).pop() {
                    Ok(Some(e)) => e,
                    Ok(None) => break,
                    Err(_) => return Err(self.violation("tx_ready indices are nonsense")),
                };
                let e = validate(RingId::TxReady, e)
                    .map_err(|_| self.violation("a bad entry in tx_ready"))?;
                self.tx
                    .mv(e.slot, Slot::StackFree, Slot::DeviceHas)
                    .map_err(|_| self.violation("tx_ready named a slot the stack did not own"))?;
                let d = qbase(1) + DESC + 16 * e.slot as usize;
                w64(d, self.tx_pa + u64::from(e.slot) * SLOT_SIZE as u64);
                w32(d + 8, (HDR_LEN + usize::from(e.len)) as u32);
                w16(d + 12, 0);
                w16(d + 14, 0);
                let i = usize::from(self.tx_avail % QSIZE);
                w16(qbase(1) + AVAIL + 4 + 2 * i, e.slot as u16);
                fence();
                self.tx_avail = self.tx_avail.wrapping_add(1);
                w16(qbase(1) + AVAIL + 2, self.tx_avail);
                self.shm.count(stat::TX_FRAMES);
                sent = true;
                moved = true;
            }
            if sent {
                self.kick(1);
            }
        }
        if posted {
            self.kick(0);
        }
        if published && must_signal(&self.shm, ctl::NET_WAITING, was_empty) {
            let _ = sys::event_signal(self.ev_stack, 0, 1 << 24);
            self.shm.count(stat::DOORBELLS_SENT);
        } else if published {
            self.shm.count(stat::DOORBELLS_SUPPRESSED);
        }
        Ok(moved)
    }

    /// Read the control channel: ATTACH makes the stack's view current; DETACH or a closed channel ends us.
    fn control(&mut self) -> Result<(), i32> {
        loop {
            let mut b = [0u8; 64];
            let r = sys::channel_read(self.ctl, b.as_mut_ptr(), b.len(), core::ptr::null_mut(), 0);
            match r.status() {
                Some(Sys::Ok) => match msg::decode(&b[..r.a1]) {
                    Some((_, msg::Msg::Attach { generation })) if generation == self.generation => {
                        self.attached = true;
                        netring::Mem::store(&self.shm, ctl::STATE, netring::state::STACK_ATTACHED);
                    }
                    Some((_, msg::Msg::Detach)) => return Err(0),
                    _ => return Err(self.violation("an unexpected control message")),
                },
                Some(Sys::ShouldWait) => return Ok(()),
                _ => return Err(EXIT_CTL),
            }
        }
    }

    /// Sleep until the stack signals, the device interrupts, the control channel moves, or the safety poll is due.
    fn sleep(&mut self) {
        // announce, fence, re-check everything that could have moved (5.5); only then sleep
        netring::Mem::store(&self.shm, ctl::DRV_WAITING, 1);
        fence();
        let more = !self.ring(RingId::RxFree).is_empty()
            || !self.ring(RingId::TxReady).is_empty()
            || q16(qbase(0) + USED + 2) != self.rx_seen
            || q16(qbase(1) + USED + 2) != self.tx_seen;
        if more {
            netring::Mem::store(&self.shm, ctl::DRV_WAITING, 0);
            return;
        }
        let r = sys::port_wait(self.port, sys::clock_get() + POLL_TICKS);
        netring::Mem::store(&self.shm, ctl::DRV_WAITING, 0);
        if r.status() == Some(Sys::Ok) {
            match r.a1 as u64 {
                1 => {
                    let _ = sys::event_signal(self.ev_me, 1 << 24 | 1 << 25, 0);
                }
                2 => {
                    // quiet the device's line, then let the kernel unmask the source
                    wr(INTERRUPT_ACK, rd(INTERRUPT_STATUS));
                    if let Some(irq) = self.irq {
                        sys::interrupt_ack(irq);
                    }
                }
                _ => {}
            }
        }
    }
}

/// Reset the device and wait until it says it is done.
fn device_reset() -> Result<(), i32> {
    wr(STATUS, 0);
    let mut spins = 0u32;
    while rd(STATUS) != 0 {
        spins += 1;
        if spins > 1_000_000 {
            return Err(EXIT_RESET);
        }
    }
    Ok(())
}

fn device_init(queue_pa: u64) -> Result<[u8; 6], i32> {
    if rd(MAGIC) != 0x7472_6976 || rd(VERSION) != 2 || rd(DEVICE_ID) != 1 {
        return Err(EXIT_NO_DEVICE);
    }
    wr(STATUS, S_ACK);
    wr(STATUS, S_ACK | S_DRIVER);
    wr(DEVICE_FEATURES_SEL, 0);
    let low = rd(DEVICE_FEATURES);
    wr(DEVICE_FEATURES_SEL, 1);
    if rd(DEVICE_FEATURES) & 1 == 0 {
        return Err(EXIT_NO_VERSION_1);
    }
    if low & F_MAC == 0 {
        return Err(EXIT_FEATURES);
    }
    // only VERSION_1 and MAC: no checksum offload, no GSO, no mergeable buffers, no control queue
    wr(DRIVER_FEATURES_SEL, 0);
    wr(DRIVER_FEATURES, F_MAC);
    wr(DRIVER_FEATURES_SEL, 1);
    wr(DRIVER_FEATURES, 1);
    wr(STATUS, S_ACK | S_DRIVER | S_FEATURES_OK);
    if rd(STATUS) & S_FEATURES_OK == 0 {
        return Err(EXIT_FEATURES);
    }
    for q in 0..2usize {
        wr(QUEUE_SEL, q as u32);
        if rd(QUEUE_READY) != 0 || rd(QUEUE_NUM_MAX) < u32::from(QSIZE) {
            return Err(EXIT_QUEUE);
        }
        wr(QUEUE_NUM, u32::from(QSIZE));
        let base = queue_pa + qbase(q) as u64;
        for (reg, at) in [
            (QUEUE_DESC, DESC),
            (QUEUE_DRIVER, AVAIL),
            (QUEUE_DEVICE, USED),
        ] {
            let pa = base + at as u64;
            wr(reg, pa as u32);
            wr(reg + 4, (pa >> 32) as u32);
        }
        wr(QUEUE_READY, 1);
    }
    wr(STATUS, S_ACK | S_DRIVER | S_FEATURES_OK | S_DRIVER_OK);
    let mut mac = [0u8; 6];
    for (i, m) in mac.iter_mut().enumerate() {
        // config space bytes
        // SAFETY: MMIO_VA maps the register page; the config space starts at CONFIG.
        *m = unsafe { core::ptr::read_volatile((MMIO_VA as usize + CONFIG + i) as *const u8) };
    }
    Ok(mac)
}

#[no_mangle]
pub fn app_main() -> i32 {
    let env = match startup::read() {
        Ok(e) => e,
        Err(startup::EnvError::NoMessage) => return EXIT_NO_STARTUP,
        Err(_) => return EXIT_BAD_STARTUP,
    };
    let (
        Some(mmio),
        Some(rx_dma),
        Some(tx_dma),
        Some(q_dma),
        Some(ring),
        Some(ev_stack),
        Some(ev_me),
        Some(ctl_ch),
    ) = (
        env.handle(Role::NetMmio),
        env.handle(Role::NetRxDma),
        env.handle(Role::NetTxDma),
        env.handle(Role::NetQueueDma),
        env.handle(Role::NetRing),
        env.handle(Role::NetEventToStack),
        env.handle(Role::NetEventToDriver),
        env.handle(Role::NetCtl),
    )
    else {
        return 3;
    };
    let irq = env.handle(Role::NetIrq);
    let me = sys::process_self().a1 as u64;
    for (vmo, va, pages) in [
        (mmio, MMIO_VA, 1u64),
        (q_dma, QUEUE_VA, 2),
        (ring, RING_VA, 2),
    ] {
        if sys::process_map(me, vmo, va, 0, pages * 4096, PF_RW).status != 0 {
            return 4;
        }
    }
    let (rx_r, tx_r, q_r) = (
        sys::vmo_phys_addr(rx_dma),
        sys::vmo_phys_addr(tx_dma),
        sys::vmo_phys_addr(q_dma),
    );
    if rx_r.status != 0 || tx_r.status != 0 || q_r.status != 0 {
        return 4;
    }
    let (rx_pa, tx_pa, q_pa) = (rx_r.a1 as u64, tx_r.a1 as u64, q_r.a1 as u64);
    let shm = Shm {
        base: RING_VA as usize,
    };
    // the stack must stop reading the rings before anything below changes them
    netring::Mem::store(&shm, ctl::STATE, netring::state::NONE);
    // the queue memory may hold a dead driver's queues: reset the device first (nothing writes it after that), then clean it
    if rd(MAGIC) != 0x7472_6976 || rd(VERSION) != 2 || rd(DEVICE_ID) != 1 {
        return EXIT_NO_DEVICE;
    }
    if let Err(code) = device_reset() {
        return code;
    }
    for q in 0..2usize {
        for i in 0..(4096 / 4) {
            w32(qbase(q) + 4 * i, 0);
        }
    }
    let mac = match device_init(q_pa) {
        Ok(m) => m,
        Err(code) => return code,
    };
    // static descriptors: every receive slot is posted with its fixed buffer
    for i in 0..SLOTS {
        let d = qbase(0) + DESC + 16 * i;
        w64(d, rx_pa + (i * SLOT_SIZE) as u64);
        w32(d + 8, SLOT_SIZE as u32);
        w16(d + 12, DESC_WRITE);
        w16(d + 14, 0);
    }
    for id in RingId::ALL {
        Ring::new(&shm, id).reset();
    }
    // the generation is the supervisor's to give (a stack that is dying could write anything into the shared header)
    let generation = env
        .var(b"GEN")
        .and_then(|g| core::str::from_utf8(g).ok())
        .and_then(|g| g.parse::<u64>().ok())
        .unwrap_or(1);
    shm.store64(ctl::GENERATION, generation);
    shm.write_bytes(ctl::MAC, &mac);
    netring::Mem::store(&shm, ctl::LINK_UP, 1);
    netring::Mem::store(&shm, ctl::DRV_WAITING, 0);
    let p = sys::port_create();
    if p.status != 0 {
        return 5;
    }
    let port = p.a1 as u64;
    let _ = sys::port_bind(port, ev_me, 1 << 24, 1);
    if let Some(i) = irq {
        let _ = sys::port_bind(port, i, signals::INTERRUPT, 2);
    }
    let _ = sys::port_bind(port, ctl_ch, signals::READABLE | signals::PEER_CLOSED, 3);
    let mut d = Drv {
        shm,
        tx_pa,
        irq,
        ev_stack,
        ev_me,
        ctl: ctl_ch,
        port,
        generation,
        attached: false,
        rx_avail: 0,
        rx_seen: 0,
        tx_avail: 0,
        tx_seen: 0,
        rx: Pool::rx_posted(),
        tx: Pool::tx_free(),
        txid: 1,
        fault_bad_slot: env.var(b"FAULT") == Some(&b"bad-slot"[..]) && generation == 1,
    };
    for i in 0..SLOTS as u32 {
        d.post_rx(i);
    }
    d.kick(0);
    netring::Mem::store(&shm, ctl::STATE, netring::state::DRIVER_READY);
    let ready = msg::encode_ready(d.txid, generation, mac, true);
    if sys::channel_write(ctl_ch, ready.as_ptr(), ready.len(), core::ptr::null(), 0).status != 0 {
        return EXIT_CTL;
    }
    rt::println!(
        "netdrv: up, generation {generation}, mac {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0],
        mac[1],
        mac[2],
        mac[3],
        mac[4],
        mac[5]
    );
    loop {
        if let Err(code) = d.control() {
            return code;
        }
        match d.work() {
            Ok(true) => continue,
            Ok(false) => d.sleep(),
            Err(code) => return code,
        }
    }
}
