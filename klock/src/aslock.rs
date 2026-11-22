//! The address-space lock (`AS`, SCHED-SMP-SPEC 3.3, L17): one lock per process around its `AddressSpace`, in a type that does not
//! depend on the machine (`Process` lives in the host-testable syscall layer, which cannot name the kernel's machine type). The machine is
//! reached through [`Hooks`], installed once at boot by the kernel; before that (and in the host tests) the hooks are no-ops for one hart.
//!
//! Two ways in: [`AsCell::lock`] takes the lock for a stretch of code (the fault path, which holds nothing else), and
//! [`AsCell::held_mut`] reaches the value of a lock **the caller took earlier** (a syscall takes the address spaces it touches before the
//! memory locks, in the order of 3.3, and the pure code that runs under them reads them through this): it checks that this hart holds
//! the lock (always, once the kernel has installed its hooks) and hands out the value. The cell is shared by `Arc`, so the borrow checker does
//! **not** keep the references unique: the pure code makes one reference at a time and drops it before asking for another, as the kernel does
//! with `state()` (`work/T214.md` files the stricter shape).

use crate::{Class, Guard, Machine, Spin};
extern crate alloc;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicPtr, Ordering};
use kcore::aspace::AddressSpace;

/// What the lock needs from the machine, as plain functions.
pub struct Hooks {
    pub irq_save: fn() -> bool,
    pub irq_restore: fn(bool),
    pub hart: fn() -> u32,
    pub relax: fn(),
    pub acquired: fn(Class, u32),
    pub released: fn(Class, u32),
}

fn no_irq() -> bool {
    false
}
fn no_restore(_: bool) {}
fn hart0() -> u32 {
    0
}
fn nothing() {}
fn no_note(_: Class, _: u32) {}

/// The hooks before the kernel installs its own: one hart, nothing to disable, no checker (the host tests).
static DEFAULT: Hooks = Hooks {
    irq_save: no_irq,
    irq_restore: no_restore,
    hart: hart0,
    relax: nothing,
    acquired: no_note,
    released: no_note,
};

static HOOKS: AtomicPtr<Hooks> = AtomicPtr::new(core::ptr::null_mut());

/// Install the machine's hooks (once, at boot, before any address-space lock is taken).
pub fn install(h: &'static Hooks) {
    HOOKS.store(h as *const Hooks as *mut Hooks, Ordering::Release);
}

fn hooks() -> &'static Hooks {
    let p = HOOKS.load(Ordering::Acquire);
    // SAFETY: the pointer is null or was made from a `&'static Hooks` in `install`.
    unsafe { p.as_ref() }.unwrap_or(&DEFAULT)
}

/// A `Machine` whose operations are the installed hooks.
pub struct HookMachine;

impl Machine for HookMachine {
    fn irq_save() -> bool {
        (hooks().irq_save)()
    }
    fn irq_restore(was: bool) {
        (hooks().irq_restore)(was)
    }
    fn hart() -> u32 {
        (hooks().hart)()
    }
    fn relax() {
        (hooks().relax)()
    }
    fn acquired(class: Class, instance: u32) {
        (hooks().acquired)(class, instance)
    }
    fn released(class: Class, instance: u32) {
        (hooks().released)(class, instance)
    }
}

/// One process's address space behind its `AS` lock (the instance is the process id).
pub struct AsCell {
    lock: Spin<AddressSpace, HookMachine>,
    /// The page-table root (it never changes while the space lives): the context switch reads it with no lock.
    root: u64,
}

impl AsCell {
    pub fn new(pid: u64, aspace: AddressSpace) -> AsCell {
        AsCell {
            root: aspace.table().root(),
            lock: Spin::new(Class::As, pid as u32, aspace),
        }
    }

    /// The page-table root of the space: fixed for its life, readable with no lock.
    pub fn root(&self) -> u64 {
        self.root
    }

    /// The value of a cell nobody else holds (a process being torn down: its threads are gone). Panics if the `Arc` is shared.
    #[allow(clippy::panic)] // INVARIANT: a process is torn down only after its threads are gone, and no syscall holds its lock then: a shared cell is a kernel bug, and the panic names it
    pub fn take(this: Arc<AsCell>) -> AddressSpace {
        match Arc::try_unwrap(this) {
            Ok(cell) => cell.lock.into_inner(),
            Err(_) => panic!("an address space torn down while another holder has it"),
        }
    }

    /// Take the lock (interrupts off while it is held; the order checker sees it).
    #[track_caller]
    pub fn lock(&self) -> Guard<'_, AddressSpace, HookMachine> {
        self.lock.lock()
    }

    /// Is this hart holding the lock?
    pub fn held(&self) -> bool {
        // (before the kernel installs its hooks there is no machine to ask: the host tests, which run one thread of control, are not asked)
        HOOKS.load(Ordering::Acquire).is_null() || self.lock.held_by_this_hart()
    }

    /// The value, for code that runs under a lock this hart took earlier. Panics if this hart does not hold it: a syscall that reaches an
    /// address space it did not lock is a bug the first run finds.
    #[track_caller]
    #[allow(clippy::mut_from_ref)] // INVARIANT: the lock is held by this hart (asserted), which is what makes the `&mut` unique; the kernel's discipline for `state()`
    pub fn held_mut(&self) -> &mut AddressSpace {
        assert!(self.held(), "an address space reached without its AS lock");
        // SAFETY: this hart holds the lock (checked above), so no other hart reaches the value. Within the hart the discipline is the
        // kernel's own for `state()`: the pure code that runs under the lock makes one reference at a time and drops it before it asks
        // for another (a reference kept across a second `held_mut` of the same cell would alias).
        unsafe { &mut *self.lock.value_ptr() }
    }

    /// The value for reading, under a lock this hart took earlier (as `held_mut`).
    #[track_caller]
    pub fn held_ref(&self) -> &AddressSpace {
        assert!(self.held(), "an address space reached without its AS lock");
        // SAFETY: this hart holds the lock, so no other hart writes the value; the caller does not keep it across a `held_mut` of the
        // same cell (the discipline of `held_mut`).
        unsafe { &*self.lock.value_ptr() }
    }

    /// The value with no lock held and no check: **only** the negative control `fault-skip-as-lock` of the kernel uses it, to show that the
    /// guarding test fails when the page-fault path does not take the lock.
    #[allow(clippy::mut_from_ref)] // (the negative control's point: it breaks the rule that makes the reference unique)
    pub fn unchecked_mut_for_negative_control(&self) -> &mut AddressSpace {
        // SAFETY: none: this is the deliberate breach, built only in the negative-control kernel.
        unsafe { &mut *self.lock.value_ptr() }
    }

    /// Take the lock of a cell shared by `Arc`, returning a holder that keeps the cell alive and releases the lock when it is dropped.
    /// The hold is not tied to a borrow, so a syscall can keep the locks of the address spaces it touches while the pure code takes the
    /// processes out of the table as it likes.
    #[track_caller]
    pub fn hold(this: &Arc<AsCell>) -> AsHold {
        let cell = this.clone();
        let guard = cell.lock.lock();
        // SAFETY: the guard borrows `cell.lock`, which lives in the `Arc`'s heap allocation; the `Arc` is kept in the same `AsHold`, after
        // the guard, so the allocation outlives the guard (fields drop in declaration order) and never moves.
        let guard: Guard<'static, AddressSpace, HookMachine> =
            unsafe { core::mem::transmute(guard) };
        AsHold {
            _guard: guard,
            _cell: cell,
        }
    }
}

/// A held `AS` lock (see [`AsCell::hold`]): the lock is released, and then the cell let go, when this is dropped.
pub struct AsHold {
    _guard: Guard<'static, AddressSpace, HookMachine>,
    _cell: Arc<AsCell>,
}
