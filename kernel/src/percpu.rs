//! Per-hart state (S5, spec section 3). `tp` holds `&PerHart` in kernel code and is never
//! restored from a trap frame on a return to the kernel; the user's `tp` lives in the trap frame,
//! and `sscratch` holds `&PerHart` while a hart is in user mode (0 in the kernel). A `PerHart` is
//! read only with interrupts off (or by its own hart in the trap path), so a thread that
//! migrates between harts never sees another hart's record through a stale pointer.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};

/// Most harts the kernel manages (QEMU `-smp` up to this).
pub const MAX_HARTS: usize = 64;

/// Why a hart was sent an IPI (bits of `PerHart::pending`).
pub const IPI_RESCHED: u32 = 1;
/// Run the hart's `work_kind` job in its IPI handler (tests of the lock).
pub const IPI_WORK: u32 = 2;
/// Open or retire interrupt identities in this hart's IMSIC file (IOMMU4, IOMMU-SPEC 8.1): the flag `aia::has_work` reads says what.
pub const IPI_IRQ: u32 = 4;

/// The lock-order checker's record of what a hart holds. Touched only by its own hart with interrupts off (X6).
pub struct HeldCell(core::cell::UnsafeCell<klock::Held>);

// SAFETY: only the owning hart reads or writes it, with interrupts off, so there is never a concurrent access.
unsafe impl Sync for HeldCell {}

impl HeldCell {
    pub fn with<R>(&self, f: impl FnOnce(&mut klock::Held) -> R) -> R {
        // SAFETY: see the `Sync` impl: one hart, interrupts off, no re-entry (the closure takes no locks).
        f(unsafe { &mut *self.0.get() })
    }
}

/// The states of a hart (PM1, KERNEL-V1-SPEC 5.3; the numbers are what `hart_info` returns).
pub const HART_OFFLINE: u8 = 0;
pub const HART_STARTING: u8 = 1;
pub const HART_ONLINE: u8 = 2;
pub const HART_DRAINING: u8 = 3;

/// The layout of the first three fields is used by the trap assembly (`arch/riscv64/trap.rs`, `arch/riscv64/usermode.rs`).
#[repr(C)]
pub struct PerHart {
    /// Offset 0: the top of the kernel stack the next trap from user mode will use; written on
    /// every return to user mode.
    pub kstack_top: AtomicU64,
    /// Offset 8: scratch for the user's `sp` while the trap entry switches stacks.
    pub scratch_sp: AtomicU64,
    /// Offset 16: the top of this hart's emergency stack, which the trap entry switches to when a trap
    /// is taken with `sp` inside a kernel stack's guard region (a kernel stack overflow).
    pub emergency_top: AtomicU64,
    /// Dense index of this hart (0 is the boot hart).
    pub index: AtomicUsize,
    /// The SBI hart id.
    pub hart_id: AtomicUsize,
    /// Set (release) once the hart is running kernel code.
    pub online: AtomicBool,
    /// The hart's state for `hart_info` (PM1): `HART_OFFLINE`, `HART_STARTING`, `HART_ONLINE`, `HART_DRAINING`. Only the hart itself finishes a transition.
    pub hart_state: AtomicU8,
    /// Timer interrupts this hart has taken.
    pub timer_ticks: AtomicU64,
    /// Inter-hart interrupts (IPIs) this hart has taken.
    pub ipis: AtomicU64,
    /// Is the hart in (or entering) user mode right now?
    pub in_user: AtomicBool,
    /// Set when this hart is part of the scheduler (its idle thread exists).
    pub in_sched: AtomicBool,
    /// Reschedule IPIs this hart has handled.
    pub resched_ipis: AtomicU64,
    /// Pending IPI reasons.
    pub pending: AtomicU32,
    /// The next timer deadline of this hart (`time` counter value).
    pub deadline: AtomicU64,
    /// The value of `tp` this hart saw when it bound itself (must equal this record's address).
    pub tp_seen: AtomicUsize,
    /// The nesting depth of the big kernel lock held on this hart (the lock follows the thread:
    /// `bkl::swap_depth`).
    pub bkl_depth: kcore::ticket::Depth,
    /// What this hart holds (a stack of lock classes), for the lock-order checker.
    pub held: HeldCell,
    /// Work for the hart's IPI handler (S5b tests): kind, argument, result and a done flag.
    pub work_kind: AtomicU32,
    pub work_arg: AtomicU64,
    pub work_result: AtomicU64,
    pub work_done: AtomicBool,
    /// The address-space token last installed for user mode on this hart (so a switch is made only when needed).
    pub active_aspace: AtomicU64,
    /// The process id of the user thread this hart is running (0: none), set where its address space is installed (`activate_current`) and
    /// so true for every return to user mode: the calls that run without the big kernel lock (`futex_wake`, L14) read it instead of the
    /// thread-to-process map.
    pub cur_pid: AtomicU64,
    /// The text range of that process's system library (AB8: zero when it has none): an `ecall` from user mode is allowed only from inside it.
    /// Written beside `cur_pid`.
    pub cur_lib_start: AtomicU64,
    pub cur_lib_end: AtomicU64,
    /// The address-space lock of that process (null: none): the page-fault path without the big lock reaches the space through it (L17 part c).
    /// Valid while this hart runs a thread of the process, which keeps the process (and its `Arc`) alive.
    pub cur_as: core::sync::atomic::AtomicPtr<klock::aslock::AsCell>,
    /// Times this hart took the big kernel lock from free (outermost takes): the measure of how often a hart needs it.
    pub bkl_takes: AtomicU64,
    /// How deep this hart is inside `heap::nofail` (class N: the allocator's reserve may be used, KT9). Touched with interrupts off.
    pub nofail_depth: AtomicU32,
    /// The thread this hart is running (its context's id). Written only by this hart; read by it without the big lock.
    pub running: AtomicU64,
    /// The thread this hart is switching away from (set just before the switch, taken by the `finish_switch` that completes it; 0:
    /// none): the thread that resumes here finds out whether the one it replaced has exited and must be reaped (SCHED-SMP-SPEC 3.6).
    pub prev: AtomicU64,
    /// A wake or priority change on this hart designated another thread to run here: the switch happens at the next defined point
    /// (`thread::preempt_point`), never inside the wake (SCHED-SMP-SPEC X7). Written and read by this hart only, interrupts off.
    pub need_resched: AtomicBool,
    /// T206: this hart is inside `irq::window()` (a syscall's interrupts-on gap); a trap taken then only sets `need_resched` (never switches or grows the heap on the syscall's stack).
    pub syscall_window: AtomicBool,
    /// The attention epoch (`user::ATTENTION`) this hart last saw at a return to user mode: a trap from user mode that finds it
    /// unchanged has no kill or stop to look for and needs no big lock (L12b).
    pub attention_seen: AtomicU64,
    /// Idle accounting (PM2): entries into `wfi`, entries into the firmware's retentive suspend (a long idle), and the nanoseconds spent idle.
    pub idle_wfi: AtomicU64,
    pub idle_suspend: AtomicU64,
    pub idle_ns: AtomicU64,
    /// The thread whose latest FP (resp. vector) state this hart's register file holds, 0 for none (PL3, T166): written under the big lock by the
    /// load and save paths of `arch::fp`, read by this hart alone on its way back to user mode, so a thread that has used a unit and is
    /// alone on its hart returns without taking the lock. A claim is cleared when another hart saves or loads the thread's state.
    pub fs_claim: AtomicU64,
    pub vs_claim: AtomicU64,
    /// Where the claimed thread's saved registers are (the address of its box), so the trap paths that take no lock can save into it.
    pub fs_buf: AtomicUsize,
    pub vs_buf: AtomicUsize,
    /// Spin iterations in the current wait for a lock (the deadlock detector's counter).
    pub spins: AtomicU64,
    /// The lock class this hart last asked for (diagnostics for the deadlock detector).
    pub last_class: AtomicU32,
    /// This hart's magazines of free slab objects, one per size class (MM4): touched only by this hart with interrupts off.
    pub magazines: Magazines,
    /// The newest ASID generation this hart has flushed for (L18, `kcore::asid::HartGen`): it flushes everything before it installs an address
    /// space of a newer generation.
    pub asid_seen: AtomicU64,
    /// The thread and the address-space tag (generation and ASID, packed) whose `UserBinding` this hart last wrote (L19): the full path's return writes it
    /// only when either changes, so a syscall that returns to the same thread under the same tag does not take the thread table.
    pub bound_tid: AtomicU64,
    pub bound_tag: AtomicU64,
    /// This hart's cache of free frames (L16): touched only by this hart with interrupts off.
    pub frame_cache: FrameCacheCell,
}

/// One hart's frame cache (`kcore::fcache`, L16). The cache itself is touched by the owning hart alone with interrupts off; its length is
/// mirrored in an atomic so the leak oracle on another hart can count the frames it holds.
pub struct FrameCacheCell {
    cache: core::cell::UnsafeCell<kcore::fcache::FrameCache>,
    len: AtomicUsize,
}

// SAFETY: `cache` is reached only through `with`, whose contract is that the owning hart calls it with interrupts off; `len` is atomic.
unsafe impl Sync for FrameCacheCell {}

impl FrameCacheCell {
    const fn new() -> FrameCacheCell {
        FrameCacheCell {
            cache: core::cell::UnsafeCell::new(kcore::fcache::FrameCache::new()),
            len: AtomicUsize::new(0),
        }
    }

    /// Frames cached now (any hart may read it).
    pub fn len(&self) -> usize {
        self.len.load(Ordering::Relaxed)
    }

    /// Run `f` on the cache.
    ///
    /// # Safety
    /// The calling hart must be the owner of this record, with interrupts off.
    pub unsafe fn with<R>(&self, f: impl FnOnce(&mut kcore::fcache::FrameCache) -> R) -> R {
        // SAFETY: the caller owns the cache with interrupts off, so nothing else touches it.
        let c = unsafe { &mut *self.cache.get() };
        let r = f(c);
        self.len.store(c.len(), Ordering::Relaxed);
        r
    }
}

/// Free objects a hart keeps per size class (MM4): the common allocation and free take no shared lock.
pub const MAGAZINE_SIZE: usize = 32;
/// Size classes of the slab (`kcore::slab::CLASSES`).
const MAGAZINE_CLASSES: usize = kcore::slab::CLASSES;

/// Objects a magazine of `class` holds at most: 32, but only 4 for the big classes (1024 and 2048 bytes), whose objects take a quarter to a whole
/// frame each, so an idle hart does not pin dozens of frames.
pub fn magazine_limit(class: usize) -> usize {
    if class >= 6 {
        4
    } else {
        MAGAZINE_SIZE
    }
}

/// One hart's magazines. The object addresses are touched by the owning hart alone, with interrupts off (so a timer interrupt cannot
/// re-enter); the counts are atomics so a leak check on another hart can read them.
pub struct Magazines {
    objects: core::cell::UnsafeCell<[[usize; MAGAZINE_SIZE]; MAGAZINE_CLASSES]>,
    len: [AtomicU8; MAGAZINE_CLASSES],
}

// SAFETY: `objects` is accessed only through the `unsafe` methods below, whose contract is that the owning hart calls them with interrupts
// off; `len` is atomic.
unsafe impl Sync for Magazines {}

impl Magazines {
    #[allow(clippy::large_stack_frames)] // KT13: a const fn that builds a static at compile time; no run-time frame
    const fn new() -> Magazines {
        Magazines {
            objects: core::cell::UnsafeCell::new([[0; MAGAZINE_SIZE]; MAGAZINE_CLASSES]),
            len: [const { AtomicU8::new(0) }; MAGAZINE_CLASSES],
        }
    }

    /// Objects in the magazine of `class` (any hart may read it).
    pub fn len(&self, class: usize) -> usize {
        self.len
            .get(class)
            .map_or(0, |l| usize::from(l.load(Ordering::Relaxed)))
    }

    /// Take the most recently freed object of `class`.
    ///
    /// # Safety
    /// The calling hart must be the owner of this record, with interrupts off.
    pub unsafe fn pop(&self, class: usize) -> Option<usize> {
        let n = self.len(class).checked_sub(1)?;
        // SAFETY: the caller owns the magazines with interrupts off, so nothing else touches `objects`.
        let objects = unsafe { &mut *self.objects.get() };
        let p = *objects.get(class)?.get(n)?;
        self.len.get(class)?.store(n as u8, Ordering::Relaxed);
        Some(p)
    }

    /// Put a freed object in the magazine of `class`; `false` if it is full.
    ///
    /// # Safety
    /// As for [`Magazines::pop`].
    pub unsafe fn push(&self, class: usize, object: usize) -> bool {
        let n = self.len(class);
        if n >= magazine_limit(class) {
            return false;
        }
        // SAFETY: as in `pop`.
        let objects = unsafe { &mut *self.objects.get() };
        let Some(slot) = objects.get_mut(class).and_then(|m| m.get_mut(n)) else {
            return false;
        };
        *slot = object;
        if let Some(l) = self.len.get(class) {
            l.store((n + 1) as u8, Ordering::Relaxed);
        }
        true
    }
}

impl PerHart {
    #[allow(clippy::large_stack_frames)] // KT13: a const fn that builds a static at compile time; no run-time frame
    const fn new() -> PerHart {
        PerHart {
            kstack_top: AtomicU64::new(0),
            scratch_sp: AtomicU64::new(0),
            emergency_top: AtomicU64::new(0),
            index: AtomicUsize::new(0),
            hart_id: AtomicUsize::new(0),
            online: AtomicBool::new(false),
            hart_state: AtomicU8::new(HART_OFFLINE),
            timer_ticks: AtomicU64::new(0),
            ipis: AtomicU64::new(0),
            in_user: AtomicBool::new(false),
            in_sched: AtomicBool::new(false),
            resched_ipis: AtomicU64::new(0),
            pending: AtomicU32::new(0),
            deadline: AtomicU64::new(0),
            tp_seen: AtomicUsize::new(0),
            bkl_depth: kcore::ticket::Depth::new(),
            held: HeldCell(core::cell::UnsafeCell::new(klock::Held::new())),
            work_kind: AtomicU32::new(0),
            work_arg: AtomicU64::new(0),
            work_result: AtomicU64::new(0),
            work_done: AtomicBool::new(false),
            active_aspace: AtomicU64::new(0),
            cur_pid: AtomicU64::new(0),
            cur_lib_start: AtomicU64::new(0),
            cur_lib_end: AtomicU64::new(0),
            cur_as: core::sync::atomic::AtomicPtr::new(core::ptr::null_mut()),
            bkl_takes: AtomicU64::new(0),
            nofail_depth: AtomicU32::new(0),
            running: AtomicU64::new(0),
            prev: AtomicU64::new(0),
            need_resched: AtomicBool::new(false),
            syscall_window: AtomicBool::new(false),
            attention_seen: AtomicU64::new(0),
            idle_wfi: AtomicU64::new(0),
            idle_suspend: AtomicU64::new(0),
            idle_ns: AtomicU64::new(0),
            fs_claim: AtomicU64::new(0),
            vs_claim: AtomicU64::new(0),
            fs_buf: AtomicUsize::new(0),
            vs_buf: AtomicUsize::new(0),
            spins: AtomicU64::new(0),
            last_class: AtomicU32::new(0),
            magazines: Magazines::new(),
            asid_seen: AtomicU64::new(0),
            bound_tid: AtomicU64::new(u64::MAX),
            bound_tag: AtomicU64::new(0),
            frame_cache: FrameCacheCell::new(),
        }
    }
}

static HARTS: [PerHart; MAX_HARTS] = [const { PerHart::new() }; MAX_HARTS];

/// One emergency stack per hart (8 KiB: enough to print the overflow report and stop).
pub const EMERGENCY_BYTES: usize = 8192;
#[repr(align(16))]
struct Emergency(#[allow(dead_code)] [u8; EMERGENCY_BYTES]);
// (`static mut`, so it lives in writable memory: an immutable all-zero static would be placed in `.rodata`)
static mut EMERGENCY: [Emergency; MAX_HARTS] =
    [const { Emergency([0; EMERGENCY_BYTES]) }; MAX_HARTS];

/// The record of hart `index`.
pub fn hart(index: usize) -> &'static PerHart {
    &HARTS[index]
}

/// The calling hart's record, through `tp`.
pub fn this() -> &'static PerHart {
    // `tp` holds `&PerHart` in kernel code (`bind`).
    let p = crate::arch::thread_pointer();
    debug_assert!(
        HARTS.as_ptr_range().contains(&(p as *const PerHart)),
        "tp does not point at a PerHart"
    );
    // SAFETY: `tp` was set by `bind` to one of the `HARTS` entries, which live for ever.
    unsafe { &*(p as *const PerHart) }
}

/// Make the calling hart's `tp` point at its record and fill the record in. Called first thing on
/// every hart, before it can take a trap.
pub fn bind(index: usize, hart_id: usize) {
    let h = &HARTS[index];
    h.index.store(index, Ordering::Relaxed);
    h.hart_id.store(hart_id, Ordering::Relaxed);
    h.emergency_top.store(
        // SAFETY: only the address of the hart's own emergency stack is taken; nothing is read or written.
        unsafe { core::ptr::addr_of_mut!(EMERGENCY[index]) } as u64 + EMERGENCY_BYTES as u64,
        Ordering::Relaxed,
    );
    // SAFETY: `h` is this hart's own record, which lives for ever.
    unsafe { crate::arch::set_thread_pointer(h as *const PerHart as usize) };
}
