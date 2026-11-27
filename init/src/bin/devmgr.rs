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
//!
//! **PCI (IOMMU8; IOMMU-SPEC sections 9 and 10).** Beside the virtio-mmio table (kept for the unconverted drivers until IOMMU12),
//! the manager enumerates bus 0 of the ECAM host the device tree names (`resource_dtb`, `kcore::iommutree`), with `pcicore` for
//! every byte it reads: it sizes and assigns every function's memory BARs in the host's 32-bit window (no firmware does it on
//! QEMU `virt`), turns memory decoding on, **disables INTx in every function's command register** and leaves bus mastering off.
//! It never hands out configuration space. It maps each function's MSI-X table BAR (and pending-bit BAR) privately. It prints one
//! line per function, `## devmgr: pci <bb:dd.f> <vendor>:<device> class=<cccccc> <claimable msix=<entries>[ table-with-registers] |
//! not-claimable <why> | refused <why>>`, then `## devmgr: pci functions=<n>`, which the boot script checks against the devices it
//! put on the command line (`table-with-registers`: the MSI-X table shares the BAR the driver is handed, so the manager cannot keep
//! it from the driver; the IOMMU confines the device either way, IOMMU-SPEC 9).
//!
//! **The class plan** (`pcicore::claim`, a table in the source): block, the configuration change and one queue; network, the
//! configuration change, receive and transmit; entropy, its one queue. Entry 0 of the MSI-X table is the configuration change and
//! entry `n + 1` queue `n`; a function whose table is smaller than its class needs is not claimable, and says so.
//!
//! **The order of bring-up and teardown** (IOMMU-SPEC 9 and 10; the kernel does not rely on it, IOMMU6 and IOMMU7 order the safe part
//! themselves; it keeps the fault log quiet). Up: the driver job's limits, the domain, the DMA VMOs, the vector grants, the MSI-X
//! table programmed and MSI-X enabled, the driver's handles, bus mastering last; the driver then maps and starts. Down: MSI-X
//! function mask and disable, bus mastering off, the device reset (and a function-level reset where offered), the domain destroyed
//! (vectors revoked and mappings removed by the kernel), the interrupt handles closed.
//!
//! **The claim**, in the specification's order, each step a precondition of the next (`ClaimPci`, which carries the driver's job):
//! 1. refuse, loudly, a function without MSI-X (8.6), a virtio function that does not offer `VIRTIO_F_ACCESS_PLATFORM` (15.1 L1),
//!    one whose configuration space `pcicore` refused, one of a class this manager has no plan for, or one whose MSI-X table is
//!    smaller than its class needs; bus mastering stays off; and refuse (`Busy`) a function already claimed, or a driver job that
//!    already holds a claim (one function per job);
//! 2. reset the device (10.4: it may have been left running; loud if it does not complete), set **finite limits of both kinds** on
//!    the driver's job (`job_set_io_limits`: the pages of the class's DMA VMOs with a small margin, and exactly the class's vectors;
//!    hierarchical, so a job the driver makes is bound by them), then `iodomain_create` for the function's requester id, naming
//!    that job;
//! 3. create the driver's DMA VMOs (`vmo_create_dma_in`, charged to the driver's job);
//! 4. grant the class's vectors (`iodomain_grant_vector`, with the control handle), and program the MSI-X table with the pairs the
//!    kernel returned with the function masked, every other entry masked and cleared, INTx disabled, MSI-X enabled
//!    (`pcicore::msix_enable`); a claim gets every vector of its class or fails whole, with a loud line
//!    (`## devmgr: pci <bb:dd.f> claim failed <what>`), never a shared vector;
//! 5. hand the driver its authority: a VMO over the BAR with the virtio structures, its **duplicate of the domain handle with
//!    IO_MAP and IO_UNMAP only** (and TRANSFER), its DMA VMOs, and a duplicate of each interrupt handle that can wait and
//!    acknowledge only (and TRANSFER); no I/O address travels (the driver maps its buffers itself);
//! 6. **bus mastering last**, once the reply is sent; then a second message (`txid | Ok`) tells the driver it may start the device.
//!
//! The manager keeps the domain's control handle and its own interrupt handles, and maps nothing into the domain.
//!
//! **Teardown** when the claimant's connection closes (its driver exited or was killed: 10.1, 10.2): MSI-X masked and disabled and
//! the table cleared, bus mastering off, the device reset (virtio status 0, read back) and a function-level reset where the
//! function advertises one (waited for, and the BARs programmed again), then `iodomain_destroy`, which revokes every vector and
//! removes every mapping the driver made, and the manager's interrupt handles are closed. A reset that does not complete, or a
//! domain that cannot be destroyed, is said on the console (`## devmgr: pci <bb:dd.f> reset failed <what>`, `## devmgr: pci
//! <bb:dd.f> teardown failed <what>`), never passed over. Nothing is kept for the next driver: a re-claim makes a new domain and
//! grants afresh (10.4, OD166 point 2). None of this is a safety property (the kernel's ordering is, IOMMU-SPEC 6.3, 10.6); it
//! keeps the device working and the fault log quiet.
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
    mmio: u64,
    dma: u64,
    irq: Option<u64>,
    /// The driver process handle, once one was started.
    driver: Option<u64>,
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

fn read_dtb(res: u64, me: u64) -> Option<Vec<u8>> {
    let r = sys::resource_dtb(res);
    if r.status != 0 {
        return None;
    }
    let (vmo, len) = (r.a1 as u64, r.a2);
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
fn probe(res: u64, me: u64, pa: u64, va: u64, want: u32) -> Option<u64> {
    let vmo = sys::vmo_create_physical(res, pa, 4096);
    if vmo.status != 0 {
        return None;
    }
    let vmo = vmo.a1 as u64;
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
    mmio: u64,
    irq: Option<u64>,
    /// While claimed: the claimant's connection key and the manager's own handles to the three DMA VMOs.
    claim: Option<(u64, [u64; 3])>,
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
    mmio: u64,
    claim: Option<(u64, u64)>,
}

impl Rng {
    fn release(&mut self) {
        if let Some((_, dma)) = self.claim.take() {
            reset(self.va);
            sys::handle_close(dma);
        }
    }
}

fn blkd_image(bi: u64) -> Option<Vec<u8>> {
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
    fn open(&mut self, image: &[u8]) -> Result<u64, Status> {
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
        let (mine, theirs) = (pair.a1 as u64, pair.a2 as u64);
        let dup = |h: u64| {
            let r = sys::handle_duplicate(h, Rights::SAME.0);
            (r.status == 0).then_some(r.a1 as u64)
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

// ---- PCI (IOMMU8) ---------------------------------------------------------------------------------------------------------------

use pcicore::{Assignment, Bdf, Function};
use virtio::Transport;

/// Where bus 0's ECAM pages are mapped in this process (32 devices of 8 functions, a page each).
const ECAM_VA: u64 = 0x700_0000;
const BUS0_BYTES: u64 =
    pcicore::DEVICES as u64 * pcicore::FUNCTIONS as u64 * pcicore::ECAM_FUNCTION_BYTES;
/// Where the BARs with virtio structures are mapped (for reading the features and for the reset), upwards.
const PCI_BAR_VA: u64 = 0x800_0000;
/// How long a virtio reset may take to read back 0 (ms).
const RESET_MS: u64 = 1000;

fn cfg_rd32(va: u64, off: u16) -> u32 {
    // SAFETY: `va` is a function's 4 KiB ECAM page inside the bus-0 window mapped at ECAM_VA; `off` is below 0x100 and aligned.
    unsafe { core::ptr::read_volatile((va + u64::from(off & 0xffc)) as *const u32) }
}
fn cfg_wr32(va: u64, off: u16, v: u32) {
    // SAFETY: as `cfg_rd32`.
    unsafe { core::ptr::write_volatile((va + u64::from(off & 0xffc)) as *mut u32, v) }
}
fn cfg_rd16(va: u64, off: u16) -> u16 {
    // SAFETY: as `cfg_rd32`, a 16-bit register (aligned to 2).
    unsafe { core::ptr::read_volatile((va + u64::from(off & 0xffe)) as *const u16) }
}
fn cfg_wr16(va: u64, off: u16, v: u16) {
    // SAFETY: as `cfg_rd16`.
    unsafe { core::ptr::write_volatile((va + u64::from(off & 0xffe)) as *mut u16, v) }
}
fn cfg_wr8(va: u64, off: u16, v: u8) {
    // SAFETY: as `cfg_rd32`, one byte.
    unsafe { core::ptr::write_volatile((va + u64::from(off & 0xfff)) as *mut u8, v) }
}

/// A copy of a function's conventional configuration space, read a doubleword at a time.
fn read_cfg(va: u64) -> [u8; pcicore::CFG_BYTES] {
    let mut cfg = [0u8; pcicore::CFG_BYTES];
    for (i, w) in cfg.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        *w = cfg_rd32(va, (4 * i) as u16).to_le_bytes();
    }
    cfg
}

/// A BAR mapped in this process (its address and its span): the virtio structures' registers, for `virtio::PciTransport`. An
/// access that would not lie wholly inside the span, or is not aligned to its width, is not made (a read gives all ones).
struct BarRegs {
    va: u64,
    span: u64,
}

impl BarRegs {
    /// The address of the `width`-byte register at `off`, if it is aligned and inside the mapping.
    fn at(&self, off: u64, width: u64) -> Option<u64> {
        (off.is_multiple_of(width) && off.checked_add(width)? <= self.span).then_some(self.va + off)
    }
}

impl virtio::Regs for BarRegs {
    fn read8(&self, off: u64) -> u8 {
        // SAFETY: `va` maps the whole BAR (`span` bytes) read-write in this process for as long as the manager runs, and `at` gives an
        // address only when the access lies inside it and is aligned.
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

/// A live claim: the claimant's connection, the driver's job (its koid: one function per job), and what the manager keeps of it:
/// the domain's control handle (destroy, grant, faults) and its own interrupt handles (the DMA VMOs went to the driver).
struct PciClaimed {
    key: u64,
    job: u64,
    claim: pcicore::claim::Claim,
}

/// The kernel calls of a claim (`pcicore::claim::Kernel`), for one function and one driver job.
struct SysKernel {
    res: u64,
    rid: u16,
    job: u64,
}

impl pcicore::claim::Kernel for SysKernel {
    fn set_io_limits(&mut self, pages: u64, vectors: u64) -> bool {
        sys::job_set_io_limits(self.job, pages, vectors).status == 0
    }
    fn domain_create(&mut self) -> Option<u64> {
        let r = sys::iodomain_create(self.res, u64::from(self.rid), self.job);
        (r.status == 0).then_some(r.a1 as u64)
    }
    fn dma_create(&mut self, pages: u32) -> Option<u64> {
        let r = sys::vmo_create_dma_in(self.res, u64::from(pages), self.job);
        (r.status == 0).then_some(r.a1 as u64)
    }
    fn grant(&mut self, domain: u64, index: u16) -> Option<(u64, u64)> {
        let r = sys::iodomain_grant_vector(domain, u64::from(index));
        (r.status == 0).then_some((r.a1 as u64, r.a2 as u64))
    }
    fn domain_destroy(&mut self, domain: u64) -> bool {
        sys::iodomain_destroy(domain).status == 0
    }
    fn close(&mut self, h: u64) {
        sys::handle_close(h);
    }
}

/// A BAR mapped in this process: (its register index, where, its span).
type MappedBar = (u8, u64, u64);

/// The function's configuration page and its mapped BARs, for `pcicore`'s MSI-X sequence. A BAR access outside a mapping, or not
/// aligned, is not made (a read gives all ones): `pcicore` checks the table against the BAR's size first, so this is a backstop.
struct DevAccess<'a> {
    cfg: u64,
    maps: &'a [MappedBar],
}

impl DevAccess<'_> {
    fn at(&self, bar: u8, off: u64) -> Option<u64> {
        let &(_, va, span) = self.maps.iter().find(|m| m.0 == bar)?;
        (off.is_multiple_of(4) && off.checked_add(4)? <= span).then_some(va + off)
    }
}

impl pcicore::MsixAccess for DevAccess<'_> {
    fn cfg_read16(&mut self, off: u16) -> u16 {
        cfg_rd16(self.cfg, off)
    }
    fn cfg_write16(&mut self, off: u16, v: u16) {
        cfg_wr16(self.cfg, off, v)
    }
    fn bar_read32(&mut self, bar: u8, off: u64) -> u32 {
        // SAFETY: `at` gives an address only inside a BAR this process mapped read-write for as long as the manager runs, aligned.
        self.at(bar, off).map_or(u32::MAX, |a| unsafe {
            core::ptr::read_volatile(a as *const u32)
        })
    }
    fn bar_write32(&mut self, bar: u8, off: u64, v: u32) {
        if let Some(a) = self.at(bar, off) {
            // SAFETY: as `bar_read32`.
            unsafe { core::ptr::write_volatile(a as *mut u32, v) }
        }
    }
}

/// One enumerated function.
struct PciDev {
    bdf: Bdf,
    vendor: u16,
    device: u16,
    class: u32,
    /// The ECAM page of the function, mapped here.
    cfg: u64,
    /// What `pcicore` made of it, or why it was refused.
    desc: Result<Function, pcicore::Error>,
    bars: Vec<Assignment>,
    /// For a virtio function: its virtio BAR mapped here (address, span), and whether it offers ACCESS_PLATFORM.
    regs: Option<(u64, u64)>,
    access_platform: bool,
    /// The BARs mapped here: the virtio structures', the MSI-X table's and the pending bits' (each once).
    maps: Vec<MappedBar>,
    claim: Option<PciClaimed>,
}

impl PciDev {
    /// The plan of the driver class of this function, or why it is not claimable.
    fn plan(&self) -> Result<&'static pcicore::claim::Plan, alloc::string::String> {
        let f = self
            .desc
            .as_ref()
            .map_err(|_| alloc::string::String::from("its configuration space was refused"))?;
        let Some(m) = f.msix else {
            return Err("no MSI-X capability".into());
        };
        let Some((id, _)) = f.virtio else {
            return Err("no driver class".into());
        };
        if self.regs.is_none() || !self.access_platform {
            return Err("no VIRTIO_F_ACCESS_PLATFORM".into());
        }
        let plan = pcicore::claim::for_virtio(id).ok_or("no driver class")?;
        if m.table_size < plan.table_needed() {
            return Err(alloc::format!(
                "MSI-X table of {} entries, the {} class needs {}",
                m.table_size,
                plan.class,
                plan.table_needed()
            ));
        }
        if !self.maps.iter().any(|b| b.0 == m.table_bar) {
            return Err("its MSI-X table could not be mapped".into());
        }
        Ok(plan)
    }

    fn flags(&self) -> u16 {
        let mut fl = 0;
        if self.plan().is_ok() {
            fl |= devmgr::PCI_CLAIMABLE;
        }
        match &self.desc {
            Ok(f) => {
                if f.msix.is_some() {
                    fl |= devmgr::PCI_MSIX;
                }
                if f.virtio.is_some() {
                    fl |= devmgr::PCI_VIRTIO;
                }
            }
            Err(_) => fl |= devmgr::PCI_REFUSED,
        }
        if self.access_platform {
            fl |= devmgr::PCI_ACCESS_PLATFORM;
        }
        if self.claim.is_some() {
            fl |= devmgr::PCI_CLAIMED;
        }
        fl
    }

    /// The virtio BAR's assignment.
    fn virtio_bar(&self) -> Option<&Assignment> {
        let (_, l) = self.desc.as_ref().ok()?.virtio?;
        self.bars.iter().find(|a| a.index == l.bar)
    }

    /// Write the BARs and the command register of an enumerated, unclaimed function (also after a function-level reset).
    fn program(&self) {
        let cmd = cfg_rd16(self.cfg, pcicore::reg::COMMAND);
        cfg_wr16(
            self.cfg,
            pcicore::reg::COMMAND,
            pcicore::command_for_sizing(cmd),
        );
        for a in &self.bars {
            for (off, v) in pcicore::bar_writes(a) {
                cfg_wr32(self.cfg, off, v);
            }
        }
        let cmd = if self.desc.is_ok() {
            pcicore::command_enabled(cmd)
        } else {
            pcicore::command::INTX_DISABLE
        };
        cfg_wr16(self.cfg, pcicore::reg::COMMAND, cmd);
    }

    fn transport(&self) -> Option<virtio::PciTransport<BarRegs>> {
        let (_, l) = self.desc.as_ref().ok()?.virtio?;
        Some(virtio::PciTransport::new(
            BarRegs {
                va: self.regs?.0,
                span: self.regs?.1,
            },
            virtio::PciLocations {
                common: l.common.offset,
                notify: l.notify.offset,
                notify_len: l.notify.len,
                notify_multiplier: l.notify_multiplier,
                isr: l.isr.offset,
                device: l.device.map(|d| (d.offset, d.len)),
            },
        ))
    }
}

fn virtio_reset(t: &mut virtio::PciTransport<BarRegs>) -> Result<(), virtio::Error> {
    let until = sys::clock_get() + RESET_MS;
    t.reset(&mut || {
        sys::thread_yield();
        sys::clock_get() < until
    })
}

/// Bus 0 of the ECAM host, enumerated.
struct Pci {
    devs: Vec<PciDev>,
    /// An event nothing signals: `sleep` waits on it until a deadline.
    nap: u64,
}

fn sleep(nap: u64, ms: u64) {
    let _ = sys::object_wait(nap, 1 << 24, sys::clock_get() + ms);
}

/// Enumerate bus 0 (see the header comment). `None` if the tree names no PCI host or its windows cannot be mapped.
fn pci_enumerate(res: u64, me: u64, nodes: &[kcore::fdt::Node]) -> Option<Pci> {
    let host = kcore::iommutree::discover(nodes)
        .ok()?
        .pci
        .into_iter()
        .next()?;
    let window = host
        .memory
        .iter()
        .copied()
        .find(|&(base, len)| base.checked_add(len).is_some_and(|e| e <= 1 << 32))?;
    if host.ecam.1 < BUS0_BYTES {
        return None;
    }
    let ecam = sys::vmo_create_physical(res, host.ecam.0, BUS0_BYTES);
    if ecam.status != 0 {
        return None;
    }
    let ecam = ecam.a1 as u64;
    let mapped = sys::process_map(me, ecam, ECAM_VA, 0, BUS0_BYTES, PF_RW).status == 0;
    sys::handle_close(ecam);
    if !mapped {
        return None;
    }
    let nap = sys::event_create();
    if nap.status != 0 {
        return None;
    }
    let mut devs: Vec<PciDev> = Vec::new();
    for dev in 0..pcicore::DEVICES {
        for func in 0..pcicore::FUNCTIONS {
            let Some(bdf) = Bdf::new(0, dev, func) else {
                continue;
            };
            let va = ECAM_VA + bdf.ecam_offset();
            let cfg = read_cfg(va);
            let Ok(Some(h)) = pcicore::header(&cfg) else {
                if func == 0 {
                    break; // no device here: no other function either
                }
                continue;
            };
            // size the BARs with decoding off (a type-0 header only: a bridge is listed and never configured)
            cfg_wr16(
                va,
                pcicore::reg::COMMAND,
                pcicore::command_for_sizing(h.command),
            );
            let mut masks = [0u32; 6];
            if h.header_type == 0 {
                for (i, m) in masks.iter_mut().enumerate() {
                    let off = pcicore::reg::BAR0 + 4 * i as u16;
                    let orig = cfg_rd32(va, off);
                    cfg_wr32(va, off, pcicore::SIZING_VALUE);
                    *m = cfg_rd32(va, off);
                    cfg_wr32(va, off, orig);
                }
            }
            let desc =
                pcicore::describe(bdf, &cfg, &masks).and_then(|f| f.ok_or(pcicore::Error::Short));
            devs.push(PciDev {
                bdf,
                vendor: h.vendor,
                device: h.device,
                class: h.class,
                cfg: va,
                desc,
                bars: Vec::new(),
                regs: None,
                access_platform: false,
                maps: Vec::new(),
                claim: None,
            });
            if func == 0 && !h.multifunction {
                break;
            }
        }
    }
    // one assignment for the whole bus (largest first, deterministic: the suite's hand-driven tests compute the same one)
    let all: Vec<(Bdf, pcicore::Bar)> = devs
        .iter()
        .filter_map(|d| d.desc.as_ref().ok())
        .flat_map(|f| f.bars.iter().map(move |b| (f.bdf, *b)))
        .collect();
    match pcicore::assign(window, &all) {
        Ok(assigned) => {
            for d in &mut devs {
                d.bars = assigned
                    .iter()
                    .filter(|a| a.bdf == d.bdf)
                    .copied()
                    .collect();
            }
        }
        Err(e) => {
            for d in &mut devs {
                if d.desc.is_ok() {
                    d.desc = Err(e);
                }
            }
        }
    }
    let mut next_va = PCI_BAR_VA;
    for d in &mut devs {
        d.program();
        // the BARs this manager touches, mapped here: the MSI-X table's and the pending bits' (kept from every driver where they
        // have a BAR of their own), and a virtio function's structures
        let Ok(f) = &d.desc else {
            continue;
        };
        let mut want: Vec<u8> = Vec::new();
        want.extend(f.virtio.map(|(_, l)| l.bar));
        want.extend(f.msix.iter().flat_map(|m| [m.table_bar, m.pba_bar]));
        want.dedup();
        for idx in want {
            if d.maps.iter().any(|m| m.0 == idx) {
                continue;
            }
            let Some(&bar) = d.bars.iter().find(|a| a.index == idx) else {
                continue;
            };
            let vmo = sys::vmo_create_physical(res, bar.addr, bar.span);
            if vmo.status != 0 {
                continue;
            }
            let vmo = vmo.a1 as u64;
            let ok = sys::process_map(me, vmo, next_va, 0, bar.span, PF_RW).status == 0;
            sys::handle_close(vmo);
            if ok {
                d.maps.push((idx, next_va, bar.span));
                next_va += bar.span;
            }
        }
        // a virtio function: ask whether it offers ACCESS_PLATFORM (reset, acknowledge, read the features, reset again: nothing
        // is accepted and the device never runs)
        let Some(&bar) = d.virtio_bar() else {
            continue;
        };
        let Some(&(_, va, span)) = d.maps.iter().find(|m| m.0 == bar.index) else {
            continue;
        };
        d.regs = Some((va, span));
        if let Some(mut t) = d.transport() {
            if virtio_reset(&mut t).is_ok() {
                t.set_status(virtio::status::ACKNOWLEDGE);
                t.set_status(virtio::status::ACKNOWLEDGE | virtio::status::DRIVER);
                d.access_platform = t.device_features() & virtio::feature::ACCESS_PLATFORM != 0;
            }
            if virtio_reset(&mut t).is_err() {
                d.access_platform = false;
                print_reset_failed(d.bdf, "virtio status did not read back 0");
            }
        }
    }
    for d in &devs {
        let mut line = alloc::format!(
            "## devmgr: pci {} {:04x}:{:04x} class={:06x} ",
            d.bdf,
            d.vendor,
            d.device,
            d.class
        );
        match (&d.desc, d.plan()) {
            (Err(e), _) => line.push_str(&alloc::format!("refused {e:?}")),
            (Ok(f), Ok(_)) => {
                let m = f.msix.map_or(0, |m| m.table_size);
                line.push_str(&alloc::format!("claimable msix={m}"));
                if f.msix.map(|m| m.table_bar) == f.virtio.map(|(_, l)| l.bar) {
                    line.push_str(" table-with-registers");
                }
            }
            (Ok(_), Err(why)) => line.push_str(&alloc::format!("not-claimable {why}")),
        }
        line.push('\n');
        sys::debug_write(line.as_bytes());
    }
    sys::debug_write(alloc::format!("## devmgr: pci functions={}\n", devs.len()).as_bytes());
    Some(Pci {
        devs,
        nap: nap.a1 as u64,
    })
}

fn print_claim_failed(bdf: Bdf, what: &str) {
    sys::debug_write(alloc::format!("## devmgr: pci {bdf} claim failed {what}\n").as_bytes());
}

fn print_reset_failed(bdf: Bdf, what: &str) {
    sys::debug_write(alloc::format!("## devmgr: pci {bdf} reset failed {what}\n").as_bytes());
}

impl Pci {
    /// Undo a claim (IOMMU-SPEC 10.1 and 10.2, the supervisor's steps; the order of the header comment): MSI-X masked and off and
    /// the table cleared, bus mastering off, the device reset (and a function-level reset where offered), then the domain destroyed
    /// and the manager's interrupt handles closed.
    fn release(&mut self, i: usize, res: u64) {
        let nap = self.nap;
        let Some(d) = self.devs.get_mut(i) else {
            return;
        };
        let Some(c) = d.claim.take() else {
            return;
        };
        if let Ok(f) = &d.desc {
            if let Some(m) = f.msix {
                let mut acc = DevAccess {
                    cfg: d.cfg,
                    maps: &d.maps,
                };
                if let Err(e) = pcicore::msix_disable(&m, &f.bars, &mut acc) {
                    print_teardown_failed(d.bdf, &alloc::format!("MSI-X off: {e:?}"));
                }
            }
        }
        let cmd = cfg_rd16(d.cfg, pcicore::reg::COMMAND);
        cfg_wr16(
            d.cfg,
            pcicore::reg::COMMAND,
            (cmd & !pcicore::command::BUS_MASTER) | pcicore::command::INTX_DISABLE,
        );
        if let Some(mut t) = d.transport() {
            if virtio_reset(&mut t).is_err() {
                print_reset_failed(d.bdf, "virtio status did not read back 0");
            }
        }
        if let Some(flr) = d.desc.as_ref().ok().and_then(|f| f.flr) {
            match flr {
                pcicore::Flr::Pcie { devctl } => {
                    let (off, v) = flr.start(cfg_rd16(d.cfg, devctl));
                    cfg_wr16(d.cfg, off, v);
                }
                pcicore::Flr::Af { .. } => {
                    let (off, v) = flr.start(0);
                    cfg_wr8(d.cfg, off, v as u8);
                }
            }
            sleep(nap, pcicore::FLR_WAIT_MS);
            let until = sys::clock_get() + pcicore::FLR_READY_MS;
            while cfg_rd16(d.cfg, pcicore::reg::VENDOR) == pcicore::NO_VENDOR
                && sys::clock_get() < until
            {
                sleep(nap, 10);
            }
            if cfg_rd16(d.cfg, pcicore::reg::VENDOR) == pcicore::NO_VENDOR {
                print_reset_failed(d.bdf, "the function-level reset did not complete");
            }
            // the reset cleared the BARs and the command register: program them again (bus mastering stays off)
            d.program();
        }
        let mut k = SysKernel {
            res,
            rid: d.bdf.rid(),
            job: 0,
        };
        if !pcicore::claim::release(c.claim, &mut k) {
            print_teardown_failed(d.bdf, "iodomain_destroy");
        }
    }
}

fn print_teardown_failed(bdf: Bdf, what: &str) {
    sys::debug_write(alloc::format!("## devmgr: pci {bdf} teardown failed {what}\n").as_bytes());
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
    res: u64,
    blocks: Vec<Block>,
    nets: Vec<Net>,
    rngs: Vec<Rng>,
    /// The real-time clocks: one VMO of each register page (claimed read-only, by any number of clients).
    rtcs: Vec<u64>,
    image: Option<Vec<u8>>,
    /// Bus 0 of the PCI host (IOMMU8), if the tree names one.
    pci: Option<Pci>,
}

impl DevMgr {
    fn reply(&mut self, key: u64, txid: u32, status: Status, handle: Option<u64>) {
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

    fn request(&mut self, key: u64, msg: &[u8], hs: &[u64]) {
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
        // ClaimPci carries exactly one handle: the driver's job
        if devmgr::op_of(msg) == Some(devmgr::OP_CLAIM_PCI) {
            return match (devmgr::decode_claim_pci(msg), hs) {
                (Ok((txid, rid)), &[job]) => self.claim_pci(key, txid, rid, job),
                (r, _) => {
                    for &h in hs {
                        sys::handle_close(h);
                    }
                    match r {
                        Ok((t, _)) | Err(Some(t)) => self.reply(key, t, Status::BadRequest, None),
                        Err(None) => {}
                    }
                }
            };
        }
        for &h in hs {
            sys::handle_close(h);
        }
        if !hs.is_empty() {
            return self.reply(key, txid_of(msg), Status::BadRequest, None);
        }
        if devmgr::op_of(msg) == Some(devmgr::OP_LIST_PCI) {
            return match devmgr::decode_list_pci(msg) {
                Ok(txid) => {
                    let entries: Vec<devmgr::PciEntry> = self
                        .pci
                        .iter()
                        .flat_map(|p| p.devs.iter())
                        .map(|d| devmgr::PciEntry {
                            rid: d.bdf.rid(),
                            vendor: d.vendor,
                            device: d.device,
                            flags: d.flags(),
                            class: d.class,
                        })
                        .collect();
                    match devmgr::encode_list_pci_reply(txid, &entries) {
                        Some(b) => {
                            let _ = self.core.send(key, b, None);
                        }
                        None => self.reply(key, txid, Status::Failed, None),
                    }
                }
                Err(Some(t)) => self.reply(key, t, Status::BadRequest, None),
                Err(None) => {}
            };
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
                    self.reply(key, txid, Status::Ok, Some(d.a1 as u64))
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
            (r.status == 0).then_some(r.a1 as u64)
        };
        let (rx, tx, q) = (mk(NET_RX_PAGES), mk(NET_TX_PAGES), mk(NET_QUEUE_PAGES));
        let (Some(rx), Some(tx), Some(q)) = (rx, tx, q) else {
            for h in [rx, tx, q].into_iter().flatten() {
                sys::handle_close(h);
            }
            return fail(self, Status::Failed);
        };
        reset(n.va);
        let dup = |h: u64| {
            let r = sys::handle_duplicate(h, Rights::SAME.0);
            (r.status == 0).then_some(r.a1 as u64)
        };
        let mut give: Vec<u64> = Vec::new();
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
        let dma = dma.a1 as u64;
        reset(r.va);
        let dup = |h: u64| {
            let d = sys::handle_duplicate(h, Rights::SAME.0);
            (d.status == 0).then_some(d.a1 as u64)
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

    /// `ClaimPci` (IOMMU-SPEC 9; the order is the header comment's). `job` is the driver's job, which is closed here whatever
    /// happens (the domain and the DMA VMOs hold what they need of it).
    fn claim_pci(&mut self, key: u64, txid: u32, rid: u16, job: u64) {
        let r = self.claim_pci_inner(key, txid, rid, job);
        sys::handle_close(job);
        match r {
            Err(st) => self.reply(key, txid, st, None),
            Ok((i, reply, handles)) => {
                if let rt::service::Sent::Sent = self.core.send_handles(key, reply, handles) {
                    // 6. bus mastering last, then the driver is told it may start the device
                    if let Some(d) = self.pci.as_ref().and_then(|p| p.devs.get(i)) {
                        let cmd = cfg_rd16(d.cfg, pcicore::reg::COMMAND);
                        cfg_wr16(
                            d.cfg,
                            pcicore::reg::COMMAND,
                            cmd | pcicore::command::BUS_MASTER | pcicore::command::INTX_DISABLE,
                        );
                    }
                    self.reply(key, txid, Status::Ok, None);
                }
                // (not sent: the connection is closed, and `closed` releases the claim)
            }
        }
    }

    #[allow(clippy::type_complexity)]
    fn claim_pci_inner(
        &mut self,
        key: u64,
        txid: u32,
        rid: u16,
        job: u64,
    ) -> Result<(usize, Vec<u8>, Vec<u64>), Status> {
        let res = self.res;
        let pci = self.pci.as_mut().ok_or(Status::NoDevice)?;
        let i = pci
            .devs
            .iter()
            .position(|d| d.bdf.rid() == rid)
            .ok_or(Status::NoDevice)?;
        let d = pci.devs.get_mut(i).ok_or(Status::NoDevice)?;
        // 1. refuse what is not claimable (said at enumeration, and again here)
        let plan = match d.plan() {
            Ok(p) => p,
            Err(why) => {
                print_claim_failed(d.bdf, &why);
                return Err(Status::Refused);
            }
        };
        if d.claim.is_some() {
            return Err(Status::Busy);
        }
        // one function per driver job: its limits are this function's (`job_set_io_limits` replaces them)
        let job_koid = sys::object_info(job);
        if job_koid.status != 0 {
            return Err(Status::BadRequest);
        }
        let job_koid = job_koid.a1 as u64;
        if pci
            .devs
            .iter()
            .any(|o| o.claim.as_ref().is_some_and(|c| c.job == job_koid))
        {
            return Err(Status::Busy);
        }
        let d = pci.devs.get_mut(i).ok_or(Status::NoDevice)?;
        let (Ok(f), Some(&bar)) = (&d.desc, d.virtio_bar()) else {
            return Err(Status::Refused);
        };
        let (Some(m), Some((_, l))) = (f.msix, f.virtio) else {
            return Err(Status::Refused);
        };
        // 2. the device reset (it may have been left running: 10.4)
        let Some(mut t) = d.transport() else {
            return Err(Status::Refused);
        };
        if virtio_reset(&mut t).is_err() {
            print_reset_failed(d.bdf, "virtio status did not read back 0");
            return Err(Status::Failed);
        }
        // 2 to 4: the job's limits, the domain, the DMA VMOs, the vectors and the MSI-X table (all or nothing)
        let mut k = SysKernel { res, rid, job };
        let mut acc = DevAccess {
            cfg: d.cfg,
            maps: &d.maps,
        };
        let mut c = match pcicore::claim::claim(
            plan,
            &m,
            &f.bars,
            rt::abi::iodomain::MSI_DATA as u32,
            &mut k,
            &mut acc,
        ) {
            Ok(c) => c,
            Err(e) => {
                print_claim_failed(d.bdf, &alloc::format!("{e:?}"));
                return Err(Status::Failed);
            }
        };
        // 5. the driver's handles: the BAR with the virtio structures, its duplicate of the domain, its DMA VMOs (moved), and a
        // duplicate of each interrupt handle that waits and acknowledges only
        let mut give: Vec<u64> = Vec::new();
        let made = (|| {
            let bar_vmo = sys::vmo_create_physical(res, bar.addr, bar.span);
            if bar_vmo.status != 0 {
                return Err("the BAR VMO");
            }
            give.push(bar_vmo.a1 as u64);
            let drv = sys::handle_duplicate(
                c.domain,
                Rights::IO_MAP.0 | Rights::IO_UNMAP.0 | Rights::TRANSFER.0,
            );
            if drv.status != 0 {
                return Err("the driver's domain handle");
            }
            give.push(drv.a1 as u64);
            for &(_, h) in &c.irqs {
                let r = sys::handle_duplicate(
                    h,
                    Rights::WAIT.0 | Rights::MANAGE.0 | Rights::TRANSFER.0,
                );
                if r.status != 0 {
                    return Err("an interrupt handle");
                }
                give.push(r.a1 as u64);
            }
            Ok(())
        })();
        let claim = devmgr::PciClaim {
            vendor: d.vendor,
            device: d.device,
            bar_bytes: bar.span,
            common: (l.common.offset as u32, l.common.len as u32),
            notify: (l.notify.offset as u32, l.notify.len as u32),
            notify_multiplier: l.notify_multiplier,
            isr: (l.isr.offset as u32, l.isr.len as u32),
            device_cfg: l.device.map_or((0, 0), |r| (r.offset as u32, r.len as u32)),
            dma_pages: plan.dma_pages.to_vec(),
            irqs: c
                .irqs
                .iter()
                .map(|&(q, _)| devmgr::PciIrq {
                    entry: q.entry(),
                    what: match q {
                        pcicore::claim::Irq::Config => devmgr::IRQ_CONFIG,
                        pcicore::claim::Irq::Queue(n) => n,
                    },
                })
                .collect(),
        };
        let reply = match (made, devmgr::encode_claim_pci_reply(txid, &claim)) {
            (Ok(()), Some(r)) => r,
            (e, _) => {
                print_claim_failed(d.bdf, e.err().unwrap_or("the reply"));
                for h in give {
                    sys::handle_close(h);
                }
                let _ = pcicore::msix_disable(&m, &f.bars, &mut acc);
                pcicore::claim::release(c, &mut k);
                return Err(Status::Failed);
            }
        };
        // (the handle order of the reply: the BAR, the domain, the DMA VMOs, the interrupts)
        let irqs = give.split_off(2);
        give.append(&mut c.dma);
        give.extend(irqs);
        d.claim = Some(PciClaimed {
            key,
            job: job_koid,
            claim: c,
        });
        Ok((i, reply, give))
    }

    /// A connection ended: if it held a network claim, release it.
    fn drop_claims_of(&mut self, key: u64) {
        if let Some(pci) = self.pci.as_mut() {
            for i in 0..pci.devs.len() {
                if pci
                    .devs
                    .get(i)
                    .and_then(|d| d.claim.as_ref())
                    .is_some_and(|c| c.key == key)
                {
                    pci.release(i, self.res);
                }
            }
        }
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
    let me = sys::process_self().a1 as u64;
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
            (i.status == 0).then_some(i.a1 as u64)
        };
        if let Some(mmio) = probe(res, me, slot.base, va, ID_BLOCK) {
            let dma = sys::vmo_create_dma(res, 4);
            if dma.status != 0 {
                return 6;
            }
            blocks.push(Block {
                va,
                mmio,
                dma: dma.a1 as u64,
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
    let mut rtcs: Vec<u64> = Vec::new();
    for n in nodes
        .iter()
        .filter(|n| n.is_compatible("google,goldfish-rtc"))
    {
        if let Some((base, size)) = n.reg() {
            if size >= 4096 && base % 4096 == 0 {
                let v = sys::vmo_create_physical(res, base, 4096);
                if v.status == 0 {
                    rtcs.push(v.a1 as u64);
                }
            }
        }
    }
    let pci = pci_enumerate(res, me, &nodes);
    let image = blkd_image(bi);
    let p = sys::port_create();
    if p.status != 0 {
        return 7;
    }
    let mut mgr = DevMgr {
        core: Core::new(
            p.a1 as u64,
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
        pci,
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
    if let Some(pci) = mgr.pci.as_mut() {
        for i in 0..pci.devs.len() {
            pci.release(i, res);
        }
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
