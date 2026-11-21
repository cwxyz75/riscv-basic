//! PL7: a fault reports how many executable pages it mapped (the kernel then synchronises the instruction caches once). Mapping N code pages
//! one fault at a time counts N; touching them again (a spurious fault) counts none; data pages count none; a fault that maps committed
//! neighbours counts every executable one.
mod common;
use common::FakeMem;
use kcore::aspace::{Access, AddressSpace, Vmos};
use kcore::frame::FrameDb;
use kcore::job::Jobs;
use kcore::pagetable::{PageTable, Perms};
use kcore::Mm;

const BASE: u64 = 0xffff_ffd0_0000_0000;

fn world() -> (Mm<FakeMem>, Vmos, Jobs, AddressSpace) {
    let mut mm = Mm {
        db: FrameDb::new(2000, 300),
        mem: FakeMem::default(),
    };
    let a = AddressSpace::new(PageTable::new(&mut mm).unwrap());
    (mm, Vmos::new(), Jobs::new(), a)
}

fn rx() -> Perms {
    Perms {
        read: true,
        write: false,
        exec: true,
        user: false,
    }
}

#[test]
fn n_code_pages_count_n_and_a_refault_counts_none() {
    let (mut mm, mut vmos, mut jobs, mut a) = world();
    let n = 5u64;
    let id = vmos.create(&mut jobs, n);
    a.map_region(&mut vmos, BASE, n, id, 0, rx()).unwrap();
    let mut total = 0;
    for p in 0..n {
        let m = a
            .handle_fault_mapped(&mut mm, &mut vmos, &mut jobs, BASE + p * 4096, Access::Exec)
            .unwrap();
        assert_eq!(
            m.exec, 1,
            "one new code page per fault (nothing committed around it)"
        );
        total += m.exec;
    }
    assert_eq!(total, n);
    for p in 0..n {
        let m = a
            .handle_fault_mapped(&mut mm, &mut vmos, &mut jobs, BASE + p * 4096, Access::Exec)
            .unwrap();
        assert_eq!(
            m.exec, 0,
            "a page that is already mapped needs no synchronisation"
        );
    }
}

#[test]
fn data_pages_count_none_and_fault_around_counts_every_executable_neighbour() {
    let (mut mm, mut vmos, mut jobs, mut a) = world();
    let data = vmos.create(&mut jobs, 4);
    a.map_region(&mut vmos, BASE, 4, data, 0, Perms::RW)
        .unwrap();
    let m = a
        .handle_fault_mapped(&mut mm, &mut vmos, &mut jobs, BASE, Access::Write)
        .unwrap();
    assert_eq!(m.exec, 0, "data is not code");
    // sixteen committed code pages, mapped by one fault and its fault-around
    let code = vmos.create(&mut jobs, 16);
    vmos.commit(&mut mm, &mut jobs, code, 0, 16).unwrap();
    a.map_region(&mut vmos, BASE + (1 << 21), 16, code, 0, rx())
        .unwrap();
    let m = a
        .handle_fault_mapped(
            &mut mm,
            &mut vmos,
            &mut jobs,
            BASE + (1 << 21) + 3 * 4096,
            Access::Exec,
        )
        .unwrap();
    assert_eq!(
        m.exec, 16,
        "the faulting page and the fifteen committed neighbours"
    );
}
