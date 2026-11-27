//! PCI configuration space as pure code (IOMMU8; IOMMU-SPEC section 9, X-BUS). `devmgr` enumerates bus 0 of the ECAM host with
//! it, and the suite's hand-driven tests use the same code, so the two assign the same BARs to the same functions.
//!
//! **The crate never touches memory.** It is given bytes (a copy of a function's first 256 bytes of configuration space, and the
//! values its BAR registers read back while being sized) and returns what to write (the BAR addresses, the command register, the
//! MSI-X table entries and message control, the function-level reset). The caller owns every volatile access.
//!
//! Everything here parses bytes a device controls, so it is hostile-input code: every read is bounds-checked, the capability walk
//! has a step bound and a visited set, every size and offset is checked arithmetic, and a malformed function is an [`Error`],
//! never a hang or a panic.
//!
//! What it covers is X-BUS's committed part: one ECAM host, bus 0, type-0 headers (a bridge is listed and never configured), memory
//! BARs (I/O BARs are found and never assigned), MSI-X (the older MSI capability and INTx routing are not read: IOMMU-SPEC 8.6),
//! the virtio-pci capabilities (VIRTIO 1.x, section 4.1.4), and the function-level reset of the PCI Express and Advanced Features
//! capabilities.
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
use core::fmt;

/// The bytes of a function's configuration space this crate reads: the conventional space (the header and the capability list).
pub const CFG_BYTES: usize = 256;
/// One function's page of ECAM (PCI Express base specification, "Enhanced Configuration Access Mechanism").
pub const ECAM_FUNCTION_BYTES: u64 = 4096;
/// Devices on a bus, functions in a device.
pub const DEVICES: u8 = 32;
pub const FUNCTIONS: u8 = 8;

/// Header registers (PCI local bus specification 3.0, "Type 00h configuration space header").
pub mod reg {
    pub const VENDOR: u16 = 0x00;
    pub const DEVICE: u16 = 0x02;
    pub const COMMAND: u16 = 0x04;
    pub const STATUS: u16 = 0x06;
    pub const REVISION: u16 = 0x08;
    pub const CLASS: u16 = 0x09;
    pub const HEADER_TYPE: u16 = 0x0e;
    pub const BAR0: u16 = 0x10;
    pub const SUBSYSTEM_VENDOR: u16 = 0x2c;
    pub const SUBSYSTEM: u16 = 0x2e;
    pub const CAP_PTR: u16 = 0x34;
    pub const INTERRUPT_PIN: u16 = 0x3d;
}

/// Command register bits.
pub mod command {
    pub const IO: u16 = 1 << 0;
    pub const MEMORY: u16 = 1 << 1;
    pub const BUS_MASTER: u16 = 1 << 2;
    pub const INTX_DISABLE: u16 = 1 << 10;
}

/// Status register: the function has a capability list.
pub const STATUS_CAP_LIST: u16 = 1 << 4;
/// A vendor id that reads as all ones: no function at this address.
pub const NO_VENDOR: u16 = 0xffff;

/// Capability ids.
pub mod cap_id {
    pub const PCIE: u8 = 0x10;
    pub const MSIX: u8 = 0x11;
    pub const AF: u8 = 0x13;
    pub const VENDOR: u8 = 0x09;
}

/// Where the capability list may live: after the header, inside the conventional space, a doubleword apart at least.
const CAP_FIRST: u8 = 0x40;
/// The most capabilities that fit between 0x40 and 0x100 (one doubleword each, at the least): the walk's step bound.
pub const MAX_CAPS: usize = (CFG_BYTES - CAP_FIRST as usize) / 4;

/// Everything that can be wrong with a function. Each is a refusal of the function, never a panic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Fewer than [`CFG_BYTES`] bytes were given.
    Short,
    /// The capability list loops, or is longer than [`MAX_CAPS`].
    CapLoop,
    /// A capability pointer outside 0x40..0xfc, or a capability whose body runs past the conventional space.
    CapPointer(u8),
    /// A BAR's type bits name no memory type the specification defines (1 or 3).
    BarType(u8),
    /// A BAR whose size is zero, not a power of two (its writable bits are not one contiguous run) or the whole address space.
    BarSize(u8),
    /// A 64-bit BAR in the last register, with no upper half.
    Bar64AtEnd(u8),
    /// Not a type-0 header (a bridge): never configured.
    NotType0(u8),
    /// The BARs do not fit in the window.
    NoSpace,
    /// The MSI-X table or pending-bit array names a BAR that is absent or not memory.
    MsixBar(u8),
    /// The MSI-X table or pending-bit array runs past its BAR, or the two overlap, or one overlaps a virtio structure.
    MsixPlacement,
    /// A virtio capability that is too short, names no BAR, or lies outside its BAR.
    VirtioCap(u8),
    /// A virtio device without the common, notify or ISR structure, or with them in more than one BAR.
    VirtioLayout,
}

/// A function's address on its bus: bus, device, function.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Bdf {
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
}

impl Bdf {
    /// `None` if `dev` or `func` is out of range.
    pub fn new(bus: u8, dev: u8, func: u8) -> Option<Bdf> {
        (dev < DEVICES && func < FUNCTIONS).then_some(Bdf { bus, dev, func })
    }

    /// The requester id: what the IOMMU knows the function by through the host's `iommu-map` (IOMMU-SPEC 2.2; on `virt` the
    /// device id is the requester id).
    pub fn rid(self) -> u16 {
        (u16::from(self.bus) << 8) | (u16::from(self.dev & 31) << 3) | u16::from(self.func & 7)
    }

    pub fn from_rid(rid: u16) -> Bdf {
        Bdf {
            bus: (rid >> 8) as u8,
            dev: ((rid >> 3) & 31) as u8,
            func: (rid & 7) as u8,
        }
    }

    /// The function's page in the ECAM window, as an offset from the window's base.
    pub fn ecam_offset(self) -> u64 {
        (u64::from(self.bus) << 20)
            | (u64::from(self.dev & 31) << 15)
            | (u64::from(self.func & 7) << 12)
    }
}

impl fmt::Display for Bdf {
    /// `bb:dd.f`, as the IOMMU's fault line prints a device (IOMMU-SPEC 7.2).
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:02x}:{:02x}.{}", self.bus, self.dev, self.func)
    }
}

fn rd8(cfg: &[u8], off: u16) -> Result<u8, Error> {
    cfg.get(usize::from(off)).copied().ok_or(Error::Short)
}

fn rd16(cfg: &[u8], off: u16) -> Result<u16, Error> {
    let lo = rd8(cfg, off)?;
    let hi = rd8(cfg, off.checked_add(1).ok_or(Error::Short)?)?;
    Ok(u16::from_le_bytes([lo, hi]))
}

fn rd32(cfg: &[u8], off: u16) -> Result<u32, Error> {
    let lo = rd16(cfg, off)?;
    let hi = rd16(cfg, off.checked_add(2).ok_or(Error::Short)?)?;
    Ok(u32::from(lo) | (u32::from(hi) << 16))
}

/// The type-0 header's fields this system uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub vendor: u16,
    pub device: u16,
    pub command: u16,
    pub status: u16,
    pub revision: u8,
    /// Class code, subclass and programming interface: `0xCCSSPP`.
    pub class: u32,
    /// The header layout (bit 7, multifunction, is in `multifunction`).
    pub header_type: u8,
    pub multifunction: bool,
    pub subsystem_vendor: u16,
    pub subsystem: u16,
    /// The INTx pin (0: none). Read for the listing only: INTx is never routed (IOMMU-SPEC 8.6).
    pub interrupt_pin: u8,
}

/// The header of `cfg` (the function's first 256 bytes), or `None` if no function answers (the vendor id reads all ones).
pub fn header(cfg: &[u8]) -> Result<Option<Header>, Error> {
    if cfg.len() < CFG_BYTES {
        return Err(Error::Short);
    }
    let vendor = rd16(cfg, reg::VENDOR)?;
    if vendor == NO_VENDOR {
        return Ok(None);
    }
    let ht = rd8(cfg, reg::HEADER_TYPE)?;
    Ok(Some(Header {
        vendor,
        device: rd16(cfg, reg::DEVICE)?,
        command: rd16(cfg, reg::COMMAND)?,
        status: rd16(cfg, reg::STATUS)?,
        revision: rd8(cfg, reg::REVISION)?,
        class: rd32(cfg, reg::REVISION)? >> 8,
        header_type: ht & 0x7f,
        multifunction: ht & 0x80 != 0,
        subsystem_vendor: rd16(cfg, reg::SUBSYSTEM_VENDOR)?,
        subsystem: rd16(cfg, reg::SUBSYSTEM)?,
        interrupt_pin: rd8(cfg, reg::INTERRUPT_PIN)?,
    }))
}

/// One capability: its id and where it starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cap {
    pub id: u8,
    pub offset: u8,
}

/// The capability list, in list order. The walk is bounded: a pointer outside 0x40..=0xfc, a pointer seen before, or more than
/// [`MAX_CAPS`] steps is an error. (The low two bits of each pointer are reserved and masked, as the specification says.)
pub fn capabilities(cfg: &[u8]) -> Result<Vec<Cap>, Error> {
    if cfg.len() < CFG_BYTES {
        return Err(Error::Short);
    }
    let mut out = Vec::new();
    if rd16(cfg, reg::STATUS)? & STATUS_CAP_LIST == 0 {
        return Ok(out);
    }
    let mut seen = [false; CFG_BYTES / 4];
    let mut p = rd8(cfg, reg::CAP_PTR)? & !3;
    while p != 0 {
        if !(CAP_FIRST..=0xfc).contains(&p) {
            return Err(Error::CapPointer(p));
        }
        let slot = seen
            .get_mut(usize::from(p / 4))
            .ok_or(Error::CapPointer(p))?;
        if *slot || out.len() >= MAX_CAPS {
            return Err(Error::CapLoop);
        }
        *slot = true;
        out.push(Cap {
            id: rd8(cfg, u16::from(p))?,
            offset: p,
        });
        p = rd8(cfg, u16::from(p) + 1)? & !3;
    }
    Ok(out)
}

/// A capability's body must fit in the conventional space: `len` bytes from its start.
fn cap_fits(c: Cap, len: u16) -> Result<(), Error> {
    if u16::from(c.offset)
        .checked_add(len)
        .is_some_and(|e| usize::from(e) <= CFG_BYTES)
    {
        Ok(())
    } else {
        Err(Error::CapPointer(c.offset))
    }
}

/// What kind of BAR a register is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarKind {
    Mem32 {
        prefetchable: bool,
    },
    Mem64 {
        prefetchable: bool,
    },
    /// An I/O-space BAR: found, never assigned (the host's I/O window is not a device window: IOMMU-SPEC 2.6).
    Io,
}

/// One implemented BAR: its register index (the lower one of a 64-bit pair), kind and size in bytes (a power of two).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bar {
    pub index: u8,
    pub kind: BarKind,
    pub size: u64,
}

impl Bar {
    pub fn is_memory(&self) -> bool {
        !matches!(self.kind, BarKind::Io)
    }
}

/// BAR sizing, the two-phase protocol the caller drives (PCI local bus specification 3.0, "Address maps"):
///
/// 1. turn decoding off ([`command_for_sizing`]), and keep the six BAR registers as they read (`orig`, which [`header`]'s bytes
///    already hold at 0x10..0x28);
/// 2. write [`SIZING_VALUE`] to each of the six registers and read each back (`masks`);
/// 3. write the assignment ([`bar_writes`]) or the original values back, then turn decoding on ([`command_enabled`]).
///
/// [`decode_bars`] reads the kinds and sizes from `masks` alone (a BAR's type bits are read-only).
pub const SIZING_VALUE: u32 = 0xffff_ffff;

/// The command register while the BARs are sized and moved: memory and I/O decoding and bus mastering off, INTx disabled.
pub fn command_for_sizing(cmd: u16) -> u16 {
    (cmd & !(command::MEMORY | command::IO | command::BUS_MASTER)) | command::INTX_DISABLE
}

/// The command register of an enumerated, unclaimed function: memory decoding on, bus mastering off, INTx disabled
/// (IOMMU-SPEC 8.6: every function's INTx is disabled; bus mastering is the claim's last step, section 9).
pub fn command_enabled(cmd: u16) -> u16 {
    (cmd & !(command::IO | command::BUS_MASTER)) | command::MEMORY | command::INTX_DISABLE
}

/// The BARs `masks` (the six registers read back after [`SIZING_VALUE`] was written) describe. A register that reads back 0 is
/// not implemented.
pub fn decode_bars(masks: &[u32; 6]) -> Result<Vec<Bar>, Error> {
    let mut out = Vec::new();
    let mut i: u8 = 0;
    while i < 6 {
        let m = masks.get(usize::from(i)).copied().unwrap_or(0);
        if m == 0 {
            i = i.saturating_add(1);
            continue;
        }
        if m & 1 != 0 {
            let addr = m & !3 & 0xffff;
            let size = u64::from((!addr & 0xffff).wrapping_add(1));
            if addr == 0 || !size.is_power_of_two() {
                return Err(Error::BarSize(i));
            }
            out.push(Bar {
                index: i,
                kind: BarKind::Io,
                size,
            });
            i = i.saturating_add(1);
            continue;
        }
        let prefetchable = m & 8 != 0;
        match (m >> 1) & 3 {
            0 => {
                let addr = m & !0xf;
                if addr == 0 {
                    return Err(Error::BarSize(i));
                }
                let size = (!addr).wrapping_add(1);
                if !size.is_power_of_two() || (addr | size.wrapping_sub(1)) != u32::MAX {
                    return Err(Error::BarSize(i));
                }
                out.push(Bar {
                    index: i,
                    kind: BarKind::Mem32 { prefetchable },
                    size: u64::from(size),
                });
                i = i.saturating_add(1);
            }
            2 => {
                let hi = masks
                    .get(usize::from(i) + 1)
                    .copied()
                    .ok_or(Error::Bar64AtEnd(i))?;
                let addr = (u64::from(hi) << 32) | u64::from(m & !0xf);
                let size = (!addr).wrapping_add(1);
                if addr == 0
                    || size == 0
                    || !size.is_power_of_two()
                    || (addr | size.wrapping_sub(1)) != u64::MAX
                {
                    return Err(Error::BarSize(i));
                }
                out.push(Bar {
                    index: i,
                    kind: BarKind::Mem64 { prefetchable },
                    size,
                });
                i = i.saturating_add(2);
            }
            t => return Err(Error::BarType(t as u8)),
        }
    }
    Ok(out)
}

/// Where one BAR was placed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Assignment {
    pub bdf: Bdf,
    pub index: u8,
    pub kind: BarKind,
    /// The bus address (on `virt`, equal to the physical address: the host's memory windows are identity ranges).
    pub addr: u64,
    /// The BAR's size.
    pub size: u64,
    /// The span reserved for it: its size rounded up to whole pages, so that no two BARs share a page and each can be handed out
    /// as a VMO of its own.
    pub span: u64,
}

/// The smallest span a BAR takes: one page.
pub const PAGE: u64 = 4096;

/// Place every memory BAR of `bars` in `window` (base, size): each at its natural alignment and at least a page apart, none
/// overlapping, all inside the window; 32-bit BARs only below 4 GiB. Deterministic: the BARs are placed largest first, then in
/// requester-id and register order, so the same set always gets the same addresses (`devmgr` and the suite agree without talking).
/// I/O BARs are skipped. [`Error::NoSpace`] if they do not all fit.
pub fn assign(window: (u64, u64), bars: &[(Bdf, Bar)]) -> Result<Vec<Assignment>, Error> {
    let (base, len) = window;
    let end = base.checked_add(len).ok_or(Error::NoSpace)?;
    let mut todo: Vec<(Bdf, Bar)> = bars
        .iter()
        .copied()
        .filter(|(_, b)| b.is_memory())
        .collect();
    todo.sort_by(|a, b| {
        b.1.size
            .cmp(&a.1.size)
            .then(a.0.cmp(&b.0))
            .then(a.1.index.cmp(&b.1.index))
    });
    let mut next = base;
    let mut out = Vec::with_capacity(todo.len());
    for (bdf, bar) in todo {
        let span = bar.size.max(PAGE);
        if !span.is_power_of_two() {
            return Err(Error::BarSize(bar.index));
        }
        let addr = next
            .checked_add(span.wrapping_sub(1))
            .ok_or(Error::NoSpace)?
            & !span.wrapping_sub(1);
        let stop = addr.checked_add(span).ok_or(Error::NoSpace)?;
        if stop > end || (matches!(bar.kind, BarKind::Mem32 { .. }) && stop > 1 << 32) {
            return Err(Error::NoSpace);
        }
        out.push(Assignment {
            bdf,
            index: bar.index,
            kind: bar.kind,
            addr,
            size: bar.size,
            span,
        });
        next = stop;
    }
    Ok(out)
}

/// The register writes that program `a`: (configuration-space offset, value), one for a 32-bit BAR and two for a 64-bit one.
pub fn bar_writes(a: &Assignment) -> Vec<(u16, u32)> {
    let off = reg::BAR0.wrapping_add(4u16.wrapping_mul(u16::from(a.index.min(5))));
    let mut w = alloc::vec![(off, a.addr as u32)];
    if let BarKind::Mem64 { .. } = a.kind {
        w.push((off.wrapping_add(4), (a.addr >> 32) as u32));
    }
    w
}

/// The MSI-X capability (PCI local bus specification 3.0, "MSI-X capability and table structure").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Msix {
    /// Where the capability is: message control is at `cap + 2`.
    pub cap: u8,
    /// The number of table entries (the field holds N - 1).
    pub table_size: u16,
    pub table_bar: u8,
    pub table_offset: u32,
    pub pba_bar: u8,
    pub pba_offset: u32,
}

pub mod msix {
    /// Message control: MSI-X enable and the function mask.
    pub const ENABLE: u16 = 1 << 15;
    pub const FUNCTION_MASK: u16 = 1 << 14;
    /// Message control's table-size field (N - 1).
    pub const TABLE_SIZE_MASK: u16 = 0x7ff;
    /// One table entry: message address low and high, message data, vector control.
    pub const ENTRY_BYTES: u32 = 16;
    /// Vector control: the entry is masked.
    pub const VECTOR_MASKED: u32 = 1;
}

impl Msix {
    /// Message control's offset in configuration space.
    pub fn control_offset(&self) -> u16 {
        u16::from(self.cap) + 2
    }

    /// The table's length in bytes.
    pub fn table_bytes(&self) -> u64 {
        u64::from(self.table_size).wrapping_mul(u64::from(msix::ENTRY_BYTES))
    }

    /// The pending-bit array's length in bytes: one bit per entry, in whole quadwords.
    pub fn pba_bytes(&self) -> u64 {
        u64::from(self.table_size).div_ceil(64).wrapping_mul(8)
    }

    /// Entry `i`'s offset inside the table's BAR, or `None` past the table.
    pub fn entry_offset(&self, i: u16) -> Option<u64> {
        (i < self.table_size).then(|| {
            u64::from(self.table_offset)
                .wrapping_add(u64::from(i).wrapping_mul(u64::from(msix::ENTRY_BYTES)))
        })
    }
}

/// Message control with MSI-X `enable`d or not and the whole function masked or not (the other bits kept from `current`).
pub fn msix_control(current: u16, enable: bool, masked: bool) -> u16 {
    let mut v = current & !(msix::ENABLE | msix::FUNCTION_MASK);
    if enable {
        v |= msix::ENABLE;
    }
    if masked {
        v |= msix::FUNCTION_MASK;
    }
    v
}

/// One MSI-X table entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MsixEntry {
    pub address: u64,
    pub data: u32,
    pub masked: bool,
}

impl MsixEntry {
    /// The entry's four doublewords, in table order (address low, address high, data, vector control).
    pub fn encode(&self) -> [u32; 4] {
        [
            self.address as u32,
            (self.address >> 32) as u32,
            self.data,
            u32::from(self.masked) * msix::VECTOR_MASKED,
        ]
    }

    pub fn decode(w: [u32; 4]) -> MsixEntry {
        MsixEntry {
            address: u64::from(w[0]) | (u64::from(w[1]) << 32),
            data: w[2],
            masked: w[3] & msix::VECTOR_MASKED != 0,
        }
    }
}

/// The MSI-X capability of `cfg`, if it has one (the first, as a device has at most one).
pub fn msix_cap(cfg: &[u8], caps: &[Cap]) -> Result<Option<Msix>, Error> {
    let Some(&c) = caps.iter().find(|c| c.id == cap_id::MSIX) else {
        return Ok(None);
    };
    cap_fits(c, 12)?;
    let o = u16::from(c.offset);
    let ctl = rd16(cfg, o + 2)?;
    let t = rd32(cfg, o + 4)?;
    let p = rd32(cfg, o + 8)?;
    Ok(Some(Msix {
        cap: c.offset,
        table_size: (ctl & msix::TABLE_SIZE_MASK).wrapping_add(1),
        table_bar: (t & 7) as u8,
        table_offset: t & !7,
        pba_bar: (p & 7) as u8,
        pba_offset: p & !7,
    }))
}

/// A byte range inside one BAR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub bar: u8,
    pub offset: u64,
    pub len: u64,
}

impl Region {
    fn end(&self) -> Option<u64> {
        self.offset.checked_add(self.len)
    }

    fn overlaps(&self, o: &Region) -> bool {
        match (self.end(), o.end()) {
            (Some(a), Some(b)) => self.bar == o.bar && self.offset < b && o.offset < a,
            _ => true,
        }
    }

    fn inside(&self, bars: &[Bar]) -> bool {
        bars.iter()
            .find(|b| b.index == self.bar && b.is_memory())
            .is_some_and(|b| self.len > 0 && self.end().is_some_and(|e| e <= b.size))
    }
}

/// The table and the pending-bit array each lie inside a memory BAR of the function, do not overlap each other and overlap none of
/// `others` (the virtio structures).
pub fn check_msix(m: &Msix, bars: &[Bar], others: &[Region]) -> Result<(), Error> {
    for bar in [m.table_bar, m.pba_bar] {
        if !bars.iter().any(|b| b.index == bar && b.is_memory()) {
            return Err(Error::MsixBar(bar));
        }
    }
    let table = Region {
        bar: m.table_bar,
        offset: u64::from(m.table_offset),
        len: m.table_bytes(),
    };
    let pba = Region {
        bar: m.pba_bar,
        offset: u64::from(m.pba_offset),
        len: m.pba_bytes(),
    };
    if !table.inside(bars) || !pba.inside(bars) || table.overlaps(&pba) {
        return Err(Error::MsixPlacement);
    }
    if others
        .iter()
        .any(|o| o.overlaps(&table) || o.overlaps(&pba))
    {
        return Err(Error::MsixPlacement);
    }
    Ok(())
}

/// The virtio PCI vendor id, and the range of modern (non-transitional) device ids: 0x1040 + the virtio device id (VIRTIO 1.x,
/// 4.1.2.1).
pub const VIRTIO_VENDOR: u16 = 0x1af4;
pub const VIRTIO_MODERN_BASE: u16 = 0x1040;

/// The virtio device id of a modern virtio-pci function (1 network, 2 block, 4 entropy, ...), or `None`. Transitional functions
/// (0x1000..0x103f) are not used: every test machine passes `disable-legacy=on`.
pub fn virtio_device_id(vendor: u16, device: u16) -> Option<u16> {
    (vendor == VIRTIO_VENDOR && (VIRTIO_MODERN_BASE..=0x107f).contains(&device))
        .then(|| device.wrapping_sub(VIRTIO_MODERN_BASE))
}

/// The `cfg_type` values of the virtio-pci capabilities (VIRTIO 1.x, 4.1.4).
pub mod virtio_cfg {
    pub const COMMON: u8 = 1;
    pub const NOTIFY: u8 = 2;
    pub const ISR: u8 = 3;
    pub const DEVICE: u8 = 4;
    pub const PCI: u8 = 5;
}

/// One virtio-pci capability (`struct virtio_pci_cap`, and for the notify structure its `notify_off_multiplier`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VirtioCap {
    pub cap: u8,
    pub cfg_type: u8,
    pub bar: u8,
    pub offset: u32,
    pub length: u32,
    pub notify_multiplier: u32,
}

/// Every vendor-specific capability of a virtio function, decoded as `struct virtio_pci_cap` (cap_vndr, cap_next, cap_len, cfg_type,
/// bar, id, padding, offset, length; the notify structure adds a 32-bit multiplier). A capability shorter than its structure is an
/// error.
pub fn virtio_caps(cfg: &[u8], caps: &[Cap]) -> Result<Vec<VirtioCap>, Error> {
    let mut out = Vec::new();
    for &c in caps.iter().filter(|c| c.id == cap_id::VENDOR) {
        cap_fits(c, 16)?;
        let o = u16::from(c.offset);
        let len = rd8(cfg, o + 2)?;
        let cfg_type = rd8(cfg, o + 3)?;
        let need = if cfg_type == virtio_cfg::NOTIFY {
            20
        } else {
            16
        };
        if len < need {
            return Err(Error::VirtioCap(c.offset));
        }
        cap_fits(c, u16::from(need))?;
        out.push(VirtioCap {
            cap: c.offset,
            cfg_type,
            bar: rd8(cfg, o + 4)?,
            offset: rd32(cfg, o + 8)?,
            length: rd32(cfg, o + 12)?,
            notify_multiplier: if cfg_type == virtio_cfg::NOTIFY {
                rd32(cfg, o + 16)?
            } else {
                0
            },
        });
    }
    Ok(out)
}

/// Where a virtio function's structures are: all in one memory BAR (this system's rule: the driver is handed that one BAR).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VirtioLayout {
    pub bar: u8,
    pub common: Region,
    pub notify: Region,
    pub notify_multiplier: u32,
    pub isr: Region,
    /// The device-specific configuration (a device may have none).
    pub device: Option<Region>,
}

/// The minimum lengths of the structures this system reads (VIRTIO 1.x 4.1.4.3: the common configuration up to `queue_device`'s
/// end; the ISR status is one byte).
const COMMON_MIN: u32 = 0x38;

/// The layout of a virtio function: the first capability of each type (as the specification tells a driver to use), every one in
/// the same memory BAR and inside it. `cfg_type` PCI (the configuration-access window) and unknown types are ignored.
pub fn virtio_layout(vcaps: &[VirtioCap], bars: &[Bar]) -> Result<VirtioLayout, Error> {
    let first = |t: u8| vcaps.iter().find(|c| c.cfg_type == t).copied();
    let (Some(common), Some(notify), Some(isr)) = (
        first(virtio_cfg::COMMON),
        first(virtio_cfg::NOTIFY),
        first(virtio_cfg::ISR),
    ) else {
        return Err(Error::VirtioLayout);
    };
    let device = first(virtio_cfg::DEVICE);
    let bar = common.bar;
    let region = |c: VirtioCap, min: u32| -> Result<Region, Error> {
        if c.bar != bar {
            return Err(Error::VirtioLayout);
        }
        let r = Region {
            bar: c.bar,
            offset: u64::from(c.offset),
            len: u64::from(c.length),
        };
        if c.length < min || !r.inside(bars) {
            return Err(Error::VirtioCap(c.cap));
        }
        Ok(r)
    };
    Ok(VirtioLayout {
        bar,
        common: region(common, COMMON_MIN)?,
        notify: region(notify, 2)?,
        notify_multiplier: notify.notify_multiplier,
        isr: region(isr, 1)?,
        device: device.map(|d| region(d, 1)).transpose()?,
    })
}

/// How a function offers a function-level reset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flr {
    /// The PCI Express capability's Device Capabilities says FLR (bit 28); Device Control (cap + 8) bit 15 starts it.
    Pcie { devctl: u16 },
    /// The Advanced Features capability says FLR (capabilities byte, cap + 3, bit 1); its control byte (cap + 4) bit 0 starts it.
    Af { control: u16 },
}

/// The wait after starting a function-level reset before the function may be touched (PCI Express base specification: 100 ms).
pub const FLR_WAIT_MS: u64 = 100;
/// How long a function may then take to answer its vendor id again before the reset is declared failed (and said so, loudly).
pub const FLR_READY_MS: u64 = 1000;

/// The function-level reset `cfg` advertises, if any (IOMMU0 item 16: the virtio-pci functions of QEMU `virt` advertise none).
pub fn flr(cfg: &[u8], caps: &[Cap]) -> Result<Option<Flr>, Error> {
    if let Some(&c) = caps.iter().find(|c| c.id == cap_id::PCIE) {
        cap_fits(c, 12)?;
        if rd32(cfg, u16::from(c.offset) + 4)? & (1 << 28) != 0 {
            return Ok(Some(Flr::Pcie {
                devctl: u16::from(c.offset) + 8,
            }));
        }
    }
    if let Some(&c) = caps.iter().find(|c| c.id == cap_id::AF) {
        cap_fits(c, 6)?;
        if rd8(cfg, u16::from(c.offset) + 3)? & 2 != 0 {
            return Ok(Some(Flr::Af {
                control: u16::from(c.offset) + 4,
            }));
        }
    }
    Ok(None)
}

impl Flr {
    /// The write that starts the reset: (offset, value) on a register whose current value is `current` (16 bits for the PCI
    /// Express Device Control register, the low byte for the Advanced Features control).
    pub fn start(&self, current: u16) -> (u16, u16) {
        match *self {
            Flr::Pcie { devctl } => (devctl, current | (1 << 15)),
            Flr::Af { control } => (control, 1),
        }
    }
}

/// One enumerated function, as `devmgr` keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Function {
    pub bdf: Bdf,
    pub header: Header,
    pub bars: Vec<Bar>,
    pub caps: Vec<Cap>,
    pub msix: Option<Msix>,
    /// For a modern virtio function: its device id and where its structures are.
    pub virtio: Option<(u16, VirtioLayout)>,
    pub flr: Option<Flr>,
}

/// Everything about one function, from its configuration bytes and its BARs' sizing read-backs: `None` if no function answers.
/// Any inconsistency (a capability walk that fails, a BAR that is not a BAR, an MSI-X table outside its BAR or over a virtio
/// structure, a virtio function without its structures) is an error: the function is listed as refused and never claimed.
pub fn describe(bdf: Bdf, cfg: &[u8], masks: &[u32; 6]) -> Result<Option<Function>, Error> {
    let Some(header) = header(cfg)? else {
        return Ok(None);
    };
    if header.header_type != 0 {
        return Err(Error::NotType0(header.header_type));
    }
    let caps = capabilities(cfg)?;
    let bars = decode_bars(masks)?;
    let msix = msix_cap(cfg, &caps)?;
    let virtio = match virtio_device_id(header.vendor, header.device) {
        Some(id) => Some((id, virtio_layout(&virtio_caps(cfg, &caps)?, &bars)?)),
        None => None,
    };
    if let Some(m) = &msix {
        let mut others = Vec::new();
        if let Some((_, l)) = &virtio {
            others.extend([l.common, l.notify, l.isr]);
            others.extend(l.device);
        }
        check_msix(m, &bars, &others)?;
    }
    let flr = flr(cfg, &caps)?;
    Ok(Some(Function {
        bdf,
        header,
        bars,
        caps,
        msix,
        virtio,
        flr,
    }))
}
