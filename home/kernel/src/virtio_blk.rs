//! A polled virtio-mmio block driver, just enough for `cartfs` (decision D21).
//!
//! QEMU's `virt` board has eight virtio-mmio slots from 0x1000_1000. We scan them
//! for a block device (id 2), negotiate only VIRTIO_F_VERSION_1, set up one
//! 8-entry queue and issue one request at a time, spinning until the device has
//! answered. No interrupts, no DMA mapping: with paging off a pointer is the
//! physical address the device needs. Only the "modern" transport (version 2) is
//! supported, which QEMU gives with `-global virtio-mmio.force-legacy=false`.

use cartfs::{BlockDevice, Error, SECTOR};
use core::ptr::{addr_of, addr_of_mut, read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

const BASE: usize = 0x1000_1000;
const STRIDE: usize = 0x1000;
const SLOTS: usize = 8;

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
const QUEUE_DESC_LOW: usize = 0x080;
const QUEUE_DRIVER_LOW: usize = 0x090;
const QUEUE_DEVICE_LOW: usize = 0x0a0;
const CONFIG: usize = 0x100;

const ACKNOWLEDGE: u32 = 1;
const DRIVER: u32 = 2;
const DRIVER_OK: u32 = 4;
const FEATURES_OK: u32 = 8;
const FAILED: u32 = 128;

const QSIZE: usize = 8;
const DESC_NEXT: u16 = 1;
const DESC_WRITE: u16 = 2;
const T_IN: u32 = 0;
const T_OUT: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy)]
struct Desc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

#[repr(C)]
struct Avail {
    flags: u16,
    idx: u16,
    ring: [u16; QSIZE],
}

#[repr(C)]
struct UsedElem {
    id: u32,
    len: u32,
}

#[repr(C)]
struct Used {
    flags: u16,
    idx: u16,
    ring: [UsedElem; QSIZE],
}

#[repr(C)]
struct RequestHeader {
    kind: u32,
    reserved: u32,
    sector: u64,
}

/// Everything the device reads or writes, in one page-aligned static.
#[repr(C, align(4096))]
struct Queue {
    desc: [Desc; QSIZE],
    avail: Avail,
    // The used ring must be 4-byte aligned; this padding keeps it so.
    _pad: [u8; 2],
    used: Used,
    header: RequestHeader,
    data: [u8; SECTOR],
    status: u8,
}

static mut QUEUE: Queue = Queue {
    desc: [Desc {
        addr: 0,
        len: 0,
        flags: 0,
        next: 0,
    }; QSIZE],
    avail: Avail {
        flags: 0,
        idx: 0,
        ring: [0; QSIZE],
    },
    _pad: [0; 2],
    used: Used {
        flags: 0,
        idx: 0,
        ring: [const { UsedElem { id: 0, len: 0 } }; QSIZE],
    },
    header: RequestHeader {
        kind: 0,
        reserved: 0,
        sector: 0,
    },
    data: [0; SECTOR],
    status: 0,
};

pub struct VirtioBlk {
    base: usize,
    sectors: u32,
    last_used: u16,
}

fn reg_read(base: usize, offset: usize) -> u32 {
    // SAFETY: `base + offset` is a register inside a virtio-mmio slot that
    // `probe` found by its magic value, or one of the eight slot addresses it is
    // probing (reading those is harmless on QEMU's virt board).
    unsafe { read_volatile((base + offset) as *const u32) }
}

fn reg_write(base: usize, offset: usize, value: u32) {
    // SAFETY: as for `reg_read`; the offsets written are the virtio-mmio
    // registers named above.
    unsafe { write_volatile((base + offset) as *mut u32, value) }
}

fn set_address(base: usize, low_offset: usize, address: usize) {
    reg_write(base, low_offset, address as u32);
    reg_write(base, low_offset + 4, (address as u64 >> 32) as u32);
}

/// Look for a block device and bring it up. `None` if the machine has none (or
/// not one this driver can use).
pub fn probe() -> Option<VirtioBlk> {
    for slot in 0..SLOTS {
        let base = BASE + slot * STRIDE;
        if reg_read(base, MAGIC) != 0x7472_6976 || reg_read(base, DEVICE_ID) != 2 {
            continue;
        }
        if reg_read(base, VERSION) != 2 {
            return None;
        }
        return VirtioBlk::init(base);
    }
    None
}

impl VirtioBlk {
    fn init(base: usize) -> Option<VirtioBlk> {
        reg_write(base, STATUS, 0);
        reg_write(base, STATUS, ACKNOWLEDGE);
        reg_write(base, STATUS, ACKNOWLEDGE | DRIVER);
        // Features: only VIRTIO_F_VERSION_1 (bit 32), which the device must offer.
        reg_write(base, DEVICE_FEATURES_SEL, 1);
        if reg_read(base, DEVICE_FEATURES) & 1 == 0 {
            reg_write(base, STATUS, FAILED);
            return None;
        }
        reg_write(base, DRIVER_FEATURES_SEL, 0);
        reg_write(base, DRIVER_FEATURES, 0);
        reg_write(base, DRIVER_FEATURES_SEL, 1);
        reg_write(base, DRIVER_FEATURES, 1);
        reg_write(base, STATUS, ACKNOWLEDGE | DRIVER | FEATURES_OK);
        if reg_read(base, STATUS) & FEATURES_OK == 0 {
            reg_write(base, STATUS, FAILED);
            return None;
        }
        reg_write(base, QUEUE_SEL, 0);
        if reg_read(base, QUEUE_NUM_MAX) < QSIZE as u32 {
            reg_write(base, STATUS, FAILED);
            return None;
        }
        reg_write(base, QUEUE_NUM, QSIZE as u32);
        // SAFETY: `QUEUE` is a static used only by this driver, which runs on
        // one hart with interrupts irrelevant to it; taking addresses of its
        // fields creates no references.
        unsafe {
            set_address(base, QUEUE_DESC_LOW, addr_of!(QUEUE.desc) as usize);
            set_address(base, QUEUE_DRIVER_LOW, addr_of!(QUEUE.avail) as usize);
            set_address(base, QUEUE_DEVICE_LOW, addr_of!(QUEUE.used) as usize);
        }
        reg_write(base, QUEUE_READY, 1);
        reg_write(base, STATUS, ACKNOWLEDGE | DRIVER | FEATURES_OK | DRIVER_OK);
        let low = reg_read(base, CONFIG) as u64;
        let high = reg_read(base, CONFIG + 4) as u64;
        let capacity = (high << 32) | low;
        Some(VirtioBlk {
            base,
            sectors: capacity.min(u32::MAX as u64) as u32,
            last_used: 0,
        })
    }

    /// One request: header, the data sector, a status byte; wait for the device.
    fn request(&mut self, kind: u32, sector: u32) -> Result<(), Error> {
        // SAFETY: `QUEUE` is used only here and in `init`, one request at a time
        // (the `&mut self` receiver and the single hart see to that). The device
        // reads and writes the same memory, so every access is volatile and the
        // fences order them against the notification and the used-index read.
        unsafe {
            let q = addr_of_mut!(QUEUE);
            write_volatile(addr_of_mut!((*q).header.kind), kind);
            write_volatile(addr_of_mut!((*q).header.sector), sector as u64);
            write_volatile(addr_of_mut!((*q).status), 0xff);
            let desc = addr_of_mut!((*q).desc) as *mut Desc;
            let data_flags = if kind == T_IN {
                DESC_WRITE | DESC_NEXT
            } else {
                DESC_NEXT
            };
            write_volatile(
                desc,
                Desc {
                    addr: addr_of!((*q).header) as u64,
                    len: 16,
                    flags: DESC_NEXT,
                    next: 1,
                },
            );
            write_volatile(
                desc.add(1),
                Desc {
                    addr: addr_of!((*q).data) as u64,
                    len: SECTOR as u32,
                    flags: data_flags,
                    next: 2,
                },
            );
            write_volatile(
                desc.add(2),
                Desc {
                    addr: addr_of!((*q).status) as u64,
                    len: 1,
                    flags: DESC_WRITE,
                    next: 0,
                },
            );
            let idx = read_volatile(addr_of!((*q).avail.idx));
            write_volatile(addr_of_mut!((*q).avail.ring[idx as usize % QSIZE]), 0u16);
            fence(Ordering::SeqCst);
            write_volatile(addr_of_mut!((*q).avail.idx), idx.wrapping_add(1));
            fence(Ordering::SeqCst);
            reg_write(self.base, QUEUE_NOTIFY, 0);
            // Spin until the device reports the request done.
            let mut spins = 0u64;
            while read_volatile(addr_of!((*q).used.idx)) == self.last_used {
                spins += 1;
                if spins > 200_000_000 {
                    return Err(Error::Device);
                }
                core::hint::spin_loop();
            }
            fence(Ordering::SeqCst);
            self.last_used = self.last_used.wrapping_add(1);
            if read_volatile(addr_of!((*q).status)) != 0 {
                return Err(Error::Device);
            }
        }
        Ok(())
    }
}

impl BlockDevice for VirtioBlk {
    fn sector_count(&self) -> u32 {
        self.sectors
    }

    fn read(&mut self, sector: u32, buf: &mut [u8; SECTOR]) -> Result<(), Error> {
        if sector >= self.sectors {
            return Err(Error::Device);
        }
        self.request(T_IN, sector)?;
        // SAFETY: the request has completed, so the device is done with `data`.
        unsafe {
            for (i, byte) in buf.iter_mut().enumerate() {
                *byte = read_volatile(addr_of!(QUEUE.data[i]));
            }
        }
        Ok(())
    }

    fn write(&mut self, sector: u32, buf: &[u8; SECTOR]) -> Result<(), Error> {
        if sector >= self.sectors {
            return Err(Error::Device);
        }
        // SAFETY: no request is in flight (they are synchronous), so the driver
        // owns `data` until the next `request`.
        unsafe {
            for (i, byte) in buf.iter().enumerate() {
                write_volatile(addr_of_mut!(QUEUE.data[i]), *byte);
            }
        }
        self.request(T_OUT, sector)
    }
}

/// What the interpreters get as their disk: the real one if the machine has a
/// usable block device, otherwise a stand-in that fails with the right error.
pub fn open() -> alloc::boxed::Box<dyn cartfs::Files> {
    use alloc::boxed::Box;
    match probe() {
        None => Box::new(cartfs::Unavailable(Error::NoDevice)),
        Some(device) => match cartfs::Disk::mount(device) {
            Ok(disk) => Box::new(disk),
            Err(error) => Box::new(cartfs::Unavailable(error)),
        },
    }
}
