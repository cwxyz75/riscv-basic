//! CNT1 (no kernel counter wraps): each counter is a u64 that refuses or retires at its end, never wraps. The ends are reached by
//! starting the counter near them (the `ff=` boot arguments do the same in the kernel).
mod common;
use kcore::object::{Object, Registry};
use kcore::sched2::{Error, FixedPrio, SchedAttr};
use kcore::sched_smp::Sched2;

struct Plain;
impl Object for Plain {
    fn held(&self, _out: &mut Vec<kcore::object::ObjId>) {}
}

#[test]
fn thread_ids_pass_the_32_bit_boundary_and_stop_at_the_end() {
    let s = Sched2::new(1, FixedPrio::new);
    s.fast_forward_tid(u64::from(u32::MAX) - 1);
    let attr = SchedAttr {
        prio: 20,
        rr: false,
    };
    let a = s.adopt_current_on(0, attr, None).unwrap();
    let b = s.spawn_on(0, attr, None).unwrap();
    let c = s.spawn_on(0, attr, None).unwrap();
    assert_eq!(a, u64::from(u32::MAX) - 1);
    assert_eq!(b, u64::from(u32::MAX));
    assert_eq!(c, u64::from(u32::MAX) + 1, "no wrap at 2^32");
    s.fast_forward_tid(u64::MAX - 1);
    let last = s.spawn_on(0, attr, None).unwrap();
    assert_eq!(
        last,
        u64::MAX - 1,
        "the last id handed out (u64::MAX itself is never issued)"
    );
    assert_eq!(s.spawn_on(0, attr, None), Err(Error::Exhausted));
    assert_eq!(
        s.spawn_on(0, attr, None),
        Err(Error::Exhausted),
        "and it stays refused"
    );
}

#[test]
fn koids_cross_2_32_and_are_never_reused() {
    let mut r: Registry<Plain> = Registry::new();
    r.fast_forward_koid(u64::from(u32::MAX) - 1);
    let ids: Vec<_> = (0..4).map(|_| r.create(Plain)).collect();
    let koids: Vec<u64> = ids.iter().map(|&i| r.koid(i).unwrap()).collect();
    assert_eq!(
        koids,
        vec![
            u64::from(u32::MAX) - 1,
            u64::from(u32::MAX),
            u64::from(u32::MAX) + 1,
            u64::from(u32::MAX) + 2
        ]
    );
    r.release(ids[0]);
    let again = r.create(Plain);
    assert_eq!(
        r.koid(again),
        Some(u64::from(u32::MAX) + 3),
        "a reused slot gets a new koid"
    );
}

#[test]
fn a_slot_whose_generation_is_exhausted_is_retired_not_wrapped() {
    let mut r: Registry<Plain> = Registry::new();
    r.set_first_generation(u64::MAX - 1);
    let a = r.create(Plain);
    r.release(a);
    let b = r.create(Plain); // the slot at generation MAX
    assert!(r.get(a).is_none(), "the old id is dead");
    r.release(b); // exhausts the slot: retired
    let c = r.create(Plain);
    assert!(
        r.get(b).is_none() && r.get(a).is_none(),
        "no old id comes back to life"
    );
    assert!(r.get(c).is_some());
    r.release(c);
    let d = r.create(Plain);
    assert!(r.get(b).is_none() && r.get(a).is_none());
    assert!(r.get(d).is_some());
}

#[test]
fn a_vmo_slot_whose_generation_is_exhausted_is_retired_not_wrapped() {
    use kcore::aspace::Vmos;
    use kcore::frame::FrameDb;
    use kcore::job::Jobs;
    use kcore::Mm;
    {
        let mut mm = Mm {
            db: FrameDb::new(100, 10),
            mem: common::FakeMem::default(),
        };
        let mut jobs = Jobs::new();
        let mut v = Vmos::new();
        v.set_first_generation(u32::MAX - 2);
        // one slot cycles through its last generations: MAX - 2, MAX - 1, MAX
        let mut dead = Vec::new();
        let mut last = 0;
        for _ in 0..3 {
            v.reserve_one().unwrap();
            let id = v.create(&mut jobs, 1);
            assert!(v.pages(id).is_some());
            assert_eq!(id & 0xffff_ffff, 1, "the same slot is reused");
            last = id;
            v.release(&mut mm, &mut jobs, id).unwrap();
            dead.push(id);
        }
        assert_eq!(last >> 32, u64::from(u32::MAX), "the last one is at MAX");
        // the slot at generation MAX is retired: the next VMO gets a new slot, and no old id is alive
        v.reserve_one().unwrap();
        let fresh = v.create(&mut jobs, 1);
        assert_ne!(fresh & 0xffff_ffff, 1, "an exhausted slot is not reused");
        for id in dead {
            assert!(v.pages(id).is_none(), "id {id:#x} came back to life");
        }
        assert!(v.pages(fresh).is_some());
    }
}
