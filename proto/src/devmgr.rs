//! The device manager's protocol (S6c): a client asks for a block device and gets a channel to a
//! driver for it.
//!
//! Request: `txid u32 | op u16 | index u16`. `index` names the block device: since IOMMU10 every disk is a `virtio-blk-pci`
//! function behind the IOMMU, and the index counts the claimable block functions of bus 0 in slot order (0 is the lowest slot;
//! slice DM: the manager serves every block device it finds, one driver each). Reply: `txid u32 | status i32`, and for `Ok` the
//! message carries one handle: the client's end of the block protocol channel (`proto::blk`). For each open the manager claims the
//! function afresh (a new domain, new grants) and starts `blkd` in a job of its own; when the driver ends, however it ends, the
//! claim is released (IOMMU-SPEC 10.1, 10.2), and an open of the device meanwhile is `Busy`.

use crate::{rd_u16, rd_u32};
use alloc::vec::Vec;

pub const OP_OPEN_BLOCK: u16 = 1;
/// Connect another client: the request carries one handle, the new client's end of a channel, and the manager
/// serves it as one more connection (slice DM). Same layout as OpenBlock; `index` must be 0.
pub const OP_CONNECT: u16 = 2;
/// Claim virtio network device number `index` (S8, NET-SPEC 4.3): same request layout as `OpenBlock`. The reply is a
/// [`NetClaim`] and carries the handles in [`NET_CLAIM_HANDLES`] order. Released when the claimant's connection closes.
pub const OP_CLAIM_NET: u16 = 3;
/// A `ClaimNet` reply: `txid u32 | status i32 | flags u32 | rx_pages u32 | tx_pages u32 | queue_pages u32`.
pub const NET_CLAIM_REPLY_LEN: usize = 24;
/// Flag: the last handle is the device's interrupt.
pub const NET_FLAG_IRQ: u32 = 1;
/// The handles of a successful claim, in order: the MMIO VMO (read-write), the receive-pool DMA VMO, the
/// transmit-pool DMA VMO, the virtqueue DMA VMO (each with the DMA right) and, if `NET_FLAG_IRQ`, the interrupt.
pub const NET_CLAIM_HANDLES: usize = 4;
/// Claim virtio entropy device number `index` (S9 step 5): same request layout. The reply is a [`RngClaim`] and carries the
/// handles in [`RNG_CLAIM_HANDLES`] order (the MMIO VMO, then the DMA VMO). Released when the claimant's connection closes.
pub const OP_CLAIM_RNG: u16 = 4;
/// A `ClaimRng` reply: `txid u32 | status i32 | flags u32 | dma_pages u32`.
pub const RNG_CLAIM_REPLY_LEN: usize = 16;
pub const RNG_CLAIM_HANDLES: usize = 2;
/// Claim the real-time clock device number `index` (the wall clock, FS-SPEC section 7; a `google,goldfish-rtc` on QEMU `virt`):
/// same request layout. The reply is a plain [`Status`] reply and, for `Ok`, carries one handle: a **read-only** VMO of the
/// device's register page. Any number of clients may hold one (reading the time changes nothing).
pub const OP_CLAIM_RTC: u16 = 5;
/// List the PCI functions the manager enumerated on bus 0 (IOMMU8): request `txid u32 | op u16 | 0 u16`, no handle. The reply is
/// `txid u32 | status i32 | count u32` and `count` [`PciEntry`] records of [`PCI_ENTRY_LEN`] bytes, in requester-id order.
pub const OP_LIST_PCI: u16 = 6;
/// Claim the PCI function with requester id `rid` (IOMMU8; IOMMU-SPEC 9): request `txid u32 | op u16 | rid u16`, carrying exactly
/// one handle: the **driver's job** (a job handle with MANAGE and SET_LIMITS), which the manager limits (`job_set_io_limits`, set to
/// this function's needs) and charges for the device's domain and its DMA VMOs: **one function per driver job**, so a job that holds
/// a live claim is refused (`Busy`). The reply is a [`PciClaim`] carrying, in order: a VMO over the BAR that holds the virtio
/// structures (never the MSI-X table's BAR when the table has a BAR of its own), the driver's duplicate of the device's domain
/// handle with IO_MAP and IO_UNMAP only (and TRANSFER, so a supervisor can pass it on), one DMA VMO per entry of
/// `dma_pages`, each with the `DMA` right, and one interrupt handle per entry of `irqs` (IOMMU9: the vectors of the driver's class,
/// granted and programmed into the MSI-X table by the manager; each handle can wait and acknowledge, and TRANSFER, nothing else). A
/// claim without its vectors fails: there is no claim without interrupts. No I/O address travels: the driver maps its buffers itself
/// with `iodomain_map`. The
/// manager enables bus mastering only after the reply is sent (IOMMU-SPEC 9, step 6), then sends a second message, the plain reply
/// `txid | Ok` ([`decode_claim_pci_started`]): the driver waits for it before its device's first DMA (a virtio-pci device does not
/// fetch from a queue while bus mastering is off, and nothing fetches again when it comes on). The claim is released (bus mastering
/// off, the device reset, the domain destroyed) when the claimant's connection closes.
pub const OP_CLAIM_PCI: u16 = 7;
pub const REQUEST_LEN: usize = 8;
pub const REPLY_LEN: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum Status {
    Ok = 0,
    /// The device tree lists no block device (or none could be probed).
    NoDevice = 1,
    /// The device's driver has not ended yet: one is in use, or its client just let go and it is
    /// winding down (poll). Closing the manager's channel ends the driver too.
    Busy = 2,
    /// The driver could not be started.
    Failed = 3,
    BadRequest = 4,
    /// The manager's connection limit is reached (`Connect`).
    Quota = 5,
    /// The PCI function exists and is never claimable: no MSI-X capability (IOMMU-SPEC 8.6), a virtio device that does not offer
    /// `VIRTIO_F_ACCESS_PLATFORM` (15.1 L1), no driver class this manager knows, or a configuration space it refused.
    Refused = 6,
}

impl Status {
    pub fn from_i32(v: i32) -> Option<Status> {
        Some(match v {
            0 => Status::Ok,
            1 => Status::NoDevice,
            2 => Status::Busy,
            3 => Status::Failed,
            4 => Status::BadRequest,
            5 => Status::Quota,
            6 => Status::Refused,
            _ => return None,
        })
    }
}

/// One function of a `ListPci` reply: `rid u16 | vendor u16 | device u16 | flags u16 | class u32` (class code, subclass,
/// programming interface: `0x00CCSSPP`).
pub const PCI_ENTRY_LEN: usize = 12;
/// The most functions on bus 0 (32 devices of 8 functions).
pub const MAX_PCI_ENTRIES: usize = 256;
/// `PciEntry::flags`: the function can be claimed.
pub const PCI_CLAIMABLE: u16 = 1;
/// It has an MSI-X capability.
pub const PCI_MSIX: u16 = 2;
/// It is a modern virtio function.
pub const PCI_VIRTIO: u16 = 4;
/// A virtio function that offers `VIRTIO_F_ACCESS_PLATFORM`.
pub const PCI_ACCESS_PLATFORM: u16 = 8;
/// Its configuration space was refused (`pcicore` found it malformed): listed, never configured.
pub const PCI_REFUSED: u16 = 16;
/// It is claimed now.
pub const PCI_CLAIMED: u16 = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PciEntry {
    pub rid: u16,
    pub vendor: u16,
    pub device: u16,
    pub flags: u16,
    pub class: u32,
}

pub fn encode_list_pci(txid: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(REQUEST_LEN);
    b.extend_from_slice(&txid.to_le_bytes());
    b.extend_from_slice(&OP_LIST_PCI.to_le_bytes());
    b.extend_from_slice(&0u16.to_le_bytes());
    b
}

/// `Ok(txid)` for a well-formed `ListPci` request.
pub fn decode_list_pci(b: &[u8]) -> Result<u32, Option<u32>> {
    let txid = rd_u32(b, 0);
    if b.len() != REQUEST_LEN || rd_u16(b, 4) != Some(OP_LIST_PCI) || rd_u16(b, 6) != Some(0) {
        return Err(txid);
    }
    txid.ok_or(None)
}

/// A `ListPci` reply; `None` if there are more than [`MAX_PCI_ENTRIES`].
pub fn encode_list_pci_reply(txid: u32, entries: &[PciEntry]) -> Option<Vec<u8>> {
    if entries.len() > MAX_PCI_ENTRIES {
        return None;
    }
    let mut b = Vec::with_capacity(12 + PCI_ENTRY_LEN * entries.len());
    b.extend_from_slice(&txid.to_le_bytes());
    b.extend_from_slice(&(Status::Ok as i32).to_le_bytes());
    b.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in entries {
        for v in [e.rid, e.vendor, e.device, e.flags] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&e.class.to_le_bytes());
    }
    Some(b)
}

/// A `ListPci` reply, or the plain error reply (with no entries): an `Ok` is always the long form, an error always the short one.
pub fn decode_list_pci_reply(b: &[u8]) -> Option<(u32, Status, Vec<PciEntry>)> {
    if b.len() == REPLY_LEN {
        let (t, s) = decode_reply(b)?;
        return (s != Status::Ok).then(|| (t, s, Vec::new()));
    }
    let n = rd_u32(b, 8)? as usize;
    if n > MAX_PCI_ENTRIES || b.len() != 12 + PCI_ENTRY_LEN * n {
        return None;
    }
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let at = 12 + PCI_ENTRY_LEN * i;
        out.push(PciEntry {
            rid: rd_u16(b, at)?,
            vendor: rd_u16(b, at + 2)?,
            device: rd_u16(b, at + 4)?,
            flags: rd_u16(b, at + 6)?,
            class: rd_u32(b, at + 8)?,
        });
    }
    let status = Status::from_i32(rd_u32(b, 4)? as i32)?;
    if status != Status::Ok {
        return None;
    }
    Some((rd_u32(b, 0)?, status, out))
}

pub fn encode_claim_pci(txid: u32, rid: u16) -> Vec<u8> {
    let mut b = Vec::with_capacity(REQUEST_LEN);
    b.extend_from_slice(&txid.to_le_bytes());
    b.extend_from_slice(&OP_CLAIM_PCI.to_le_bytes());
    b.extend_from_slice(&rid.to_le_bytes());
    b
}

/// `Ok((txid, rid))` for a well-formed `ClaimPci` request.
pub fn decode_claim_pci(b: &[u8]) -> Result<(u32, u16), Option<u32>> {
    let txid = rd_u32(b, 0);
    if b.len() != REQUEST_LEN || rd_u16(b, 4) != Some(OP_CLAIM_PCI) {
        return Err(txid);
    }
    match (txid, rd_u16(b, 6)) {
        (Some(t), Some(r)) => Ok((t, r)),
        _ => Err(None),
    }
}

/// The most DMA VMOs a claim hands out.
pub const MAX_CLAIM_DMA: usize = 8;
/// The most interrupt handles a claim hands out.
pub const MAX_CLAIM_IRQS: usize = 16;
/// A `ClaimPci` reply without its lists: `txid u32 | status i32 | vendor u16 | device u16 | bar_bytes u64 | common (offset u32,
/// length u32) | notify (offset u32, length u32) | notify_multiplier u32 | isr (offset u32, length u32) | device configuration
/// (offset u32, length u32: 0 if none) | dma_count u32`, then `dma_count` page counts (u32), one per DMA VMO in handle order, then
/// `irq_count u32` and `irq_count` [`PciIrq`] records of 4 bytes (`entry u16 | what u16`), one per interrupt handle in handle order.
pub const PCI_CLAIM_FIXED_LEN: usize = 60;
/// `PciIrq::what` for the configuration-change interrupt; any other value is a queue's index.
pub const IRQ_CONFIG: u16 = 0xffff;

/// What one interrupt handle of a claim is for (VIRTIO 1.x 4.1.5.1.2): the device's configuration change, or queue `n`'s used
/// buffers. The driver maps that event to MSI-X table entry `entry` (`config_msix_vector`, `queue_msix_vector`): the entry the
/// manager programmed with this vector's pair. By the manager's rule the entry is 0 for the configuration change and `n + 1` for
/// queue `n` ([`irq_entry`]), so a role alone names it ([`crate::startup::Role::DeviceIrqConfig`] and the queue roles).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PciIrq {
    pub entry: u16,
    /// [`IRQ_CONFIG`], or the queue's index.
    pub what: u16,
}

/// The MSI-X entry of the interrupt that means `what` ([`IRQ_CONFIG`] or a queue's index).
pub fn irq_entry(what: u16) -> u16 {
    if what == IRQ_CONFIG {
        0
    } else {
        what.saturating_add(1)
    }
}

/// Interrupt records as `devmgr` makes them: each at the entry its meaning has ([`irq_entry`]), no meaning twice. Both directions
/// refuse anything else.
fn irqs_consistent(irqs: &[PciIrq]) -> bool {
    irqs.iter()
        .enumerate()
        .all(|(i, r)| r.entry == irq_entry(r.what) && !irqs[..i].iter().any(|o| o.what == r.what))
}

/// What a driver is handed with its handles (IOMMU-SPEC 9). Offsets are inside the BAR VMO.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PciClaim {
    pub vendor: u16,
    pub device: u16,
    /// The BAR VMO's size in bytes.
    pub bar_bytes: u64,
    pub common: (u32, u32),
    pub notify: (u32, u32),
    pub notify_multiplier: u32,
    pub isr: (u32, u32),
    pub device_cfg: (u32, u32),
    /// The pages of each DMA VMO, in handle order.
    pub dma_pages: Vec<u32>,
    /// What each interrupt handle is for, in handle order (after the DMA VMOs).
    pub irqs: Vec<PciIrq>,
}

impl PciClaim {
    /// The handles the reply carries: the BAR VMO, the domain, the DMA VMOs, the interrupts.
    pub fn handles(&self) -> usize {
        2 + self.dma_pages.len() + self.irqs.len()
    }

    /// The handle index of the interrupt that means `what` ([`IRQ_CONFIG`] or a queue's index), and its entry.
    pub fn irq(&self, what: u16) -> Option<(usize, u16)> {
        self.irqs
            .iter()
            .position(|i| i.what == what)
            .map(|k| (2 + self.dma_pages.len() + k, self.irqs[k].entry))
    }
}

/// `None` if the claim names more than [`MAX_CLAIM_DMA`] DMA VMOs or [`MAX_CLAIM_IRQS`] interrupts.
pub fn encode_claim_pci_reply(txid: u32, c: &PciClaim) -> Option<Vec<u8>> {
    if c.dma_pages.len() > MAX_CLAIM_DMA
        || c.irqs.len() > MAX_CLAIM_IRQS
        || !irqs_consistent(&c.irqs)
    {
        return None;
    }
    let mut b =
        Vec::with_capacity(PCI_CLAIM_FIXED_LEN + 4 * c.dma_pages.len() + 4 + 4 * c.irqs.len());
    b.extend_from_slice(&txid.to_le_bytes());
    b.extend_from_slice(&(Status::Ok as i32).to_le_bytes());
    b.extend_from_slice(&c.vendor.to_le_bytes());
    b.extend_from_slice(&c.device.to_le_bytes());
    b.extend_from_slice(&c.bar_bytes.to_le_bytes());
    for (o, l) in [c.common, c.notify] {
        b.extend_from_slice(&o.to_le_bytes());
        b.extend_from_slice(&l.to_le_bytes());
    }
    b.extend_from_slice(&c.notify_multiplier.to_le_bytes());
    for (o, l) in [c.isr, c.device_cfg] {
        b.extend_from_slice(&o.to_le_bytes());
        b.extend_from_slice(&l.to_le_bytes());
    }
    b.extend_from_slice(&(c.dma_pages.len() as u32).to_le_bytes());
    for p in &c.dma_pages {
        b.extend_from_slice(&p.to_le_bytes());
    }
    b.extend_from_slice(&(c.irqs.len() as u32).to_le_bytes());
    for i in &c.irqs {
        b.extend_from_slice(&i.entry.to_le_bytes());
        b.extend_from_slice(&i.what.to_le_bytes());
    }
    Some(b)
}

/// A `ClaimPci` reply, or the plain error reply (`None` claim): an `Ok` is always the long form, an error always the short one.
pub fn decode_claim_pci_reply(b: &[u8]) -> Option<(u32, Status, Option<PciClaim>)> {
    if b.len() == REPLY_LEN {
        let (t, s) = decode_reply(b)?;
        return (s != Status::Ok).then_some((t, s, None));
    }
    let n = rd_u32(b, 56)? as usize;
    if n > MAX_CLAIM_DMA {
        return None;
    }
    let at_irqs = PCI_CLAIM_FIXED_LEN + 4 * n;
    let m = rd_u32(b, at_irqs)? as usize;
    if m > MAX_CLAIM_IRQS || b.len() != at_irqs + 4 + 4 * m {
        return None;
    }
    let pair = |at: usize| Some((rd_u32(b, at)?, rd_u32(b, at + 4)?));
    let mut dma_pages = Vec::with_capacity(n);
    for i in 0..n {
        dma_pages.push(rd_u32(b, PCI_CLAIM_FIXED_LEN + 4 * i)?);
    }
    let mut irqs = Vec::with_capacity(m);
    for i in 0..m {
        let at = at_irqs + 4 + 4 * i;
        irqs.push(PciIrq {
            entry: rd_u16(b, at)?,
            what: rd_u16(b, at + 2)?,
        });
    }
    if !irqs_consistent(&irqs) {
        return None;
    }
    let status = Status::from_i32(rd_u32(b, 4)? as i32)?;
    if status != Status::Ok {
        return None;
    }
    Some((
        rd_u32(b, 0)?,
        status,
        Some(PciClaim {
            vendor: rd_u16(b, 8)?,
            device: rd_u16(b, 10)?,
            bar_bytes: u64::from(rd_u32(b, 12)?) | (u64::from(rd_u32(b, 16)?) << 32),
            common: pair(20)?,
            notify: pair(28)?,
            notify_multiplier: rd_u32(b, 36)?,
            isr: pair(40)?,
            device_cfg: pair(48)?,
            dma_pages,
            irqs,
        }),
    ))
}

/// The message that follows a successful `ClaimPci` reply once bus mastering is on: its `txid`, or `None` if `b` is not that.
pub fn decode_claim_pci_started(b: &[u8]) -> Option<u32> {
    match decode_reply(b)? {
        (t, Status::Ok) if b.len() == REPLY_LEN => Some(t),
        _ => None,
    }
}

/// A claim as a driver's startup argument (IOMMU10): the bytes of its `ClaimPci` reply (txid 0) in lower-case hexadecimal, since a
/// startup argument holds no NUL byte. It carries the layout of the virtio structures inside the BAR VMO, the DMA VMOs' sizes and what
/// each interrupt is for: offsets and sizes only, never an address. `None` where the reply could not be encoded.
pub fn encode_claim_arg(c: &PciClaim) -> Option<Vec<u8>> {
    let b = encode_claim_pci_reply(0, c)?;
    let hex = b"0123456789abcdef";
    Some(
        b.iter()
            .flat_map(|&x| [hex[usize::from(x >> 4)], hex[usize::from(x & 15)]])
            .collect(),
    )
}

/// The claim a driver's startup argument carries ([`encode_claim_arg`]), or `None` for anything else.
pub fn decode_claim_arg(a: &[u8]) -> Option<PciClaim> {
    if !a.len().is_multiple_of(2) {
        return None;
    }
    let nib = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let mut b = Vec::with_capacity(a.len() / 2);
    for [hi, lo] in a.as_chunks::<2>().0 {
        b.push((nib(*hi)? << 4) | nib(*lo)?);
    }
    match decode_claim_pci_reply(&b)? {
        (0, Status::Ok, Some(c)) => Some(c),
        _ => None,
    }
}

/// Ask for the first block device.
pub fn encode_open_block(txid: u32) -> Vec<u8> {
    encode_open_block_at(txid, 0)
}

/// Ask for the block device number `index` (lowest slot address first).
pub fn encode_open_block_at(txid: u32, index: u16) -> Vec<u8> {
    let mut b = Vec::with_capacity(REQUEST_LEN);
    b.extend_from_slice(&txid.to_le_bytes());
    b.extend_from_slice(&OP_OPEN_BLOCK.to_le_bytes());
    b.extend_from_slice(&index.to_le_bytes());
    b
}

/// A `Connect` request (the client's endpoint travels with it).
pub fn encode_connect(txid: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(REQUEST_LEN);
    b.extend_from_slice(&txid.to_le_bytes());
    b.extend_from_slice(&OP_CONNECT.to_le_bytes());
    b.extend_from_slice(&0u16.to_le_bytes());
    b
}

/// The operation of a request, if the message is long enough to have one.
pub fn op_of(b: &[u8]) -> Option<u16> {
    rd_u16(b, 4)
}

/// `Ok(txid)` for a well-formed `Connect` request.
pub fn decode_connect(b: &[u8]) -> Result<u32, Option<u32>> {
    let txid = rd_u32(b, 0);
    if b.len() != REQUEST_LEN || rd_u16(b, 4) != Some(OP_CONNECT) || rd_u16(b, 6) != Some(0) {
        return Err(txid);
    }
    txid.ok_or(None)
}

/// `Ok((txid, index))` for an OpenBlock request; `Err(txid if there was one)` otherwise.
pub fn decode_request(b: &[u8]) -> Result<(u32, u16), Option<u32>> {
    let txid = rd_u32(b, 0);
    if b.len() != REQUEST_LEN || rd_u16(b, 4) != Some(OP_OPEN_BLOCK) {
        return Err(txid);
    }
    match (txid, rd_u16(b, 6)) {
        (Some(t), Some(i)) => Ok((t, i)),
        _ => Err(None),
    }
}

/// Ask for network device number `index` (lowest slot address first).
pub fn encode_claim_rng(txid: u32, index: u16) -> Vec<u8> {
    let mut b = Vec::with_capacity(REQUEST_LEN);
    b.extend_from_slice(&txid.to_le_bytes());
    b.extend_from_slice(&OP_CLAIM_RNG.to_le_bytes());
    b.extend_from_slice(&index.to_le_bytes());
    b
}

pub fn decode_claim_rng(b: &[u8]) -> Result<(u32, u16), Option<u32>> {
    let txid = rd_u32(b, 0);
    if b.len() != REQUEST_LEN || rd_u16(b, 4) != Some(OP_CLAIM_RNG) {
        return Err(txid);
    }
    match (txid, rd_u16(b, 6)) {
        (Some(t), Some(i)) => Ok((t, i)),
        _ => Err(None),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RngClaim {
    pub flags: u32,
    pub dma_pages: u32,
}

pub fn encode_claim_rng_reply(txid: u32, status: Status, c: &RngClaim) -> Vec<u8> {
    let mut b = Vec::with_capacity(RNG_CLAIM_REPLY_LEN);
    b.extend_from_slice(&txid.to_le_bytes());
    b.extend_from_slice(&(status as i32).to_le_bytes());
    b.extend_from_slice(&c.flags.to_le_bytes());
    b.extend_from_slice(&c.dma_pages.to_le_bytes());
    b
}

pub fn decode_claim_rng_reply(b: &[u8]) -> Option<(u32, Status, Option<RngClaim>)> {
    if b.len() == REPLY_LEN {
        let (t, s) = decode_reply(b)?;
        return Some((t, s, None));
    }
    if b.len() != RNG_CLAIM_REPLY_LEN {
        return None;
    }
    Some((
        rd_u32(b, 0)?,
        Status::from_i32(rd_u32(b, 4)? as i32)?,
        Some(RngClaim {
            flags: rd_u32(b, 8)?,
            dma_pages: rd_u32(b, 12)?,
        }),
    ))
}

pub fn encode_claim_rtc(txid: u32, index: u16) -> Vec<u8> {
    let mut b = Vec::with_capacity(REQUEST_LEN);
    b.extend_from_slice(&txid.to_le_bytes());
    b.extend_from_slice(&OP_CLAIM_RTC.to_le_bytes());
    b.extend_from_slice(&index.to_le_bytes());
    b
}

pub fn decode_claim_rtc(b: &[u8]) -> Result<(u32, u16), Option<u32>> {
    let txid = rd_u32(b, 0);
    if b.len() != REQUEST_LEN || rd_u16(b, 4) != Some(OP_CLAIM_RTC) {
        return Err(txid);
    }
    match (txid, rd_u16(b, 6)) {
        (Some(t), Some(i)) => Ok((t, i)),
        _ => Err(None),
    }
}

pub fn encode_claim_net(txid: u32, index: u16) -> Vec<u8> {
    let mut b = Vec::with_capacity(REQUEST_LEN);
    b.extend_from_slice(&txid.to_le_bytes());
    b.extend_from_slice(&OP_CLAIM_NET.to_le_bytes());
    b.extend_from_slice(&index.to_le_bytes());
    b
}

/// `Ok((txid, index))` for a well-formed `ClaimNet` request; `Err(txid if there was one)` otherwise.
pub fn decode_claim_net(b: &[u8]) -> Result<(u32, u16), Option<u32>> {
    let txid = rd_u32(b, 0);
    if b.len() != REQUEST_LEN || rd_u16(b, 4) != Some(OP_CLAIM_NET) {
        return Err(txid);
    }
    match (txid, rd_u16(b, 6)) {
        (Some(t), Some(i)) => Ok((t, i)),
        _ => Err(None),
    }
}

/// The sizes a claim reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetClaim {
    pub flags: u32,
    pub rx_pages: u32,
    pub tx_pages: u32,
    pub queue_pages: u32,
}

pub fn encode_claim_net_reply(txid: u32, status: Status, c: &NetClaim) -> Vec<u8> {
    let mut b = Vec::with_capacity(NET_CLAIM_REPLY_LEN);
    b.extend_from_slice(&txid.to_le_bytes());
    b.extend_from_slice(&(status as i32).to_le_bytes());
    for v in [c.flags, c.rx_pages, c.tx_pages, c.queue_pages] {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b
}

/// A `ClaimNet` reply, or the plain 8-byte error reply (`Err` carries it).
pub fn decode_claim_net_reply(b: &[u8]) -> Option<(u32, Status, Option<NetClaim>)> {
    if b.len() == REPLY_LEN {
        let (t, s) = decode_reply(b)?;
        return Some((t, s, None));
    }
    if b.len() != NET_CLAIM_REPLY_LEN {
        return None;
    }
    Some((
        rd_u32(b, 0)?,
        Status::from_i32(rd_u32(b, 4)? as i32)?,
        Some(NetClaim {
            flags: rd_u32(b, 8)?,
            rx_pages: rd_u32(b, 12)?,
            tx_pages: rd_u32(b, 16)?,
            queue_pages: rd_u32(b, 20)?,
        }),
    ))
}

pub fn encode_reply(txid: u32, status: Status) -> Vec<u8> {
    let mut b = Vec::with_capacity(REPLY_LEN);
    b.extend_from_slice(&txid.to_le_bytes());
    b.extend_from_slice(&(status as i32).to_le_bytes());
    b
}

pub fn decode_reply(b: &[u8]) -> Option<(u32, Status)> {
    if b.len() != REPLY_LEN {
        return None;
    }
    Some((rd_u32(b, 0)?, Status::from_i32(rd_u32(b, 4)? as i32)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golden_and_round_trip() {
        assert_eq!(encode_open_block(7), [7, 0, 0, 0, 1, 0, 0, 0]);
        assert_eq!(decode_request(&encode_open_block(7)), Ok((7, 0)));
        assert_eq!(encode_open_block_at(7, 3), [7, 0, 0, 0, 1, 0, 3, 0]);
        assert_eq!(decode_request(&encode_open_block_at(7, 3)), Ok((7, 3)));
        assert_eq!(encode_reply(7, Status::Busy), [7, 0, 0, 0, 2, 0, 0, 0]);
        assert_eq!(
            decode_reply(&encode_reply(9, Status::NoDevice)),
            Some((9, Status::NoDevice))
        );
    }

    #[test]
    fn claim_rtc_golden_and_round_trip() {
        assert_eq!(encode_claim_rtc(7, 1), [7, 0, 0, 0, 5, 0, 1, 0]);
        assert_eq!(decode_claim_rtc(&encode_claim_rtc(7, 1)), Ok((7, 1)));
        assert_eq!(decode_claim_rtc(&encode_claim_rng(7, 1)), Err(Some(7)));
        assert_eq!(decode_claim_rng(&encode_claim_rtc(7, 1)), Err(Some(7)));
        assert_eq!(decode_claim_rtc(&[1, 2]), Err(None));
        assert_eq!(decode_claim_rtc(&[1, 0, 0, 0, 5, 0]), Err(Some(1)));
    }

    #[test]
    fn claim_rng_golden_and_round_trip() {
        assert_eq!(encode_claim_rng(7, 1), [7, 0, 0, 0, 4, 0, 1, 0]);
        assert_eq!(decode_claim_rng(&encode_claim_rng(7, 1)), Ok((7, 1)));
        assert_eq!(decode_claim_rng(&encode_claim_net(7, 1)), Err(Some(7)));
        assert_eq!(decode_claim_net(&encode_claim_rng(7, 1)), Err(Some(7)));
        assert_eq!(decode_claim_rng(&[1, 2]), Err(None));
        let c = RngClaim {
            flags: 0,
            dma_pages: 2,
        };
        let r = encode_claim_rng_reply(9, Status::Ok, &c);
        assert_eq!(r.len(), RNG_CLAIM_REPLY_LEN);
        assert_eq!(decode_claim_rng_reply(&r), Some((9, Status::Ok, Some(c))));
        assert_eq!(
            decode_claim_rng_reply(&encode_reply(9, Status::NoDevice)),
            Some((9, Status::NoDevice, None))
        );
        assert_eq!(decode_claim_rng_reply(&r[..15]), None);
    }

    #[test]
    fn claim_net_golden_and_round_trip() {
        assert_eq!(encode_claim_net(7, 1), [7, 0, 0, 0, 3, 0, 1, 0]);
        assert_eq!(decode_claim_net(&encode_claim_net(7, 1)), Ok((7, 1)));
        assert_eq!(
            decode_claim_net(&encode_open_block(7)),
            Err(Some(7)),
            "another op"
        );
        assert_eq!(
            decode_request(&encode_claim_net(7, 0)),
            Err(Some(7)),
            "OpenBlock refuses it too"
        );
        assert_eq!(decode_claim_net(&[1, 2]), Err(None));
        assert_eq!(decode_claim_net(&[5, 0, 0, 0, 3, 0, 0, 0, 0]), Err(Some(5)));
        let c = NetClaim {
            flags: NET_FLAG_IRQ,
            rx_pages: 64,
            tx_pages: 64,
            queue_pages: 2,
        };
        let r = encode_claim_net_reply(9, Status::Ok, &c);
        assert_eq!(r.len(), NET_CLAIM_REPLY_LEN);
        assert_eq!(decode_claim_net_reply(&r), Some((9, Status::Ok, Some(c))));
        assert_eq!(
            decode_claim_net_reply(&encode_reply(9, Status::NoDevice)),
            Some((9, Status::NoDevice, None)),
            "an error is the short reply"
        );
        assert_eq!(decode_claim_net_reply(&[0; 12]), None);
        assert_eq!(decode_claim_net_reply(&r[..23]), None);
    }

    #[test]
    fn malformed_input_is_an_error_never_a_panic() {
        // Connect: same layout, its own op, index 0
        assert_eq!(decode_connect(&encode_connect(9)), Ok(9));
        assert_eq!(op_of(&encode_connect(9)), Some(OP_CONNECT));
        assert_eq!(decode_connect(&encode_open_block_at(9, 0)), Err(Some(9)));
        assert_eq!(decode_request(&encode_connect(9)), Err(Some(9)));
        let mut bad = encode_connect(9);
        bad[6] = 1; // a nonzero index
        assert_eq!(decode_connect(&bad), Err(Some(9)));
        assert_eq!(decode_connect(&[]), Err(None));
        assert_eq!(Status::from_i32(5), Some(Status::Quota));
        assert_eq!(decode_request(&[]), Err(None));
        assert_eq!(decode_request(&[1, 2, 3, 4]), Err(Some(0x0403_0201)));
        assert_eq!(decode_request(&[5, 0, 0, 0, 9, 0, 0, 0]), Err(Some(5)));
        assert_eq!(decode_request(&[5, 0, 0, 0, 1, 0, 0, 0, 0]), Err(Some(5)));
        assert_eq!(decode_reply(&[0; 7]), None);
        assert_eq!(decode_reply(&[0, 0, 0, 0, 99, 0, 0, 0]), None);
    }
}
