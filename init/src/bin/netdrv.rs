//! `netdrv`: the virtio-net driver (S8, NET-SPEC 5, as amended 2026-11-25; IOMMU11), on a `virtio-net-pci` function behind the
//! IOMMU. It moves frames between the device and the stack through the shared rings and **never maps a packet pool** into its own
//! address space (the stack owns packet memory; the driver knows only slot numbers and lengths, and the pools' I/O addresses). It
//! negotiates `VIRTIO_F_VERSION_1`, `VIRTIO_F_ACCESS_PLATFORM` (without which the device would bypass the IOMMU) and
//! `VIRTIO_NET_F_MAC`, and refuses every other feature the PCI device offers (checksum offload, GSO, mergeable receive buffers,
//! which would change the header's size, the control queue).
//!
//! Handles (startup message, all from its supervisor `netsup`, which claimed the function from `devmgr`): `NetMmio` (a VMO over the
//! BAR with the virtio structures; their layout is the second argument, `proto::devmgr::encode_claim_arg`), `DeviceDomain` (the
//! device's domain with IO_MAP and IO_UNMAP only), `NetRxDma` (DMA, READ, WRITE), `NetTxDma` (DMA and READ, **no WRITE**),
//! `NetQueueDma` (DMA, READ, WRITE, MAP), the interrupts `DeviceIrqConfig`, `DeviceIrqQueue0` (receive) and `DeviceIrqQueue1`
//! (transmit), `NetRing`, the two doorbell events and the control channel `NetCtl`. It holds no physical address, no control right
//! on its domain, no MSI-X table and no configuration space.
//!
//! **The driver maps** (IOMMU-SPEC 4.1, 9): every time it starts it maps the three DMA VMOs into its device's domain itself, the
//! queue memory and the receive pool read-write for the device and the **transmit pool read-only** for the device (its handle has
//! no WRITE, so nothing else is possible: a device can never overwrite a frame the stack is sending), and programs the queues with
//! the I/O addresses `iodomain_map` returned. A restarted driver has a new domain (its supervisor re-claimed the function), so it
//! learns the addresses afresh. On its way out it resets the device and unmaps what it mapped (IOMMU-SPEC 10.1).
//!
//! Lifecycle (5.6): reset the device, bring it up, zero the rings, bump the generation, post every receive slot, set
//! `state = DRIVER_READY` and send READY; the stack answers ATTACH{generation} and `state` becomes STACK_ATTACHED. A DETACH or
//! the control channel closing ends the driver (exit 0): the supervisor starts a fresh one, which is a re-attach with
//! `generation + 1`. A violation by the peer ends it with code 30; a device that is not usable with 20 to 24 (no restart).
//!
//! Wake sources: the device's three vectors (there is no mode without interrupts), the stack's doorbell, the control channel, and the
//! safety poll of 5.5 (every 10 ms). Device work that only the safety poll found (a timeout with work in a used ring and no vector
//! signalled) is counted (`stat::POLL_FOUND`): a quiet run must count none, or a vector is broken.
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
use proto::devmgr::{self, PciClaim};
use proto::startup::Role;
use rt::abi::signals;
use rt::abi::status::Status as Sys;
use rt::startup::{self, EXIT_BAD_STARTUP, EXIT_NO_STARTUP};
use rt::sys;
use virtio::Transport;

/// Where the BAR, the queue memory and the ring VMO are mapped in this process.
const BAR_VA: u64 = 0x600_0000;
const QUEUE_VA: u64 = 0x500_0000;
const RING_VA: u64 = 0x510_0000;
const PF_RW: u64 = 3;
/// The largest BAR this driver maps (a virtio-net-pci BAR is 16 KiB on QEMU).
const MAX_BAR: u64 = 1 << 20;
/// `iodomain_map`'s access: the device may read (1), or read and write (3).
const IO_R: u64 = 1;
const IO_RW: u64 = 3;
const PAGE: u64 = 4096;
/// The pools' and the queue memory's sizes (NET-SPEC 5.1): the claim says the same, and a handle of another size is refused.
const POOL_PAGES: u64 = (SLOTS * SLOT_SIZE) as u64 / PAGE;
const QUEUE_PAGES: u64 = 2;

/// VIRTIO_NET_F_MAC (VIRTIO 1.x 5.1.3): the one device feature asked for (the transport adds VERSION_1 and ACCESS_PLATFORM).
const F_MAC: u64 = 1 << 5;
/// How long a reset may take to read back 0 (ticks).
const RESET_MS: u64 = 1000;

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

/// The device's three interrupts, in `Drv::irqs`: the configuration change (0), receive (queue 0) and transmit (queue 1).
const IRQ_RX: usize = 1;
const IRQ_TX: usize = 2;

struct Drv {
    shm: Shm,
    t: virtio::PciTransport<Bar>,
    /// The transmit pool's I/O address (read-only for the device).
    tx_io: u64,
    irqs: [u64; 3],
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

    fn kick(&mut self, q: u16) {
        fence();
        self.t.notify(q);
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
                w64(d, self.tx_io + u64::from(e.slot) * SLOT_SIZE as u64);
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
        match r.status() {
            Some(Sys::Ok) => match r.a1 as u64 {
                1 => {
                    let _ = sys::event_signal(self.ev_me, 1 << 24 | 1 << 25, 0);
                }
                // a vector fired: acknowledge it (an MSI-X vector needs nothing quietened on the device); the work loop reads the
                // used rings next. The configuration change asks for nothing: the link is reported up from the start.
                k @ 4..=6 => {
                    if let Some(&irq) = self.irqs.get((k - 4) as usize) {
                        sys::interrupt_ack(irq);
                    }
                }
                _ => {}
            },
            Some(Sys::TimedOut) => {
                // the safety poll: device work no vector announced is counted (a quiet run must find none)
                fence();
                let rx_moved = q16(qbase(0) + USED + 2) != self.rx_seen;
                let tx_moved = q16(qbase(1) + USED + 2) != self.tx_seen;
                // each queue that moved is checked against its own vector (another queue's pending message proves nothing). The
                // device writes the used ring before it sends the message, so a timeout can fall between the two: the vector gets a
                // grace of 10 ms (a working one arrives within microseconds) before the work counts as found only by the poll
                for (moved, i) in [(rx_moved, IRQ_RX), (tx_moved, IRQ_TX)] {
                    let irq = self.irqs[i];
                    if moved
                        && sys::object_wait(irq, signals::INTERRUPT, sys::clock_get() + 10).status
                            != 0
                    {
                        self.shm.count(stat::POLL_FOUND);
                    }
                }
            }
            _ => {}
        }
    }
}

/// The device's handles and where they go.
struct Handles {
    dom: u64,
    rx: u64,
    tx: u64,
    q: u64,
}

/// The I/O addresses of the three mappings (queue memory, receive pool, transmit pool), once made.
struct Mapped {
    q: u64,
    rx: u64,
    tx: u64,
}

/// The pages of a VMO, if it is a whole number of them.
fn pages_of(vmo: u64) -> Option<u64> {
    let r = sys::vmo_get_size(vmo);
    let bytes = r.a1 as u64;
    (r.status() == Some(Sys::Ok) && bytes > 0 && bytes.is_multiple_of(PAGE)).then_some(bytes / PAGE)
}

/// Map the three DMA VMOs into the device's domain: the queue memory and the receive pool read-write for the device, the
/// transmit pool read-only (its handle has no WRITE: a read-write map would be ACCESS_DENIED). Anything mapped before a failure is
/// unmapped again.
fn map_into_domain(h: &Handles) -> Result<Mapped, i32> {
    let mut made: [(u64, u64); 3] = [(0, 0); 3];
    for (n, (vmo, pages, access)) in [
        (h.q, QUEUE_PAGES, IO_RW),
        (h.rx, POOL_PAGES, IO_RW),
        (h.tx, POOL_PAGES, IO_R),
    ]
    .into_iter()
    .enumerate()
    {
        let r = sys::iodomain_map(h.dom, vmo, 0, pages, access);
        if r.status() != Some(Sys::Ok) {
            for &(io, p) in &made[..n] {
                let _ = sys::iodomain_unmap(h.dom, io, p);
            }
            return Err(5);
        }
        made[n] = (r.a1 as u64, pages);
    }
    Ok(Mapped {
        q: made[0].0,
        rx: made[1].0,
        tx: made[2].0,
    })
}

fn unmap_from_domain(h: &Handles, m: &Mapped) {
    for (io, pages) in [(m.q, QUEUE_PAGES), (m.rx, POOL_PAGES), (m.tx, POOL_PAGES)] {
        let _ = sys::iodomain_unmap(h.dom, io, pages);
    }
}

/// Bring the device up on the queue memory at I/O address `q_io` (the virtio initialisation of VIRTIO 1.x 3.1.1, through the
/// transport): the features, the configuration vector, the two queues with their vectors, DRIVER_OK; the MAC from the device's
/// configuration. `Err` is an exit code.
fn device_init(
    t: &mut virtio::PciTransport<Bar>,
    claim: &PciClaim,
    q_io: u64,
) -> Result<[u8; 6], i32> {
    let entry = |what: u16| claim.irq(what).map(|(_, e)| e).ok_or(3);
    let (config, rx, tx) = (entry(devmgr::IRQ_CONFIG)?, entry(0)?, entry(1)?);
    let accepted = t.negotiate(F_MAC).map_err(|e| match e {
        virtio::Error::MissingFeature(_) => EXIT_NO_VERSION_1,
        _ => EXIT_FEATURES,
    })?;
    // only VERSION_1, ACCESS_PLATFORM and MAC: no checksum offload, no GSO, no mergeable buffers, no control queue
    if accepted != F_MAC | virtio::feature::REQUIRED {
        return Err(EXIT_FEATURES);
    }
    t.set_config_vector(config).map_err(|_| EXIT_QUEUE)?;
    for (q, vector) in [(0u16, rx), (1, tx)] {
        if t.queue_max(q) < QSIZE {
            return Err(EXIT_QUEUE);
        }
        let base = q_io + qbase(usize::from(q)) as u64;
        t.setup_queue(
            q,
            QSIZE,
            (base + DESC as u64, base + AVAIL as u64, base + USED as u64),
            vector,
        )
        .map_err(|_| EXIT_QUEUE)?;
    }
    t.driver_ok();
    let mut mac = [0u8; 6];
    for (i, m) in mac.iter_mut().enumerate() {
        *m = t.config_read8(i as u64).ok_or(EXIT_FEATURES)?;
    }
    Ok(mac)
}

fn reset(t: &mut virtio::PciTransport<Bar>) -> Result<(), i32> {
    let until = sys::clock_get() + RESET_MS;
    t.reset(&mut || sys::clock_get() < until)
        .map_err(|_| EXIT_RESET)
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
        Some(rx),
        Some(tx),
        Some(q),
        Some(irq_config),
        Some(irq_rx),
        Some(irq_tx),
        Some(ring),
        Some(ev_stack),
        Some(ev_me),
        Some(ctl_ch),
    ) = (
        h(Role::NetMmio),
        h(Role::DeviceDomain),
        h(Role::NetRxDma),
        h(Role::NetTxDma),
        h(Role::NetQueueDma),
        h(Role::DeviceIrqConfig),
        h(Role::DeviceIrqQueue0),
        h(Role::DeviceIrqQueue1),
        h(Role::NetRing),
        h(Role::NetEventToStack),
        h(Role::NetEventToDriver),
        h(Role::NetCtl),
    )
    else {
        return 3;
    };
    if (claim.vendor, claim.device) != (0x1af4, 0x1041) {
        return EXIT_NO_DEVICE; // not a modern virtio-net-pci function
    }
    if claim.bar_bytes == 0 || claim.bar_bytes > MAX_BAR || !claim.bar_bytes.is_multiple_of(PAGE) {
        return 3;
    }
    if pages_of(q) != Some(QUEUE_PAGES)
        || pages_of(rx) != Some(POOL_PAGES)
        || pages_of(tx) != Some(POOL_PAGES)
    {
        return 3;
    }
    let me = sys::process_self().a1 as u64;
    for (vmo, va, bytes) in [
        (bar, BAR_VA, claim.bar_bytes),
        (q, QUEUE_VA, QUEUE_PAGES * PAGE),
        (ring, RING_VA, 2 * PAGE),
    ] {
        if sys::process_map(me, vmo, va, 0, bytes, PF_RW).status != 0 {
            return 4;
        }
    }
    let mut t = transport(&claim);
    let shm = Shm {
        base: RING_VA as usize,
    };
    // the stack must stop reading the rings before anything below changes them
    netring::Mem::store(&shm, ctl::STATE, netring::state::NONE);
    // the queue memory may hold a dead driver's queues: reset the device first (the supervisor did at the re-claim, and a driver
    // starts from a known state; nothing writes the queues after it), then clean it
    if let Err(code) = reset(&mut t) {
        return code;
    }
    for q in 0..2usize {
        for i in 0..(4096 / 4) {
            w32(qbase(q) + 4 * i, 0);
        }
    }
    let handles = Handles { dom, rx, tx, q };
    // the buffers go into the domain here, at addresses the kernel chooses: the device is programmed with what came back
    let io = match map_into_domain(&handles) {
        Ok(m) => m,
        Err(code) => return code,
    };
    let code = run(
        &env,
        t,
        &claim,
        &io,
        shm,
        [irq_config, irq_rx, irq_tx],
        (ev_stack, ev_me, ctl_ch),
    );
    // a clean exit (IOMMU-SPEC 10.1 step 1): the device stopped, then the buffers out of the domain
    let mut t = transport(&claim);
    let _ = reset(&mut t);
    unmap_from_domain(&handles, &io);
    code
}

/// The transport over the BAR mapped at BAR_VA, where the claim says the virtio structures are.
fn transport(claim: &PciClaim) -> virtio::PciTransport<Bar> {
    virtio::PciTransport::new(
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
    )
}

/// The driver from bring-up to exit; the exit code.
fn run(
    env: &startup::Env,
    mut t: virtio::PciTransport<Bar>,
    claim: &PciClaim,
    io: &Mapped,
    shm: Shm,
    irqs: [u64; 3],
    (ev_stack, ev_me, ctl_ch): (u64, u64, u64),
) -> i32 {
    let mac = match device_init(&mut t, claim, io.q) {
        Ok(m) => m,
        Err(code) => return code,
    };
    // static descriptors: every receive slot is posted with its fixed buffer
    for i in 0..SLOTS {
        let d = qbase(0) + DESC + 16 * i;
        w64(d, io.rx + (i * SLOT_SIZE) as u64);
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
    for (k, &i) in irqs.iter().enumerate() {
        let _ = sys::port_bind(port, i, signals::INTERRUPT, 4 + k as u64);
    }
    let _ = sys::port_bind(port, ctl_ch, signals::READABLE | signals::PEER_CLOSED, 3);
    let mut d = Drv {
        shm,
        t,
        tx_io: io.tx,
        irqs,
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
    netring::Mem::store(&d.shm, ctl::STATE, netring::state::DRIVER_READY);
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
