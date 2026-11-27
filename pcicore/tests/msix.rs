//! IOMMU9's acceptance tests for the MSI-X sequence and the supervisor's claim. The device is a model of a function written from the
//! PCI local bus specification 3.0 ("MSI-X capability and table structure"), not from the code: message control is the word at
//! capability + 2 (bit 15 enable, bit 14 function mask, bits 10:0 the table size minus one, bits 13:11 reserved); a table entry is four
//! doublewords (address low, address high, data, vector control with bit 0 the mask and bits 31:1 reserved); the pending-bit array is
//! one bit per entry in quadwords. The model checks at every access what a real device requires: no access outside the BAR; no
//! entry's address or data written while the entry is unmasked and MSI-X is enabled with the function unmasked; the reserved bits
//! of message control and of every vector control preserved; the table size read-only.
//!
//! The claim model runs random claims and drops over devices with different table sizes and driver jobs whose vector limit an
//! ancestor may cap, with a failure injected at every kernel call: a claim gets exactly its class's vectors or fails whole, nothing
//! is leaked, a job has both limits before any domain is made for it, and no device is left with MSI-X enabled and an entry that is
//! unmasked but not a live granted vector of its own domain.
use pcicore::claim::{self, Claim, Failed, Irq, Kernel, Plan};
use pcicore::*;
use std::collections::{BTreeMap, BTreeSet};

/// xorshift64*: a fixed seed per test, so a failure reproduces.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// The capability's offset in the model's configuration space, and message control's (capability + 2, from the text).
const CAP: u8 = 0x40;
const CTL: u16 = 0x42;
const COMMAND: u16 = 0x04;
const INTX_DISABLE: u16 = 1 << 10;
const ENABLE: u16 = 1 << 15;
const FMASK: u16 = 1 << 14;
const RESERVED_CTL: u16 = 0b111 << 11;

/// A function with MSI-X: its table in BAR 1 at offset 0 and its pending-bit array in BAR `pba_bar` at `pba_offset` (QEMU's
/// virtio-pci layout is BAR 1, 0x800). `entries` is how many the hardware really has; the capability says `cap_size`.
#[derive(Clone)]
struct Function {
    ctl: u16,
    command: u16,
    cap_size: u16,
    table: Vec<[u32; 4]>,
    /// The reserved bits (31:1) of each vector control, as the device keeps them.
    vc_reserved: Vec<u32>,
    pba: Vec<u32>,
    bar1: u64,
    pba_bar: u8,
    pba_offset: u32,
    accesses: usize,
}

impl Function {
    fn new(r: &mut Rng, entries: u16, cap_size: u16) -> Function {
        let mut f = Function {
            ctl: (cap_size - 1) | (r.below(8) as u16) << 11,
            command: 0x0002 | (r.below(2) as u16) << 4,
            cap_size,
            table: (0..entries)
                .map(|_| [r.next() as u32, r.next() as u32, r.next() as u32, 1])
                .collect(),
            vc_reserved: (0..entries).map(|_| (r.next() as u32) & !1).collect(),
            pba: vec![0; usize::from(entries).div_ceil(32).max(2)],
            bar1: 4096,
            pba_bar: 1,
            pba_offset: 0x800,
            accesses: 0,
        };
        for (e, res) in f.table.iter_mut().zip(&f.vc_reserved) {
            e[3] = res | 1;
        }
        if r.below(3) == 0 {
            f.pba_bar = 2; // (the pending bits in a BAR of their own)
            f.pba_offset = 0;
        }
        f
    }
    fn msix(&self) -> Msix {
        Msix {
            cap: CAP,
            table_size: self.cap_size,
            table_bar: 1,
            table_offset: 0,
            pba_bar: self.pba_bar,
            pba_offset: self.pba_offset,
        }
    }
    fn bars(&self) -> Vec<Bar> {
        let mut b = vec![Bar {
            index: 1,
            kind: BarKind::Mem32 {
                prefetchable: false,
            },
            size: self.bar1,
        }];
        if self.pba_bar == 2 {
            b.push(Bar {
                index: 2,
                kind: BarKind::Mem32 {
                    prefetchable: false,
                },
                size: 4096,
            });
        }
        b
    }
    fn enabled(&self) -> bool {
        self.ctl & ENABLE != 0
    }
    fn delivering(&self) -> bool {
        self.ctl & ENABLE != 0 && self.ctl & FMASK == 0
    }
    /// The entries a message of the device would go out through: (index, address, data).
    fn live(&self) -> Vec<(u16, u64, u32)> {
        if !self.delivering() {
            return vec![];
        }
        self.table
            .iter()
            .enumerate()
            .filter(|(_, e)| e[3] & 1 == 0)
            .map(|(i, e)| (i as u16, u64::from(e[0]) | u64::from(e[1]) << 32, e[2]))
            .collect()
    }
    fn quiet(&self) -> bool {
        !self.enabled()
            && self
                .table
                .iter()
                .all(|e| e[3] & 1 == 1 && e[0] == 0 && e[1] == 0 && e[2] == 0)
    }
}

impl MsixAccess for Function {
    fn cfg_read16(&mut self, off: u16) -> u16 {
        self.accesses += 1;
        match off {
            CTL => self.ctl,
            COMMAND => self.command,
            o => panic!("configuration read at {o:#x}"),
        }
    }
    fn cfg_write16(&mut self, off: u16, v: u16) {
        self.accesses += 1;
        match off {
            CTL => {
                assert_eq!(
                    v & RESERVED_CTL,
                    self.ctl & RESERVED_CTL,
                    "message control's reserved bits are preserved"
                );
                // (the table size is read-only: a write does not change it)
                self.ctl = (v & (ENABLE | FMASK | RESERVED_CTL)) | (self.ctl & 0x7ff);
            }
            COMMAND => {
                assert_eq!(
                    v & !INTX_DISABLE,
                    self.command & !INTX_DISABLE,
                    "only INTx disable changes in the command register"
                );
                self.command = v;
            }
            o => panic!("configuration write at {o:#x}"),
        }
    }
    fn bar_read32(&mut self, bar: u8, off: u64) -> u32 {
        self.accesses += 1;
        assert_eq!(off % 4, 0, "aligned");
        if bar == self.pba_bar && off >= u64::from(self.pba_offset) {
            let w = (off - u64::from(self.pba_offset)) / 4;
            assert!(
                w < self.pba.len() as u64 + 2,
                "a read past the pending bits"
            );
            return self.pba.get(w as usize).copied().unwrap_or(0);
        }
        assert_eq!(bar, 1, "the table's BAR");
        assert!(off + 4 <= self.bar1, "a read outside BAR 1 at {off:#x}");
        let (i, w) = ((off / 16) as usize, (off % 16 / 4) as usize);
        assert!(
            i < self.table.len(),
            "entry {i} does not exist (the capability lies)"
        );
        self.table[i][w]
    }
    fn bar_write32(&mut self, bar: u8, off: u64, v: u32) {
        self.accesses += 1;
        assert_eq!(bar, 1, "only the table is written");
        assert!(off + 4 <= self.bar1, "a write outside BAR 1 at {off:#x}");
        assert!(
            off < u64::from(self.pba_offset) || self.pba_bar != 1,
            "a write to the pending bits"
        );
        let (i, w) = ((off / 16) as usize, (off % 16 / 4) as usize);
        assert!(
            i < self.table.len(),
            "entry {i} does not exist (the capability lies)"
        );
        if w < 3 {
            assert!(
                !(self.delivering() && self.table[i][3] & 1 == 0),
                "entry {i}'s address or data written while it is unmasked and MSI-X is on"
            );
            // (the ticket's sequence is stricter than the device: the whole function is masked or MSI-X is off while any entry is
            // written, so a stale entry not reached yet can never send meanwhile)
            assert!(
                !self.delivering(),
                "entry {i} written while the function delivers"
            );
        } else {
            assert_eq!(
                v & !1,
                self.vc_reserved[i],
                "entry {i}'s vector control keeps its reserved bits"
            );
        }
        self.table[i][w] = v;
    }
}

fn entry(address: u64, data: u32) -> MsixEntry {
    MsixEntry {
        address,
        data,
        masked: false,
    }
}

#[test]
fn the_table_is_programmed_with_the_function_masked_and_msix_comes_on_last() {
    let mut r = Rng(0x1357_9bdf_2468_ace0);
    for _ in 0..2000 {
        let size = 1 + r.below(64) as u16;
        let mut f = Function::new(&mut r, size, size);
        // a function left as an earlier claim may have left it: on, unmasked, with stale entries live
        if r.below(2) == 0 {
            f.ctl |= ENABLE;
            for e in &mut f.table {
                e[3] &= !1;
            }
        }
        let mut want: Vec<(u16, MsixEntry)> = Vec::new();
        for i in 0..size {
            if r.below(3) == 0 {
                let mut e = entry(0x1000_0000 + u64::from(i) * 4096, 1);
                e.masked = r.below(5) == 0;
                want.push((i, e));
            }
        }
        let (m, bars) = (f.msix(), f.bars());
        msix_enable(&m, &bars, &want, &mut f).unwrap();
        assert!(
            f.enabled() && f.delivering(),
            "MSI-X on, the function unmasked"
        );
        assert_ne!(f.command & INTX_DISABLE, 0, "INTx disabled");
        for i in 0..size {
            let e = f.table[usize::from(i)];
            match want.iter().find(|(j, _)| *j == i) {
                Some((_, w)) => {
                    assert_eq!(
                        (u64::from(e[0]) | u64::from(e[1]) << 32, e[2], e[3] & 1 == 1),
                        (w.address, w.data, w.masked),
                        "entry {i}"
                    );
                }
                None => assert_eq!(
                    (e[0], e[1], e[2], e[3] & 1),
                    (0, 0, 0, 1),
                    "entry {i}, not named: masked and cleared"
                ),
            }
        }
        let live: Vec<u16> = f.live().iter().map(|l| l.0).collect();
        let named: Vec<u16> = want
            .iter()
            .filter(|(_, e)| !e.masked)
            .map(|(i, _)| *i)
            .collect();
        assert_eq!(live, named, "exactly the named, unmasked entries can send");
        // and back: the function masked, MSI-X off, every entry masked and cleared
        msix_disable(&m, &bars, &mut f).unwrap();
        assert!(f.quiet(), "disabled and every entry cleared");
        assert_ne!(f.ctl & FMASK, 0, "the function is left masked");
    }
}

#[test]
fn a_table_the_capability_lies_about_or_a_bad_index_writes_nothing() {
    let mut r = Rng(0xdead_beef_0bad_f00d);
    for _ in 0..500 {
        let real = 1 + r.below(16) as u16;
        // the capability says more entries than fit before the pending bits, or than the BAR holds
        let lie = [129, 300, 2048][r.below(3) as usize];
        let mut f = Function::new(&mut r, real, lie);
        f.pba_bar = 1;
        f.pba_offset = 0x800;
        let (m, bars) = (f.msix(), f.bars());
        assert!(msix_enable(&m, &bars, &[(0, entry(0x1000_0000, 1))], &mut f).is_err());
        assert!(msix_disable(&m, &bars, &mut f).is_err());
        assert!(msix_pending(&m, &bars, &mut f).is_err());
        assert_eq!(
            f.accesses, 0,
            "the table is checked against its BAR before any access"
        );
        // an index outside the table, or named twice
        let mut f = Function::new(&mut r, real, real);
        let (m, bars) = (f.msix(), f.bars());
        assert_eq!(
            msix_enable(&m, &bars, &[(real, entry(0x1000_0000, 1))], &mut f),
            Err(Error::MsixEntry(real))
        );
        assert_eq!(
            msix_enable(
                &m,
                &bars,
                &[(0, entry(0x1000_0000, 1)), (0, entry(0x1000_1000, 1))],
                &mut f
            ),
            Err(Error::MsixEntry(0))
        );
        assert_eq!(f.accesses, 0, "a refused programming makes no access");
    }
}

#[test]
fn the_pending_bits_are_read_for_the_table_only() {
    let mut r = Rng(0x0f0f_1234_5678_9abc);
    for _ in 0..300 {
        let size = 1 + r.below(70) as u16;
        let mut f = Function::new(&mut r, size, size);
        let mut want = Vec::new();
        for w in &mut f.pba {
            *w = r.next() as u32;
        }
        for i in 0..(f.pba.len() * 32) {
            if f.pba[i / 32] & (1 << (i % 32)) != 0 && i < usize::from(size) {
                want.push(i as u16);
            }
        }
        let (m, bars) = (f.msix(), f.bars());
        assert_eq!(msix_pending(&m, &bars, &mut f).unwrap(), want);
    }
}

// ---- the claim, over a model of the kernel ------------------------------------------------------------------------------------------

/// The I/O address of vector `k`'s page (IOMMU-SPEC 8.2: `IO_BASE + k * 4096`; any base serves the model).
const IO_BASE: u64 = 0x4000_0000_0000;
const MSI_DATA: u32 = 1;

#[derive(Debug)]
enum Obj {
    Domain { dev: usize, job: usize, live: bool },
    Vector { domain: u64, index: u16 },
    Dma { job: usize, pages: u32 },
}

/// The kernel as the claim sees it: handles, domains, granted vectors and the two job limits, hierarchical (a job's effective vector
/// limit is the smaller of its own and its parent's cap).
#[derive(Default)]
struct World {
    next: u64,
    handles: BTreeMap<u64, Obj>,
    /// job -> (pages limit, vectors limit) once set
    limits: BTreeMap<usize, (u64, u64)>,
    /// job -> the cap an ancestor puts on its vectors
    parent_cap: BTreeMap<usize, u64>,
    /// live granted vectors: (domain, index)
    granted: BTreeSet<(u64, u16)>,
    /// The kernel call that fails (counted down; 0 fails).
    fail_in: Option<u32>,
}

impl World {
    fn new_handle(&mut self, o: Obj) -> u64 {
        self.next += 1;
        self.handles.insert(self.next, o);
        self.next
    }
    fn fails(&mut self) -> bool {
        match self.fail_in {
            Some(0) => {
                self.fail_in = None;
                true
            }
            Some(n) => {
                self.fail_in = Some(n - 1);
                false
            }
            None => false,
        }
    }
    fn vectors_of(&self, job: usize) -> u64 {
        self.granted
            .iter()
            .filter(|(d, _)| matches!(self.handles.get(d), Some(Obj::Domain { job: j, .. }) if *j == job))
            .count() as u64
    }
    fn revoke_all(&mut self, domain: u64) {
        self.granted.retain(|(d, _)| *d != domain);
    }
}

/// One claim's view: the device, the job.
struct ForClaim<'a> {
    w: &'a mut World,
    dev: usize,
    job: usize,
}

impl Kernel for ForClaim<'_> {
    fn set_io_limits(&mut self, pages: u64, vectors: u64) -> bool {
        if self.w.fails() {
            return false;
        }
        self.w.limits.insert(self.job, (pages, vectors));
        true
    }
    fn domain_create(&mut self) -> Option<u64> {
        let (pages, vectors) = *self
            .w
            .limits
            .get(&self.job)
            .expect("a domain is made only for a job with both limits set (IOMMU-SPEC 5.3, 9)");
        assert!(pages < u64::MAX && vectors < u64::MAX, "finite limits");
        if self.w.fails() {
            return None;
        }
        let (dev, job) = (self.dev, self.job);
        assert!(
            !self
                .w
                .handles
                .values()
                .any(|o| matches!(o, Obj::Domain { dev: d, live: true, .. } if *d == dev)),
            "one live domain per device"
        );
        Some(self.w.new_handle(Obj::Domain {
            dev,
            job,
            live: true,
        }))
    }
    fn dma_create(&mut self, pages: u32) -> Option<u64> {
        if self.w.fails() {
            return None;
        }
        let job = self.job;
        Some(self.w.new_handle(Obj::Dma { job, pages }))
    }
    fn grant(&mut self, domain: u64, index: u16) -> Option<(u64, u64)> {
        if self.w.fails() {
            return None;
        }
        let Some(Obj::Domain {
            live: true, job, ..
        }) = self.w.handles.get(&domain)
        else {
            return None;
        };
        let job = *job;
        let cap = self.w.limits[&job]
            .1
            .min(*self.w.parent_cap.get(&job).unwrap_or(&u64::MAX));
        if self.w.granted.contains(&(domain, index)) || self.w.vectors_of(job) + 1 > cap {
            return None; // BAD_STATE, NO_MEMORY
        }
        self.w.granted.insert((domain, index));
        let h = self.w.new_handle(Obj::Vector { domain, index });
        Some((h, IO_BASE + u64::from(index) * 4096))
    }
    fn domain_destroy(&mut self, domain: u64) -> bool {
        if let Some(Obj::Domain { live, .. }) = self.w.handles.get_mut(&domain) {
            *live = false;
        }
        self.w.revoke_all(domain);
        true
    }
    fn close(&mut self, h: u64) {
        match self.w.handles.remove(&h).expect("a handle closed twice") {
            Obj::Vector { domain, index } => {
                self.w.granted.remove(&(domain, index));
            }
            Obj::Domain { .. } => self.w.revoke_all(h),
            Obj::Dma { .. } => {}
        }
    }
}

struct Dev {
    f: Function,
    plan: &'static Plan,
    job: usize,
    claim: Option<Claim>,
}

/// Everything the world must say after any operation.
fn check(w: &World, devs: &[Dev]) {
    // nothing leaked: the live handles are exactly those of the live claims
    let mut held = BTreeSet::new();
    for d in devs {
        if let Some(c) = &d.claim {
            held.insert(c.domain);
            held.extend(c.dma.iter().copied());
            held.extend(c.irqs.iter().map(|x| x.1));
        }
    }
    let live: BTreeSet<u64> = w.handles.keys().copied().collect();
    assert_eq!(live, held, "every handle made belongs to a live claim");
    for (i, d) in devs.iter().enumerate() {
        match &d.claim {
            Some(c) => {
                // exactly the class's vectors, granted on its own live domain, each with its table entry live
                let got: Vec<Irq> = c.irqs.iter().map(|x| x.0).collect();
                assert_eq!(got, d.plan.irqs, "device {i}: the class's vectors");
                let mut want: Vec<(u16, u64, u32)> = d
                    .plan
                    .irqs
                    .iter()
                    .map(|q| (q.entry(), IO_BASE + u64::from(q.entry()) * 4096, MSI_DATA))
                    .collect();
                want.sort();
                assert_eq!(d.f.live(), want, "device {i}: the entries that can send");
                assert_ne!(d.f.command & INTX_DISABLE, 0);
                let pages: Vec<u32> = c
                    .dma
                    .iter()
                    .map(|h| match w.handles[h] {
                        Obj::Dma { pages, job } => {
                            assert_eq!(job, d.job, "the DMA is charged to the driver's job");
                            pages
                        }
                        _ => panic!("not a DMA VMO"),
                    })
                    .collect();
                assert_eq!(pages, d.plan.dma_pages);
                assert_eq!(w.limits[&d.job], (d.plan.io_pages(), d.plan.vectors()));
            }
            None => assert!(
                d.f.live().is_empty(),
                "device {i}: unclaimed, nothing can send"
            ),
        }
        // never enabled with an entry that is unmasked but not a granted live vector of the device's own domain
        let dom = d.claim.as_ref().map(|c| c.domain);
        for (k, address, data) in d.f.live() {
            assert!(
                dom.is_some_and(|dm| w.granted.contains(&(dm, k)))
                    && address == IO_BASE + u64::from(k) * 4096
                    && data == MSI_DATA,
                "device {i}: entry {k} can send to {address:#x} without a grant"
            );
        }
    }
}

fn do_claim(w: &mut World, d: &mut Dev, dev: usize) -> Result<(), Failed> {
    let (m, bars) = (d.f.msix(), d.f.bars());
    let mut k = ForClaim { w, dev, job: d.job };
    let c = claim::claim(d.plan, &m, &bars, MSI_DATA, &mut k, &mut d.f)?;
    d.claim = Some(c);
    Ok(())
}

fn do_release(w: &mut World, d: &mut Dev, dev: usize) {
    let Some(c) = d.claim.take() else { return };
    let (m, bars) = (d.f.msix(), d.f.bars());
    msix_disable(&m, &bars, &mut d.f).unwrap();
    let mut k = ForClaim { w, dev, job: d.job };
    assert!(claim::release(c, &mut k));
}

#[test]
fn claims_get_every_vector_of_their_class_or_fail_whole_and_leak_nothing() {
    let plans: [&'static Plan; 3] = [&claim::BLOCK, &claim::NET, &claim::ENTROPY];
    let mut r = Rng(0x005e_ed0f_1077_9e55);
    let (mut ok, mut failed) = (0u32, BTreeMap::<String, u32>::new());
    for round in 0..300 {
        let mut w = World::default();
        let mut devs: Vec<Dev> = (0..4)
            .map(|i| {
                let size = 1 + r.below(6) as u16;
                // one device in ten has a capability that lies about its table
                let cap = if r.below(10) == 0 { 300 } else { size };
                let mut f = Function::new(&mut r, size, cap);
                f.pba_bar = 1;
                f.pba_offset = 0x800;
                Dev {
                    f,
                    plan: plans[r.below(3) as usize],
                    job: round * 10 + i,
                    claim: None,
                }
            })
            .collect();
        for d in &devs {
            if r.below(3) == 0 {
                w.parent_cap.insert(d.job, r.below(4));
            }
        }
        for _ in 0..40 {
            let i = r.below(devs.len() as u64) as usize;
            if devs[i].claim.is_some() {
                do_release(&mut w, &mut devs[i], i);
            } else {
                // a failure at a random kernel call, or none
                w.fail_in = (r.below(2) == 0).then(|| r.below(8) as u32);
                let before = devs[i].f.table.clone();
                let was_quiet = devs[i].f.quiet();
                match do_claim(&mut w, &mut devs[i], i) {
                    Ok(()) => ok += 1,
                    Err(e) => {
                        *failed
                            .entry(format!("{e:?}").split('(').next().unwrap().to_string())
                            .or_default() += 1;
                        assert!(devs[i].claim.is_none());
                        if matches!(
                            e,
                            Failed::TableTooSmall { .. } | Failed::Limits | Failed::Domain
                        ) || devs[i].f.cap_size > devs[i].f.table.len() as u16
                        {
                            assert_eq!(devs[i].f.table, before, "nothing was written");
                        } else if was_quiet {
                            assert!(
                                devs[i].f.quiet(),
                                "MSI-X turned off again and the table cleared"
                            );
                        }
                    }
                }
                w.fail_in = None;
            }
            check(&w, &devs);
        }
        for (i, d) in devs.iter_mut().enumerate() {
            do_release(&mut w, d, i);
        }
        check(&w, &devs);
        assert!(
            w.handles.is_empty() && w.granted.is_empty(),
            "everything given back"
        );
    }
    // the random walk reached every way out
    for kind in [
        "TableTooSmall { has",
        "Limits",
        "Domain",
        "Dma",
        "Grant",
        "Msix",
    ] {
        assert!(
            failed
                .keys()
                .any(|k| k.starts_with(kind.split(' ').next().unwrap())),
            "no claim failed at {kind}: {failed:?}"
        );
    }
    assert!(ok > 100, "{ok} claims succeeded");
}

#[test]
fn a_vector_limit_below_the_class_fails_the_claim_at_the_grant() {
    let mut r = Rng(7);
    let mut f = Function::new(&mut r, 4, 4);
    f.pba_bar = 1;
    f.pba_offset = 0x800;
    let mut w = World::default();
    w.parent_cap.insert(1, 2);
    let mut d = Dev {
        f,
        plan: &claim::NET,
        job: 1,
        claim: None,
    };
    assert_eq!(
        do_claim(&mut w, &mut d, 0),
        Err(Failed::Grant(Irq::Queue(1))),
        "the third vector is over the cap: no vector is silently shared"
    );
    check(&w, std::slice::from_ref(&d));
    assert!(w.handles.is_empty());
}

#[test]
fn the_class_table() {
    // entry 0 the configuration change, entry n + 1 queue n
    assert_eq!(
        (
            Irq::Config.entry(),
            Irq::Queue(0).entry(),
            Irq::Queue(1).entry()
        ),
        (0, 1, 2)
    );
    for (p, pages, vectors, table) in [
        (&claim::BLOCK, 5 + claim::IO_MARGIN_PAGES, 2, 2),
        (&claim::NET, 130 + claim::IO_MARGIN_PAGES, 3, 3),
        (&claim::ENTROPY, 2 + claim::IO_MARGIN_PAGES, 1, 2),
    ] {
        assert_eq!(
            (p.io_pages(), p.vectors(), p.table_needed()),
            (pages, vectors, table),
            "{}",
            p.class
        );
    }
    assert_eq!(claim::for_virtio(2), Some(&claim::BLOCK));
    assert_eq!(claim::for_virtio(1), Some(&claim::NET));
    assert_eq!(claim::for_virtio(4), Some(&claim::ENTROPY));
    assert_eq!(claim::for_virtio(3), None);
}
