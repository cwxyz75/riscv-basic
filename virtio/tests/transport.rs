//! IOMMU8's acceptance tests for the `virtio` crate. The numbers are the VIRTIO 1.x text's, written here as literals (not imported
//! from the code): the common configuration's layout (4.1.4.3, `struct virtio_pci_common_cfg`), the feature bits (6), the status
//! bits (2.1), the notification address (4.1.4.4), and the split virtqueue's parts with their sizes and alignments (2.7: the
//! descriptor table 16 bytes per entry aligned to 16, the available ring 6 + 2 per entry aligned to 2, the used ring 6 + 8 per entry
//! aligned to 4). The device is a model of a modern virtio-pci function as the text describes it, including the
//! `ACCESS_PLATFORM` rule QEMU follows (IOMMU0 item 2: with `iommu_platform=on` a driver that does not accept bit 33 does not keep
//! `FEATURES_OK`).
use std::cell::RefCell;
use std::collections::HashMap;
use virtio::*;

#[derive(Default)]
struct Queue {
    max: u16,
    size: u16,
    vector: u16,
    enable: u16,
    notify_off: u16,
    desc: u64,
    driver: u64,
    device: u64,
}

/// A modern virtio-pci function's BAR: common configuration at 0x0, ISR at 0x1000, device configuration at 0x2000, notify at
/// 0x3000 with a multiplier of 4 (QEMU's layout).
#[derive(Default)]
struct Device {
    offered: u64,
    /// With `iommu_platform=on`: FEATURES_OK only if ACCESS_PLATFORM was accepted.
    platform: bool,
    /// Refuse FEATURES_OK whatever the driver accepts.
    refuse: bool,
    /// Reads of the status that still return non-zero after a reset.
    slow_reset: u32,
    dfsel: u32,
    gfsel: u32,
    accepted: u64,
    status: u8,
    qsel: u16,
    queues: Vec<Queue>,
    notified: RefCell<Vec<(u64, u16)>>,
    /// The MSI-X table's entries: a vector at or past it is refused (read back as 0xffff, VIRTIO 1.x 4.1.5.1.2).
    table: u16,
    config_vector: u16,
    reads_left: RefCell<u32>,
    config: HashMap<u64, u8>,
}

const COMMON: u64 = 0;
const ISR: u64 = 0x1000;
const DEVCFG: u64 = 0x2000;
const NOTIFY: u64 = 0x3000;
/// The lengths the transport is told (a notification structure for queues 0 to 3 at multiplier 4; 8 bytes of configuration).
const NOTIFY_LEN: u64 = 16;
const DEVCFG_LEN: u64 = 8;

impl Device {
    fn new(offered: u64, queues: &[u16]) -> Device {
        Device {
            offered,
            platform: true,
            table: 4,
            config_vector: 0xffff,
            queues: queues
                .iter()
                .enumerate()
                .map(|(i, &max)| Queue {
                    max,
                    notify_off: i as u16,
                    ..Queue::default()
                })
                .collect(),
            ..Device::default()
        }
    }
    fn q(&mut self) -> Option<&mut Queue> {
        self.queues.get_mut(self.qsel as usize)
    }
    fn set_status(&mut self, v: u8) {
        if v == 0 {
            self.status = 0;
            self.accepted = 0;
            for q in &mut self.queues {
                q.enable = 0;
                q.size = 0;
                q.vector = 0xffff;
            }
            self.config_vector = 0xffff;
            *self.reads_left.borrow_mut() = self.slow_reset;
            return;
        }
        let mut v = v;
        if v & status::FEATURES_OK != 0 && self.status & status::FEATURES_OK == 0 {
            let ok = !self.refuse
                && self.accepted & !self.offered == 0
                && (!self.platform || self.accepted & (1 << 33) != 0);
            if !ok {
                v &= !status::FEATURES_OK;
            }
        }
        self.status = v;
    }
}

struct Bar(RefCell<Device>);

impl Regs for Bar {
    fn read8(&self, off: u64) -> u8 {
        let d = self.0.borrow();
        match off {
            o if o == COMMON + 0x14 => {
                let mut left = d.reads_left.borrow_mut();
                if *left > 0 {
                    *left -= 1;
                    0x0f
                } else {
                    d.status
                }
            }
            o if o == ISR => 1,
            o if (DEVCFG..DEVCFG + 0x1000).contains(&o) => {
                *d.config.get(&(o - DEVCFG)).unwrap_or(&0)
            }
            o => panic!("read8 at {o:#x}"),
        }
    }
    fn read16(&self, off: u64) -> u16 {
        let mut d = self.0.borrow_mut();
        let qsel = d.qsel;
        match off - COMMON {
            0x10 => d.config_vector,
            0x12 => d.queues.len() as u16,
            0x16 => qsel,
            0x18 => d
                .q()
                .map_or(0, |q| if q.size != 0 { q.size } else { q.max }),
            0x1a => d.q().map_or(0, |q| q.vector),
            0x1c => d.q().map_or(0, |q| q.enable),
            0x1e => d.q().map_or(0, |q| q.notify_off),
            o => panic!("read16 at {o:#x}"),
        }
    }
    fn read32(&self, off: u64) -> u32 {
        let d = self.0.borrow();
        match off {
            0x04 => {
                if d.dfsel == 0 {
                    d.offered as u32
                } else if d.dfsel == 1 {
                    (d.offered >> 32) as u32
                } else {
                    0
                }
            }
            o if o >= DEVCFG => (0..4).fold(0, |a, k| {
                a | (u32::from(*d.config.get(&(o - DEVCFG + k)).unwrap_or(&0)) << (8 * k))
            }),
            o => panic!("read32 at {o:#x}"),
        }
    }
    fn write8(&mut self, off: u64, v: u8) {
        assert_eq!(
            off,
            COMMON + 0x14,
            "the only byte register written is device_status"
        );
        self.0.get_mut().set_status(v);
    }
    fn write16(&mut self, off: u64, v: u16) {
        let d = self.0.get_mut();
        if off >= NOTIFY {
            d.notified.borrow_mut().push((off - NOTIFY, v));
            return;
        }
        match off - COMMON {
            0x16 => d.qsel = v,
            0x18 => d.q().unwrap().size = v,
            0x1a => {
                let ok = v == 0xffff || v < d.table;
                d.q().unwrap().vector = if ok { v } else { 0xffff };
            }
            0x1c => d.q().unwrap().enable = v,
            0x10 => {
                d.config_vector = if v == 0xffff || v < d.table {
                    v
                } else {
                    0xffff
                }
            }
            o => panic!("write16 at {o:#x}"),
        }
    }
    fn write32(&mut self, off: u64, v: u32) {
        let d = self.0.get_mut();
        let set = |x: &mut u64, hi: bool| {
            if hi {
                *x = (*x & 0xffff_ffff) | (u64::from(v) << 32)
            } else {
                *x = (*x & !0xffff_ffff) | u64::from(v)
            }
        };
        match off - COMMON {
            0x00 => d.dfsel = v,
            0x08 => d.gfsel = v,
            0x0c => {
                let hi = d.gfsel == 1;
                set(&mut d.accepted, hi)
            }
            o @ 0x20..=0x37 => {
                let q = d.q().unwrap();
                let field = match o & !7 {
                    0x20 => &mut q.desc,
                    0x28 => &mut q.driver,
                    _ => &mut q.device,
                };
                set(field, o & 4 != 0);
            }
            o => panic!("write32 at {o:#x}"),
        }
    }
}

fn transport(d: Device) -> PciTransport<Bar> {
    PciTransport::new(
        Bar(RefCell::new(d)),
        PciLocations {
            common: COMMON,
            notify: NOTIFY,
            notify_len: NOTIFY_LEN,
            notify_multiplier: 4,
            isr: ISR,
            device: Some((DEVCFG, DEVCFG_LEN)),
        },
    )
}

#[test]
fn the_common_configuration_is_where_the_text_puts_it() {
    use virtio::common::*;
    let regs = [
        (DEVICE_FEATURE_SELECT, 0x00),
        (DEVICE_FEATURE, 0x04),
        (DRIVER_FEATURE_SELECT, 0x08),
        (DRIVER_FEATURE, 0x0c),
        (CONFIG_MSIX_VECTOR, 0x10),
        (NUM_QUEUES, 0x12),
        (DEVICE_STATUS, 0x14),
        (CONFIG_GENERATION, 0x15),
        (QUEUE_SELECT, 0x16),
        (QUEUE_SIZE, 0x18),
        (QUEUE_MSIX_VECTOR, 0x1a),
        (QUEUE_ENABLE, 0x1c),
        (QUEUE_NOTIFY_OFF, 0x1e),
        (QUEUE_DESC, 0x20),
        (QUEUE_DRIVER, 0x28),
        (QUEUE_DEVICE, 0x30),
        (LEN, 0x38),
    ];
    for (got, want) in regs {
        assert_eq!(got, want);
    }
    assert_eq!(
        (
            feature::VERSION_1,
            feature::ACCESS_PLATFORM,
            feature::RING_EVENT_IDX,
            feature::INDIRECT_DESC
        ),
        (1 << 32, 1 << 33, 1 << 29, 1 << 28)
    );
    assert_eq!(
        [
            status::ACKNOWLEDGE,
            status::DRIVER,
            status::DRIVER_OK,
            status::FEATURES_OK,
            status::DEVICE_NEEDS_RESET,
            status::FAILED
        ],
        [1, 2, 4, 8, 64, 128]
    );
    assert_eq!(NO_VECTOR, 0xffff);
    assert_eq!(
        (desc::NEXT, desc::WRITE, desc::INDIRECT, desc::BYTES),
        (1, 2, 4, 16)
    );
}

#[test]
fn a_device_offering_version_1_and_access_platform_gets_both() {
    let mut t = transport(Device::new(
        (1 << 33) | (1 << 32) | (1 << 29) | (1 << 9),
        &[8],
    ));
    t.reset(&mut || true).unwrap();
    let got = t.negotiate(1 << 9).unwrap();
    assert_eq!(
        got,
        (1 << 33) | (1 << 32) | (1 << 9),
        "the required pair and what was wanted, nothing else"
    );
    let d = t.regs.0.borrow();
    assert_eq!(d.accepted, got);
    assert_eq!(d.status, 1 | 2 | 8, "ACKNOWLEDGE, DRIVER, FEATURES_OK");
}

#[test]
fn a_device_without_access_platform_fails_the_bring_up() {
    // a device started without iommu_platform: it would bypass the IOMMU (IOMMU-SPEC 15.1 L1)
    let mut t = transport(Device::new(1 << 32, &[8]));
    t.regs.0.borrow_mut().platform = false;
    t.reset(&mut || true).unwrap();
    assert_eq!(t.negotiate(0), Err(Error::MissingFeature(1 << 33)));
    assert_eq!(
        t.regs.0.borrow().status,
        status::ACKNOWLEDGE | status::DRIVER | status::FAILED,
        "FAILED added, no bit cleared (2.1.2)"
    );
    // nor without VERSION_1
    let mut t = transport(Device::new(1 << 33, &[8]));
    assert_eq!(t.negotiate(0), Err(Error::MissingFeature(1 << 32)));
}

#[test]
fn a_device_that_drops_features_ok_fails_the_bring_up() {
    let mut d = Device::new((1 << 33) | (1 << 32), &[8]);
    d.refuse = true;
    let mut t = transport(d);
    assert_eq!(t.negotiate(0), Err(Error::FeaturesRefused));
    let st = t.regs.0.borrow().status;
    assert_eq!(st & status::FAILED, status::FAILED);
    assert_eq!(
        st & (status::ACKNOWLEDGE | status::DRIVER),
        status::ACKNOWLEDGE | status::DRIVER,
        "no bit cleared (2.1.2)"
    );
}

#[test]
fn reset_waits_for_zero_and_gives_up_when_told() {
    let mut d = Device::new(0, &[]);
    d.slow_reset = 5;
    let mut t = transport(d);
    t.set_status(0x0f);
    let mut asked = 0;
    assert_eq!(
        t.reset(&mut || {
            asked += 1;
            asked < 3
        }),
        Err(Error::ResetTimeout)
    );
    let mut t2 = transport({
        let mut d = Device::new(0, &[]);
        d.slow_reset = 5;
        d
    });
    t2.set_status(0x0f);
    let mut n = 0;
    t2.reset(&mut || {
        n += 1;
        true
    })
    .unwrap();
    assert_eq!(n, 5, "one question per non-zero read");
}

#[test]
fn queue_setup_programs_addresses_and_notifies_at_offset_times_multiplier() {
    let mut t = transport(Device::new((1 << 33) | (1 << 32), &[256, 128, 0]));
    t.negotiate(0).unwrap();
    assert_eq!(t.queue_max(1), 128);
    assert_eq!(
        t.setup_queue(1, 256, (0, 0, 0), 0xffff),
        Err(Error::QueueSize(256)),
        "above the device's maximum"
    );
    assert_eq!(
        t.setup_queue(1, 48, (0, 0, 0), 0xffff),
        Err(Error::QueueSize(48)),
        "not a power of two"
    );
    assert_eq!(
        t.setup_queue(2, 8, (0, 0, 0), 0xffff),
        Err(Error::QueueUnavailable(2)),
        "no such queue"
    );
    let a = (0x100_0000_5000, 0x100_0000_5080, 0x100_0000_5098);
    t.setup_queue(1, 8, a, 0xffff).unwrap();
    assert_eq!(
        t.setup_queue(1, 8, a, 0xffff),
        Err(Error::QueueUnavailable(1)),
        "already enabled"
    );
    {
        let d = t.regs.0.borrow();
        let q = &d.queues[1];
        assert_eq!((q.size, q.vector, q.enable), (8, 0xffff, 1));
        assert_eq!(
            (q.desc, q.driver, q.device),
            a,
            "64-bit I/O addresses, low then high word"
        );
    }
    t.driver_ok();
    assert_eq!(t.status() & status::DRIVER_OK, status::DRIVER_OK);
    t.notify(1);
    t.notify(0); // never set up: nothing written
    assert_eq!(
        *t.regs.0.borrow().notified.borrow(),
        vec![(4, 1)],
        "queue_notify_off 1 times the multiplier 4"
    );
    assert_eq!(t.notify_address(1), Some(NOTIFY + 4));
    assert_eq!(t.isr(), 1);
    t.regs.0.borrow_mut().config.insert(0, 0x10);
    t.regs.0.borrow_mut().config.insert(1, 0x02);
    assert_eq!(t.config_read8(0), Some(0x10));
    assert_eq!(t.config_read32(0), Some(0x210));
    // never outside the device configuration's length, never misaligned
    assert_eq!(t.config_read8(DEVCFG_LEN - 1), Some(0));
    assert_eq!(t.config_read8(DEVCFG_LEN), None);
    assert_eq!(t.config_read32(DEVCFG_LEN - 4), Some(0));
    assert_eq!(t.config_read32(DEVCFG_LEN - 2), None);
    assert_eq!(t.config_read32(2), None);
    assert_eq!(t.config_read8(u64::MAX), None);
    assert_eq!(t.config_read32(u64::MAX - 3), None);
    assert!(!t.needs_reset());
    t.regs.0.borrow_mut().status |= status::DEVICE_NEEDS_RESET;
    assert!(t.needs_reset());
}

#[test]
fn a_notify_offset_outside_the_structure_is_refused() {
    // queue 3: offset 3 * 4 = 12, the last 16-bit slot that fits in 16 bytes; queue 4's would end at 18
    let mut t = transport(Device::new((1 << 33) | (1 << 32), &[8, 8, 8, 8, 8]));
    t.negotiate(0).unwrap();
    t.setup_queue(3, 8, (0, 0, 0), 0xffff).unwrap();
    assert_eq!(t.notify_address(3), Some(NOTIFY + 12));
    assert_eq!(
        t.setup_queue(4, 8, (0, 0, 0), 0xffff),
        Err(Error::NotifyOutside(4))
    );
    assert_eq!(t.notify_address(4), None);
    t.notify(4);
    assert!(
        t.regs.0.borrow().notified.borrow().is_empty(),
        "nothing written outside the structure"
    );
    // a hostile multiplier: the product overflows nothing and is refused
    let mut t = PciTransport::new(
        Bar(RefCell::new(Device::new((1 << 33) | (1 << 32), &[8, 8]))),
        PciLocations {
            common: COMMON,
            notify: NOTIFY,
            notify_len: NOTIFY_LEN,
            notify_multiplier: u32::MAX,
            isr: ISR,
            device: None,
        },
    );
    t.negotiate(0).unwrap();
    assert_eq!(
        t.setup_queue(1, 8, (0, 0, 0), 0xffff),
        Err(Error::NotifyOutside(1))
    );
    assert_eq!(t.config_read8(0), None, "no device configuration");
}

#[test]
fn the_split_queue_layout_is_the_texts_arithmetic() {
    for (n, avail, used, bytes) in [
        // descriptors 16n at 0; available ring 6 + 2n at 16n (aligned to 2); used ring 6 + 8n aligned to 4
        (1u16, 16u64, 24u64, 24 + 14),
        (8, 128, 152, 152 + 70),
        (256, 4096, 4616, 4616 + 2054),
        (32768, 524_288, 589_832, 589_832 + 262_150),
    ] {
        let l = Layout::new(n).unwrap();
        assert_eq!(
            (l.desc, l.avail, l.used, l.bytes),
            (0, avail, used, bytes),
            "size {n}"
        );
        assert_eq!(l.avail_idx(), avail + 2);
        assert_eq!(l.avail_ring(0), avail + 4);
        assert_eq!(
            l.avail_ring(n),
            avail + 4,
            "the ring index wraps at the size"
        );
        assert_eq!(l.used_idx(), used + 2);
        assert_eq!(l.used_elem(1 % n), used + 4 + 8 * u64::from(1 % n));
        assert_eq!(l.desc_at(n - 1), 16 * u64::from(n - 1));
    }
    for bad in [0u16, 3, 48, 65535] {
        assert_eq!(Layout::new(bad), Err(Error::QueueSize(bad)));
    }
}

/// Queue memory as a byte vector.
struct Ram(Vec<u8>);
impl Mem for Ram {
    fn read16(&self, o: u64) -> u16 {
        u16::from_le_bytes(self.0[o as usize..o as usize + 2].try_into().unwrap())
    }
    fn read32(&self, o: u64) -> u32 {
        u32::from_le_bytes(self.0[o as usize..o as usize + 4].try_into().unwrap())
    }
    fn write16(&mut self, o: u64, v: u16) {
        self.0[o as usize..o as usize + 2].copy_from_slice(&v.to_le_bytes())
    }
    fn write32(&mut self, o: u64, v: u32) {
        self.0[o as usize..o as usize + 4].copy_from_slice(&v.to_le_bytes())
    }
    fn write64(&mut self, o: u64, v: u64) {
        self.0[o as usize..o as usize + 8].copy_from_slice(&v.to_le_bytes())
    }
}

/// What the device does: take available entry `k`, return its head with `len` in used slot `k`.
fn device_completes(m: &mut Ram, l: &Layout, k: u16, len: u32) {
    let head = m.read16(l.avail_ring(k));
    let e = l.used_elem(k);
    m.write32(e, u32::from(head));
    m.write32(e + 4, len);
    m.write16(l.used_idx(), k.wrapping_add(1));
}

#[test]
fn a_chain_is_written_made_available_and_taken_back() {
    let io = 0x100_0000_2000;
    let mut q = SplitQueue::new(8, io, 4096).unwrap();
    assert_eq!(
        SplitQueue::new(256, io, 4096).err(),
        Some(Error::NoRoom),
        "6670 bytes do not fit a page"
    );
    let mut m = Ram(vec![0; 4096]);
    let l = q.layout;
    assert_eq!(q.addresses(), (io, io + 128, io + 152));
    let chain = [
        Buf {
            addr: 0x100_0000_3000,
            len: 16,
            device_writes: false,
        },
        Buf {
            addr: 0x100_0000_3200,
            len: 512,
            device_writes: true,
        },
        Buf {
            addr: 0x100_0000_3400,
            len: 1,
            device_writes: true,
        },
    ];
    let head = q.add(&mut m, &chain, false).unwrap();
    assert_eq!(q.free(), 5);
    // the descriptors: address, length, flags (NEXT on all but the last, WRITE where the device writes), next
    let mut id = head;
    for (k, b) in chain.iter().enumerate() {
        let d = l.desc_at(id) as usize;
        assert_eq!(
            u64::from_le_bytes(m.0[d..d + 8].try_into().unwrap()),
            b.addr
        );
        assert_eq!(m.read32(d as u64 + 8), b.len);
        let flags = m.read16(d as u64 + 12);
        assert_eq!(flags & 2 != 0, b.device_writes);
        assert_eq!(flags & 1 != 0, k + 1 < chain.len());
        id = m.read16(d as u64 + 14);
    }
    assert_eq!(
        m.read16(l.avail_flags()),
        1,
        "VIRTQ_AVAIL_F_NO_INTERRUPT: this slice polls"
    );
    assert_eq!(m.read16(l.avail_ring(0)), head);
    assert_eq!(m.read16(l.avail_idx()), 1);
    assert_eq!(q.take_used(&m), Ok(None), "nothing used yet");
    device_completes(&mut m, &l, 0, 513);
    assert_eq!(q.take_used(&m), Ok(Some((head, 513))));
    assert_eq!(q.free(), 8, "the chain's descriptors are free again");
    assert_eq!(q.take_used(&m), Ok(None));
    assert_eq!(q.add(&mut m, &[], false), Err(Error::NoRoom));
    let nine = [chain[0]; 9];
    assert_eq!(
        q.add(&mut m, &nine, false),
        Err(Error::NoRoom),
        "more than the queue holds"
    );
}

#[test]
fn a_used_id_that_is_not_in_flight_is_refused() {
    let mut q = SplitQueue::new(8, 0, 4096).unwrap();
    let mut m = Ram(vec![0; 4096]);
    let l = q.layout;
    let b = [Buf {
        addr: 0x1000,
        len: 8,
        device_writes: true,
    }];
    let head = q.add(&mut m, &b, true).unwrap();
    assert_eq!(m.read16(l.avail_flags()), 0, "interrupts wanted");
    // the device names a descriptor that is free, then one past the table
    for bogus in [u32::from(head) + 1, 8, 0x1_0000] {
        m.write32(l.used_elem(0), bogus);
        m.write16(l.used_idx(), 1);
        let mut q2 = SplitQueue::new(8, 0, 4096).unwrap();
        let mut m2 = Ram(m.0.clone());
        q2.add(&mut m2, &b, false).unwrap();
        m2.write32(l.used_elem(0), bogus);
        m2.write16(l.used_idx(), 1);
        assert_eq!(q2.take_used(&m2), Err(Error::BadUsed(bogus)));
        assert_eq!(q2.free(), 7, "nothing was freed for it");
    }
    // the same id twice: the second is not in flight any more
    m.write32(l.used_elem(0), u32::from(head));
    m.write32(l.used_elem(1), u32::from(head));
    m.write16(l.used_idx(), 2);
    assert_eq!(q.take_used(&m), Ok(Some((head, 0))));
    assert_eq!(q.take_used(&m), Err(Error::BadUsed(u32::from(head))));
}

#[test]
fn the_indexes_wrap_past_65535() {
    let mut q = SplitQueue::new(4, 0, 4096).unwrap();
    let mut m = Ram(vec![0; 4096]);
    let l = q.layout;
    let b = [Buf {
        addr: 0x1000,
        len: 8,
        device_writes: true,
    }];
    for k in 0..70_000u32 {
        let h = q.add(&mut m, &b, false).unwrap();
        device_completes(&mut m, &l, k as u16, k);
        assert_eq!(q.take_used(&m), Ok(Some((h, k))));
    }
    assert_eq!(m.read16(l.avail_idx()), (70_000u32 % 65536) as u16);
    assert_eq!(q.free(), 4);
}

#[test]
fn a_block_request_header_is_type_reserved_sector() {
    assert_eq!(
        blk::header(blk::T_IN, 0x0102_0304_0506_0708),
        [0, 0, 0, 0, 0, 0, 0, 0, 8, 7, 6, 5, 4, 3, 2, 1]
    );
    assert_eq!(&blk::header(blk::T_OUT, 3)[..4], &[1, 0, 0, 0]);
    assert_eq!(
        (
            blk::SECTOR,
            blk::HEADER_BYTES,
            blk::S_OK,
            blk::S_IOERR,
            blk::S_UNSUPP
        ),
        (512, 16, 0, 1, 2)
    );
}

#[test]
fn the_vectors_are_written_and_read_back() {
    // VIRTIO 1.x 4.1.4.3: config_msix_vector at 0x10, queue_msix_vector at 0x1a; 4.1.5.1.2: 0xffff is "no vector"
    let mut t = transport(Device::new((1 << 33) | (1 << 32), &[8, 8, 8]));
    t.negotiate(0).unwrap();
    t.set_config_vector(0).unwrap();
    t.setup_queue(0, 8, (0, 0, 0), 1).unwrap();
    t.setup_queue(1, 8, (0, 0, 0), 2).unwrap();
    {
        let d = t.regs.0.borrow();
        assert_eq!(d.config_vector, 0, "the configuration change on entry 0");
        assert_eq!(
            (d.queues[0].vector, d.queues[1].vector),
            (1, 2),
            "each queue on its entry"
        );
    }
    // unmapping is always possible
    t.set_config_vector(0xffff).unwrap();
    t.setup_queue(2, 8, (0, 0, 0), 0xffff).unwrap();
    assert_eq!(t.regs.0.borrow().queues[2].vector, 0xffff);
}

#[test]
fn a_vector_the_device_refuses_fails_the_bring_up() {
    let mut t = transport(Device::new((1 << 33) | (1 << 32), &[8, 8]));
    t.negotiate(0).unwrap();
    // the device has four entries: entry 4 does not exist and reads back 0xffff
    assert_eq!(
        t.set_config_vector(4),
        Err(Error::VectorRefused {
            queue: None,
            vector: 4
        })
    );
    assert_eq!(
        t.setup_queue(1, 8, (0, 0, 0), 9),
        Err(Error::VectorRefused {
            queue: Some(1),
            vector: 9
        })
    );
    assert_eq!(
        t.regs.0.borrow().queues[1].enable,
        0,
        "a queue whose vector was refused is not enabled"
    );
    // a device with no MSI-X table at all refuses every vector
    let mut d = Device::new((1 << 33) | (1 << 32), &[8]);
    d.table = 0;
    let mut t = transport(d);
    t.negotiate(0).unwrap();
    assert!(matches!(
        t.setup_queue(0, 8, (0, 0, 0), 0),
        Err(Error::VectorRefused { .. })
    ));
    assert!(t.set_config_vector(0).is_err());
}

#[test]
fn a_vector_map_gives_each_queue_its_entry_or_shares_the_last() {
    let m = VectorMap {
        config: 0,
        queues: vec![1, 2],
    };
    assert_eq!((m.queue(0), m.queue(1), m.queue(5)), (1, 2, 2));
    assert_eq!(VectorMap::none().queue(0), 0xffff);
    assert_eq!(VectorMap::none().config, 0xffff);
}
