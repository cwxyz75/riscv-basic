//! The spinlock on the host: exclusion, the interrupt state, the checker hooks, try_lock.
#![cfg(not(any(loom, shuttle)))]
use klock::{Class, Machine, Spin};
use std::cell::RefCell;
use std::sync::atomic::{AtomicU32, Ordering};

thread_local! {
    static IRQ_ON: RefCell<bool> = const { RefCell::new(true) };
    static LOG: RefCell<Vec<(&'static str, Class, u32)>> = const { RefCell::new(Vec::new()) };
}

struct Host;
impl Machine for Host {
    fn irq_save() -> bool {
        IRQ_ON.with(|i| i.replace(false))
    }
    fn irq_restore(was: bool) {
        IRQ_ON.with(|i| *i.borrow_mut() = was);
    }
    fn hart() -> u32 {
        // one id per OS thread
        static NEXT: AtomicU32 = AtomicU32::new(0);
        thread_local!(static ME: u32 = NEXT.fetch_add(1, Ordering::Relaxed));
        ME.with(|m| *m)
    }
    fn relax() {
        std::thread::yield_now();
    }
    fn acquired(class: Class, instance: u32) {
        LOG.with(|l| l.borrow_mut().push(("take", class, instance)));
    }
    fn released(class: Class, instance: u32) {
        LOG.with(|l| l.borrow_mut().push(("drop", class, instance)));
    }
}

#[test]
fn the_guard_gives_exclusive_access_and_restores_interrupts() {
    let lock: Spin<u32, Host> = Spin::new(Class::Jobs, 0, 5);
    assert!(IRQ_ON.with(|i| *i.borrow()));
    {
        let mut g = lock.lock();
        assert!(
            !IRQ_ON.with(|i| *i.borrow()),
            "interrupts are off while held"
        );
        *g += 1;
        assert!(
            lock.try_lock().is_none(),
            "held: try_lock fails (and does not leave interrupts off)"
        );
        assert!(!IRQ_ON.with(|i| *i.borrow()));
    }
    assert!(
        IRQ_ON.with(|i| *i.borrow()),
        "interrupts are back on after the drop"
    );
    assert!(lock.is_free(), "the drop released the lock"); // (before the blocking take below: a missing unlock fails here, not by hanging)
    assert_eq!(*lock.lock(), 6);
    assert!(lock.is_free());
}

#[test]
fn try_lock_takes_a_free_lock_and_is_free_says_so() {
    // (KT10: the mutants "try_lock gives None", "delete !" in it and "is_free gives true" survived the first mutation run)
    LOG.with(|l| l.borrow_mut().clear());
    let lock: Spin<u32, Host> = Spin::new(Class::Jobs, 7, 1);
    assert!(lock.is_free(), "a new lock is free");
    {
        let mut g = lock.try_lock().expect("try_lock takes a lock nobody holds");
        assert!(!lock.is_free(), "held: not free");
        assert!(
            !IRQ_ON.with(|i| *i.borrow()),
            "interrupts are off while a try_lock guard lives"
        );
        *g += 1;
        assert!(
            lock.try_lock().is_none(),
            "a second try_lock fails while held"
        );
        assert!(
            !IRQ_ON.with(|i| *i.borrow()),
            "and the failed one leaves interrupts as they were (off: the guard is alive)"
        );
    }
    // (checked before any blocking take: a drop that did not unlock fails here instead of hanging the test)
    assert!(lock.is_free(), "the drop of the guard releases the lock");
    assert!(IRQ_ON.with(|i| *i.borrow()), "and gives interrupts back");
    let log = LOG.with(|l| l.borrow().clone());
    assert_eq!(
        log,
        vec![("take", Class::Jobs, 7), ("drop", Class::Jobs, 7)],
        "the checker hears a try_lock like any take (the failed one is not a take)"
    );
    assert_eq!(*lock.try_lock().expect("free again"), 2);
}

#[test]
fn the_checker_hears_every_take_and_release() {
    LOG.with(|l| l.borrow_mut().clear());
    let a: Spin<(), Host> = Spin::new(Class::Obj, 0, ());
    let b: Spin<(), Host> = Spin::new(Class::Rq, 3, ());
    {
        let _ga = a.lock();
        let _gb = b.lock();
    }
    let log = LOG.with(|l| l.borrow().clone());
    assert_eq!(
        log,
        vec![
            ("take", Class::Obj, 0),
            ("take", Class::Rq, 3),
            ("drop", Class::Rq, 3),
            ("drop", Class::Obj, 0)
        ]
    );
}

#[test]
#[should_panic(expected = "recursive take")]
fn a_recursive_take_is_a_bug() {
    let l: Spin<(), Host> = Spin::new(Class::Vmo, 0, ());
    let _a = l.lock();
    let _b = l.lock();
}

#[test]
fn many_threads_count_correctly() {
    use std::sync::Arc;
    let lock: Arc<Spin<u64, Host>> = Arc::new(Spin::new(Class::Frame, 0, 0));
    let hs: Vec<_> = (0..4)
        .map(|_| {
            let l = lock.clone();
            std::thread::spawn(move || {
                for _ in 0..(if cfg!(miri) { 20 } else { 2000 }) {
                    *l.lock() += 1;
                }
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(*lock.lock(), 4 * if cfg!(miri) { 20 } else { 2000 });
}
