//! The virtio transport, as pure code (IOMMU8; X-DRIVER-MODEL: one shared virtio crate instead of a copy of the register code in
//! each driver). VIRTIO 1.x: the modern PCI transport's common configuration (section 4.1.4.3), feature negotiation and device
//! initialisation (3.1), notification (4.1.4.4) and the split virtqueue (2.7).
//!
//! The crate touches no memory itself. Device registers are reached through [`Regs`] (the caller maps the BAR that holds the
//! virtio structures and implements it with volatile accesses), and the queue memory through [`Mem`] (the caller's mapping of its
//! DMA VMO). Every address the device is given is an **I/O address**, which the caller learned from `iodomain_map`
//! (IOMMU-SPEC 4.3): the crate never sees a physical address.
//!
//! What the device writes (the used ring) is hostile input: an id outside the queue or not in flight is an error, never an index
//! into the driver's own state.
//!
//! The virtio-mmio register layout is deliberately not here: the mmio drivers keep their own until they are removed (IOMMU12).
#![no_std]
#![forbid(unsafe_code)]
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::panic,
        clippy::unreachable
    )
)]

extern crate alloc;

use alloc::vec::Vec;
use core::sync::atomic::{fence, Ordering};

/// Device status bits (VIRTIO 1.x, 2.1).
pub mod status {
    pub const ACKNOWLEDGE: u8 = 1;
    pub const DRIVER: u8 = 2;
    pub const DRIVER_OK: u8 = 4;
    pub const FEATURES_OK: u8 = 8;
    pub const DEVICE_NEEDS_RESET: u8 = 64;
    pub const FAILED: u8 = 128;
}

/// Feature bits this system names (VIRTIO 1.x, section 6, "Reserved feature bits").
pub mod feature {
    pub const INDIRECT_DESC: u64 = 1 << 28;
    pub const RING_EVENT_IDX: u64 = 1 << 29;
    pub const VERSION_1: u64 = 1 << 32;
    pub const ACCESS_PLATFORM: u64 = 1 << 33;
    /// What every driver of this system requires and accepts: the modern interface, and DMA through the platform's IOMMU
    /// (IOMMU-SPEC 15.1 L1: a device that does not offer `ACCESS_PLATFORM` would bypass the IOMMU, so it is refused).
    pub const REQUIRED: u64 = VERSION_1 | ACCESS_PLATFORM;
}

/// The common configuration structure's registers (VIRTIO 1.x, 4.1.4.3, `struct virtio_pci_common_cfg`): offsets from its start.
pub mod common {
    pub const DEVICE_FEATURE_SELECT: u64 = 0x00;
    pub const DEVICE_FEATURE: u64 = 0x04;
    pub const DRIVER_FEATURE_SELECT: u64 = 0x08;
    pub const DRIVER_FEATURE: u64 = 0x0c;
    pub const CONFIG_MSIX_VECTOR: u64 = 0x10;
    pub const NUM_QUEUES: u64 = 0x12;
    pub const DEVICE_STATUS: u64 = 0x14;
    pub const CONFIG_GENERATION: u64 = 0x15;
    pub const QUEUE_SELECT: u64 = 0x16;
    pub const QUEUE_SIZE: u64 = 0x18;
    pub const QUEUE_MSIX_VECTOR: u64 = 0x1a;
    pub const QUEUE_ENABLE: u64 = 0x1c;
    pub const QUEUE_NOTIFY_OFF: u64 = 0x1e;
    pub const QUEUE_DESC: u64 = 0x20;
    pub const QUEUE_DRIVER: u64 = 0x28;
    pub const QUEUE_DEVICE: u64 = 0x30;
    /// The structure's length up to `queue_device`'s end: what a driver of VIRTIO 1.0 reads.
    pub const LEN: u64 = 0x38;
}

/// "No MSI-X vector" for `config_msix_vector` and `queue_msix_vector` (VIRTIO 1.x, 4.1.5.1.2).
pub const NO_VECTOR: u16 = 0xffff;

/// Access to the registers of the BAR that holds a function's virtio structures: byte offsets from the BAR's start. The caller
/// implements it with volatile accesses to its mapping of the BAR VMO.
pub trait Regs {
    fn read8(&self, off: u64) -> u8;
    fn read16(&self, off: u64) -> u16;
    fn read32(&self, off: u64) -> u32;
    fn write8(&mut self, off: u64, v: u8);
    fn write16(&mut self, off: u64, v: u16);
    fn write32(&mut self, off: u64, v: u32);
}

/// What can go wrong in bring-up or on a queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The device did not read back status 0 after a reset in the time the caller allowed.
    ResetTimeout,
    /// The device does not offer a feature every driver requires (`VERSION_1`, `ACCESS_PLATFORM`): the bit.
    MissingFeature(u64),
    /// The device did not keep `FEATURES_OK` after the driver set it.
    FeaturesRefused,
    /// The queue does not exist (its maximum size reads 0), or is already enabled.
    QueueUnavailable(u16),
    /// A queue size that is 0, not a power of two, above 32768, or above the device's maximum.
    QueueSize(u16),
    /// The queue memory is too small for the layout, or the chain is empty or longer than the free descriptors.
    NoRoom,
    /// The device returned a used element whose id is not a chain in flight (hostile, or a bug).
    BadUsed(u32),
    /// The queue's notification address (`queue_notify_off` times the multiplier, which the device chooses) is outside the
    /// notification structure.
    NotifyOutside(u16),
    /// The device did not take an MSI-X vector: `config_msix_vector` (`queue` None) or queue `queue`'s `queue_msix_vector` read back
    /// something other than what was written (VIRTIO 1.x 4.1.5.1.2: a device that cannot map an event reads back `NO_VECTOR`).
    VectorRefused { queue: Option<u16>, vector: u16 },
}

/// Which MSI-X table entry each event of a device is mapped to (VIRTIO 1.x 4.1.5.1.2), given by the driver's caller from what its
/// supervisor granted: the configuration change, and the queues. A queue past the end of `queues` shares the last entry; with no
/// entry at all, or [`NO_VECTOR`], the event is not mapped (a polling driver).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VectorMap {
    pub config: u16,
    pub queues: Vec<u16>,
}

impl VectorMap {
    /// Nothing mapped: the driver polls.
    pub fn none() -> VectorMap {
        VectorMap {
            config: NO_VECTOR,
            queues: Vec::new(),
        }
    }

    /// The entry queue `q` is mapped to.
    pub fn queue(&self, q: u16) -> u16 {
        self.queues
            .get(usize::from(q))
            .or(self.queues.last())
            .copied()
            .unwrap_or(NO_VECTOR)
    }
}

/// The transport: what a driver does to a virtio device, whatever the bus. One implementation here, [`PciTransport`].
pub trait Transport {
    /// Write status 0 and wait until it reads back 0; `still_waiting` is asked between reads and ends the wait by returning false.
    fn reset(&mut self, still_waiting: &mut dyn FnMut() -> bool) -> Result<(), Error>;
    fn status(&self) -> u8;
    fn set_status(&mut self, s: u8);
    /// The 64 feature bits the device offers.
    fn device_features(&mut self) -> u64;
    fn set_driver_features(&mut self, f: u64);
    /// The largest size queue `q` may have (0: no such queue).
    fn queue_max(&mut self, q: u16) -> u16;
    /// Program queue `q` with `size` entries at I/O addresses `(descriptors, driver area, device area)`, map its used-buffer event to
    /// MSI-X entry `vector` ([`NO_VECTOR`]: none, the driver polls) and enable it. The vector is read back: a device that did not
    /// take it fails the bring-up ([`Error::VectorRefused`]) before the queue is enabled.
    fn setup_queue(
        &mut self,
        q: u16,
        size: u16,
        addrs: (u64, u64, u64),
        vector: u16,
    ) -> Result<(), Error>;
    /// Map the configuration-change event to MSI-X entry `vector` ([`NO_VECTOR`]: none), and read it back: `VectorRefused` if the
    /// device did not take it.
    fn set_config_vector(&mut self, vector: u16) -> Result<(), Error>;
    /// Tell the device queue `q` has new buffers.
    fn notify(&mut self, q: u16);
    /// The device-specific configuration's byte at `off`; `None` if the device has none or `off` is outside it.
    fn config_read8(&self, off: u64) -> Option<u8>;
    /// The aligned 32-bit field at `off` of the device-specific configuration, as `config_read8`.
    fn config_read32(&self, off: u64) -> Option<u32>;
    /// Whether the device has stopped and asks for a reset (`DEVICE_NEEDS_RESET`, VIRTIO 1.x 2.1.2): a polling driver checks it
    /// so that a broken device ends its wait.
    fn needs_reset(&self) -> bool {
        self.status() & status::DEVICE_NEEDS_RESET != 0
    }

    /// Device initialisation up to `FEATURES_OK` (VIRTIO 1.x, 3.1.1, steps 1 to 6), from a device just reset: acknowledge, read
    /// the offered features, require [`feature::REQUIRED`], accept `offered & (wanted | REQUIRED)`, set `FEATURES_OK` and check
    /// it stayed. On a refusal the device is left with `FAILED` set. Returns the accepted features.
    fn negotiate(&mut self, wanted: u64) -> Result<u64, Error> {
        self.set_status(status::ACKNOWLEDGE);
        self.set_status(status::ACKNOWLEDGE | status::DRIVER);
        let offered = self.device_features();
        for bit in [feature::VERSION_1, feature::ACCESS_PLATFORM] {
            if offered & bit == 0 {
                // (a driver never clears a status bit: 2.1.2)
                let s = self.status();
                self.set_status(s | status::FAILED);
                return Err(Error::MissingFeature(bit));
            }
        }
        let accepted = offered & (wanted | feature::REQUIRED);
        self.set_driver_features(accepted);
        let s = status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK;
        self.set_status(s);
        if self.status() & status::FEATURES_OK == 0 {
            let s = self.status();
            self.set_status(s | status::FAILED);
            return Err(Error::FeaturesRefused);
        }
        Ok(accepted)
    }

    /// The last step of initialisation: the device may now use its queues.
    fn driver_ok(&mut self) {
        let s = self.status();
        self.set_status(s | status::DRIVER_OK);
    }
}

/// Where the virtio structures are inside the BAR (from `pcicore::VirtioLayout`, or a claim reply).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciLocations {
    pub common: u64,
    pub notify: u64,
    /// The notification structure's length: every queue's notification address is checked against it.
    pub notify_len: u64,
    pub notify_multiplier: u32,
    pub isr: u64,
    /// The device configuration: (offset, length); every read is checked against it.
    pub device: Option<(u64, u64)>,
}

/// The modern virtio-pci transport over one BAR's registers.
pub struct PciTransport<R: Regs> {
    pub regs: R,
    pub at: PciLocations,
    /// Queue `q`'s notification offset, read when it was set up (VIRTIO 1.x 4.1.4.4: `queue_notify_off * multiplier`).
    notify_off: Vec<(u16, u64)>,
}

impl<R: Regs> PciTransport<R> {
    pub fn new(regs: R, at: PciLocations) -> PciTransport<R> {
        PciTransport {
            regs,
            at,
            notify_off: Vec::new(),
        }
    }

    fn c(&self, reg: u64) -> u64 {
        self.at.common.wrapping_add(reg)
    }

    /// The interrupt status register (reading it clears it).
    pub fn isr(&mut self) -> u8 {
        self.regs.read8(self.at.isr)
    }

    /// Where queue `q` is notified, inside the BAR: `None` if it was not set up.
    pub fn notify_address(&self, q: u16) -> Option<u64> {
        self.notify_off
            .iter()
            .find(|(n, _)| *n == q)
            .map(|(_, o)| self.at.notify.wrapping_add(*o))
    }
}

impl<R: Regs> Transport for PciTransport<R> {
    fn reset(&mut self, still_waiting: &mut dyn FnMut() -> bool) -> Result<(), Error> {
        self.regs.write8(self.c(common::DEVICE_STATUS), 0);
        loop {
            if self.regs.read8(self.c(common::DEVICE_STATUS)) == 0 {
                self.notify_off.clear();
                return Ok(());
            }
            if !still_waiting() {
                return Err(Error::ResetTimeout);
            }
        }
    }

    fn status(&self) -> u8 {
        self.regs.read8(self.c(common::DEVICE_STATUS))
    }

    fn set_status(&mut self, s: u8) {
        self.regs.write8(self.c(common::DEVICE_STATUS), s);
    }

    fn device_features(&mut self) -> u64 {
        self.regs.write32(self.c(common::DEVICE_FEATURE_SELECT), 0);
        let lo = self.regs.read32(self.c(common::DEVICE_FEATURE));
        self.regs.write32(self.c(common::DEVICE_FEATURE_SELECT), 1);
        let hi = self.regs.read32(self.c(common::DEVICE_FEATURE));
        u64::from(lo) | (u64::from(hi) << 32)
    }

    fn set_driver_features(&mut self, f: u64) {
        self.regs.write32(self.c(common::DRIVER_FEATURE_SELECT), 0);
        self.regs.write32(self.c(common::DRIVER_FEATURE), f as u32);
        self.regs.write32(self.c(common::DRIVER_FEATURE_SELECT), 1);
        self.regs
            .write32(self.c(common::DRIVER_FEATURE), (f >> 32) as u32);
    }

    fn queue_max(&mut self, q: u16) -> u16 {
        self.regs.write16(self.c(common::QUEUE_SELECT), q);
        self.regs.read16(self.c(common::QUEUE_SIZE))
    }

    fn setup_queue(
        &mut self,
        q: u16,
        size: u16,
        addrs: (u64, u64, u64),
        vector: u16,
    ) -> Result<(), Error> {
        let max = self.queue_max(q);
        if max == 0 || self.regs.read16(self.c(common::QUEUE_ENABLE)) != 0 {
            return Err(Error::QueueUnavailable(q));
        }
        if size == 0 || !size.is_power_of_two() || size > max {
            return Err(Error::QueueSize(size));
        }
        self.regs.write16(self.c(common::QUEUE_SIZE), size);
        self.regs.write16(self.c(common::QUEUE_MSIX_VECTOR), vector);
        let got = self.regs.read16(self.c(common::QUEUE_MSIX_VECTOR));
        if got != vector {
            return Err(Error::VectorRefused {
                queue: Some(q),
                vector,
            });
        }
        for (reg, a) in [
            (common::QUEUE_DESC, addrs.0),
            (common::QUEUE_DRIVER, addrs.1),
            (common::QUEUE_DEVICE, addrs.2),
        ] {
            self.regs.write32(self.c(reg), a as u32);
            self.regs
                .write32(self.c(reg).wrapping_add(4), (a >> 32) as u32);
        }
        let off = u64::from(self.regs.read16(self.c(common::QUEUE_NOTIFY_OFF)))
            .wrapping_mul(u64::from(self.at.notify_multiplier));
        // the device chose `queue_notify_off`: the 16-bit notification must land inside the structure (4.1.4.4)
        if off
            .checked_add(2)
            .is_none_or(|end| end > self.at.notify_len)
        {
            return Err(Error::NotifyOutside(q));
        }
        self.notify_off.retain(|(n, _)| *n != q);
        self.notify_off.push((q, off));
        self.regs.write16(self.c(common::QUEUE_ENABLE), 1);
        Ok(())
    }

    fn set_config_vector(&mut self, vector: u16) -> Result<(), Error> {
        self.regs
            .write16(self.c(common::CONFIG_MSIX_VECTOR), vector);
        if self.regs.read16(self.c(common::CONFIG_MSIX_VECTOR)) != vector {
            return Err(Error::VectorRefused {
                queue: None,
                vector,
            });
        }
        Ok(())
    }

    fn notify(&mut self, q: u16) {
        if let Some(a) = self.notify_address(q) {
            self.regs.write16(a, q);
        }
    }

    fn config_read8(&self, off: u64) -> Option<u8> {
        let (at, len) = self.at.device?;
        (off.checked_add(1)? <= len).then(|| self.regs.read8(at.wrapping_add(off)))
    }

    fn config_read32(&self, off: u64) -> Option<u32> {
        let (at, len) = self.at.device?;
        (off.is_multiple_of(4) && off.checked_add(4)? <= len)
            .then(|| self.regs.read32(at.wrapping_add(off)))
    }
}

/// Descriptor flags (VIRTIO 1.x, 2.7.5).
pub mod desc {
    pub const NEXT: u16 = 1;
    pub const WRITE: u16 = 2;
    pub const INDIRECT: u16 = 4;
    /// One descriptor: address (8), length (4), flags (2), next (2).
    pub const BYTES: u64 = 16;
}

/// The available ring's flag that asks the device not to interrupt (2.7.7).
pub const AVAIL_F_NO_INTERRUPT: u16 = 1;

/// Where the three parts of a split virtqueue of `size` entries sit in its memory (2.7: the descriptor table, aligned to 16, then
/// the driver area (the available ring, aligned to 2) and the device area (the used ring, aligned to 4)), with the event-index words
/// included so a later driver can turn `RING_EVENT_IDX` on without moving anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub size: u16,
    pub desc: u64,
    pub avail: u64,
    pub used: u64,
    /// Bytes from the start to the end of the used ring.
    pub bytes: u64,
}

/// The largest queue size the specification allows.
pub const MAX_QUEUE: u16 = 32768;

fn align(v: u64, a: u64) -> u64 {
    v.div_ceil(a).saturating_mul(a)
}

impl Layout {
    pub fn new(size: u16) -> Result<Layout, Error> {
        if size == 0 || !size.is_power_of_two() || size > MAX_QUEUE {
            return Err(Error::QueueSize(size));
        }
        let n = u64::from(size);
        let desc_bytes = desc::BYTES.saturating_mul(n);
        let avail = align(desc_bytes, 2);
        let avail_bytes = n.saturating_mul(2).saturating_add(6);
        let used = align(avail.saturating_add(avail_bytes), 4);
        let used_bytes = n.saturating_mul(8).saturating_add(6);
        Ok(Layout {
            size,
            desc: 0,
            avail,
            used,
            bytes: used.saturating_add(used_bytes),
        })
    }

    pub fn desc_at(&self, i: u16) -> u64 {
        self.desc.saturating_add(
            desc::BYTES.saturating_mul(u64::from(i.checked_rem(self.size).unwrap_or(0))),
        )
    }
    pub fn avail_flags(&self) -> u64 {
        self.avail
    }
    pub fn avail_idx(&self) -> u64 {
        self.avail.saturating_add(2)
    }
    /// The available ring's slot for running index `idx`.
    pub fn avail_ring(&self, idx: u16) -> u64 {
        self.avail
            .saturating_add(4)
            .saturating_add(2u64.wrapping_mul(u64::from(idx.checked_rem(self.size).unwrap_or(0))))
    }
    pub fn used_flags(&self) -> u64 {
        self.used
    }
    pub fn used_idx(&self) -> u64 {
        self.used.saturating_add(2)
    }
    /// The used ring's element for running index `idx`: id (4 bytes), then length (4 bytes).
    pub fn used_elem(&self, idx: u16) -> u64 {
        self.used
            .saturating_add(4)
            .saturating_add(8u64.wrapping_mul(u64::from(idx.checked_rem(self.size).unwrap_or(0))))
    }
}

/// The queue's memory, as the driver maps it: byte offsets from the start of the queue's area. The caller implements it with
/// volatile accesses to its mapping of the DMA VMO the device reads and writes.
pub trait Mem {
    fn read16(&self, off: u64) -> u16;
    fn read32(&self, off: u64) -> u32;
    fn write16(&mut self, off: u64, v: u16);
    fn write32(&mut self, off: u64, v: u32);
    fn write64(&mut self, off: u64, v: u64);
}

/// One buffer of a chain: its I/O address, its length, and whether the device writes it (otherwise it reads it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Buf {
    pub addr: u64,
    pub len: u32,
    pub device_writes: bool,
}

/// A split virtqueue, driver side. The free list and what is in flight are kept here, never read back from memory the device can
/// write.
pub struct SplitQueue {
    pub layout: Layout,
    /// The queue memory's I/O address (`iodomain_map`'s answer for it).
    pub io_base: u64,
    free: Vec<u16>,
    /// For each descriptor that heads a chain in flight: the chain's descriptors.
    in_flight: Vec<Option<Vec<u16>>>,
    next_avail: u16,
    last_used: u16,
}

impl SplitQueue {
    /// A queue of `size` at I/O address `io_base`, over memory of `mem_bytes` bytes (which the caller zeroed); `NoRoom` if the
    /// layout does not fit.
    pub fn new(size: u16, io_base: u64, mem_bytes: u64) -> Result<SplitQueue, Error> {
        let layout = Layout::new(size)?;
        if layout.bytes > mem_bytes {
            return Err(Error::NoRoom);
        }
        Ok(SplitQueue {
            layout,
            io_base,
            free: (0..size).rev().collect(),
            in_flight: (0..size).map(|_| None).collect(),
            next_avail: 0,
            last_used: 0,
        })
    }

    /// The three I/O addresses to give [`Transport::setup_queue`].
    pub fn addresses(&self) -> (u64, u64, u64) {
        (
            self.io_base.wrapping_add(self.layout.desc),
            self.io_base.wrapping_add(self.layout.avail),
            self.io_base.wrapping_add(self.layout.used),
        )
    }

    /// Free descriptors.
    pub fn free(&self) -> usize {
        self.free.len()
    }

    /// Write `chain` into free descriptors and make it available; the head's id is returned. The caller then notifies the device
    /// ([`Transport::notify`]). `interrupt` false sets `VIRTQ_AVAIL_F_NO_INTERRUPT` (a polling driver).
    pub fn add(&mut self, mem: &mut dyn Mem, chain: &[Buf], interrupt: bool) -> Result<u16, Error> {
        if chain.is_empty() || chain.len() > self.free.len() {
            return Err(Error::NoRoom);
        }
        let mut ids = Vec::with_capacity(chain.len());
        for _ in chain {
            ids.push(self.free.pop().ok_or(Error::NoRoom)?);
        }
        for (k, b) in chain.iter().enumerate() {
            let id = ids.get(k).copied().unwrap_or(0);
            let next = ids.get(k.wrapping_add(1)).copied();
            let mut flags = if b.device_writes { desc::WRITE } else { 0 };
            if next.is_some() {
                flags |= desc::NEXT;
            }
            let d = self.layout.desc_at(id);
            mem.write64(d, b.addr);
            mem.write32(d.wrapping_add(8), b.len);
            mem.write16(d.wrapping_add(12), flags);
            mem.write16(d.wrapping_add(14), next.unwrap_or(0));
        }
        let head = ids.first().copied().unwrap_or(0);
        mem.write16(
            self.layout.avail_flags(),
            if interrupt { 0 } else { AVAIL_F_NO_INTERRUPT },
        );
        mem.write16(self.layout.avail_ring(self.next_avail), head);
        // the descriptors and the ring slot before the index the device reads (2.7.13)
        fence(Ordering::SeqCst);
        self.next_avail = self.next_avail.wrapping_add(1);
        mem.write16(self.layout.avail_idx(), self.next_avail);
        fence(Ordering::SeqCst);
        if let Some(slot) = self.in_flight.get_mut(usize::from(head)) {
            *slot = Some(ids);
        }
        Ok(head)
    }

    /// The next used element, if the device has returned one: (the head's id, the length the device wrote). Its descriptors are
    /// free again. An id that is not a chain in flight is [`Error::BadUsed`] (and the element is consumed).
    pub fn take_used(&mut self, mem: &dyn Mem) -> Result<Option<(u16, u32)>, Error> {
        let idx = mem.read16(self.layout.used_idx());
        if idx == self.last_used {
            return Ok(None);
        }
        // the element after the index that says it is there
        fence(Ordering::SeqCst);
        let e = self.layout.used_elem(self.last_used);
        let id = mem.read32(e);
        let len = mem.read32(e.wrapping_add(4));
        self.last_used = self.last_used.wrapping_add(1);
        let chain = u16::try_from(id)
            .ok()
            .and_then(|i| self.in_flight.get_mut(usize::from(i)))
            .and_then(Option::take)
            .ok_or(Error::BadUsed(id))?;
        self.free.extend(chain.iter().rev());
        Ok(Some((id as u16, len)))
    }
}

/// The block device (VIRTIO 1.x, 5.2): a request's header and status, and the sector size.
pub mod blk {
    /// The device id (5.2.1).
    pub const DEVICE_ID: u16 = 2;
    pub const SECTOR: u32 = 512;
    pub const T_IN: u32 = 0;
    pub const T_OUT: u32 = 1;
    /// The header: type (4), reserved (4), sector (8).
    pub const HEADER_BYTES: u32 = 16;
    pub const S_OK: u8 = 0;
    pub const S_IOERR: u8 = 1;
    pub const S_UNSUPP: u8 = 2;
    /// The device configuration's `capacity` (in sectors), at offset 0.
    pub const CONFIG_CAPACITY: u64 = 0;

    /// The 16 bytes of a request header.
    pub fn header(kind: u32, sector: u64) -> [u8; 16] {
        let mut h = [0u8; 16];
        for (d, s) in h.iter_mut().zip(
            kind.to_le_bytes()
                .iter()
                .chain([0u8; 4].iter())
                .chain(sector.to_le_bytes().iter()),
        ) {
            *d = *s;
        }
        h
    }
}
