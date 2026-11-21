//! MM5: a handle table's growth is charged to its job, and a job that cannot pay keeps the table from growing.
use abi::status::Status;
use kcore::handle::HandleTable;
use kcore::ipc::{event_create, Kobjs};

#[test]
fn growth_is_charged_in_step_and_a_refusal_stops_the_table_growing_until_it_is_paid() {
    let mut reg = Kobjs::new();
    let mut t = HandleTable::new(1000);
    let mut budget: i64 = 400; // bytes the "job" can still pay
    let mut handles = vec![];
    let mut refused_at = None;
    for i in 0..200 {
        match event_create(&mut reg, &mut t) {
            Ok(h) => handles.push(h),
            Err(e) => {
                assert_eq!(e, Status::NoMemory);
                refused_at.get_or_insert(i);
                break;
            }
        }
        t.settle_growth(|n| {
            if budget >= n as i64 {
                budget -= n as i64;
                true
            } else {
                false
            }
        });
        assert!(t.charged() <= 400, "never more than the job could pay");
    }
    let n = refused_at.expect("the table stopped growing");
    assert!(n > 2, "a few handles fit ({n})");
    // a slot freed and reused is not growth: the table can still turn over
    let h = handles.pop().unwrap();
    t.close(&mut reg, h).unwrap();
    assert!(
        event_create(&mut reg, &mut t).is_ok(),
        "a free slot is usable at the limit"
    );
    // the job gets room: the pending growth is paid and the table grows again
    budget = 10_000;
    t.settle_growth(|n| {
        budget -= n as i64;
        true
    });
    assert!(event_create(&mut reg, &mut t).is_ok());
    t.settle_growth(|_| true);
    assert_eq!(
        t.charged(),
        t.slot_bytes(),
        "everything the table holds is paid for"
    );
}
