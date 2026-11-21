//! MM5: a handle table's growth is charged to its job, and a job that cannot pay keeps the table from growing.
use abi::status::Status;
use kcore::handle::HandleTable;
use kcore::ipc::{event_create, Kobjs};

static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn growth_is_charged_in_step_and_a_refusal_stops_the_table_growing_until_it_is_paid() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
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

#[test]
fn growth_and_clamp_counters_move_with_the_tables() {
    use kcore::handle::{clamped_tables, growth_events};
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut reg = Kobjs::new();
    let mut t = HandleTable::new(100);
    let g0 = growth_events();
    event_create(&mut reg, &mut t).unwrap();
    assert!(growth_events() > g0, "a new slot is a growth event");
    // (the counters are process-wide: the tests of this file run one at a time)
    let c0 = clamped_tables();
    t.settle_growth(|_| false);
    assert_eq!(clamped_tables(), c0 + 1, "a refusal clamps the table");
    t.settle_growth(|_| true);
    assert_eq!(clamped_tables(), c0, "a grant lifts it");
    t.settle_growth(|_| false);
    drop(t);
    assert_eq!(
        clamped_tables(),
        c0,
        "a dropped clamped table is not counted any more"
    );
}
