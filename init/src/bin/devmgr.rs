//! `devmgr`: the device manager (S6c, S6-SPEC section 2). It reads the device tree through the root
//! resource, finds the virtio block device, and starts `blkd` for a client that asks, handing the
//! driver only what it needs: that slot's MMIO window, one DMA buffer and (on a PLIC machine) the
//! slot's interrupt.
//!
//! The manager is the *supervisor* of the driver in the sense of TIDY-UP T42: it holds its own
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
use rt::abi::status::Status as Sys;
use rt::startup::{self, EXIT_BAD_STARTUP, EXIT_NO_STARTUP};
use rt::sys;

const PF_R: u64 = 1;
const PF_RW: u64 = 3;
/// Where the device tree and the probed slot are mapped in this process.
const DTB_VA: u64 = 0x300_0000;
const MMIO_VA: u64 = 0x600_0000;

const STATUS: usize = 0x070;
const INTERRUPT_STATUS: usize = 0x060;
const INTERRUPT_ACK: usize = 0x064;
const MAGIC: usize = 0x000;
const DEVICE_ID: usize = 0x008;
const ID_BLOCK: u32 = 2;

struct Block {
    /// The slot's MMIO page, mapped at `MMIO_VA` (for the reset), and its VMO.
    mmio: u32,
    dma: u32,
    irq: Option<u32>,
    /// The driver process handle, once one was started.
    driver: Option<u32>,
}

fn rd(off: usize) -> u32 {
    // SAFETY: MMIO_VA maps the block device's 4 KiB register page in this process.
    unsafe { core::ptr::read_volatile((MMIO_VA as usize + off) as *const u32) }
}

fn wr(off: usize, v: u32) {
    // SAFETY: as in `rd`.
    unsafe { core::ptr::write_volatile((MMIO_VA as usize + off) as *mut u32, v) }
}

/// Put the device back to its reset state and quiet its interrupt line.
fn reset() {
    wr(STATUS, 0);
    for _ in 0..1_000_000 {
        if rd(STATUS) == 0 {
            break;
        }
    }
    wr(INTERRUPT_ACK, rd(INTERRUPT_STATUS));
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

/// Is the slot at `pa` a block device? (Maps its page briefly.)
fn probe(res: u32, me: u32, pa: u64) -> Option<u32> {
    let vmo = sys::vmo_create_physical(res, pa, 4096);
    if vmo.status != 0 {
        return None;
    }
    let vmo = vmo.a1 as u32;
    let mapped = sys::process_map(me, vmo, MMIO_VA, 0, 4096, PF_RW).status == 0;
    let found = mapped && rd(MAGIC) == 0x7472_6976 && rd(DEVICE_ID) == ID_BLOCK;
    if mapped && !found {
        sys::process_unmap(me, MMIO_VA);
    }
    if found {
        Some(vmo) // stays mapped at MMIO_VA
    } else {
        sys::handle_close(vmo);
        None
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
        reset();
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

fn reply(chan: u32, txid: u32, status: Status, handle: Option<u32>) {
    let b = devmgr::encode_reply(txid, status);
    let hs = [handle.unwrap_or(0)];
    let r = sys::channel_write(
        chan,
        b.as_ptr(),
        b.len(),
        hs.as_ptr(),
        usize::from(handle.is_some()),
    );
    if r.status != 0 {
        if let Some(h) = handle {
            sys::handle_close(h); // the client is not taking it
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
    // the first virtio slot that really is a block device
    let mut block: Option<Block> = None;
    for slot in kcore::fdt::virtio_mmio(&nodes) {
        if slot.size != 4096 {
            continue;
        }
        if let Some(mmio) = probe(res, me, slot.base) {
            let dma = sys::vmo_create_dma(res, 4);
            if dma.status != 0 {
                return 6;
            }
            // an interrupt where the machine has a PLIC; NOT_SUPPORTED (AIA) means the driver polls
            let i = sys::interrupt_create(res, slot.irq);
            let irq = (i.status == 0).then_some(i.a1 as u32);
            block = Some(Block {
                mmio,
                dma: dma.a1 as u32,
                irq,
                driver: None,
            });
            break;
        }
    }
    let image = blkd_image(bi);
    let mut rx = [0u8; 64];
    let code = loop {
        let w = sys::object_wait(chan, signals::READABLE | signals::PEER_CLOSED, u64::MAX);
        if w.status() != Some(Sys::Ok) {
            break 7;
        }
        let r = sys::channel_read(chan, rx.as_mut_ptr(), rx.len(), core::ptr::null_mut(), 0);
        match r.status() {
            Some(Sys::Ok) => {}
            Some(Sys::ShouldWait) => continue,
            Some(Sys::PeerClosed) => break 0,
            _ => {
                // too big for the buffer (or a handle came with it): not ours to serve; drain it
                let mut big = alloc::vec![0u8; rt::abi::limits::MAX_MSG_BYTES];
                let mut hs = [0u32; rt::abi::limits::MAX_MSG_HANDLES];
                let d =
                    sys::channel_read(chan, big.as_mut_ptr(), big.len(), hs.as_mut_ptr(), hs.len());
                if d.status == 0 {
                    for &h in &hs[..d.a2] {
                        sys::handle_close(h);
                    }
                    let txid = big
                        .get(..4)
                        .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
                    reply(chan, txid, Status::BadRequest, None);
                    continue;
                }
                break 8;
            }
        }
        match devmgr::decode_request(&rx[..r.a1]) {
            Err(Some(t)) => reply(chan, t, Status::BadRequest, None),
            Err(None) => {}
            Ok(txid) => match (&mut block, &image) {
                (None, _) => reply(chan, txid, Status::NoDevice, None),
                (Some(_), None) => reply(chan, txid, Status::Failed, None),
                (Some(b), Some(img)) => match b.open(img) {
                    Ok(h) => reply(chan, txid, Status::Ok, Some(h)),
                    Err(s) => reply(chan, txid, s, None),
                },
            },
        }
    };
    // the client is gone (or the manager failed): end the driver, wait until it is really gone,
    // and only then leave the device quiet (T42: reset after the driver has stopped)
    if let Some(b) = &mut block {
        if let Some(p) = b.driver {
            sys::process_kill(p, -1);
            let _ = sys::object_wait(p, signals::TERMINATED, sys::clock_get() + 2000);
        }
        reset();
    }
    code
}
