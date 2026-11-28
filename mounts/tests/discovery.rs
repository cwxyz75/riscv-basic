//! `volmgr`'s retry decision (T286, T209): Busy-then-probe-fails is retried within its bound; a non-volume is not; a failing
//! device that was never Busy is not; nothing is asked past the device's bound. (Kani proves the same over every run of up to eight
//! answers: `cargo kani -p mounts`.)

use mounts::discovery::{
    Device, Next, Opened, Probed, BUSY_MS, DEVICE_BOUND_MS, PROBE_RETRY_MS, READY_MS,
};

#[test]
fn a_volume_is_used_and_a_non_volume_is_not_probed_again() {
    let mut d = Device::new(100);
    assert_eq!(d.opened(Opened::Ok, 100), Next::Probe);
    assert_eq!(d.probed(Probed::Volume, 110), Next::Use);
    let mut d = Device::new(100);
    assert_eq!(d.opened(Opened::Ok, 100), Next::Probe);
    assert_eq!(d.probed(Probed::NotVolume, 110), Next::Ignore);
    // even after a Busy: a readable device that holds no volume is a verdict
    let mut d = Device::new(100);
    assert_eq!(d.opened(Opened::Busy, 100), Next::OpenAgain);
    assert_eq!(d.opened(Opened::Ok, 200), Next::Probe);
    assert_eq!(d.probed(Probed::NotVolume, 210), Next::Ignore);
}

#[test]
fn the_end_of_the_list_and_a_refused_open() {
    let mut d = Device::new(0);
    assert_eq!(d.opened(Opened::NoDevice, 0), Next::End);
    let mut d = Device::new(0);
    assert_eq!(d.opened(Opened::Failed, 0), Next::Ignore);
}

#[test]
fn busy_is_waited_for_its_bound_and_no_longer() {
    let mut d = Device::new(1000);
    for t in (1000..1000 + BUSY_MS).step_by(250) {
        assert_eq!(d.opened(Opened::Busy, t), Next::OpenAgain, "at {t}");
    }
    assert_eq!(d.opened(Opened::Busy, 1000 + BUSY_MS), Next::Ignore);
    assert!(d.was_busy());
}

#[test]
fn busy_then_unreadable_is_retried_within_its_bound() {
    // the T209 shape: the old driver winds down (Busy), the first probe of the new one fails, a later one finds the volume
    let mut d = Device::new(0);
    assert_eq!(d.opened(Opened::Busy, 0), Next::OpenAgain);
    assert_eq!(d.opened(Opened::Ok, 300), Next::Probe);
    assert_eq!(d.probed(Probed::Unreadable, 350), Next::OpenAgain);
    // a Busy while the failed driver winds down is waited for too
    assert_eq!(d.opened(Opened::Busy, 400), Next::OpenAgain);
    assert_eq!(d.opened(Opened::Ok, 600), Next::Probe);
    assert_eq!(d.probed(Probed::Volume, 650), Next::Use);
}

#[test]
fn an_unreadable_device_is_given_up_when_its_window_closes() {
    let mut d = Device::new(0);
    assert_eq!(d.opened(Opened::Busy, 0), Next::OpenAgain);
    assert_eq!(d.opened(Opened::Ok, 100), Next::Probe);
    assert_eq!(d.probed(Probed::Unreadable, 200), Next::OpenAgain);
    let mut t = 200;
    while t < 200 + PROBE_RETRY_MS - 100 {
        t += 100;
        assert_eq!(d.opened(Opened::Ok, t), Next::Probe);
        assert_eq!(d.probed(Probed::Unreadable, t), Next::OpenAgain, "at {t}");
    }
    assert_eq!(d.opened(Opened::Ok, 200 + PROBE_RETRY_MS), Next::Probe);
    assert_eq!(
        d.probed(Probed::Unreadable, 200 + PROBE_RETRY_MS),
        Next::Ignore
    );
    // and a Busy past the window is not waited for either
    let mut d = Device::new(0);
    assert_eq!(d.opened(Opened::Busy, 0), Next::OpenAgain);
    assert_eq!(d.opened(Opened::Ok, 10), Next::Probe);
    assert_eq!(d.probed(Probed::Unreadable, 20), Next::OpenAgain);
    assert_eq!(d.opened(Opened::Busy, 20 + PROBE_RETRY_MS), Next::Ignore);
}

#[test]
fn an_unreadable_device_that_was_never_busy_is_a_failing_device() {
    let mut d = Device::new(0);
    assert_eq!(d.opened(Opened::Ok, 0), Next::Probe);
    assert_eq!(d.probed(Probed::Unreadable, 10), Next::Ignore);
    assert!(!d.was_busy());
}

#[test]
fn nothing_is_asked_past_the_device_bound() {
    // Busy until just before its bound, then a probe that fails: the retry window would reach past the device's bound, and is cut
    let mut d = Device::new(0);
    assert_eq!(d.opened(Opened::Busy, BUSY_MS - 1), Next::OpenAgain);
    assert_eq!(d.opened(Opened::Ok, BUSY_MS - 1), Next::Probe);
    // (a probe that took long: it fails just before the device's bound)
    assert_eq!(
        d.probed(Probed::Unreadable, DEVICE_BOUND_MS - 1),
        Next::OpenAgain
    );
    assert_eq!(d.opened(Opened::Busy, DEVICE_BOUND_MS), Next::Ignore);
    let mut d = Device::new(0);
    assert_eq!(d.opened(Opened::Busy, 0), Next::OpenAgain);
    assert_eq!(d.opened(Opened::Ok, 0), Next::Probe);
    assert_eq!(d.probed(Probed::Unreadable, DEVICE_BOUND_MS), Next::Ignore);
}

#[test]
fn the_budgets_add_up() {
    assert_eq!(DEVICE_BOUND_MS, BUSY_MS + PROBE_RETRY_MS);
    const { assert!(READY_MS >= BUSY_MS) };
}
