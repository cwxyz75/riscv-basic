//! A fake machine for the ksys tests: the kernel's real address space, usercopy and job ledger over a Vec-backed PhysMem.
#![allow(dead_code)]
use abi::rights::Rights;
use abi::status::Status;
use abi::syscall as nr;
use kcore::aspace::{AddressSpace, Vmos};
use kcore::frame::{FrameDb, Pfn};
use kcore::handle::HandleTable;
use kcore::ipc::Kobjs;
use kcore::job::JobId;
use kcore::pagetable::{PageTable, Perms, PhysMem};
use kcore::Mm;
use ksys::{ipc_call, job_call, vmo_call, Ctx, Faults, Reply};
use std::collections::HashMap;

/// Physical memory as a map from frame number to 512 words, created on demand (the kernel's direct map, on the host).
#[derive(Default)]
pub struct FakeMem {
    frames: HashMap<Pfn, Box<[u64; 512]>>,
}

impl PhysMem for FakeMem {
    fn read_u64(&self, pa: u64) -> u64 {
        self.frames
            .get(&(pa >> 12))
            .map_or(0xdead_beef_dead_beef, |f| f[((pa & 0xfff) / 8) as usize])
    }
    fn write_u64(&mut self, pa: u64, value: u64) {
        self.frames
            .entry(pa >> 12)
            .or_insert_with(|| Box::new([0; 512]))[((pa & 0xfff) / 8) as usize] = value;
    }
}

/// A user page range mapped read-write (16 pages, lazily backed) and one mapped read-only page.
pub const BASE: u64 = 0x1_0000;
pub const RO: u64 = 0x20_0000;

pub struct World {
    pub objs: Kobjs,
    pub handles: HandleTable,
    pub mm: Mm<FakeMem>,
    pub vmos: Vmos,
    pub aspace: AddressSpace,
    pub job: JobId,
    pub faults: Faults,
}

pub fn world_with(handles: usize) -> World {
    let mut mm = Mm {
        db: FrameDb::new(3000, 400),
        mem: FakeMem::default(),
    };
    let kernel = PageTable::new(&mut mm).unwrap();
    let mut aspace = AddressSpace::new(PageTable::new_with_kernel_half(&mut mm, &kernel).unwrap());
    let mut vmos = Vmos::new();
    let rw = vmos.create(16);
    aspace
        .map_region(&vmos, BASE, 16, rw, 0, Perms::RW.user())
        .unwrap();
    let ro = vmos.create(1);
    aspace
        .map_region(&vmos, RO, 1, ro, 0, Perms::R.user())
        .unwrap();
    World {
        objs: Kobjs::new(),
        handles: HandleTable::new(handles),
        mm,
        vmos,
        aspace,
        job: JobId::ROOT,
        faults: Faults::default(),
    }
}

pub fn world() -> World {
    world_with(64)
}

impl World {
    pub fn call(&mut self, n: usize, a: [u64; 6]) -> Option<Reply> {
        let mut c = Ctx {
            objs: &mut self.objs,
            handles: &mut self.handles,
            job: self.job,
            aspace: &mut self.aspace,
            mm: &mut self.mm,
            vmos: &mut self.vmos,
            faults: self.faults,
            harts_online: 4,
        };
        ipc_call(n, a, &mut c)
            .or_else(|| vmo_call(n, a, &mut c))
            .or_else(|| job_call(n, a, &mut c))
    }
    pub fn ok(&mut self, n: usize, a: [u64; 6]) -> (Option<u64>, Option<u64>) {
        self.call(n, a).expect("handled").expect("succeeds")
    }
    pub fn err(&mut self, n: usize, a: [u64; 6]) -> Status {
        self.call(n, a).expect("handled").expect_err("fails").0
    }
    pub fn pair(&mut self, n: usize) -> (u64, u64) {
        let (x, y) = self.ok(n, [0; 6]);
        (x.unwrap(), y.unwrap())
    }
    pub fn poke(&mut self, at: u64, d: &[u8]) {
        kcore::usercopy::copy_to_user(&mut self.aspace, &mut self.mm, &mut self.vmos, at, d)
            .unwrap();
    }
    pub fn peek(&mut self, at: u64, n: usize) -> Vec<u8> {
        kcore::usercopy::copy_from_user(&mut self.aspace, &mut self.mm, &mut self.vmos, at, n)
            .unwrap()
    }
    /// A duplicate of `h` with exactly `rights`.
    pub fn narrow(&mut self, h: u64, rights: Rights) -> u64 {
        self.ok(nr::HANDLE_DUPLICATE, [h, u64::from(rights.0), 0, 0, 0, 0])
            .0
            .unwrap()
    }
}
